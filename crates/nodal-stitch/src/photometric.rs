//! Exposure and white-balance compensation: per-photo RGB gains that make overlapping areas agree.

use crate::warp::Layer;
use nalgebra::{DMatrix, DVector};
use rayon::prelude::*;

struct PairStat {
    i: usize,
    j: usize,
    n: f64,
    mean_i: [f64; 3],
    mean_j: [f64; 3],
}

#[inline]
fn lin(v: f32) -> f64 {
    ((v as f64) * (1.0 / 255.0)).max(0.0).powf(2.2)
}

/// Solves for linear-light RGB gains per layer from the overlaps of reduced-size layers.
/// Returns one `[r, g, b]` gain per layer (1.0 = unchanged).
pub fn solve_gains(layers: &[Layer], cw: usize, ch: usize) -> Vec<[f32; 3]> {
    let n = layers.len();
    if n < 2 {
        return vec![[1.0; 3]; n];
    }
    let pairs: Vec<(usize, usize)> = (0..n).flat_map(|i| (i + 1..n).map(move |j| (i, j))).collect();
    let stats: Vec<PairStat> = pairs
        .par_iter()
        .filter_map(|&(i, j)| {
            let (a, b) = (&layers[i], &layers[j]);
            let roi = a.roi;
            let mut s = PairStat { i, j, n: 0.0, mean_i: [0.0; 3], mean_j: [0.0; 3] };
            for ly in 0..roi.h {
                let cy = roi.y0 + ly as i64;
                if cy < 0 || cy >= ch as i64 {
                    continue;
                }
                for lx in 0..roi.w {
                    let oa = ly * roi.w + lx;
                    if a.alpha[oa] == 0 {
                        continue;
                    }
                    let cx = (roi.x0 + lx as i64).rem_euclid(cw as i64) as usize;
                    let Some(ob) = b.index(cx, cy as usize, cw) else { continue };
                    if b.alpha[ob] == 0 {
                        continue;
                    }
                    let (pa, pb) = (&a.rgb[oa * 3..oa * 3 + 3], &b.rgb[ob * 3..ob * 3 + 3]);
                    // Skip clipped or nearly black pixels: they say nothing about relative exposure.
                    let ok = |p: &[f32]| p.iter().all(|&v| v > 6.0 && v < 249.0);
                    if !ok(pa) || !ok(pb) {
                        continue;
                    }
                    s.n += 1.0;
                    for c in 0..3 {
                        s.mean_i[c] += lin(pa[c]);
                        s.mean_j[c] += lin(pb[c]);
                    }
                }
            }
            if s.n < 50.0 {
                return None;
            }
            for c in 0..3 {
                s.mean_i[c] /= s.n;
                s.mean_j[c] /= s.n;
            }
            Some(s)
        })
        .collect();
    if stats.is_empty() {
        return vec![[1.0; 3]; n];
    }
    let mut gains = vec![[1.0f32; 3]; n];
    // Error model: overlaps should agree within a few percent; gains stay near 1 unless needed.
    let (sigma_n, sigma_g) = (0.04f64, 0.35f64);
    for c in 0..3 {
        let mut a = DMatrix::<f64>::zeros(n, n);
        let mut b = DVector::<f64>::zeros(n);
        let mut weight = vec![0.0f64; n];
        for s in &stats {
            let scale = 0.5 * (s.mean_i[c] + s.mean_j[c]).max(1e-4);
            let (ai, bj) = (s.mean_i[c] / scale, s.mean_j[c] / scale);
            let w = s.n / (sigma_n * sigma_n);
            a[(s.i, s.i)] += w * ai * ai;
            a[(s.j, s.j)] += w * bj * bj;
            a[(s.i, s.j)] -= w * ai * bj;
            a[(s.j, s.i)] -= w * ai * bj;
            weight[s.i] += s.n;
            weight[s.j] += s.n;
        }
        for i in 0..n {
            let w = weight[i].max(1.0) / (sigma_g * sigma_g);
            a[(i, i)] += w;
            b[i] += w;
        }
        if let Some(sol) = a.lu().solve(&b) {
            for i in 0..n {
                gains[i][c] = (sol[i] as f32).clamp(0.2, 5.0);
            }
        }
    }
    // Keep overall brightness and colour: the geometric mean gain per channel is 1.
    for c in 0..3 {
        let mean_log = gains.iter().map(|g| g[c].ln()).sum::<f32>() / n as f32;
        let k = (-mean_log).exp();
        gains.iter_mut().for_each(|g| g[c] *= k);
    }
    gains
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::warp::Roi;

    #[test]
    fn recovers_exposure_ratio() {
        // Same scene, second layer shot one third of a stop darker.
        let (w, h) = (100, 40);
        let scene = |x: usize, y: usize| 60.0 + ((x * 7 + y * 13) % 120) as f32;
        let dark = 0.8f32.powf(1.0 / 2.2);
        let mk = |x0: i64, k: f32| {
            let mut rgb = vec![0.0; w * h * 3];
            for y in 0..h {
                for x in 0..w {
                    rgb[(y * w + x) * 3..(y * w + x) * 3 + 3].fill(scene(x0 as usize + x, y) * k);
                }
            }
            Layer { roi: Roi { x0, y0: 0, w, h }, rgb, alpha: vec![255; w * h], weight: Vec::new() }
        };
        let layers = vec![mk(0, 1.0), mk(50, dark)];
        let g = solve_gains(&layers, 200, h);
        let ratio = g[1][0] / g[0][0];
        assert!((ratio - 1.25).abs() < 0.04, "ratio {ratio}");
        assert!((g[0][1] * g[1][1] - 1.0).abs() < 1e-3);
    }
}
