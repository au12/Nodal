//! Pole patching: fills the gaps a panorama has straight up (zenith) and straight down (nadir).
//!
//! The gap is synthesised in a pole-centred view, where it is a compact disc surrounded by real
//! image: a top-down view of the ground or a straight-up view of the sky. The image is split into
//! a smooth *base* (lighting and colour gradients, continued across the gap by smooth
//! interpolation) and *detail* (texture), and the detail is synthesised from the surroundings with
//! multi-scale PatchMatch. Adding the two gives a patch whose brightness flows continuously into
//! the photo and whose texture is made of the scene's own material. The result is written back at
//! full resolution by re-sampling the panorama through the patch correspondence, so the fill is as
//! sharp as its source.

use crate::blend::{expand, extrapolated_pyramid, reduce, Plane};
use crate::lens::V3;
use crate::matching::Rng;
use crate::render::Panorama;
use crate::synth::sample_bicubic;
use crate::Progress;
use rayon::prelude::*;
use std::f64::consts::{PI, TAU};
use std::sync::atomic::{AtomicU32, Ordering};

#[derive(Clone, Copy, Debug)]
pub struct FillOptions {
    /// Patch at least this many degrees around the zenith even if covered (0 = only real gaps).
    pub zenith_patch: f32,
    /// Same for the nadir, e.g. to paint out a tripod.
    pub nadir_patch: f32,
    /// Gaps wider than this (degrees from the pole) are left alone: that is a partial panorama, not a pole gap.
    pub max_gap: f32,
    /// Side of the working view in pixels; the fill is synthesised at this size and re-sampled at full resolution.
    pub work_size: usize,
}

impl Default for FillOptions {
    fn default() -> Self {
        FillOptions { zenith_patch: 0.0, nadir_patch: 0.0, max_gap: 62.0, work_size: 1400 }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct FillReport {
    /// Radius of the filled cap in degrees, if a patch was made.
    pub zenith: Option<f32>,
    pub nadir: Option<f32>,
    /// A gap was found but it is too large to patch.
    pub zenith_too_large: bool,
    pub nadir_too_large: bool,
}

const HALF: isize = 3; // 7×7 patches
/// Shift allowed for fully structured material (8-bit levels); smooth material follows the base freely.
const MIN_SHIFT: f32 = 12.0;

/// The part of a base difference that is *not* applied to borrowed material. `rho` says how
/// structured the source is: smooth material (sky, evenly lit ground; rho → 0) follows the base
/// exactly, strongly structured material (rho → 1) keeps its own tones rather than being washed
/// towards the local average.
#[inline]
fn excess_shift(delta: [f32; 3], rho: f32) -> [f32; 3] {
    let allowed = MIN_SHIFT + (1.0 - rho) * (1.0 - rho) * 400.0;
    let m = delta[0].abs().max(delta[1].abs()).max(delta[2].abs());
    if m <= allowed {
        [0.0; 3]
    } else {
        let k = 1.0 - allowed / m;
        [delta[0] * k, delta[1] * k, delta[2] * k]
    }
}

/// Angular radius (degrees) of the gap around a pole, or `None` when the pole is covered.
/// Returns `Some(f32::INFINITY)` when the gap runs past `max_gap`.
pub fn pole_gap(pano: &Panorama, top: bool, max_gap: f32) -> Option<f32> {
    let (w, h) = (pano.width, pano.height);
    let limit = ((max_gap / 180.0) * h as f32).ceil() as usize;
    let mut last = None;
    for r in 0..(limit + 2).min(h) {
        let y = if top { r } else { h - 1 - r };
        if pano.alpha[y * w..(y + 1) * w].contains(&0) {
            last = Some(r);
        }
    }
    let last = last?;
    if last >= limit {
        return Some(f32::INFINITY);
    }
    Some((last + 1) as f32 / h as f32 * 180.0)
}

/// Pole-centred view: gnomonic (a true top-down view of flat ground) or stereographic.
#[derive(Clone, Copy)]
struct View {
    top: bool,
    gnomonic: bool,
    /// Pixels per unit of projected radius.
    f: f64,
    size: usize,
}

impl View {
    #[inline]
    fn radius(&self, theta: f64) -> f64 {
        if self.gnomonic {
            theta.tan()
        } else {
            2.0 * (theta * 0.5).tan()
        }
    }

    #[inline]
    fn theta(&self, r: f64) -> f64 {
        if self.gnomonic {
            r.atan()
        } else {
            2.0 * (r * 0.5).atan()
        }
    }

    /// View pixel → (world direction, angle from the pole).
    #[inline]
    fn dir(&self, x: f64, y: f64) -> (V3, f64) {
        let c = self.size as f64 * 0.5;
        let (u, v) = ((x - c) / self.f, (y - c) / self.f);
        let r = u.hypot(v);
        let theta = self.theta(r);
        let (st, ct) = theta.sin_cos();
        let (cu, cv) = if r > 1e-12 { (u / r, v / r) } else { (1.0, 0.0) };
        let up = if self.top { ct } else { -ct };
        (V3::new(st * cu, up, st * cv), theta)
    }

    /// World direction → view pixel.
    #[inline]
    fn pixel(&self, d: V3) -> (f64, f64) {
        let up = if self.top { d.y } else { -d.y };
        let st = d.x.hypot(d.z);
        let theta = st.atan2(up);
        let r = self.radius(theta);
        let c = self.size as f64 * 0.5;
        if st < 1e-12 {
            return (c, c);
        }
        (c + self.f * r * d.x / st, c + self.f * r * d.z / st)
    }
}

#[inline]
fn pano_coords(pano: &Panorama, d: V3) -> (f32, f32) {
    let yaw = d.x.atan2(d.z);
    let pitch = d.y.clamp(-1.0, 1.0).asin();
    (((yaw / TAU + 0.5) * pano.width as f64) as f32, ((0.5 - pitch / PI) * pano.height as f64) as f32)
}

#[inline]
fn pano_alpha(pano: &Panorama, px: f32, py: f32) -> bool {
    let x = (px as isize).rem_euclid(pano.width as isize) as usize;
    let y = (py as isize).clamp(0, pano.height as isize - 1) as usize;
    pano.alpha[y * pano.width + x] != 0
}

/// One pyramid level of the synthesis problem.
struct Level {
    w: usize,
    h: usize,
    /// Detail (image − base), valid where `known`; the current estimate where `target`.
    detail: Vec<f32>,
    base: Vec<f32>,
    /// Real image data.
    known: Vec<bool>,
    /// Pixels to synthesise (the gap).
    hole: Vec<bool>,
    /// Patch centres that take part in synthesis (their window touches the gap).
    target: Vec<bool>,
    /// Patch centres usable as sources (window entirely known).
    source: Vec<bool>,
    sources: Vec<u32>,
    /// Local structure strength in [0, 1] (0 = smooth, 1 = strongly textured), from detail energy.
    rho: Vec<f32>,
    /// Local detail energy (same units as patch distances).
    energy: Vec<f32>,
    /// How deep inside the gap each pixel is: 0 at the rim, 1 at the centre.
    depth: Vec<f32>,
    /// How many target patches currently borrow from each source patch.
    usage: Vec<AtomicU32>,
    /// Cost added per extra borrower of the same source: spreads use across all the available
    /// material instead of repeating its blandest parts.
    reuse_cost: f32,
}

fn dilate(mask: &[bool], w: usize, h: usize, r: isize) -> Vec<bool> {
    let mut tmp = vec![false; w * h];
    tmp.par_chunks_mut(w).enumerate().for_each(|(y, row)| {
        for x in 0..w {
            let x0 = (x as isize - r).max(0) as usize;
            let x1 = ((x as isize + r) as usize).min(w - 1);
            row[x] = mask[y * w + x0..=y * w + x1].contains(&true);
        }
    });
    let mut out = vec![false; w * h];
    out.par_chunks_mut(w).enumerate().for_each(|(y, row)| {
        let y0 = (y as isize - r).max(0) as usize;
        let y1 = ((y as isize + r) as usize).min(h - 1);
        for x in 0..w {
            row[x] = (y0..=y1).any(|yy| tmp[yy * w + x]);
        }
    });
    out
}

fn finish_level(w: usize, h: usize, image: &[f32], base: Vec<f32>, known: Vec<bool>, hole: Vec<bool>) -> Level {
    let mut detail = vec![0.0f32; w * h * 3];
    detail.par_chunks_mut(3).enumerate().for_each(|(i, d)| {
        if known[i] {
            for c in 0..3 {
                d[c] = image[i * 3 + c] - base[i * 3 + c];
            }
        }
    });
    let target = dilate(&hole, w, h, HALF);
    let not_known: Vec<bool> = known.iter().map(|k| !k).collect();
    let blocked = dilate(&not_known, w, h, HALF);
    let mut source: Vec<bool> = blocked.iter().map(|b| !b).collect();
    // Keep patch windows inside the image.
    for y in 0..h {
        for x in 0..w {
            if (x as isize) < HALF || (y as isize) < HALF || x as isize >= w as isize - HALF || y as isize >= h as isize - HALF {
                source[y * w + x] = false;
            }
        }
    }
    let sources: Vec<u32> = (0..w * h).filter(|&i| source[i]).map(|i| i as u32).collect();
    let usage = (0..w * h).map(|_| AtomicU32::new(0)).collect();
    // Local detail energy in a 7×7 window over known pixels, and the structure strength from it.
    let energy: Vec<f32> = (0..w * h)
        .into_par_iter()
        .map(|i| {
            let (x, y) = ((i % w) as isize, (i / w) as isize);
            let (mut sum, mut n) = (0.0f32, 0u32);
            for dy in -HALF..=HALF {
                for dx in -HALF..=HALF {
                    let (xx, yy) = (x + dx, y + dy);
                    if xx < 0 || yy < 0 || xx >= w as isize || yy >= h as isize {
                        continue;
                    }
                    let j = (yy * w as isize + xx) as usize;
                    if known[j] {
                        let d = &detail[j * 3..j * 3 + 3];
                        sum += d[0] * d[0] + d[1] * d[1] + d[2] * d[2];
                        n += 1;
                    }
                }
            }
            if n == 0 {
                0.0
            } else {
                sum / n as f32
            }
        })
        .collect();
    let rho: Vec<f32> = energy
        .iter()
        .map(|e| {
            let rms = (e / 3.0).sqrt();
            let t = ((rms - 5.0) / 15.0).clamp(0.0, 1.0);
            t * t * (3.0 - 2.0 * t)
        })
        .collect();
    // Depth inside the gap (breadth-first distance from its rim, normalised).
    let mut dist = vec![u32::MAX; w * h];
    let mut frontier: Vec<usize> = Vec::new();
    for i in 0..w * h {
        if !hole[i] {
            continue;
        }
        let (x, y) = (i % w, i / w);
        let rim = (x > 0 && !hole[i - 1]) || (x + 1 < w && !hole[i + 1]) || (y > 0 && !hole[i - w]) || (y + 1 < h && !hole[i + w]);
        if rim {
            dist[i] = 0;
            frontier.push(i);
        }
    }
    let mut d = 0u32;
    while !frontier.is_empty() {
        d += 1;
        let mut next = Vec::new();
        for &i in &frontier {
            let (x, y) = (i % w, i / w);
            for j in [(x > 0).then(|| i - 1), (x + 1 < w).then_some(i + 1), (y > 0).then(|| i - w), (y + 1 < h).then_some(i + w)]
                .into_iter()
                .flatten()
            {
                if hole[j] && dist[j] == u32::MAX {
                    dist[j] = d;
                    next.push(j);
                }
            }
        }
        frontier = next;
    }
    let max_d = d.max(1) as f32;
    let depth: Vec<f32> = dist.iter().map(|&v| if v == u32::MAX { 0.0 } else { v as f32 / max_d }).collect();
    Level { w, h, detail, base, known, hole, target, source, sources, rho, energy, depth, usage, reuse_cost: 0.0 }
}

/// Patch distance between target patch `p` and source patch `q`, abandoning once above `limit`.
#[inline]
fn patch_dist(l: &Level, p: (isize, isize), q: (isize, isize), limit: f32) -> f32 {
    let (w, h) = (l.w as isize, l.h as isize);
    let mut sum = 0.0f32;
    let mut n = 0u32;
    for dy in -HALF..=HALF {
        let ty = p.1 + dy;
        if ty < 0 || ty >= h {
            continue;
        }
        let sy = q.1 + dy;
        for dx in -HALF..=HALF {
            let tx = p.0 + dx;
            if tx < 0 || tx >= w {
                continue;
            }
            let ti = (ty * w + tx) as usize;
            // Pixels that are neither image nor gap (outside the view) carry no information.
            if !l.known[ti] && !l.hole[ti] {
                continue;
            }
            let si = (sy * w + q.0 + dx) as usize;
            let (td, sd) = (&l.detail[ti * 3..ti * 3 + 3], &l.detail[si * 3..si * 3 + 3]);
            let (tb, sb) = (&l.base[ti * 3..ti * 3 + 3], &l.base[si * 3..si * 3 + 3]);
            // Structured material is compared by its actual colour, smooth material by texture
            // and base separately (its base offset will be corrected when it is placed).
            let rho = l.rho[si];
            for c in 0..3 {
                let b = tb[c] - sb[c];
                let d = td[c] - sd[c] + rho * b;
                sum += d * d + (1.0 - rho) * b * b;
            }
            n += 1;
        }
        if n > 0 && sum > limit * 49.0 {
            return f32::INFINITY;
        }
    }
    if n == 0 {
        return f32::INFINITY;
    }
    sum / n as f32
}

const COHERENCE: f32 = 0.6;

/// How strongly a candidate is favoured for continuing its neighbours' correspondence: large
/// verbatim-copied regions keep structure (tile joints, plank lines, cloud shapes) intact.
#[inline]
fn coherence_factor(l: &Level, nnf: &[(i32, i32)], p: (isize, isize), q: (i32, i32)) -> f32 {
    let (w, h) = (l.w as isize, l.h as isize);
    let off = (q.0 - p.0 as i32, q.1 - p.1 as i32);
    let mut same = 0;
    for (dx, dy) in [(-1isize, 0isize), (1, 0), (0, -1), (0, 1)] {
        let (nx, ny) = (p.0 + dx, p.1 + dy);
        if nx < 0 || ny < 0 || nx >= w || ny >= h {
            continue;
        }
        let ni = (ny * w + nx) as usize;
        if l.target[ni] {
            let nq = nnf[ni];
            if (nq.0 - nx as i32, nq.1 - ny as i32) == off {
                same += 1;
            }
        }
    }
    1.0 - COHERENCE * same as f32 / 4.0
}

/// Prior cost of placing source patch `q` at target `p`: deep inside the gap, plain material is
/// preferred over busy material. Structure may continue a little way in from the rim, but the
/// unknown middle is filled with the calmest material around (sky rather than rooftops). Where
/// everything around is equally textured the term is the same for every candidate and has no effect.
#[inline]
fn structure_prior(l: &Level, p: usize, q: usize) -> f32 {
    l.energy[q] * (l.depth[p] * 2.5).min(1.0)
}

/// One PatchMatch sweep (propagation + random search), parallel over horizontal strips.
fn search(l: &Level, nnf: &mut [(i32, i32)], cost: &mut [f32], iter: usize, seed: u64, max_radius: f32) {
    let (w, h) = (l.w, l.h);
    let snapshot = nnf.to_vec();
    let strip = 24usize;
    let reverse = iter % 2 == 1;
    nnf.par_chunks_mut(w * strip).zip(cost.par_chunks_mut(w * strip)).enumerate().for_each(|(si, (nn, cc))| {
        let y_base = si * strip;
        let rows = nn.len() / w;
        let mut rng = Rng::new(seed ^ ((iter as u64) << 32) ^ si as u64);
        let ys: Vec<usize> = if reverse { (0..rows).rev().collect() } else { (0..rows).collect() };
        for ly in ys {
            let y = y_base + ly;
            let xs: Box<dyn Iterator<Item = usize>> = if reverse { Box::new((0..w).rev()) } else { Box::new(0..w) };
            for x in xs {
                let i = y * w + x;
                if !l.target[i] {
                    continue;
                }
                let li = ly * w + x;
                let p = (x as isize, y as isize);
                let (mut best, mut best_c) = (nn[li], cc[li]);
                let try_q = |qx: i32, qy: i32, best: &mut (i32, i32), best_c: &mut f32| {
                    if qx < 0 || qy < 0 || qx >= w as i32 || qy >= h as i32 || !l.source[qy as usize * w + qx as usize] {
                        return;
                    }
                    if (qx, qy) == *best {
                        return;
                    }
                    let factor = coherence_factor(l, &snapshot, p, (qx, qy));
                    let reuse = l.reuse_cost * l.usage[qy as usize * w + qx as usize].load(Ordering::Relaxed) as f32
                        + structure_prior(l, i, qy as usize * w + qx as usize);
                    if reuse >= *best_c {
                        return;
                    }
                    let d = patch_dist(l, p, (qx as isize, qy as isize), (*best_c - reuse) / factor) * factor + reuse;
                    if d < *best_c {
                        *best_c = d;
                        *best = (qx, qy);
                    }
                };
                // Propagation: a neighbour's match, shifted by one pixel, is probably good here too.
                let step: isize = if reverse { 1 } else { -1 };
                let nx = x as isize + step;
                if nx >= 0 && nx < w as isize {
                    let nb = nn[ly * w + nx as usize];
                    try_q(nb.0 - step as i32, nb.1, &mut best, &mut best_c);
                }
                let ny = y as isize + step;
                if ny >= 0 && ny < h as isize {
                    let lny = ny - y_base as isize;
                    let nb = if lny >= 0 && (lny as usize) < rows { nn[lny as usize * w + x] } else { snapshot[ny as usize * w + x] };
                    try_q(nb.0, nb.1 - step as i32, &mut best, &mut best_c);
                }
                // Random search around the current best at shrinking radii.
                let mut radius = max_radius;
                while radius >= 1.0 {
                    let qx = best.0 + ((rng.unit() * 2.0 - 1.0) as f32 * radius) as i32;
                    let qy = best.1 + ((rng.unit() * 2.0 - 1.0) as f32 * radius) as i32;
                    try_q(qx, qy, &mut best, &mut best_c);
                    radius *= 0.5;
                }
                if best != nn[li] {
                    let old = nn[li];
                    l.usage[old.1 as usize * w + old.0 as usize].fetch_sub(1, Ordering::Relaxed);
                    l.usage[best.1 as usize * w + best.0 as usize].fetch_add(1, Ordering::Relaxed);
                }
                nn[li] = best;
                cc[li] = best_c;
            }
        }
    });
}

/// Rebuilds the detail estimate in the gap by averaging the source patches that cover each pixel.
fn vote(l: &mut Level, nnf: &[(i32, i32)], cost: &[f32]) {
    let (w, h) = (l.w, l.h);
    // Similarity bandwidth from the typical patch cost.
    let mut costs: Vec<f32> = (0..w * h).filter(|&i| l.target[i] && cost[i].is_finite()).map(|i| cost[i]).collect();
    if costs.is_empty() {
        return;
    }
    let k = costs.len() / 4;
    costs.select_nth_unstable_by(k, |a, b| a.total_cmp(b));
    // A tight bandwidth lets the best-matching patches dominate, which keeps the result crisp.
    let sigma = (costs[k] * 0.5).max(0.5);
    let detail = &l.detail;
    let updated: Vec<(usize, [f32; 3])> = (0..w * h)
        .into_par_iter()
        .filter(|&i| l.hole[i])
        .map(|i| {
            let (x, y) = ((i % w) as isize, (i / w) as isize);
            let mut acc = [0.0f32; 3];
            let mut wsum = 0.0f32;
            for dy in -HALF..=HALF {
                let py = y - dy;
                if py < 0 || py >= h as isize {
                    continue;
                }
                for dx in -HALF..=HALF {
                    let px = x - dx;
                    if px < 0 || px >= w as isize {
                        continue;
                    }
                    let pi = (py * w as isize + px) as usize;
                    if !l.target[pi] || !cost[pi].is_finite() {
                        continue;
                    }
                    let q = nnf[pi];
                    let si = ((q.1 as isize + dy) * w as isize + q.0 as isize + dx) as usize;
                    let wgt = (-cost[pi] / sigma).exp().max(1e-6);
                    let ex = excess_shift(
                        [l.base[i * 3] - l.base[si * 3], l.base[i * 3 + 1] - l.base[si * 3 + 1], l.base[i * 3 + 2] - l.base[si * 3 + 2]],
                        l.rho[si],
                    );
                    acc[0] += wgt * (detail[si * 3] - ex[0]);
                    acc[1] += wgt * (detail[si * 3 + 1] - ex[1]);
                    acc[2] += wgt * (detail[si * 3 + 2] - ex[2]);
                    wsum += wgt;
                }
            }
            if wsum > 0.0 {
                (i, [acc[0] / wsum, acc[1] / wsum, acc[2] / wsum])
            } else {
                (i, [0.0; 3])
            }
        })
        .collect();
    for (i, v) in updated {
        l.detail[i * 3..i * 3 + 3].copy_from_slice(&v);
    }
}

fn recompute_costs(l: &Level, nnf: &[(i32, i32)], cost: &mut [f32]) {
    let w = l.w;
    cost.par_iter_mut().enumerate().for_each(|(i, c)| {
        if l.target[i] {
            let q = nnf[i];
            let p = ((i % w) as isize, (i / w) as isize);
            let others = l.usage[q.1 as usize * w + q.0 as usize].load(Ordering::Relaxed).saturating_sub(1) as f32;
            *c = patch_dist(l, p, (q.0 as isize, q.1 as isize), f32::INFINITY) * coherence_factor(l, nnf, p, q)
                + l.reuse_cost * others
                + structure_prior(l, i, q.1 as usize * w + q.0 as usize);
        }
    });
}

/// Structure-aware initial guess at the coarsest level: grows the gap inwards ring by ring, each
/// pixel copied from the source whose neighbourhood best matches what is already there. Lines and
/// edges that enter the gap are continued instead of dissolving into an average.
fn onion_init(l: &mut Level, nnf: &mut [(i32, i32)]) {
    let (w, h) = (l.w, l.h);
    // A bounded set of candidate sources keeps this exhaustive search quick.
    let stride = (l.sources.len() / 6000).max(1);
    let cands: Vec<u32> = l.sources.iter().step_by(stride).copied().collect();
    if cands.is_empty() {
        return;
    }
    let mut filled: Vec<bool> = l.known.clone();
    loop {
        // Gap pixels touching filled ones.
        let layer: Vec<usize> = (0..w * h)
            .filter(|&i| {
                if !l.hole[i] || filled[i] {
                    return false;
                }
                let (x, y) = (i % w, i / w);
                (x > 0 && filled[i - 1]) || (x + 1 < w && filled[i + 1]) || (y > 0 && filled[i - w]) || (y + 1 < h && filled[i + w])
            })
            .collect();
        if layer.is_empty() {
            break;
        }
        let detail = &l.detail;
        let base = &l.base;
        let results: Vec<(usize, u32)> = layer
            .par_iter()
            .map(|&i| {
                let (x, y) = ((i % w) as isize, (i / w) as isize);
                // Offsets of the window that already hold data.
                let mut offs: Vec<(isize, isize, usize)> = Vec::with_capacity(49);
                for dy in -HALF..=HALF {
                    for dx in -HALF..=HALF {
                        let (tx, ty) = (x + dx, y + dy);
                        if tx >= 0 && ty >= 0 && tx < w as isize && ty < h as isize {
                            let ti = (ty * w as isize + tx) as usize;
                            if filled[ti] {
                                offs.push((dx, dy, ti));
                            }
                        }
                    }
                }
                let mut best = (f32::INFINITY, cands[0]);
                for &q in &cands {
                    let (qx, qy) = ((q as usize % w) as isize, (q as usize / w) as isize);
                    let mut sum = structure_prior(l, i, q as usize) * offs.len() as f32;
                    for &(dx, dy, ti) in &offs {
                        let si = ((qy + dy) * w as isize + qx + dx) as usize;
                        let rho = l.rho[si];
                        for c in 0..3 {
                            let b = base[ti * 3 + c] - base[si * 3 + c];
                            let d = detail[ti * 3 + c] - detail[si * 3 + c] + rho * b;
                            sum += d * d + (1.0 - rho) * b * b;
                        }
                        if sum >= best.0 {
                            break;
                        }
                    }
                    if sum < best.0 {
                        best = (sum, q);
                    }
                }
                (i, best.1)
            })
            .collect();
        for (i, q) in results {
            let q = q as usize;
            let ex = excess_shift(
                [l.base[i * 3] - l.base[q * 3], l.base[i * 3 + 1] - l.base[q * 3 + 1], l.base[i * 3 + 2] - l.base[q * 3 + 2]],
                l.rho[q],
            );
            for c in 0..3 {
                l.detail[i * 3 + c] = l.detail[q * 3 + c] - ex[c];
            }
            nnf[i] = ((q % w) as i32, (q / w) as i32);
            filled[i] = true;
        }
    }
}

struct Synthesis {
    view: View,
    w: usize,
    /// Smooth base at working resolution (defined everywhere).
    base: Vec<f32>,
    nnf: Vec<(i32, i32)>,
    target: Vec<bool>,
    hole: Vec<bool>,
    rho: Vec<f32>,
}

fn bilinear3(data: &[f32], w: usize, h: usize, x: f32, y: f32) -> [f32; 3] {
    let fx = (x - 0.5).clamp(0.0, (w - 1) as f32);
    let fy = (y - 0.5).clamp(0.0, (h - 1) as f32);
    let (x0, y0) = (fx as usize, fy as usize);
    let (x1, y1) = ((x0 + 1).min(w - 1), (y0 + 1).min(h - 1));
    let (tx, ty) = (fx - x0 as f32, fy - y0 as f32);
    let mut out = [0.0; 3];
    for (c, o) in out.iter_mut().enumerate() {
        let p = |xx: usize, yy: usize| data[(yy * w + xx) * 3 + c];
        let top = p(x0, y0) + (p(x1, y0) - p(x0, y0)) * tx;
        let bot = p(x0, y1) + (p(x1, y1) - p(x0, y1)) * tx;
        *o = top + (bot - top) * ty;
    }
    out
}

/// Smooth base layer: normalised low-pass of the known image, continued across the gap by solving
/// the biharmonic (thin-plate) equation on a coarse grid, then upsampled back.
fn smooth_base(image: &[f32], known: &[bool], hole: &[bool], s: usize, hole_radius: f64) -> Vec<f32> {
    // Coarse enough that the gap is only a few pixels across.
    let mut k = ((hole_radius / 6.0).log2().round() as isize).clamp(2, 7) as usize;
    while (s >> k) < 12 && k > 1 {
        k -= 1;
    }
    let mut pre = Plane { w: s, h: s, c: 3, data: image.to_vec() };
    let mut cov = Plane { w: s, h: s, c: 1, data: known.iter().map(|v| if *v { 1.0 } else { 0.0 }).collect() };
    let mut gap = Plane { w: s, h: s, c: 1, data: hole.iter().map(|v| if *v { 1.0 } else { 0.0 }).collect() };
    let mut dims = Vec::new();
    for _ in 0..k {
        dims.push((pre.w, pre.h));
        pre = reduce(&pre);
        cov = reduce(&cov);
        gap = reduce(&gap);
    }
    let (cw, ch) = (pre.w, pre.h);
    // Normalised colour where there is enough real data.
    let solid: Vec<bool> = cov.data.iter().map(|c| *c >= 0.6).collect();
    let mut colour = vec![0.0f32; cw * ch * 3];
    for i in 0..cw * ch {
        if solid[i] {
            for c in 0..3 {
                colour[i * 3 + c] = pre.data[i * 3 + c] / cov.data[i];
            }
        }
    }
    // Rough fill everywhere (also provides values outside the view circle).
    let alpha: Vec<f32> = solid.iter().map(|v| if *v { 1.0 } else { 0.0 }).collect();
    let levels = ((cw.min(ch) as f64).log2() as usize).saturating_sub(1).clamp(1, 8);
    let mut u = extrapolated_pyramid(Plane { w: cw, h: ch, c: 3, data: colour }, Plane { w: cw, h: ch, c: 1, data: alpha }, levels)
        .swap_remove(0)
        .data;
    // Thin-plate continuation across the gap.
    let unknown: Vec<usize> = (0..cw * ch)
        .filter(|&i| {
            let (x, y) = (i % cw, i / cw);
            !solid[i] && gap.data[i] > 0.05 && x >= 2 && y >= 2 && x + 2 < cw && y + 2 < ch
        })
        .collect();
    let n = unknown.len();
    if n > 0 && n <= 3000 {
        let mut index = vec![usize::MAX; cw * ch];
        for (j, &i) in unknown.iter().enumerate() {
            index[i] = j;
        }
        const STENCIL: [(isize, isize, f64); 13] = [
            (0, 0, 20.0),
            (1, 0, -8.0),
            (-1, 0, -8.0),
            (0, 1, -8.0),
            (0, -1, -8.0),
            (1, 1, 2.0),
            (1, -1, 2.0),
            (-1, 1, 2.0),
            (-1, -1, 2.0),
            (2, 0, 1.0),
            (-2, 0, 1.0),
            (0, 2, 1.0),
            (0, -2, 1.0),
        ];
        let mut a = nalgebra::DMatrix::<f64>::zeros(n, n);
        let mut b = nalgebra::DMatrix::<f64>::zeros(n, 3);
        for (j, &i) in unknown.iter().enumerate() {
            let (x, y) = ((i % cw) as isize, (i / cw) as isize);
            for (dx, dy, wgt) in STENCIL {
                let q = ((y + dy) * cw as isize + x + dx) as usize;
                if index[q] != usize::MAX {
                    a[(j, index[q])] += wgt;
                } else {
                    for c in 0..3 {
                        b[(j, c)] -= wgt * u[q * 3 + c] as f64;
                    }
                }
            }
        }
        if let Some(sol) = a.lu().solve(&b) {
            for (j, &i) in unknown.iter().enumerate() {
                for c in 0..3 {
                    let v = sol[(j, c)] as f32;
                    if v.is_finite() {
                        u[i * 3 + c] = v;
                    }
                }
            }
        }
    }
    let mut base = Plane { w: cw, h: ch, c: 3, data: u };
    while let Some((w, h)) = dims.pop() {
        base = expand(&base, w, h);
    }
    base.data
}

/// Synthesises the patch for one pole at working resolution.
fn synthesise(pano: &Panorama, top: bool, gap_deg: f32, opts: &FillOptions) -> Option<Synthesis> {
    let gap = (gap_deg as f64).to_radians();
    // Context ring around the gap to draw material from. Looking down, a gnomonic view shows flat
    // ground at one uniform scale (so borrowed texture has the right size); it is usable while the
    // ring stays clear of the horizon. Otherwise, and for the sky, the view is stereographic.
    let gnomonic = !top && gap_deg <= 36.0;
    // For the ground the ring stays close to the gap: the nearer the material, the closer its real
    // resolution is to what the camera would have recorded straight down.
    let ctx = if gnomonic {
        (gap + 20f64.to_radians()).min(55f64.to_radians())
    } else {
        (gap * 2.3 + 10f64.to_radians()).min(82f64.to_radians())
    };
    let mut view = View { top, gnomonic, f: pano.width as f64 / TAU, size: 0 };
    let full_size = 2.0 * view.f * view.radius(ctx);
    let shrink = (full_size / opts.work_size as f64).max(1.0);
    view.f /= shrink;
    view.size = (full_size / shrink).ceil() as usize + 2;
    let s = view.size;
    if s < 48 {
        return None;
    }

    // Working-resolution view of the panorama.
    let sub = (shrink.ceil() as usize).clamp(1, 4);
    let gap_r = view.f * view.radius(gap);
    let mut image = vec![0.0f32; s * s * 3];
    let mut known = vec![false; s * s];
    let mut hole = vec![false; s * s];
    image.par_chunks_mut(s * 3).zip(known.par_chunks_mut(s)).zip(hole.par_chunks_mut(s)).enumerate().for_each(
        |(y, ((row, krow), hrow))| {
            for x in 0..s {
                let (_, theta) = view.dir(x as f64 + 0.5, y as f64 + 0.5);
                if theta > ctx {
                    continue; // outside the view circle: neither image nor gap
                }
                let mut acc = [0.0f32; 3];
                let mut all = true;
                for sy in 0..sub {
                    for sx in 0..sub {
                        let (d, _) = view.dir(x as f64 + (sx as f64 + 0.5) / sub as f64, y as f64 + (sy as f64 + 0.5) / sub as f64);
                        let (px, py) = pano_coords(pano, d);
                        if !pano_alpha(pano, px, py) {
                            all = false;
                        }
                        let c = sample_bicubic(&pano.rgb, pano.width, pano.height, px, py, true);
                        acc[0] += c[0];
                        acc[1] += c[1];
                        acc[2] += c[2];
                    }
                }
                let c = s as f64 * 0.5;
                let r = (x as f64 + 0.5 - c).hypot(y as f64 + 0.5 - c);
                if all && r > gap_r {
                    let n = (sub * sub) as f32;
                    row[x * 3] = acc[0] / n;
                    row[x * 3 + 1] = acc[1] / n;
                    row[x * 3 + 2] = acc[2] / n;
                    krow[x] = true;
                } else {
                    hrow[x] = true;
                }
            }
        },
    );
    // Trim a thin fringe around the gap: blended coverage edges are not fully trustworthy.
    let hole = dilate(&hole, s, s, 2);
    // …but never beyond the view circle.
    let hole: Vec<bool> = hole
        .par_iter()
        .enumerate()
        .map(|(i, &hv)| {
            if !hv {
                return false;
            }
            let (_, theta) = view.dir((i % s) as f64 + 0.5, (i / s) as f64 + 0.5);
            theta <= ctx
        })
        .collect();
    let known: Vec<bool> = known.iter().zip(&hole).map(|(k, h)| *k && !*h).collect();
    let hole_count = hole.iter().filter(|h| **h).count();
    if hole_count == 0 || known.iter().filter(|k| **k).count() < 500 {
        return None;
    }
    for (i, k) in known.iter().enumerate() {
        if !k {
            image[i * 3..i * 3 + 3].fill(0.0);
        }
    }

    // Base: a heavily smoothed image whose continuation across the gap is a thin-plate surface, so
    // brightness and colour flow through the patch with continuous gradients (no visible rim).
    let hole_radius = (hole_count as f64 / PI).sqrt();
    let base = smooth_base(&image, &known, &hole, s, hole_radius);
    // Pyramid of synthesis problems.
    // Coarsest level: the gap is a couple of patches across, and the ring of real image around it
    // is still thick enough to offer whole patches.
    let ring_px = (view.f * (view.radius(ctx) - view.radius(gap))).max(1.0);
    let n_levels = ((hole_radius / 9.0).log2().floor().min((ring_px / 16.0).log2().floor()) as isize).clamp(0, 6) as usize;
    let mut levels: Vec<Level> = Vec::new();
    {
        let mut img = Plane { w: s, h: s, c: 3, data: image };
        let mut bas = Plane { w: s, h: s, c: 3, data: base.clone() };
        let mut kn = Plane { w: s, h: s, c: 1, data: known.iter().map(|k| if *k { 1.0 } else { 0.0 }).collect() };
        let mut ho = Plane { w: s, h: s, c: 1, data: hole.iter().map(|k| if *k { 1.0 } else { 0.0 }).collect() };
        for li in 0..=n_levels {
            let known_b: Vec<bool> = kn.data.iter().map(|v| *v > 0.999).collect();
            let hole_b: Vec<bool> = ho.data.iter().map(|v| *v > 0.3).collect();
            levels.push(finish_level(img.w, img.h, &img.data, bas.data.clone(), known_b, hole_b));
            if li == n_levels {
                break;
            }
            // Alpha-weighted reduction of the image so gap pixels do not darken their neighbours.
            let mut pre = img.clone();
            pre.data.par_chunks_mut(3).zip(kn.data.par_iter()).for_each(|(p, &a)| {
                p[0] *= a;
                p[1] *= a;
                p[2] *= a;
            });
            let mut next = reduce(&pre);
            let next_k = reduce(&kn);
            next.data.par_chunks_mut(3).zip(next_k.data.par_iter()).for_each(|(p, &a)| {
                if a > 1e-4 {
                    p[0] /= a;
                    p[1] /= a;
                    p[2] /= a;
                }
            });
            img = next;
            bas = reduce(&bas);
            ho = reduce(&ho);
            kn = next_k;
        }
    }

    // Coarse to fine.
    let mut rng = Rng::new(0x5EED ^ top as u64);
    let mut nnf: Vec<(i32, i32)> = Vec::new();
    for li in (0..=n_levels).rev() {
        let l = &mut levels[li];
        if l.sources.is_empty() {
            return None;
        }
        let (w, h) = (l.w, l.h);
        let random_source = |rng: &mut Rng, l: &Level| {
            let i = l.sources[rng.below(l.sources.len())] as usize;
            ((i % w) as i32, (i / w) as i32)
        };
        if nnf.is_empty() {
            nnf = (0..w * h).map(|_| random_source(&mut rng, l)).collect();
            onion_init(l, &mut nnf);
        } else {
            // Upsample the correspondence from the coarser level.
            let (cw, chh) = (levels_dim(w), levels_dim(h));
            let coarse = std::mem::take(&mut nnf);
            nnf = (0..w * h)
                .map(|i| {
                    let (x, y) = (i % w, i / w);
                    let c = coarse[(y / 2).min(chh - 1) * cw + (x / 2).min(cw - 1)];
                    let q = (c.0 * 2 + (x % 2) as i32, c.1 * 2 + (y % 2) as i32);
                    if q.0 >= 0 && q.1 >= 0 && (q.0 as usize) < w && (q.1 as usize) < h && l.source[q.1 as usize * w + q.0 as usize] {
                        q
                    } else {
                        random_source(&mut rng, l)
                    }
                })
                .collect();
        }
        // Usage counts for this level's correspondence, and the price of re-using a source.
        for u in &l.usage {
            u.store(0, Ordering::Relaxed);
        }
        for i in 0..w * h {
            if l.target[i] {
                let q = nnf[i];
                l.usage[q.1 as usize * w + q.0 as usize].fetch_add(1, Ordering::Relaxed);
            }
        }
        let mut cost = vec![f32::INFINITY; w * h];
        l.reuse_cost = 0.0;
        recompute_costs(l, &nnf, &mut cost);
        {
            let mut cs: Vec<f32> = (0..w * h).filter(|&i| l.target[i] && cost[i].is_finite()).map(|i| cost[i]).collect();
            if !cs.is_empty() {
                let k = cs.len() / 2;
                cs.select_nth_unstable_by(k, |a, b| a.total_cmp(b));
                l.reuse_cost = 0.5 * cs[k];
            }
        }
        recompute_costs(l, &nnf, &mut cost);
        if li != n_levels {
            vote(l, &nnf, &cost);
            recompute_costs(l, &nnf, &mut cost);
        }
        let iters = if li == n_levels {
            14
        } else if li >= 2 {
            6
        } else if li == 1 {
            4
        } else {
            3
        };
        // The coarse levels decide the layout (which chunk of the surroundings goes where); finer
        // levels only refine it locally, so large-scale structure survives to full resolution.
        let radius = if li == n_levels {
            w.max(h) as f32
        } else if li + 1 == n_levels {
            w.max(h) as f32 / 4.0
        } else {
            6.0
        };
        for it in 0..iters {
            search(l, &mut nnf, &mut cost, it, 0xABCD + li as u64 * 7919, radius);
            vote(l, &nnf, &cost);
            recompute_costs(l, &nnf, &mut cost);
        }
    }
    let l0 = &levels[0];
    if std::env::var_os("NODAL_FILL_DEBUG").is_some() {
        let (w, h) = (l0.w, l0.h);
        let (mut same, mut tot) = (0usize, 0usize);
        for y in 0..h {
            for x in 0..w - 1 {
                let i = y * w + x;
                if l0.target[i] && l0.target[i + 1] {
                    tot += 1;
                    if nnf[i].0 + 1 == nnf[i + 1].0 && nnf[i].1 == nnf[i + 1].1 {
                        same += 1;
                    }
                }
            }
        }
        eprintln!(
            "fill debug: view {}px gnomonic={} levels={} targets={} sources={} coherent={:.1}%",
            s,
            view.gnomonic,
            n_levels + 1,
            tot,
            l0.sources.len(),
            100.0 * same as f64 / tot.max(1) as f64
        );
    }
    Some(Synthesis { view, w: s, base, nnf, target: l0.target.clone(), hole: l0.hole.clone(), rho: l0.rho.clone() })
}

fn levels_dim(fine: usize) -> usize {
    fine.div_ceil(2)
}

/// Writes a synthesised patch into the panorama at full resolution.
fn apply(pano: &mut Panorama, syn: &Synthesis, gap_deg: f32) {
    let (w, h) = (pano.width, pano.height);
    let s = syn.w;
    let view = syn.view;
    // Distance (in working pixels) from each ring pixel to the gap, for a short cross-fade into the photo.
    let ring = 3isize;
    let near = dilate(&syn.hole, s, s, ring);
    // Rows that can contain gap or ring pixels.
    let extent = ((gap_deg as f64 + 4.0) / 180.0 * h as f64).ceil() as usize + 2 + (ring as usize * 2);
    let extent = extent.min(h / 2);
    let src_rgb = pano.rgb.clone();
    let src_alpha = pano.alpha.clone();
    let source = Panorama {
        width: w,
        height: h,
        rgb: src_rgb,
        alpha: src_alpha,
        full_width: w,
        full_height: h,
        crop_x: 0,
        crop_y: 0,
        stats: Default::default(),
    };
    let row_range = if view.top { 0..extent } else { h - extent..h };
    let y_off = row_range.start;
    let rows = &mut pano.rgb[row_range.start * w * 3..row_range.end * w * 3];
    let arows = &mut pano.alpha[row_range.start * w..row_range.end * w];
    rows.par_chunks_mut(w * 3).zip(arows.par_chunks_mut(w)).enumerate().for_each(|(ry, (row, arow))| {
        let y = y_off + ry;
        let pitch = (0.5 - (y as f64 + 0.5) / h as f64) * PI;
        let (sp, cp) = pitch.sin_cos();
        for x in 0..w {
            let yaw = ((x as f64 + 0.5) / w as f64 - 0.5) * TAU;
            let d = V3::new(yaw.sin() * cp, sp, yaw.cos() * cp);
            let (vx, vy) = view.pixel(d);
            if vx < 1.0 || vy < 1.0 || vx >= (s - 1) as f64 || vy >= (s - 1) as f64 {
                continue;
            }
            let (ix, iy) = (vx as usize, vy as usize);
            let i = iy * s + ix;
            if !near[i] {
                continue;
            }
            // Blend the four surrounding correspondences: identical where the match is coherent
            // (so the copy stays sharp) and a soft transition where it jumps.
            let fx = (vx - 0.5).max(0.0);
            let fy = (vy - 0.5).max(0.0);
            let (x0, y0) = ((fx as usize).min(s - 2), (fy as usize).min(s - 2));
            let (tx, ty) = (fx - x0 as f64, fy - y0 as f64);
            let bp = bilinear3(&syn.base, s, s, vx as f32, vy as f32);
            let mut acc = [0.0f32; 3];
            let mut wsum = 0.0f32;
            for (cx, cy, wgt) in
                [(x0, y0, (1.0 - tx) * (1.0 - ty)), (x0 + 1, y0, tx * (1.0 - ty)), (x0, y0 + 1, (1.0 - tx) * ty), (x0 + 1, y0 + 1, tx * ty)]
            {
                let ci = cy * s + cx;
                if !syn.target[ci] || wgt <= 0.0 {
                    continue;
                }
                let q = syn.nnf[ci];
                // Source position for this output pixel under that correspondence.
                let qx = q.0 as f64 + 0.5 + (vx - (cx as f64 + 0.5));
                let qy = q.1 as f64 + 0.5 + (vy - (cy as f64 + 0.5));
                let (qd, _) = view.dir(qx, qy);
                let (px, py) = pano_coords(&source, qd);
                let c = sample_bicubic(&source.rgb, w, h, px, py, true);
                let bq = bilinear3(&syn.base, s, s, qx as f32, qy as f32);
                let rho = syn.rho[(qy.clamp(0.0, (s - 1) as f64) as usize) * s + (qx.clamp(0.0, (s - 1) as f64) as usize)];
                let ex = excess_shift([bp[0] - bq[0], bp[1] - bq[1], bp[2] - bq[2]], rho);
                acc[0] += wgt as f32 * (c[0] - bq[0] - ex[0]);
                acc[1] += wgt as f32 * (c[1] - bq[1] - ex[1]);
                acc[2] += wgt as f32 * (c[2] - bq[2] - ex[2]);
                wsum += wgt as f32;
            }
            if wsum <= 0.0 {
                continue;
            }
            let synth = [bp[0] + acc[0] / wsum, bp[1] + acc[1] / wsum, bp[2] + acc[2] / wsum];
            let covered = arow[x] != 0;
            let in_hole = syn.hole[i];
            // Inside the gap the patch replaces everything; in the ring it fades into the photo.
            let mix = if !covered || in_hole {
                1.0
            } else {
                // Fraction of nearby working pixels that are gap: 0 far from the gap, →1 at its edge.
                let mut cnt = 0;
                let mut tot = 0;
                for dy in -ring..=ring {
                    for dx in -ring..=ring {
                        let (nx, ny) = (ix as isize + dx, iy as isize + dy);
                        if nx >= 0 && ny >= 0 && (nx as usize) < s && (ny as usize) < s {
                            tot += 1;
                            if syn.hole[ny as usize * s + nx as usize] {
                                cnt += 1;
                            }
                        }
                    }
                }
                (cnt as f32 / tot as f32 * 2.0).min(1.0)
            };
            if mix <= 0.0 {
                continue;
            }
            for c in 0..3 {
                let orig = row[x * 3 + c] as f32;
                let v = if covered { orig + (synth[c] - orig) * mix } else { synth[c] };
                row[x * 3 + c] = (v + 0.5).clamp(0.0, 255.0) as u8;
            }
            arow[x] = 255;
        }
    });
}

/// Fills zenith and nadir gaps of a full-sphere panorama in place.
pub fn fill_poles(pano: &mut Panorama, opts: &FillOptions, progress: Progress) -> FillReport {
    let mut report = FillReport::default();
    if !pano.is_full_sphere() {
        return report;
    }
    for (k, top) in [true, false].into_iter().enumerate() {
        let forced = if top { opts.zenith_patch } else { opts.nadir_patch };
        let gap = pole_gap(pano, top, opts.max_gap);
        let radius = match gap {
            Some(g) if g.is_infinite() => {
                if top {
                    report.zenith_too_large = true
                } else {
                    report.nadir_too_large = true
                }
                continue;
            }
            Some(g) => g.max(forced),
            None if forced > 0.0 => forced,
            None => continue,
        };
        progress(k as f32 * 0.5, if top { "Patching zenith" } else { "Patching nadir" });
        if let Some(syn) = synthesise(pano, top, radius, opts) {
            apply(pano, &syn, radius);
            if top {
                report.zenith = Some(radius);
                pano.stats.filled_zenith = true;
            } else {
                report.nadir = Some(radius);
                pano.stats.filled_nadir = true;
            }
        }
    }
    progress(1.0, "Poles patched");
    report
}
