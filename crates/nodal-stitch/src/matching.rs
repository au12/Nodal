//! Descriptor matching and geometric verification.
//!
//! Panorama shots share (approximately) one viewpoint, so two overlapping photos are related by a
//! pure 3-D rotation once pixels are turned into rays through the lens model. Verifying matches
//! with that model works for any lens — rectilinear or fisheye — unlike homographies.

use crate::features::{FeatureSet, DESC_LEN};
use crate::lens::{LensModel, V3};
use nalgebra::{Matrix3, Rotation3};
use rayon::prelude::*;

#[inline]
fn dist2(a: &[u8], b: &[u8]) -> u32 {
    let mut acc = 0u32;
    for (x, y) in a.iter().zip(b) {
        let d = *x as i32 - *y as i32;
        acc += (d * d) as u32;
    }
    acc
}

/// A putative correspondence: feature index in each set.
#[derive(Clone, Copy, Debug)]
pub struct RawMatch {
    pub a: u32,
    pub b: u32,
    pub dist: u32,
}

/// Nearest-neighbour matching with Lowe's ratio test; each feature in `b` is used at most once.
pub fn match_descriptors(fa: &FeatureSet, fb: &FeatureSet, ratio: f32) -> Vec<RawMatch> {
    if fa.len() < 2 || fb.len() < 2 {
        return Vec::new();
    }
    let ratio2 = ratio * ratio;
    let cands: Vec<RawMatch> = fa
        .desc
        .par_chunks(DESC_LEN)
        .enumerate()
        .filter_map(|(ia, da)| {
            let (mut best, mut second, mut best_i) = (u32::MAX, u32::MAX, 0u32);
            for (ib, db) in fb.desc.chunks_exact(DESC_LEN).enumerate() {
                let d = dist2(da, db);
                if d < best {
                    second = best;
                    best = d;
                    best_i = ib as u32;
                } else if d < second {
                    second = d;
                }
            }
            ((best as f32) < ratio2 * second as f32).then_some(RawMatch { a: ia as u32, b: best_i, dist: best })
        })
        .collect();
    // One-to-one: keep the closest match per `b` feature.
    let mut best_for_b: Vec<Option<RawMatch>> = vec![None; fb.len()];
    for m in cands {
        let slot = &mut best_for_b[m.b as usize];
        if slot.is_none_or(|s| m.dist < s.dist) {
            *slot = Some(m);
        }
    }
    let mut out: Vec<RawMatch> = best_for_b.into_iter().flatten().collect();
    out.sort_by_key(|m| m.a);
    out
}

/// Guided matching: for each feature in `a`, only consider features of `b` within `radius` pixels of
/// its predicted position. With geometry known the ratio test can be far more permissive.
pub fn match_guided(
    fa: &FeatureSet,
    fb: &FeatureSet,
    predict: impl Fn(f32, f32) -> Option<(f32, f32)> + Sync,
    radius: f32,
    ratio: f32,
    max_dist: u32,
) -> Vec<RawMatch> {
    if fa.is_empty() || fb.is_empty() {
        return Vec::new();
    }
    // Bucket grid over b.
    let cell = radius.max(8.0);
    let (mut max_x, mut max_y) = (0.0f32, 0.0f32);
    for f in &fb.feats {
        max_x = max_x.max(f.x);
        max_y = max_y.max(f.y);
    }
    let gw = (max_x / cell) as usize + 1;
    let gh = (max_y / cell) as usize + 1;
    let mut grid: Vec<Vec<u32>> = vec![Vec::new(); gw * gh];
    for (i, f) in fb.feats.iter().enumerate() {
        grid[(f.y / cell) as usize * gw + (f.x / cell) as usize].push(i as u32);
    }
    let ratio2 = ratio * ratio;
    let r2 = radius * radius;
    let cands: Vec<RawMatch> = fa
        .feats
        .par_iter()
        .enumerate()
        .filter_map(|(ia, f)| {
            let (px, py) = predict(f.x, f.y)?;
            if px < -radius || py < -radius || px > max_x + radius || py > max_y + radius {
                return None;
            }
            let da = fa.descriptor(ia);
            let (mut best, mut second, mut best_i) = (u32::MAX, u32::MAX, 0u32);
            let cx0 = ((px - radius) / cell).floor().max(0.0) as usize;
            let cy0 = ((py - radius) / cell).floor().max(0.0) as usize;
            let cx1 = (((px + radius) / cell) as usize).min(gw - 1);
            let cy1 = (((py + radius) / cell) as usize).min(gh - 1);
            for cy in cy0..=cy1 {
                for cx in cx0..=cx1 {
                    for &ib in &grid[cy * gw + cx] {
                        let g = &fb.feats[ib as usize];
                        if (g.x - px).powi(2) + (g.y - py).powi(2) > r2 {
                            continue;
                        }
                        let d = dist2(da, fb.descriptor(ib as usize));
                        if d < best {
                            second = best;
                            best = d;
                            best_i = ib;
                        } else if d < second {
                            second = d;
                        }
                    }
                }
            }
            (best < max_dist && (second == u32::MAX || (best as f32) < ratio2 * second as f32))
                .then_some(RawMatch { a: ia as u32, b: best_i, dist: best })
        })
        .collect();
    let mut best_for_b: Vec<Option<RawMatch>> = vec![None; fb.len()];
    for m in cands {
        let slot = &mut best_for_b[m.b as usize];
        if slot.is_none_or(|s| m.dist < s.dist) {
            *slot = Some(m);
        }
    }
    let mut out: Vec<RawMatch> = best_for_b.into_iter().flatten().collect();
    out.sort_by_key(|m| m.a);
    out
}

/// Small deterministic PRNG (xorshift64*), so results are reproducible run to run.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Rng(seed.wrapping_mul(0x9E3779B97F4A7C15) | 1)
    }

    #[inline]
    pub fn next_u64(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545F4914F6CDD1D)
    }

    #[inline]
    pub fn below(&mut self, n: usize) -> usize {
        (self.next_u64() >> 11) as usize % n.max(1)
    }

    #[inline]
    pub fn unit(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }
}

/// Least-squares rotation `R` with `a ≈ R·b` (Kabsch).
pub fn kabsch(pairs: impl Iterator<Item = (V3, V3)>) -> Option<Rotation3<f64>> {
    let mut h = Matrix3::<f64>::zeros();
    let mut n = 0;
    for (a, b) in pairs {
        h += a * b.transpose();
        n += 1;
    }
    if n < 2 {
        return None;
    }
    let svd = h.svd(true, true);
    let (u, vt) = (svd.u?, svd.v_t?);
    let mut d = Matrix3::identity();
    if (u * vt).determinant() < 0.0 {
        d[(2, 2)] = -1.0;
    }
    Some(Rotation3::from_matrix_unchecked(u * d * vt))
}

fn rotation_from_two(a1: V3, a2: V3, b1: V3, b2: V3) -> Option<Rotation3<f64>> {
    let frame = |p: V3, q: V3| {
        let n = p.cross(&q);
        let len = n.norm();
        if len < 1e-6 {
            return None;
        }
        let e2 = n / len;
        let e3 = p.cross(&e2);
        Some(Matrix3::from_columns(&[p, e2, e3]))
    };
    let fa = frame(a1, a2)?;
    let fb = frame(b1, b2)?;
    Some(Rotation3::from_matrix_unchecked(fa * fb.transpose()))
}

#[derive(Clone, Debug)]
pub struct RotationFit {
    /// Rotation taking camera-b rays into camera a: `ray_a ≈ r · ray_b`.
    pub r: Rotation3<f64>,
    /// Indices into the input match list.
    pub inliers: Vec<usize>,
}

/// RANSAC for a pure rotation between two views. `rays` are unit rays (camera a, camera b);
/// `tol` is the inlier threshold in radians.
pub fn ransac_rotation(rays: &[(V3, V3)], tol: f64, seed: u64) -> Option<RotationFit> {
    let n = rays.len();
    if n < 4 {
        return None;
    }
    let cos_tol = tol.cos();
    let count = |r: &Rotation3<f64>| rays.iter().filter(|(a, b)| a.dot(&(r * b)) > cos_tol).count();
    let mut rng = Rng::new(seed ^ n as u64);
    let mut best: Option<(usize, Rotation3<f64>)> = None;
    let mut iters = 600usize;
    let mut i = 0;
    while i < iters {
        i += 1;
        let (i1, i2) = (rng.below(n), rng.below(n));
        if i1 == i2 {
            continue;
        }
        let (a1, b1) = rays[i1];
        let (a2, b2) = rays[i2];
        // The angle between the two points must agree in both views.
        if (a1.dot(&a2) - b1.dot(&b2)).abs() > 4.0 * tol {
            continue;
        }
        let Some(r) = rotation_from_two(a1, a2, b1, b2) else { continue };
        let c = count(&r);
        if best.as_ref().is_none_or(|(bc, _)| c > *bc) {
            best = Some((c, r));
            // Adaptive stopping: enough iterations to find an all-inlier pair with 99.9 % confidence.
            let w = (c as f64 / n as f64).max(0.02);
            let needed = ((1.0f64 - 0.999).ln() / (1.0 - w * w).ln()).ceil() as usize;
            iters = iters.min(needed.max(60) * 2);
        }
    }
    let (_, mut r) = best?;
    // Local optimisation: re-fit on inliers a few times.
    for _ in 0..4 {
        let inliers: Vec<usize> = (0..n).filter(|&k| rays[k].0.dot(&(r * rays[k].1)) > cos_tol).collect();
        if inliers.len() < 3 {
            break;
        }
        match kabsch(inliers.iter().map(|&k| rays[k])) {
            Some(nr) => r = nr,
            None => break,
        }
    }
    let inliers = (0..n).filter(|&k| rays[k].0.dot(&(r * rays[k].1)) > cos_tol).collect();
    Some(RotationFit { r, inliers })
}

/// Converts raw matches to ray pairs through the two lens models.
pub fn rays_for(fa: &FeatureSet, fb: &FeatureSet, matches: &[RawMatch], la: &LensModel, lb: &LensModel) -> Vec<(V3, V3)> {
    matches
        .iter()
        .map(|m| {
            let (pa, pb) = (&fa.feats[m.a as usize], &fb.feats[m.b as usize]);
            (la.pixel_to_ray(pa.x as f64, pa.y as f64), lb.pixel_to_ray(pb.x as f64, pb.y as f64))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ransac_recovers_rotation_with_outliers() {
        let truth = Rotation3::from_euler_angles(0.2, -0.5, 0.1);
        let mut rng = Rng::new(7);
        let mut rays = Vec::new();
        for i in 0..200 {
            let b = V3::new(rng.unit() - 0.5, rng.unit() - 0.5, 1.0).normalize();
            let a = if i % 3 == 0 {
                V3::new(rng.unit() - 0.5, rng.unit() - 0.5, rng.unit() - 0.5).normalize()
            } else {
                (truth * b + V3::new(rng.unit() - 0.5, rng.unit() - 0.5, rng.unit() - 0.5) * 2e-4).normalize()
            };
            rays.push((a, b));
        }
        let fit = ransac_rotation(&rays, 0.002, 1).unwrap();
        assert!(fit.inliers.len() >= 125 && fit.inliers.len() <= 140, "{}", fit.inliers.len());
        assert!(fit.r.angle_to(&truth) < 1e-3);
    }
}
