//! Bundle adjustment: Levenberg–Marquardt over image rotations and shared lens parameters,
//! minimising symmetric reprojection error of the control points with a robust (Huber) loss.

use crate::lens::{Lens, LensModel, V3};
use crate::project::{ControlPoint, Project};
use nalgebra::{DMatrix, DVector, Rotation3};

/// Which lens parameters may move.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct LensFlags {
    pub fov: bool,
    pub a: bool,
    pub b: bool,
    pub c: bool,
    pub shift: bool,
}

impl LensFlags {
    pub const NONE: LensFlags = LensFlags { fov: false, a: false, b: false, c: false, shift: false };

    fn list(&self) -> [bool; 6] {
        [self.fov, self.a, self.b, self.c, self.shift, self.shift]
    }
}

#[derive(Clone, Debug)]
pub struct OptOptions {
    pub flags: LensFlags,
    /// Huber threshold in source pixels.
    pub huber_px: f64,
    pub max_iters: usize,
    /// Per-lens prior on the field of view: (degrees, sigma in degrees).
    pub fov_prior: Vec<Option<(f64, f64)>>,
    /// Images whose rotation stays fixed (the anchor, plus anything not being optimised).
    pub fixed: Vec<bool>,
}

#[derive(Clone, Debug, Default)]
pub struct OptReport {
    pub iterations: usize,
    /// Root-mean-square control point distance in source pixels.
    pub rms: f64,
    pub max: f64,
    pub cost: f64,
}

const N_LENS: usize = 6;
const EPS_ROT: f64 = 1e-6;
const EPS_LENS: [f64; N_LENS] = [1e-4, 1e-6, 1e-6, 1e-6, 1e-3, 1e-3];

fn lens_get(l: &Lens, k: usize) -> f64 {
    match k {
        0 => l.hfov,
        1 => l.a,
        2 => l.b,
        3 => l.c,
        4 => l.d,
        _ => l.e,
    }
}

fn lens_add(l: &mut Lens, k: usize, v: f64) {
    match k {
        0 => l.hfov = (l.hfov + v).clamp(1.0, if l.kind.is_fisheye() { 340.0 } else { 175.0 }),
        1 => l.a = (l.a + v).clamp(-0.6, 0.6),
        2 => l.b = (l.b + v).clamp(-0.8, 0.8),
        3 => l.c = (l.c + v).clamp(-0.6, 0.6),
        4 => l.d += v,
        _ => l.e += v,
    }
}

/// Symmetric reprojection residual of one control point, in source pixels:
/// (a projected into b) − b, and (b projected into a) − a.
#[inline]
pub fn residual(cp: &ControlPoint, ra: &Rotation3<f64>, rb: &Rotation3<f64>, ma: &LensModel, mb: &LensModel) -> [f64; 4] {
    let ray_a = ma.pixel_to_ray(cp.ax as f64, cp.ay as f64);
    let ray_b = mb.pixel_to_ray(cp.bx as f64, cp.by as f64);
    let a_in_b: V3 = rb.inverse_transform_vector(&(ra * ray_a));
    let b_in_a: V3 = ra.inverse_transform_vector(&(rb * ray_b));
    let mut out = [0.0; 4];
    match mb.ray_to_pixel(a_in_b) {
        Some((x, y)) => {
            out[0] = x - cp.bx as f64;
            out[1] = y - cp.by as f64;
        }
        None => {
            // Outside the lens's field: fall back to an angular residual scaled to pixels.
            let d = (a_in_b - ray_b) * mb.f;
            out[0] = d.x;
            out[1] = d.y + d.z;
        }
    }
    match ma.ray_to_pixel(b_in_a) {
        Some((x, y)) => {
            out[2] = x - cp.ax as f64;
            out[3] = y - cp.ay as f64;
        }
        None => {
            let d = (b_in_a - ray_a) * ma.f;
            out[2] = d.x;
            out[3] = d.y + d.z;
        }
    }
    out
}

/// Control point distance in source pixels (mean of both directions).
pub fn cp_error(cp: &ControlPoint, ra: &Rotation3<f64>, rb: &Rotation3<f64>, ma: &LensModel, mb: &LensModel) -> f64 {
    let r = residual(cp, ra, rb, ma, mb);
    0.5 * ((r[0] * r[0] + r[1] * r[1]).sqrt() + (r[2] * r[2] + r[3] * r[3]).sqrt())
}

struct State {
    rots: Vec<Rotation3<f64>>,
    lenses: Vec<Lens>,
}

struct Layout {
    rot: Vec<Option<usize>>,
    lens: Vec<[Option<usize>; N_LENS]>,
    n: usize,
}

fn huber_weight(norm: f64, delta: f64) -> f64 {
    if norm <= delta {
        1.0
    } else {
        delta / norm
    }
}

fn cost_of(state: &State, cps: &[ControlPoint], img_lens: &[usize], opts: &OptOptions, prior_w: f64) -> f64 {
    let models: Vec<LensModel> = state.lenses.iter().map(|l| l.model()).collect();
    let mut cost = 0.0;
    for cp in cps {
        let (a, b) = (cp.a as usize, cp.b as usize);
        let r = residual(cp, &state.rots[a], &state.rots[b], &models[img_lens[a]], &models[img_lens[b]]);
        let n2 = r.iter().map(|v| v * v).sum::<f64>();
        let n = (n2 * 0.5).sqrt();
        // Huber loss on the per-point distance.
        cost += if n <= opts.huber_px { n2 } else { 2.0 * (2.0 * opts.huber_px * n - opts.huber_px * opts.huber_px) };
    }
    cost + prior_cost(state, opts, prior_w)
}

/// Soft priors keep weakly-observed lens parameters sane (partial panoramas barely constrain the
/// field of view; distortion terms need overlap across the frame).
fn prior_terms(state: &State, opts: &OptOptions, prior_w: f64) -> Vec<(usize, usize, f64, f64)> {
    // (lens, param, residual, d residual / d param)
    let mut out = Vec::new();
    for (li, l) in state.lenses.iter().enumerate() {
        if let Some(Some((fov, sigma))) = opts.fov_prior.get(li) {
            out.push((li, 0, prior_w * (l.hfov - fov) / sigma, prior_w / sigma));
        }
        for (k, sigma) in [(1usize, 0.08), (2, 0.15), (3, 0.08)] {
            out.push((li, k, prior_w * lens_get(l, k) / sigma, prior_w / sigma));
        }
        let shift_sigma = 0.03 * l.width.min(l.height) as f64;
        for k in [4usize, 5] {
            out.push((li, k, prior_w * lens_get(l, k) / shift_sigma, prior_w / shift_sigma));
        }
    }
    out
}

fn prior_cost(state: &State, opts: &OptOptions, prior_w: f64) -> f64 {
    prior_terms(state, opts, prior_w).iter().map(|t| t.2 * t.2).sum()
}

/// Runs the optimiser in place on `project` using `cps`. Returns the fit statistics.
pub fn optimize(project: &mut Project, cps: &[ControlPoint], opts: &OptOptions) -> OptReport {
    let n_img = project.images.len();
    let img_lens: Vec<usize> = project.images.iter().map(|i| i.lens).collect();
    let mut state = State { rots: project.images.iter().map(|i| i.rot()).collect(), lenses: project.lenses.clone() };

    // Only images and lenses that appear in control points get parameters.
    let mut img_used = vec![false; n_img];
    let mut lens_used = vec![false; state.lenses.len()];
    for cp in cps {
        for i in [cp.a as usize, cp.b as usize] {
            img_used[i] = true;
            lens_used[img_lens[i]] = true;
        }
    }
    let mut layout = Layout { rot: vec![None; n_img], lens: vec![[None; N_LENS]; state.lenses.len()], n: 0 };
    for i in 0..n_img {
        if img_used[i] && !opts.fixed.get(i).copied().unwrap_or(false) {
            layout.rot[i] = Some(layout.n);
            layout.n += 3;
        }
    }
    let flags = opts.flags.list();
    for (li, used) in lens_used.iter().enumerate() {
        if !*used {
            continue;
        }
        for k in 0..N_LENS {
            if flags[k] {
                layout.lens[li][k] = Some(layout.n);
                layout.n += 1;
            }
        }
    }
    let n = layout.n;
    let mut report = OptReport::default();
    if n == 0 || cps.is_empty() {
        finish(project, &state, cps, &img_lens, &mut report);
        return report;
    }

    // Prior strength: one sigma of deviation costs as much as every point being 0.35 px further off.
    let prior_w = 0.35 * ((cps.len() * 4) as f64).sqrt();
    let mut lambda = 1e-3;
    let mut cost = cost_of(&state, cps, &img_lens, opts, prior_w);

    for iter in 0..opts.max_iters {
        report.iterations = iter + 1;
        let models: Vec<LensModel> = state.lenses.iter().map(|l| l.model()).collect();
        // Perturbed rotations and lens models for the numeric Jacobian.
        let rot_pert: Vec<[Rotation3<f64>; 3]> = state
            .rots
            .iter()
            .map(|r| {
                let p = |axis: V3| r * Rotation3::new(axis * EPS_ROT);
                [p(V3::x()), p(V3::y()), p(V3::z())]
            })
            .collect();
        let lens_pert: Vec<[LensModel; N_LENS]> = state
            .lenses
            .iter()
            .map(|l| {
                let mut out = [l.model(); N_LENS];
                for (k, m) in out.iter_mut().enumerate() {
                    let mut lp = *l;
                    match k {
                        0 => lp.hfov += EPS_LENS[0],
                        1 => lp.a += EPS_LENS[1],
                        2 => lp.b += EPS_LENS[2],
                        3 => lp.c += EPS_LENS[3],
                        4 => lp.d += EPS_LENS[4],
                        _ => lp.e += EPS_LENS[5],
                    }
                    *m = lp.model();
                }
                out
            })
            .collect();

        let mut jtj = DMatrix::<f64>::zeros(n, n);
        let mut jtr = DVector::<f64>::zeros(n);
        let mut cols: Vec<(usize, [f64; 4])> = Vec::with_capacity(18);
        for cp in cps {
            let (a, b) = (cp.a as usize, cp.b as usize);
            let (la, lb) = (img_lens[a], img_lens[b]);
            let r0 = residual(cp, &state.rots[a], &state.rots[b], &models[la], &models[lb]);
            let norm = (r0.iter().map(|v| v * v).sum::<f64>() * 0.5).sqrt();
            let w = huber_weight(norm, opts.huber_px);
            cols.clear();
            let diff = |r: [f64; 4], eps: f64| [(r[0] - r0[0]) / eps, (r[1] - r0[1]) / eps, (r[2] - r0[2]) / eps, (r[3] - r0[3]) / eps];
            if let Some(p) = layout.rot[a] {
                for k in 0..3 {
                    cols.push((p + k, diff(residual(cp, &rot_pert[a][k], &state.rots[b], &models[la], &models[lb]), EPS_ROT)));
                }
            }
            if let Some(p) = layout.rot[b] {
                for k in 0..3 {
                    cols.push((p + k, diff(residual(cp, &state.rots[a], &rot_pert[b][k], &models[la], &models[lb]), EPS_ROT)));
                }
            }
            for k in 0..N_LENS {
                if let Some(p) = layout.lens[la][k] {
                    let mb = if la == lb { &lens_pert[lb][k] } else { &models[lb] };
                    cols.push((p, diff(residual(cp, &state.rots[a], &state.rots[b], &lens_pert[la][k], mb), EPS_LENS[k])));
                }
                if la != lb {
                    if let Some(p) = layout.lens[lb][k] {
                        cols.push((p, diff(residual(cp, &state.rots[a], &state.rots[b], &models[la], &lens_pert[lb][k]), EPS_LENS[k])));
                    }
                }
            }
            for (i, (pi, ji)) in cols.iter().enumerate() {
                jtr[*pi] += w * (ji[0] * r0[0] + ji[1] * r0[1] + ji[2] * r0[2] + ji[3] * r0[3]);
                for (pj, jj) in &cols[i..] {
                    let v = w * (ji[0] * jj[0] + ji[1] * jj[1] + ji[2] * jj[2] + ji[3] * jj[3]);
                    jtj[(*pi, *pj)] += v;
                    if pi != pj {
                        jtj[(*pj, *pi)] += v;
                    }
                }
            }
        }
        for (li, k, res, dres) in prior_terms(&state, opts, prior_w) {
            if let Some(p) = layout.lens[li][k] {
                jtj[(p, p)] += dres * dres;
                jtr[p] += dres * res;
            }
        }

        // Damped solve; retry with more damping until the step lowers the cost.
        let mut improved = false;
        for _ in 0..10 {
            let mut m = jtj.clone();
            for i in 0..n {
                m[(i, i)] += lambda * jtj[(i, i)].max(1e-12) + 1e-12;
            }
            let Some(chol) = m.cholesky() else {
                lambda *= 10.0;
                continue;
            };
            let step = chol.solve(&(-&jtr));
            let mut trial = State { rots: state.rots.clone(), lenses: state.lenses.clone() };
            for i in 0..n_img {
                if let Some(p) = layout.rot[i] {
                    trial.rots[i] = state.rots[i] * Rotation3::new(V3::new(step[p], step[p + 1], step[p + 2]));
                }
            }
            for li in 0..trial.lenses.len() {
                for k in 0..N_LENS {
                    if let Some(p) = layout.lens[li][k] {
                        lens_add(&mut trial.lenses[li], k, step[p]);
                    }
                }
            }
            let trial_cost = cost_of(&trial, cps, &img_lens, opts, prior_w);
            if trial_cost.is_finite() && trial_cost < cost {
                let rel = (cost - trial_cost) / cost.max(1e-30);
                state = trial;
                cost = trial_cost;
                lambda = (lambda / 3.0).max(1e-10);
                improved = rel > 1e-7;
                break;
            }
            lambda *= 4.0;
            if lambda > 1e12 {
                break;
            }
        }
        if !improved {
            break;
        }
    }
    report.cost = cost;
    finish(project, &state, cps, &img_lens, &mut report);
    report
}

fn finish(project: &mut Project, state: &State, cps: &[ControlPoint], img_lens: &[usize], report: &mut OptReport) {
    for (img, r) in project.images.iter_mut().zip(&state.rots) {
        // Re-orthonormalise to stop drift over many small updates.
        let q = nalgebra::UnitQuaternion::from_rotation_matrix(r);
        img.set_rot(&q.to_rotation_matrix());
    }
    project.lenses = state.lenses.clone();
    let (rms, max) = error_stats(project, cps, img_lens);
    report.rms = rms;
    report.max = max;
}

/// RMS and maximum control point distance in source pixels.
pub fn error_stats(project: &Project, cps: &[ControlPoint], img_lens: &[usize]) -> (f64, f64) {
    if cps.is_empty() {
        return (0.0, 0.0);
    }
    let models: Vec<LensModel> = project.lenses.iter().map(|l| l.model()).collect();
    let rots: Vec<Rotation3<f64>> = project.images.iter().map(|i| i.rot()).collect();
    let (mut sum, mut max) = (0.0, 0.0f64);
    for cp in cps {
        let (a, b) = (cp.a as usize, cp.b as usize);
        let e = cp_error(cp, &rots[a], &rots[b], &models[img_lens[a]], &models[img_lens[b]]);
        sum += e * e;
        max = max.max(e);
    }
    ((sum / cps.len() as f64).sqrt(), max)
}

/// Writes each control point's current distance into its `error` field.
pub fn update_errors(project: &mut Project) {
    let models: Vec<LensModel> = project.lenses.iter().map(|l| l.model()).collect();
    let rots: Vec<Rotation3<f64>> = project.images.iter().map(|i| i.rot()).collect();
    let lens: Vec<usize> = project.images.iter().map(|i| i.lens).collect();
    for cp in &mut project.control_points {
        let (a, b) = (cp.a as usize, cp.b as usize);
        cp.error = cp_error(cp, &rots[a], &rots[b], &models[lens[a]], &models[lens[b]]) as f32;
    }
}
