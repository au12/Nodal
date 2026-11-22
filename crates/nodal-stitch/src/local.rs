//! Local alignment: smooth per-photo corrections that make matched points coincide.
//!
//! Global optimisation assumes every photo was taken from exactly one point through one ideal
//! lens. Real sets have small viewpoint shifts (parallax) and lens quirks, which leave control
//! points a few pixels apart. Here each photo gets a gentle displacement field that moves its
//! control points onto the midpoint agreed with its neighbours. Fields are fitted coarse to fine
//! with regularised kernel regression, so they stay smooth, fade to zero away from the overlaps,
//! and ignore isolated outliers.

use crate::lens::{LensModel, V3};
use crate::project::{Project, WarpField};
use nalgebra::Rotation3;

#[derive(Clone, Copy, Debug, Default)]
pub struct LocalReport {
    /// RMS control point distance (source pixels) before and after local alignment.
    pub before: f64,
    pub after: f64,
    /// Largest displacement applied anywhere, in source pixels.
    pub max_shift: f64,
    pub images_warped: usize,
}

#[derive(Clone, Copy)]
struct Sample {
    /// Where the agreed direction lands in this photo (full-resolution pixels).
    x: f32,
    y: f32,
    /// Remaining displacement needed there.
    dx: f32,
    dy: f32,
}

fn bilinear(field: &[f32], cols: usize, rows: usize, gx: f32, gy: f32) -> f32 {
    let gx = gx.clamp(0.0, (cols - 1) as f32);
    let gy = gy.clamp(0.0, (rows - 1) as f32);
    let (x0, y0) = ((gx as usize).min(cols - 2), (gy as usize).min(rows - 2));
    let (fx, fy) = (gx - x0 as f32, gy - y0 as f32);
    let i = y0 * cols + x0;
    let top = field[i] + (field[i + 1] - field[i]) * fx;
    let bot = field[i + cols] + (field[i + cols + 1] - field[i + cols]) * fx;
    top + (bot - top) * fy
}

/// Fits a displacement field to scattered samples.
fn fit_field(samples: &mut [Sample], w: f32, h: f32) -> WarpField {
    let cols = 33usize;
    let rows = ((cols as f32 * h / w).round() as usize).clamp(9, 65);
    let mut dx = vec![0.0f32; cols * rows];
    let mut dy = vec![0.0f32; cols * rows];
    let dim = w.max(h);
    // Coarse to fine: broad corrections first, then progressively local ones.
    for (sigma, lambda) in [(0.30 * dim, 3.0f32), (0.15 * dim, 2.0), (0.075 * dim, 1.5), (0.04 * dim, 1.5)] {
        let inv = 1.0 / (2.0 * sigma * sigma);
        let mut lx = vec![0.0f32; cols * rows];
        let mut ly = vec![0.0f32; cols * rows];
        for gy in 0..rows {
            for gx in 0..cols {
                let (px, py) = (gx as f32 / (cols - 1) as f32 * w, gy as f32 / (rows - 1) as f32 * h);
                let (mut sx, mut sy, mut sw) = (0.0f32, 0.0f32, 0.0f32);
                for s in samples.iter() {
                    let d2 = (s.x - px).powi(2) + (s.y - py).powi(2);
                    let wgt = (-d2 * inv).exp();
                    sx += wgt * s.dx;
                    sy += wgt * s.dy;
                    sw += wgt;
                }
                // The regulariser acts like `lambda` phantom points asking for no displacement:
                // a lone point cannot bend the photo, a consistent group can.
                lx[gy * cols + gx] = sx / (sw + lambda);
                ly[gy * cols + gx] = sy / (sw + lambda);
            }
        }
        for s in samples.iter_mut() {
            let (gx, gy) = (s.x / w * (cols - 1) as f32, s.y / h * (rows - 1) as f32);
            s.dx -= bilinear(&lx, cols, rows, gx, gy);
            s.dy -= bilinear(&ly, cols, rows, gx, gy);
        }
        for i in 0..cols * rows {
            dx[i] += lx[i];
            dy[i] += ly[i];
        }
    }
    WarpField { cols, rows, dx, dy }
}

/// Source position that the warp maps onto `(x, y)`: solves `s + D(s) = (x, y)`.
pub fn unwarp(field: Option<&WarpField>, x: f32, y: f32, w: f32, h: f32) -> (f32, f32) {
    let Some(f) = field else { return (x, y) };
    let (mut sx, mut sy) = (x, y);
    for _ in 0..4 {
        let (dx, dy) = f.sample(sx, sy, w, h);
        sx = x - dx;
        sy = y - dy;
    }
    (sx, sy)
}

/// RMS control point distance in source pixels, taking the local warps into account.
pub fn warped_rms(project: &Project) -> f64 {
    let models: Vec<LensModel> = project.lenses.iter().map(|l| l.model()).collect();
    let rots: Vec<Rotation3<f64>> = project.images.iter().map(|i| i.rot()).collect();
    let mut sum = 0.0;
    let mut n = 0usize;
    for cp in &project.control_points {
        let (a, b) = (cp.a as usize, cp.b as usize);
        let (ia, ib) = (&project.images[a], &project.images[b]);
        let (ma, mb) = (&models[ia.lens], &models[ib.lens]);
        // Positions in the un-warped photos that end up showing these features.
        let (ax, ay) = unwarp(ia.warp.as_ref(), cp.ax, cp.ay, ia.width as f32, ia.height as f32);
        let (bx, by) = unwarp(ib.warp.as_ref(), cp.bx, cp.by, ib.width as f32, ib.height as f32);
        let da: V3 = rots[a] * ma.pixel_to_ray(ax as f64, ay as f64);
        if let Some((px, py)) = mb.ray_to_pixel(rots[b].inverse_transform_vector(&da)) {
            sum += (px - bx as f64).powi(2) + (py - by as f64).powi(2);
            n += 1;
        }
    }
    if n == 0 {
        0.0
    } else {
        (sum / n as f64).sqrt()
    }
}

/// Computes local alignment fields for every aligned photo from the project's control points.
pub fn fit(project: &mut Project) -> LocalReport {
    for img in &mut project.images {
        img.warp = None;
    }
    let mut report = LocalReport { before: warped_rms(project), ..Default::default() };
    report.after = report.before;
    if project.control_points.len() < 8 {
        return report;
    }
    let models: Vec<LensModel> = project.lenses.iter().map(|l| l.model()).collect();
    let rots: Vec<Rotation3<f64>> = project.images.iter().map(|i| i.rot()).collect();
    // Ignore gross outliers: they are mismatches or moving objects, not geometry.
    let mut errs: Vec<f32> = project.control_points.iter().map(|c| c.error).collect();
    let k = errs.len() / 2;
    errs.select_nth_unstable_by(k, |a, b| a.total_cmp(b));
    let limit = (errs[k] * 4.0).max(3.0);

    let mut samples: Vec<Vec<Sample>> = (0..project.images.len()).map(|_| Vec::new()).collect();
    for cp in &project.control_points {
        if cp.error > limit {
            continue;
        }
        let (a, b) = (cp.a as usize, cp.b as usize);
        let (ma, mb) = (&models[project.images[a].lens], &models[project.images[b].lens]);
        let ua: V3 = rots[a] * ma.pixel_to_ray(cp.ax as f64, cp.ay as f64);
        let ub: V3 = rots[b] * mb.pixel_to_ray(cp.bx as f64, cp.by as f64);
        let t = (ua + ub).normalize();
        for (img, m, r, px, py) in [(a, ma, &rots[a], cp.ax, cp.ay), (b, mb, &rots[b], cp.bx, cp.by)] {
            if let Some((qx, qy)) = m.ray_to_pixel(r.inverse_transform_vector(&t)) {
                samples[img].push(Sample { x: qx as f32, y: qy as f32, dx: px - qx as f32, dy: py - qy as f32 });
            }
        }
    }
    for (i, s) in samples.iter_mut().enumerate() {
        let img = &project.images[i];
        if !img.aligned || s.len() < 6 {
            continue;
        }
        let (w, h) = (img.width as f32, img.height as f32);
        // Corrections stay small relative to the frame: this polishes alignment, it does not replace it.
        let cap = 0.03 * w.min(h);
        s.retain(|p| p.dx.hypot(p.dy) <= cap);
        if s.len() < 6 {
            continue;
        }
        let field = fit_field(s, w, h);
        let shift = field.max_shift() as f64;
        if shift < 0.05 {
            continue;
        }
        report.max_shift = report.max_shift.max(shift);
        report.images_warped += 1;
        project.images[i].warp = Some(field);
    }
    report.after = warped_rms(project);
    // Never make things worse.
    if report.after > report.before {
        for img in &mut project.images {
            img.warp = None;
        }
        report.after = report.before;
        report.images_warped = 0;
        report.max_shift = 0.0;
    }
    report
}

/// Fits local warps from dense overlap measurements. Every measured correspondence asks both
/// photos to move toward the direction halfway between where the global model places them; each
/// photo's field is a smooth regression of those requests that fades to zero away from overlaps.
/// A field is kept only if it clearly reduces that photo's residual. Returns how many photos were
/// warped.
pub fn fit_dense(project: &mut Project, matches: &[crate::dense::DenseMatch]) -> usize {
    use rayon::prelude::*;
    for img in &mut project.images {
        img.warp = None;
    }
    let n = project.images.len();
    let models: Vec<LensModel> = project.lenses.iter().map(|l| l.model()).collect();
    let rots: Vec<Rotation3<f64>> = project.images.iter().map(|i| i.rot()).collect();
    let mut samples: Vec<Vec<Sample>> = (0..n).map(|_| Vec::new()).collect();
    let mut used: Vec<Vec<usize>> = (0..n).map(|_| Vec::new()).collect();
    for (k, m) in matches.iter().enumerate() {
        let (a, b) = (m.a as usize, m.b as usize);
        let (ma, mb) = (&models[project.images[a].lens], &models[project.images[b].lens]);
        let da: V3 = rots[a] * ma.pixel_to_ray(m.ax as f64, m.ay as f64);
        let db: V3 = rots[b] * mb.pixel_to_ray(m.bx as f64, m.by as f64);
        let err = crate::global::vec_angle(&da, &db) * ma.f;
        // Gross disagreements are motion or mismatches, not parallax.
        if err > 0.02 * ma.w.max(ma.h) {
            continue;
        }
        let mid = (da + db).normalize();
        let (Some(ta), Some(tb)) = (ma.ray_to_pixel(rots[a].inverse() * mid), mb.ray_to_pixel(rots[b].inverse() * mid)) else { continue };
        samples[a].push(Sample { x: ta.0 as f32, y: ta.1 as f32, dx: m.ax - ta.0 as f32, dy: m.ay - ta.1 as f32 });
        samples[b].push(Sample { x: tb.0 as f32, y: tb.1 as f32, dx: m.bx - tb.0 as f32, dy: m.by - tb.1 as f32 });
        used[a].push(k);
        used[b].push(k);
    }
    // Thin very dense sample sets: the field has limited resolution anyway.
    for s in samples.iter_mut() {
        if s.len() > 4000 {
            let step = s.len().div_ceil(4000);
            let kept: Vec<Sample> = s.iter().step_by(step).map(|x| Sample { ..*x }).collect();
            *s = kept;
        }
    }
    let fields: Vec<Option<WarpField>> = samples
        .par_iter_mut()
        .enumerate()
        .map(|(i, s)| {
            if s.len() < 40 {
                return None;
            }
            let img = &project.images[i];
            let before: f64 = s.iter().map(|p| (p.dx * p.dx + p.dy * p.dy) as f64).sum::<f64>() / s.len() as f64;
            // Nothing worth correcting.
            if before.sqrt() < 0.35 {
                return None;
            }
            let mut work: Vec<Sample> = s.iter().map(|x| Sample { ..*x }).collect();
            let field = fit_field(&mut work, img.width as f32, img.height as f32);
            let after: f64 = work.iter().map(|p| (p.dx * p.dx + p.dy * p.dy) as f64).sum::<f64>() / work.len() as f64;
            (after < before * 0.8).then_some(field)
        })
        .collect();
    let mut warped = 0;
    for (img, f) in project.images.iter_mut().zip(fields) {
        if f.is_some() {
            warped += 1;
        }
        img.warp = f;
    }
    warped
}
