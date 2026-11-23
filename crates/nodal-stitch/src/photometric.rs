//! Photometric calibration: exposure, white balance and lens vignetting from the overlaps.
//!
//! Corresponding points in every overlapping pair are sampled from the working copies (in linear
//! light, slightly blurred so small misalignment does not matter). In the log domain the model is
//! linear:
//!
//! `log I_i(p) − log g_i − log V(r_i) = log I_j(q) − log g_j − log V(r_j)`
//!
//! with `g` a per-image gain and `log V(r) = a₁r² + a₂r⁴ + a₃r⁶` a per-lens vignetting curve. It is
//! solved by robust (Huber) iteratively reweighted least squares, first on luminance for exposure
//! and vignetting, then per channel for white balance. EXIF exposure acts as a weak prior so a
//! bracketed or auto-exposed set starts from the right place.

use crate::project::Project;
use crate::Source;
use nalgebra::{DMatrix, DVector};
use rayon::prelude::*;

#[derive(Clone, Debug, Default)]
pub struct PhotoReport {
    pub samples: usize,
    /// Robust RMS of log-luminance differences before and after, in stops.
    pub rms_before: f64,
    pub rms_after: f64,
    /// Corner illumination of each lens relative to the centre (1 = no vignetting).
    pub corner_falloff: Vec<f64>,
    /// Exposure range applied across the photos, in stops.
    pub exposure_spread: f64,
}

impl PhotoReport {
    /// One line for the app: how well neighbouring photos now agree in brightness.
    pub fn short(&self) -> String {
        format!("{:.2} → {:.2} EV", self.rms_before, self.rms_after)
    }

    pub fn summary(&self) -> String {
        let falloff = self.corner_falloff.iter().map(|f| format!("{:.2} EV", -f.max(1e-3).log2())).collect::<Vec<_>>().join(", ");
        format!(
            "Photometric calibration: {} samples, mismatch {:.3} → {:.3} EV, vignetting at corners {}, exposure spread {:.2} EV",
            self.samples, self.rms_before, self.rms_after, falloff, self.exposure_spread
        )
    }
}

struct Sample {
    i: u32,
    j: u32,
    r2i: f32,
    r2j: f32,
    ci: [f32; 3],
    cj: [f32; 3],
}

/// Blurred linear working copy at 1/4 size (smooth enough that a pixel of misalignment does not
/// change the value much).
struct Smooth {
    w: usize,
    h: usize,
    data: Vec<f32>,
    /// Smooth pixels per full-resolution pixel.
    scale: f32,
}

impl Smooth {
    fn new(src: &Source) -> Smooth {
        let lin = crate::pixels::LinearImage::from_srgb8(&src.image);
        let target = (lin.width.max(lin.height) / 4).max(64);
        let small = lin.downscale_to(target);
        let scale = small.width as f32 / src.width.max(1) as f32;
        // 3×3 box blur.
        let (w, h) = (small.width, small.height);
        let mut out = vec![0.0f32; w * h * 3];
        out.par_chunks_mut(w * 3).enumerate().for_each(|(y, row)| {
            for x in 0..w {
                let mut acc = [0.0f32; 3];
                let mut n = 0.0;
                for dy in -1i32..=1 {
                    let yy = (y as i32 + dy).clamp(0, h as i32 - 1) as usize;
                    for dx in -1i32..=1 {
                        let xx = (x as i32 + dx).clamp(0, w as i32 - 1) as usize;
                        let p = &small.data[(yy * w + xx) * 3..(yy * w + xx) * 3 + 3];
                        acc[0] += p[0];
                        acc[1] += p[1];
                        acc[2] += p[2];
                        n += 1.0;
                    }
                }
                row[x * 3] = acc[0] / n;
                row[x * 3 + 1] = acc[1] / n;
                row[x * 3 + 2] = acc[2] / n;
            }
        });
        Smooth { w, h, data: out, scale }
    }

    fn at(&self, x: f64, y: f64) -> Option<[f32; 3]> {
        let fx = x as f32 * self.scale - 0.5;
        let fy = y as f32 * self.scale - 0.5;
        if fx < 0.0 || fy < 0.0 || fx > (self.w - 2) as f32 || fy > (self.h - 2) as f32 {
            return None;
        }
        let (x0, y0) = (fx as usize, fy as usize);
        let (tx, ty) = (fx - x0 as f32, fy - y0 as f32);
        let p = |xx: usize, yy: usize, c: usize| self.data[(yy * self.w + xx) * 3 + c];
        let mut out = [0.0; 3];
        for c in 0..3 {
            let top = p(x0, y0, c) + (p(x0 + 1, y0, c) - p(x0, y0, c)) * tx;
            let bot = p(x0, y0 + 1, c) + (p(x0 + 1, y0 + 1, c) - p(x0, y0 + 1, c)) * tx;
            out[c] = top + (bot - top) * ty;
        }
        Some(out)
    }
}

fn r2_of(project: &Project, i: usize, x: f64, y: f64) -> f32 {
    let l = project.lens_of(i);
    let (w, h) = (l.width as f64, l.height as f64);
    (((x - w * 0.5 - l.d).powi(2) + (y - h * 0.5 - l.e).powi(2)) / ((w * w + h * h) * 0.25)) as f32
}

fn collect_samples(project: &Project, sources: &[Source], smooth: &[Option<Smooth>]) -> Vec<Sample> {
    let active: Vec<usize> = project.active().collect();
    let models: Vec<_> = project.lenses.iter().map(|l| l.model()).collect();
    let rots: Vec<_> = project.images.iter().map(|i| i.rot()).collect();
    let axes: Vec<_> = rots.iter().map(|r| r * crate::lens::V3::z()).collect();
    let mut pairs = Vec::new();
    for (a, &i) in active.iter().enumerate() {
        for &j in &active[a + 1..] {
            // Generous angular test; the sampling below finds the real overlap.
            let fov_i = project.lens_of(i).diagonal_fov().to_radians() * 0.5;
            let fov_j = project.lens_of(j).diagonal_fov().to_radians() * 0.5;
            if axes[i].angle(&axes[j]) < fov_i + fov_j {
                pairs.push((i, j));
            }
        }
    }
    let _ = sources;
    pairs
        .par_iter()
        .flat_map_iter(|&(i, j)| {
            let (Some(si), Some(sj)) = (&smooth[i], &smooth[j]) else { return Vec::new() };
            let (mi, mj) = (&models[project.images[i].lens], &models[project.images[j].lens]);
            let li = project.lens_of(i);
            let (wi, hi) = (li.width as f64, li.height as f64);
            let to_j = rots[j].inverse() * rots[i];
            let mut out = Vec::new();
            const N: usize = 28;
            for gy in 0..N {
                for gx in 0..N {
                    let x = (gx as f64 + 0.5) / N as f64 * wi;
                    let y = (gy as f64 + 0.5) / N as f64 * hi;
                    let ray = to_j * mi.pixel_to_ray(x, y);
                    let Some((xj, yj)) = mj.ray_to_pixel(ray) else { continue };
                    let lj = project.lens_of(j);
                    let m = 0.02 * lj.width.min(lj.height) as f64;
                    if xj < m || yj < m || xj > lj.width as f64 - m || yj > lj.height as f64 - m {
                        continue;
                    }
                    if let Some(mask) = &project.images[i].mask {
                        if mask.excluded(x as f32, y as f32, wi as f32, hi as f32) {
                            continue;
                        }
                    }
                    if let Some(mask) = &project.images[j].mask {
                        if mask.excluded(xj as f32, yj as f32, lj.width as f32, lj.height as f32) {
                            continue;
                        }
                    }
                    let (Some(ci), Some(cj)) = (si.at(x, y), sj.at(xj, yj)) else { continue };
                    // Skip clipped and nearly black samples: they carry no gain information.
                    let ok = |c: &[f32; 3]| c.iter().all(|v| *v > 0.004 && *v < 0.9);
                    if !ok(&ci) || !ok(&cj) {
                        continue;
                    }
                    out.push(Sample { i: i as u32, j: j as u32, r2i: r2_of(project, i, x, y), r2j: r2_of(project, j, xj, yj), ci, cj });
                }
            }
            out
        })
        .collect()
}

fn luma(c: &[f32; 3]) -> f64 {
    (0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2]) as f64
}

/// Estimates exposure, white balance and vignetting and stores them in the project
/// (`images[i].gain`, `lenses[l].vignette`).
pub fn calibrate(project: &mut Project, sources: &[Source]) -> PhotoReport {
    let active: Vec<usize> = project.active().collect();
    let n_img = project.images.len();
    let n_lens = project.lenses.len();
    if active.len() < 2 {
        return PhotoReport::default();
    }
    let smooth: Vec<Option<Smooth>> = (0..n_img)
        .into_par_iter()
        .map(|i| (project.images[i].aligned && project.images[i].enabled).then(|| Smooth::new(&sources[i])))
        .collect();
    let samples = collect_samples(project, sources, &smooth);
    drop(smooth);
    if samples.len() < 50 {
        return PhotoReport { samples: samples.len(), ..Default::default() };
    }
    // Unknown layout: [log gain per image (n_img)] [3 vignetting coefficients per lens].
    let nv = n_img + 3 * n_lens;
    let ev: Vec<Option<f64>> = project.images.iter().map(|im| im.exif.ev100()).collect();
    let ev_ref = {
        let mut v: Vec<f64> = active.iter().filter_map(|&i| ev[i]).collect();
        v.sort_by(|a, b| a.total_cmp(b));
        v.get(v.len() / 2).copied()
    };
    // Prior: brighter capture settings (lower EV) mean brighter pixels: log g = (ev_ref − ev)·ln 2.
    let prior: Vec<f64> = (0..n_img)
        .map(|i| match (ev[i], ev_ref) {
            (Some(e), Some(r)) => (r - e) * std::f64::consts::LN_2,
            _ => 0.0,
        })
        .collect();
    let lens_of: Vec<usize> = project.images.iter().map(|im| im.lens).collect();
    let mut per_image = vec![0usize; n_img];
    for s in &samples {
        per_image[s.i as usize] += 1;
        per_image[s.j as usize] += 1;
    }
    // When the camera recorded the exposure of every frame, trust it: real differences between
    // frames shot at the same settings are a few hundredths of a stop, while flare, moving clouds
    // and parallax easily fake tenths.
    let exif_known = active.iter().all(|&i| ev[i].is_some());

    let solve =
        |target: &dyn Fn(&Sample) -> f64, with_vignetting: bool, fixed_vig: &[[f64; 3]], prior_w: f64| -> (DVector<f64>, f64, f64) {
            let mut x = DVector::<f64>::zeros(nv);
            let mut rms0 = 0.0;
            let mut rms1 = 0.0;
            let mut delta = 0.25f64; // Huber threshold (natural log units)
            for it in 0..6 {
                let mut a = DMatrix::<f64>::zeros(nv, nv);
                let mut b = DVector::<f64>::zeros(nv);
                let mut sq = 0.0;
                let mut wsum = 0.0;
                for s in &samples {
                    let (i, j) = (s.i as usize, s.j as usize);
                    let (li, lj) = (lens_of[i], lens_of[j]);
                    let (r2i, r2j) = (s.r2i as f64, s.r2j as f64);
                    let pi = [r2i, r2i * r2i, r2i * r2i * r2i];
                    let pj = [r2j, r2j * r2j, r2j * r2j * r2j];
                    // residual = d − (gi − gj) − (Vi − Vj), d = log Ii − log Ij.
                    let mut d = target(s);
                    if !with_vignetting {
                        let vi: f64 = (0..3).map(|k| fixed_vig[li][k] * pi[k]).sum();
                        let vj: f64 = (0..3).map(|k| fixed_vig[lj][k] * pj[k]).sum();
                        d -= vi - vj;
                    }
                    let mut pred = x[i] - x[j];
                    if with_vignetting {
                        for k in 0..3 {
                            pred += x[n_img + 3 * li + k] * pi[k] - x[n_img + 3 * lj + k] * pj[k];
                        }
                    }
                    let r = d - pred;
                    if it == 0 {
                        rms0 += r * r;
                    }
                    let w = if r.abs() <= delta { 1.0 } else { delta / r.abs() };
                    sq += w * r * r;
                    wsum += w;
                    // Jacobian row.
                    let mut idx: Vec<(usize, f64)> = vec![(i, 1.0), (j, -1.0)];
                    if with_vignetting {
                        for k in 0..3 {
                            idx.push((n_img + 3 * li + k, pi[k]));
                            idx.push((n_img + 3 * lj + k, -pj[k]));
                        }
                    }
                    for &(p, vp) in &idx {
                        b[p] += w * vp * r;
                        for &(q, vq) in &idx {
                            a[(p, q)] += w * vp * vq;
                        }
                    }
                }
                if it == 0 {
                    rms0 = (rms0 / samples.len() as f64).sqrt();
                }
                rms1 = (sq / wsum.max(1e-9)).sqrt();
                // Priors: gains toward EXIF, vignetting toward zero, unused unknowns pinned.
                for k in 0..n_img {
                    let active_k = project.images[k].aligned && project.images[k].enabled;
                    let w = if active_k { prior_w * if exif_known { per_image[k].max(1) as f64 } else { 1.0 } } else { 1e6 };
                    let target = if active_k { prior[k] } else { 0.0 };
                    a[(k, k)] += w;
                    b[k] += w * (target - x[k]);
                }
                for k in n_img..nv {
                    a[(k, k)] += if with_vignetting { 1e-3 } else { 1e6 };
                    b[k] += -x[k] * if with_vignetting { 1e-3 } else { 0.0 };
                }
                let Some(chol) = a.clone().cholesky() else { break };
                let step = chol.solve(&b);
                x += &step;
                delta = (1.5 * rms1).clamp(0.05, 0.5);
                if step.amax() < 1e-5 {
                    break;
                }
            }
            (x, rms0, rms1)
        };

    // 1. Luminance: exposure and vignetting.
    let lum = |s: &Sample| luma(&s.ci).ln() - luma(&s.cj).ln();
    let (x, rms0, rms1) = solve(&lum, true, &vec![[0.0; 3]; n_lens], if exif_known { 0.3 } else { 0.02 });
    let mut vig_log = vec![[0.0f64; 3]; n_lens];
    for l in 0..n_lens {
        for k in 0..3 {
            vig_log[l][k] = x[n_img + 3 * l + k];
        }
    }
    // A plausible vignetting curve only: monotonic falloff, at most 3 stops in the corners.
    let mut falloff = Vec::new();
    for v in vig_log.iter_mut() {
        let c = *v;
        let at = |r2: f64| c[0] * r2 + c[1] * r2 * r2 + c[2] * r2 * r2 * r2;
        let corner = at(1.0);
        let monotone = (1..=20).all(|t| at(t as f64 / 20.0) <= at((t - 1) as f64 / 20.0) + 0.01);
        if !(corner < 0.05 && corner > -2.1 && monotone) {
            *v = [0.0; 3];
        }
        falloff.push((v[0] + v[1] + v[2]).exp());
    }
    let mut log_gain: Vec<f64> = (0..n_img).map(|i| x[i]).collect();
    // 2. Chroma: per-channel gains relative to luminance, vignetting fixed.
    let mut chroma = vec![[0.0f64; 3]; n_img];
    for c in [0usize, 2] {
        let t = move |s: &Sample| ((s.ci[c] as f64).ln() - luma(&s.ci).ln()) - ((s.cj[c] as f64).ln() - luma(&s.cj).ln());
        let (xc, _, _) = solve(&t, false, &vec![[0.0; 3]; n_lens], if exif_known { 0.3 } else { 0.5 });
        for i in 0..n_img {
            chroma[i][c] = xc[i].clamp(-0.3, 0.3);
        }
    }
    // Normalise so the median photo keeps its brightness.
    let mut act: Vec<f64> = active.iter().map(|&i| log_gain[i]).collect();
    act.sort_by(|a, b| a.total_cmp(b));
    let med = act[act.len() / 2];
    for g in log_gain.iter_mut() {
        *g -= med;
    }
    let spread = act.last().unwrap() - act.first().unwrap();
    for (i, im) in project.images.iter_mut().enumerate() {
        if !(im.aligned && im.enabled) {
            im.gain = [1.0; 3];
            continue;
        }
        // The model's g multiplies the observation; the renderer divides it out.
        let base = -log_gain[i];
        let g = [(base - chroma[i][0]).exp(), base.exp(), (base - chroma[i][2]).exp()];
        // Keep green as the anchor of the chroma correction.
        im.gain = [g[0].clamp(0.25, 4.0) as f32, g[1].clamp(0.25, 4.0) as f32, g[2].clamp(0.25, 4.0) as f32];
    }
    // Convert log-vignetting to the polynomial the renderer uses: fit 1 + v₁r² + v₂r⁴ + v₃r⁶ to
    // exp(a₁r² + a₂r⁴ + a₃r⁶) on r ∈ [0, 1.05].
    for (l, lens) in project.lenses.iter_mut().enumerate() {
        let a = vig_log[l];
        let mut m = DMatrix::<f64>::zeros(3, 3);
        let mut rhs = DVector::<f64>::zeros(3);
        for t in 0..=60 {
            let r2 = (t as f64 / 60.0 * 1.05).powi(2);
            let y = (a[0] * r2 + a[1] * r2 * r2 + a[2] * r2 * r2 * r2).exp() - 1.0;
            let p = [r2, r2 * r2, r2 * r2 * r2];
            for u in 0..3 {
                rhs[u] += p[u] * y;
                for v in 0..3 {
                    m[(u, v)] += p[u] * p[v];
                }
            }
        }
        let v = m.lu().solve(&rhs).unwrap_or_else(|| DVector::zeros(3));
        lens.vignette = [v[0], v[1], v[2]];
    }
    PhotoReport {
        samples: samples.len(),
        rms_before: rms0 / std::f64::consts::LN_2,
        rms_after: rms1 / std::f64::consts::LN_2,
        corner_falloff: falloff,
        exposure_spread: spread / std::f64::consts::LN_2,
    }
}
