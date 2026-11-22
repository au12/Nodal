//! Dense overlap registration.
//!
//! After global alignment every overlapping pair is rendered into a shared rectified (gnomonic)
//! view centred on its overlap. If the model were perfect the two renders would be identical;
//! whatever displacement remains is lens-model error, parallax or motion, and it is measured
//! densely with coarse-to-fine block correlation (zero-mean normalised cross-correlation, with
//! sub-pixel peaks). The result is thousands of evenly spread correspondences per pair, which
//!
//! * feed a second bundle adjustment (sub-pixel measurements reaching into the frame corners,
//!   where feature matches are scarce),
//! * drive the local warps that remove residual parallax (`local::fit_dense`),
//! * and are reported as the true remaining misregistration of the stitch.

use crate::lens::{LensModel, V3};
use crate::project::{ControlPoint, Project};
use crate::{AlignOptions, Progress, Source};
use nalgebra::Rotation3;
use rayon::prelude::*;
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Clone, Debug, Default)]
pub struct DenseReport {
    pub log: Vec<String>,
    pub pairs: usize,
    pub matches: usize,
    /// RMS / median / 90th percentile residual of the dense matches (full-resolution pixels)
    /// under the global model before and after the second adjustment, and after local warps.
    pub rms_before: f64,
    pub rms_after: f64,
    pub median_after: f64,
    pub p90_after: f64,
    pub rms_warped: f64,
    pub median_warped: f64,
    pub p90_warped: f64,
    pub images_warped: usize,
}

/// One dense correspondence: full-resolution pixels in images `a` and `b`, with a confidence.
#[derive(Clone, Copy, Debug)]
pub struct DenseMatch {
    pub a: u32,
    pub b: u32,
    pub ax: f32,
    pub ay: f32,
    pub bx: f32,
    pub by: f32,
    /// Correlation peak (0–1).
    pub score: f32,
    /// Peak sharpness: inverse localisation variance in view pixels² (higher = more precise).
    pub sharpness: f32,
}

/// Single-channel float image with validity.
#[derive(Clone)]
pub struct Plane {
    pub w: usize,
    pub h: usize,
    pub v: Vec<f32>,
    pub ok: Vec<bool>,
}

impl Plane {
    fn new(w: usize, h: usize) -> Plane {
        Plane { w, h, v: vec![0.0; w * h], ok: vec![false; w * h] }
    }

    /// 2× reduction with a 2×2 box (valid only where all four are valid).
    fn half(&self) -> Plane {
        let (w, h) = ((self.w / 2).max(1), (self.h / 2).max(1));
        let mut out = Plane::new(w, h);
        out.v.par_chunks_mut(w).zip(out.ok.par_chunks_mut(w)).enumerate().for_each(|(y, (row, okr))| {
            for x in 0..w {
                let (x0, y0) = (2 * x, 2 * y);
                let (x1, y1) = ((x0 + 1).min(self.w - 1), (y0 + 1).min(self.h - 1));
                let idx = [y0 * self.w + x0, y0 * self.w + x1, y1 * self.w + x0, y1 * self.w + x1];
                if idx.iter().all(|&i| self.ok[i]) {
                    row[x] = 0.25 * idx.iter().map(|&i| self.v[i]).sum::<f32>();
                    okr[x] = true;
                }
            }
        });
        out
    }
}

/// Gnomonic view centred on a direction, levelled to the world's up axis.
#[derive(Clone, Copy, Debug)]
pub struct View {
    pub x: V3,
    pub y: V3,
    pub z: V3,
    pub f: f64,
    pub w: usize,
    pub h: usize,
    /// Tangent-plane position of the view centre, in view pixels (x right, y up).
    pub offset: (f64, f64),
}

impl View {
    pub fn new(centre: V3, f: f64, w: usize, h: usize) -> View {
        let z = centre.normalize();
        let up = V3::y();
        let mut x = up.cross(&z);
        if x.norm() < 1e-6 {
            x = V3::x();
        }
        let x = x.normalize();
        let y = z.cross(&x);
        View { x, y, z, f, w, h, offset: (0.0, 0.0) }
    }

    #[inline]
    pub fn dir(&self, u: f64, v: f64) -> V3 {
        let (cx, cy) = (self.w as f64 * 0.5 - self.offset.0, self.h as f64 * 0.5 + self.offset.1);
        (self.z + self.x * ((u - cx) / self.f) - self.y * ((v - cy) / self.f)).normalize()
    }

    #[inline]
    pub fn pixel(&self, d: &V3) -> Option<(f64, f64)> {
        let dz = d.dot(&self.z);
        if dz <= 1e-6 {
            return None;
        }
        let (cx, cy) = (self.w as f64 * 0.5 - self.offset.0, self.h as f64 * 0.5 + self.offset.1);
        Some((cx + self.f * d.dot(&self.x) / dz, cy - self.f * d.dot(&self.y) / dz))
    }
}

/// Perceptual luma of a working copy, with its scale to full resolution.
pub struct LumaImage {
    pub w: usize,
    pub h: usize,
    pub v: Vec<f32>,
    pub scale: f32,
}

impl LumaImage {
    pub fn from_source(s: &Source) -> LumaImage {
        let img = &s.image;
        let (w, h) = (img.width() as usize, img.height() as usize);
        let v: Vec<f32> = img.as_raw().par_chunks(3).map(|p| (0.299 * p[0] as f32 + 0.587 * p[1] as f32 + 0.114 * p[2] as f32) / 255.0).collect();
        LumaImage { w, h, v, scale: s.work_scale() }
    }

    /// Bilinear sample at full-resolution coordinates.
    #[inline]
    fn sample(&self, x: f32, y: f32) -> Option<f32> {
        let fx = x * self.scale - 0.5;
        let fy = y * self.scale - 0.5;
        if fx < 0.0 || fy < 0.0 || fx >= (self.w - 1) as f32 || fy >= (self.h - 1) as f32 {
            return None;
        }
        let (x0, y0) = (fx as usize, fy as usize);
        let (tx, ty) = (fx - x0 as f32, fy - y0 as f32);
        let i = y0 * self.w + x0;
        let top = self.v[i] + (self.v[i + 1] - self.v[i]) * tx;
        let bot = self.v[i + self.w] + (self.v[i + self.w + 1] - self.v[i + self.w]) * tx;
        Some(top + (bot - top) * ty)
    }
}

/// Everything needed to map an image into views.
struct Cam<'a> {
    model: LensModel,
    world_to_cam: Rotation3<f64>,
    luma: &'a LumaImage,
    mask: Option<&'a crate::project::ImageMask>,
    w: f32,
    h: f32,
    inset: f32,
}

impl Cam<'_> {
    #[inline]
    fn to_pixel(&self, d: &V3) -> Option<(f64, f64)> {
        let (x, y) = self.model.ray_to_pixel(self.world_to_cam * d)?;
        let (xf, yf) = (x as f32, y as f32);
        if xf < self.inset || yf < self.inset || xf > self.w - self.inset || yf > self.h - self.inset {
            return None;
        }
        if let Some(m) = self.mask {
            if m.excluded(xf, yf, self.w, self.h) {
                return None;
            }
        }
        Some((x, y))
    }
}

/// Renders a camera into a view (mapping evaluated on an 8-pixel grid and interpolated).
fn render_view(view: &View, cam: &Cam) -> Plane {
    const B: usize = 8;
    let (w, h) = (view.w, view.h);
    let gw = w.div_ceil(B) + 1;
    let gh = h.div_ceil(B) + 1;
    let nodes: Vec<Option<(f64, f64)>> = (0..gw * gh)
        .into_par_iter()
        .map(|k| {
            let (gx, gy) = (k % gw, k / gw);
            cam.model.ray_to_pixel(cam.world_to_cam * view.dir((gx * B) as f64, (gy * B) as f64))
        })
        .collect();
    let mut out = Plane::new(w, h);
    out.v.par_chunks_mut(w).zip(out.ok.par_chunks_mut(w)).enumerate().for_each(|(y, (row, okr))| {
        let by = y / B;
        let fy = (y % B) as f64 / B as f64 + 0.5 / B as f64;
        for x in 0..w {
            let bx = x / B;
            let fx = (x % B) as f64 / B as f64 + 0.5 / B as f64;
            let n = [nodes[by * gw + bx], nodes[by * gw + bx + 1], nodes[(by + 1) * gw + bx], nodes[(by + 1) * gw + bx + 1]];
            let (sx, sy) = if let [Some(a), Some(b), Some(c), Some(d)] = n {
                let top = (a.0 + (b.0 - a.0) * fx, a.1 + (b.1 - a.1) * fx);
                let bot = (c.0 + (d.0 - c.0) * fx, c.1 + (d.1 - c.1) * fx);
                (top.0 + (bot.0 - top.0) * fy, top.1 + (bot.1 - top.1) * fy)
            } else {
                match cam.model.ray_to_pixel(cam.world_to_cam * view.dir(x as f64 + 0.5, y as f64 + 0.5)) {
                    Some(p) => p,
                    None => continue,
                }
            };
            let (xf, yf) = (sx as f32, sy as f32);
            if xf < cam.inset || yf < cam.inset || xf > cam.w - cam.inset || yf > cam.h - cam.inset {
                continue;
            }
            if let Some(m) = cam.mask {
                if m.excluded(xf, yf, cam.w, cam.h) {
                    continue;
                }
            }
            if let Some(v) = cam.luma.sample(xf, yf) {
                row[x] = v;
                okr[x] = true;
            }
        }
    });
    out
}

/// Summed-area tables for fast block means and variances.
struct Integral {
    w: usize,
    s: Vec<f64>,
    s2: Vec<f64>,
}

impl Integral {
    fn new(p: &Plane) -> Integral {
        let (w, h) = (p.w + 1, p.h + 1);
        let mut s = vec![0.0f64; w * h];
        let mut s2 = vec![0.0f64; w * h];
        for y in 0..p.h {
            let mut rs = 0.0;
            let mut rs2 = 0.0;
            for x in 0..p.w {
                let v = p.v[y * p.w + x] as f64;
                rs += v;
                rs2 += v * v;
                s[(y + 1) * w + x + 1] = s[y * w + x + 1] + rs;
                s2[(y + 1) * w + x + 1] = s2[y * w + x + 1] + rs2;
            }
        }
        Integral { w, s, s2 }
    }

    /// Sum and sum of squares over [x0, x1) × [y0, y1).
    #[inline]
    fn block(&self, x0: usize, y0: usize, x1: usize, y1: usize) -> (f64, f64) {
        let w = self.w;
        let a = |t: &Vec<f64>| t[y1 * w + x1] - t[y0 * w + x1] - t[y1 * w + x0] + t[y0 * w + x0];
        (a(&self.s), a(&self.s2))
    }
}

/// Valid-count integral (to reject blocks touching invalid pixels).
struct ValidIntegral {
    w: usize,
    s: Vec<u32>,
}

impl ValidIntegral {
    fn new(p: &Plane) -> ValidIntegral {
        let (w, h) = (p.w + 1, p.h + 1);
        let mut s = vec![0u32; w * h];
        for y in 0..p.h {
            let mut rs = 0u32;
            for x in 0..p.w {
                rs += p.ok[y * p.w + x] as u32;
                s[(y + 1) * w + x + 1] = s[y * w + x + 1] + rs;
            }
        }
        ValidIntegral { w, s }
    }

    #[inline]
    fn full(&self, x0: usize, y0: usize, x1: usize, y1: usize) -> bool {
        let w = self.w;
        let n = self.s[y1 * w + x1] + self.s[y0 * w + x0] - self.s[y0 * w + x1] - self.s[y1 * w + x0];
        n as usize == (x1 - x0) * (y1 - y0)
    }
}

struct Level {
    a: Plane,
    b: Plane,
    ib: Integral,
    va: ValidIntegral,
    vb: ValidIntegral,
}

/// ZNCC between the block of `a` centred at (xa, ya) and the block of `b` centred at (xb, yb).
#[inline]
fn zncc(l: &Level, xa: isize, ya: isize, xb: isize, yb: isize, half: isize, a_mean: f64, a_norm: f64) -> Option<f64> {
    let (w, h) = (l.b.w as isize, l.b.h as isize);
    if xb - half < 0 || yb - half < 0 || xb + half >= w || yb + half >= h {
        return None;
    }
    let (x0, y0, x1, y1) = ((xb - half) as usize, (yb - half) as usize, (xb + half + 1) as usize, (yb + half + 1) as usize);
    if !l.vb.full(x0, y0, x1, y1) {
        return None;
    }
    let n = ((2 * half + 1) * (2 * half + 1)) as f64;
    let (sb, sb2) = l.ib.block(x0, y0, x1, y1);
    let b_mean = sb / n;
    let b_var = (sb2 - sb * b_mean).max(0.0);
    if b_var < 1e-9 {
        return None;
    }
    let aw = l.a.w as isize;
    let bw = l.b.w as isize;
    let mut sab = 0.0f64;
    for dy in -half..=half {
        let ra = ((ya + dy) * aw + xa - half) as usize;
        let rb = ((yb + dy) * bw + xb - half) as usize;
        let len = (2 * half + 1) as usize;
        let (pa, pb) = (&l.a.v[ra..ra + len], &l.b.v[rb..rb + len]);
        let mut acc = 0.0f32;
        for k in 0..len {
            acc += pa[k] * pb[k];
        }
        sab += acc as f64;
    }
    let cov = sab - n * a_mean * b_mean;
    Some(cov / (a_norm * b_var.sqrt()))
}

/// Coarse-to-fine dense displacement from `a` to `b` on a grid. Returns
/// (x, y, dx, dy, score, sharpness) in finest-level view pixels.
fn match_planes(a: Plane, b: Plane, step: usize, levels: usize) -> Vec<(f32, f32, f32, f32, f32, f32)> {
    let mut pyr: Vec<Level> = Vec::new();
    let (mut ca, mut cb) = (a, b);
    for l in 0..=levels {
        let ib = Integral::new(&cb);
        let va = ValidIntegral::new(&ca);
        let vb = ValidIntegral::new(&cb);
        let next = if l < levels { Some((ca.half(), cb.half())) } else { None };
        pyr.push(Level { a: ca, b: cb, ib, va, vb });
        match next {
            Some((na, nb)) => {
                ca = na;
                cb = nb;
            }
            None => break,
        }
    }
    let (w0, h0) = (pyr[0].a.w, pyr[0].a.h);
    let gx: Vec<usize> = (step / 2..w0.saturating_sub(step / 2)).step_by(step).collect();
    let gy: Vec<usize> = (step / 2..h0.saturating_sub(step / 2)).step_by(step).collect();
    let (nx, ny) = (gx.len(), gy.len());
    if nx == 0 || ny == 0 {
        return Vec::new();
    }
    // Flow per grid point in finest-level pixels; NaN = unknown.
    let mut flow = vec![(0.0f32, 0.0f32); nx * ny];
    let mut alive = vec![true; nx * ny];
    let mut score = vec![0.0f32; nx * ny];
    let mut sharp = vec![0.0f32; nx * ny];
    for l in (0..pyr.len()).rev() {
        let lev = &pyr[l];
        let sc = (1usize << l) as f32;
        let half: isize = if l == 0 { 6 } else { 5 };
        let radius: isize = if l == pyr.len() - 1 { 6 } else { 2 };
        let results: Vec<Option<(f32, f32, f32, f32)>> = (0..nx * ny)
            .into_par_iter()
            .map(|k| {
                if !alive[k] {
                    return None;
                }
                let (x0, y0) = (gx[k % nx], gy[k / nx]);
                let xa = (x0 as f32 / sc) as isize;
                let ya = (y0 as f32 / sc) as isize;
                let (aw, ah) = (lev.a.w as isize, lev.a.h as isize);
                if xa - half < 0 || ya - half < 0 || xa + half >= aw || ya + half >= ah {
                    return None;
                }
                let (bx0, by0, bx1, by1) = ((xa - half) as usize, (ya - half) as usize, (xa + half + 1) as usize, (ya + half + 1) as usize);
                if !lev.va.full(bx0, by0, bx1, by1) {
                    return None;
                }
                // Block statistics of a.
                let n = ((2 * half + 1) * (2 * half + 1)) as f64;
                let mut sa = 0.0f64;
                let mut sa2 = 0.0f64;
                for yy in by0..by1 {
                    for xx in bx0..bx1 {
                        let v = lev.a.v[yy * lev.a.w + xx] as f64;
                        sa += v;
                        sa2 += v * v;
                    }
                }
                let a_mean = sa / n;
                let a_var = (sa2 - sa * a_mean).max(0.0);
                // Texture: standard deviation per pixel below ~0.4 % of the range is featureless.
                if a_var / n < 1.5e-5 {
                    return None;
                }
                let a_norm = a_var.sqrt();
                let (fx, fy) = flow[k];
                let cx = xa + (fx / sc).round() as isize;
                let cy = ya + (fy / sc).round() as isize;
                let mut best = (f64::NEG_INFINITY, 0isize, 0isize);
                let side = (2 * radius + 1) as usize;
                let mut grid = vec![f64::NAN; side * side];
                for dy in -radius..=radius {
                    for dx in -radius..=radius {
                        if let Some(s) = zncc(lev, xa, ya, cx + dx, cy + dy, half, a_mean, a_norm) {
                            grid[((dy + radius) as usize) * side + (dx + radius) as usize] = s;
                            if s > best.0 {
                                best = (s, dx, dy);
                            }
                        }
                    }
                }
                if !best.0.is_finite() {
                    return None;
                }
                let (bs, bdx, bdy) = best;
                // Sub-pixel parabola fits through the peak.
                let at = |dx: isize, dy: isize| -> Option<f64> {
                    if dx.abs() <= radius && dy.abs() <= radius {
                        let v = grid[((dy + radius) as usize) * side + (dx + radius) as usize];
                        if v.is_finite() {
                            return Some(v);
                        }
                    }
                    zncc(lev, xa, ya, cx + dx, cy + dy, half, a_mean, a_norm)
                };
                let (l1, r1, u1, d1) = (at(bdx - 1, bdy), at(bdx + 1, bdy), at(bdx, bdy - 1), at(bdx, bdy + 1));
                let (mut ox, mut oy, mut curv) = (0.0f64, 0.0f64, 0.0f64);
                if let (Some(lv), Some(rv)) = (l1, r1) {
                    let den = lv - 2.0 * bs + rv;
                    if den < -1e-9 {
                        ox = (0.5 * (lv - rv) / den).clamp(-0.5, 0.5);
                        curv = -den;
                    }
                }
                if let (Some(uv), Some(dv)) = (u1, d1) {
                    let den = uv - 2.0 * bs + dv;
                    if den < -1e-9 {
                        oy = (0.5 * (uv - dv) / den).clamp(-0.5, 0.5);
                        curv = curv.min(-den);
                    } else {
                        curv = 0.0;
                    }
                } else {
                    curv = 0.0;
                }
                let dxf = ((cx + bdx) as f64 + ox - xa as f64) as f32 * sc;
                let dyf = ((cy + bdy) as f64 + oy - ya as f64) as f32 * sc;
                Some((dxf, dyf, bs as f32, curv as f32))
            })
            .collect();
        let ok: Vec<bool> = results.iter().map(|r| r.is_some()).collect();
        for k in 0..nx * ny {
            if let Some((dx, dy, s, c)) = results[k] {
                flow[k] = (dx, dy);
                score[k] = s;
                sharp[k] = c;
            }
        }
        // Robustness between levels: vectors that disagree with their neighbourhood median are
        // replaced by it (coarse levels) or dropped (finest level); points that could not be
        // measured inherit the median so the next level can try again.
        let snapshot = flow.clone();
        for yy in 0..ny {
            for xx in 0..nx {
                let k = yy * nx + xx;
                let mut vx = Vec::with_capacity(9);
                let mut vy = Vec::with_capacity(9);
                for dy in -1i32..=1 {
                    for dx in -1i32..=1 {
                        let (x2, y2) = (xx as i32 + dx, yy as i32 + dy);
                        if (dx == 0 && dy == 0) || x2 < 0 || y2 < 0 || x2 >= nx as i32 || y2 >= ny as i32 {
                            continue;
                        }
                        let k2 = y2 as usize * nx + x2 as usize;
                        if ok[k2] {
                            vx.push(snapshot[k2].0);
                            vy.push(snapshot[k2].1);
                        }
                    }
                }
                let median = if vx.len() >= 2 {
                    vx.sort_by(|a, b| a.total_cmp(b));
                    vy.sort_by(|a, b| a.total_cmp(b));
                    Some((vx[vx.len() / 2], vy[vy.len() / 2]))
                } else {
                    None
                };
                if l > 0 {
                    alive[k] = true;
                    match (ok[k], median) {
                        (true, Some((mx, my))) => {
                            let dev = ((snapshot[k].0 - mx).powi(2) + (snapshot[k].1 - my).powi(2)).sqrt();
                            if dev > 1.5 * sc && vx.len() >= 3 {
                                flow[k] = (mx, my);
                            }
                        }
                        (false, Some(m)) => flow[k] = m,
                        _ => {}
                    }
                } else {
                    alive[k] = ok[k]
                        && match median {
                            Some((mx, my)) if vx.len() >= 3 => ((snapshot[k].0 - mx).powi(2) + (snapshot[k].1 - my).powi(2)).sqrt() <= 1.5,
                            Some(_) => true,
                            None => false,
                        };
                }
            }
        }
    }
    (0..nx * ny)
        .filter(|&k| alive[k] && score[k] > 0.75 && sharp[k] > 0.02)
        .map(|k| (gx[k % nx] as f32 + 0.5, gy[k / nx] as f32 + 0.5, flow[k].0, flow[k].1, score[k], sharp[k]))
        .collect()
}

/// Candidate overlapping pairs with their overlap centre and angular radius.
fn overlap_pairs(project: &Project) -> Vec<(usize, usize, V3, f64, Vec<V3>)> {
    let active: Vec<usize> = project.active().collect();
    let models: Vec<LensModel> = project.lenses.iter().map(|l| l.model()).collect();
    let rots: Vec<Rotation3<f64>> = project.images.iter().map(|i| i.rot()).collect();
    let pairs: Vec<(usize, usize)> = active.iter().enumerate().flat_map(|(k, &i)| active[k + 1..].iter().map(move |&j| (i, j))).collect();
    pairs
        .par_iter()
        .filter_map(|&(i, j)| {
            let (mi, mj) = (&models[project.images[i].lens], &models[project.images[j].lens]);
            let (ri, rj) = (&rots[i], &rots[j]);
            let ax_i = ri * V3::z();
            let ax_j = rj * V3::z();
            let fov = |m: &LensModel| (m.w.hypot(m.h) * 0.5 / m.f).atan().min(1.5);
            if ax_i.angle(&ax_j) > fov(mi) + fov(mj) {
                return None;
            }
            // Sample image i on a grid; keep the directions image j also sees.
            const N: usize = 24;
            let mut dirs: Vec<V3> = Vec::new();
            let inv_j = rj.inverse();
            for gy in 0..N {
                for gx in 0..N {
                    let x = (gx as f64 + 0.5) / N as f64 * mi.w;
                    let y = (gy as f64 + 0.5) / N as f64 * mi.h;
                    let d = ri * mi.pixel_to_ray(x, y);
                    if let Some((u, v)) = mj.ray_to_pixel(inv_j * d) {
                        if u > 0.0 && v > 0.0 && u < mj.w && v < mj.h {
                            dirs.push(d);
                        }
                    }
                }
            }
            // At least ~8 % of the frame.
            if dirs.len() < N * N / 12 {
                return None;
            }
            let c = dirs.iter().sum::<V3>().normalize();
            let radius = dirs.iter().map(|d| d.angle(&c)).fold(0.0f64, f64::max) + 2.0 / N as f64;
            Some((i, j, c, radius.min(1.25), dirs))
        })
        .collect()
}

/// Dense matches for every overlapping pair under the project's current model.
pub fn register(project: &Project, lumas: &[Option<LumaImage>], progress: Progress) -> Vec<DenseMatch> {
    let mut pairs = overlap_pairs(project);
    // Redundant handheld sets overlap many ways: measure the substantial overlaps, plus each
    // photo's three largest so every photo stays well connected.
    {
        let n = project.images.len();
        let mut per: Vec<Vec<(usize, usize)>> = vec![Vec::new(); n];
        for (k, p) in pairs.iter().enumerate() {
            per[p.0].push((p.4.len(), k));
            per[p.1].push((p.4.len(), k));
        }
        let mut keep = vec![false; pairs.len()];
        for list in per.iter_mut() {
            list.sort_by(|a, b| b.0.cmp(&a.0));
            for &(_, k) in list.iter().take(3) {
                keep[k] = true;
            }
        }
        const N: usize = 24;
        for (k, p) in pairs.iter().enumerate() {
            if p.4.len() >= N * N * 18 / 100 {
                keep[k] = true;
            }
        }
        let mut k = 0;
        pairs.retain(|_| {
            k += 1;
            keep[k - 1]
        });
    }
    let models: Vec<LensModel> = project.lenses.iter().map(|l| l.model()).collect();
    let rots: Vec<Rotation3<f64>> = project.images.iter().map(|i| i.rot()).collect();
    let done = AtomicUsize::new(0);
    let total = pairs.len().max(1);
    pairs
        .par_iter()
        .flat_map_iter(|(i, j, centre, _radius, dirs)| {
            let (i, j, centre) = (*i, *j, *centre);
            let (Some(li), Some(lj)) = (&lumas[i], &lumas[j]) else { return Vec::new() };
            fn cam<'c>(project: &'c Project, models: &[LensModel], rots: &[Rotation3<f64>], k: usize, luma: &'c LumaImage) -> Cam<'c> {
                let img = &project.images[k];
                let lens = project.lens_of(k);
                Cam {
                    model: models[img.lens],
                    world_to_cam: rots[k].inverse(),
                    luma,
                    mask: img.mask.as_ref().filter(|m| !m.is_empty()),
                    w: lens.width as f32,
                    h: lens.height as f32,
                    inset: (3.0 / luma.scale).max(2.0),
                }
            }
            let (ci, cj) = (cam(project, &models, &rots, i, li), cam(project, &models, &rots, j, lj));
            // View at about the working copies' resolution, cropped to the overlap's bounding box
            // and capped in size (precision then drops a little, speed does not collapse).
            let mut f = models[project.images[i].lens].f.min(models[project.images[j].lens].f) * li.scale.min(lj.scale) as f64;
            let probe = View::new(centre, 1.0, 0, 0);
            let (mut u0, mut u1, mut v0, mut v1) = (f64::MAX, f64::MIN, f64::MAX, f64::MIN);
            for d in dirs {
                if let Some((u, v)) = probe.pixel(d) {
                    u0 = u0.min(u);
                    u1 = u1.max(u);
                    v0 = v0.min(v);
                    v1 = v1.max(v);
                }
            }
            if !(u1 > u0 && v1 > v0) {
                return Vec::new();
            }
            // Grid spacing of the overlap sample is ~1/24 of the frame: pad by that much.
            let pad = 0.06 * (u1 - u0).max(v1 - v0);
            let (u0, u1, v0, v1) = (u0 - pad, u1 + pad, v0 - pad, v1 + pad);
            const MAX_SIDE: f64 = 1400.0;
            let longest = (u1 - u0).max(v1 - v0) * f;
            if longest > MAX_SIDE {
                f *= MAX_SIDE / longest;
            }
            let (w, h) = (((u1 - u0) * f).ceil() as usize, ((v1 - v0) * f).ceil() as usize);
            // Shift the centre so the rectangle is centred: the view's principal point sits at
            // the rectangle centre in tangent-plane units.
            let cu = 0.5 * (u0 + u1);
            let cv = 0.5 * (v0 + v1);
            let centre2 = (probe.z + probe.x * cu - probe.y * cv).normalize();
            let mut view = View::new(centre2, f, w.max(64), h.max(64));
            // Keep the original axes (re-levelling around a shifted centre is fine; the plane only
            // needs to be consistent for both images).
            view.x = probe.x;
            view.y = probe.y;
            view.z = probe.z;
            view.offset = (cu * f, -cv * f);
            let (pa, pb) = (render_view(&view, &ci), render_view(&view, &cj));
            let both = pa.ok.iter().zip(&pb.ok).filter(|(a, b)| **a && **b).count();
            if both < 64 * 64 {
                return Vec::new();
            }
            let debug = std::env::var("NODAL_DENSE_DEBUG").ok().filter(|v| v == &format!("{i}-{j}"));
            let dbg_planes = debug.as_ref().map(|_| (pa.clone(), pb.clone()));
            let raw = match_planes(pa, pb, 16, 4);
            let _ = centre;
            if let Some((da, db)) = dbg_planes {
                let save = |p: &Plane, name: &str| {
                    let px: Vec<u8> = p.v.iter().zip(&p.ok).map(|(v, ok)| if *ok { (v * 255.0).clamp(0.0, 255.0) as u8 } else { 0 }).collect();
                    let _ = image::GrayImage::from_raw(p.w as u32, p.h as u32, px).map(|g| g.save(format!("/tmp/dense_{i}_{j}_{name}.png")));
                };
                save(&da, "a");
                save(&db, "b");
                let txt: String = raw.iter().map(|r| format!("{:.1} {:.1} {:.2} {:.2} {:.3} {:.3}\n", r.0, r.1, r.2, r.3, r.4, r.5)).collect();
                let _ = std::fs::write(format!("/tmp/dense_{i}_{j}_flow.txt"), txt);
            }
            let mut out = Vec::with_capacity(raw.len());
            for (x, y, dx, dy, s, c) in raw {
                let da = view.dir(x as f64, y as f64);
                let db = view.dir((x + dx) as f64, (y + dy) as f64);
                let (Some(pa), Some(pb)) = (ci.to_pixel(&da), cj.to_pixel(&db)) else { continue };
                out.push(DenseMatch { a: i as u32, b: j as u32, ax: pa.0 as f32, ay: pa.1 as f32, bx: pb.0 as f32, by: pb.1 as f32, score: s, sharpness: c });
            }
            let k = done.fetch_add(1, Ordering::Relaxed) + 1;
            progress(k as f32 / total as f32, "Measuring overlaps");
            out
        })
        .collect()
}

/// Residual of a dense match under the project's model (full-resolution pixels of image a),
/// optionally including the local warps.
pub fn residual(project: &Project, m: &DenseMatch, with_warp: bool) -> f64 {
    let (a, b) = (m.a as usize, m.b as usize);
    let (la, lb) = (project.lens_of(a), project.lens_of(b));
    let (ma, mb) = (la.model(), lb.model());
    let (mut ax, mut ay, mut bx, mut by) = (m.ax, m.ay, m.bx, m.by);
    if with_warp {
        // The renderer samples source position t + D(t) for the direction that maps to t, so the
        // direction a measured pixel q shows is the one whose t satisfies t + D(t) = q.
        if let Some(f) = &project.images[a].warp {
            (ax, ay) = crate::local::unwarp(Some(f), ax, ay, la.width as f32, la.height as f32);
        }
        if let Some(f) = &project.images[b].warp {
            (bx, by) = crate::local::unwarp(Some(f), bx, by, lb.width as f32, lb.height as f32);
        }
    }
    let da: V3 = project.images[a].rot() * ma.pixel_to_ray(ax as f64, ay as f64);
    let db: V3 = project.images[b].rot() * mb.pixel_to_ray(bx as f64, by as f64);
    crate::global::vec_angle(&da, &db) * ma.f
}

/// (RMS of the best 95 %, median, 90th percentile): a few gross mismatches (moving people,
/// repetitive texture) should not dominate the figure.
fn stats(mut v: Vec<f64>) -> (f64, f64, f64) {
    if v.is_empty() {
        return (0.0, 0.0, 0.0);
    }
    v.sort_by(|a, b| a.total_cmp(b));
    let keep = &v[..(v.len() * 95 / 100).max(1)];
    let rms = (keep.iter().map(|x| x * x).sum::<f64>() / keep.len() as f64).sqrt();
    (rms, v[v.len() / 2], v[v.len() * 9 / 10])
}

/// Picks well-spread, confident matches of each pair as control points.
pub fn to_control_points(project: &Project, matches: &[DenseMatch], per_pair: usize) -> Vec<ControlPoint> {
    use std::collections::HashMap;
    // Rotations should be decided by content far enough away that the camera's movement does not
    // matter. Near content (large residual parallax) is left to the local warps.
    let res: Vec<f64> = matches.par_iter().map(|m| residual(project, m, false)).collect();
    let mut sorted = res.clone();
    sorted.sort_by(|a, b| a.total_cmp(b));
    let limit = sorted.get(sorted.len() * 6 / 10).copied().unwrap_or(f64::MAX).max(3.0);
    let mut by_pair: HashMap<(u32, u32), Vec<&DenseMatch>> = HashMap::new();
    for (m, r) in matches.iter().zip(&res) {
        if *r <= limit {
            by_pair.entry((m.a, m.b)).or_default().push(m);
        }
    }
    let mut out = Vec::new();
    for ((a, _), list) in by_pair {
        let la = project.lens_of(a as usize);
        const G: usize = 10;
        let mut cells: Vec<Option<&DenseMatch>> = vec![None; G * G];
        for m in list {
            let gx = ((m.ax / la.width as f32 * G as f32) as usize).min(G - 1);
            let gy = ((m.ay / la.height as f32 * G as f32) as usize).min(G - 1);
            let c = &mut cells[gy * G + gx];
            let better = match c {
                None => true,
                Some(o) => m.score * m.sharpness.min(1.0) > o.score * o.sharpness.min(1.0),
            };
            if better {
                *c = Some(m);
            }
        }
        out.extend(cells.into_iter().flatten().take(per_pair).map(|m| ControlPoint {
            a: m.a,
            b: m.b,
            ax: m.ax,
            ay: m.ay,
            bx: m.bx,
            by: m.by,
            error: 0.0,
        }));
    }
    out
}

/// Dense registration of every overlap, a second adjustment with the dense measurements, and
/// local warps where residual parallax remains.
pub fn refine_project(project: &mut Project, sources: &[Source], opts: &AlignOptions, progress: Progress) -> DenseReport {
    let mut report = DenseReport::default();
    let lumas: Vec<Option<LumaImage>> = (0..project.images.len())
        .into_par_iter()
        .map(|i| (project.images[i].aligned && project.images[i].enabled).then(|| LumaImage::from_source(&sources[i])))
        .collect();
    progress(0.0, "Measuring overlaps");
    let t0 = std::time::Instant::now();
    let matches = register(project, &lumas, &|f, s| progress(f * 0.6, s));
    let t_register = t0.elapsed().as_secs_f32();
    drop(lumas);
    report.matches = matches.len();
    report.pairs = {
        let mut p: Vec<(u32, u32)> = matches.iter().map(|m| (m.a, m.b)).collect();
        p.sort();
        p.dedup();
        p.len()
    };
    if matches.len() < 50 {
        report.log.push("Dense registration: too little overlap to measure".into());
        return report;
    }
    let (rms0, _, _) = stats(matches.par_iter().map(|m| residual(project, m, false)).collect());
    report.rms_before = rms0;
    // Second adjustment: the feature control points plus well-spread dense measurements.
    progress(0.62, "Optimising with dense measurements");
    let dense_cps = to_control_points(project, &matches, 60);
    let saved = project.control_points.clone();
    project.control_points.extend(dense_cps.iter().copied());
    let t1 = std::time::Instant::now();
    crate::align::polish(project, opts);
    let t_polish = t1.elapsed().as_secs_f32();
    project.control_points = saved;
    project.control_points.extend(dense_cps);
    crate::optimize::update_errors(project);
    let res_after: Vec<f64> = matches.par_iter().map(|m| residual(project, m, false)).collect();
    let (rms1, med1, p90_1) = stats(res_after);
    report.rms_after = rms1;
    report.median_after = med1;
    report.p90_after = p90_1;
    report.log.push(format!(
        "Dense registration: {} pairs, {} measurements; misregistration (best 95 %) RMS {:.2} → {:.2} px (median {:.2}, 90 % under {:.2})",
        report.pairs, report.matches, rms0, rms1, med1, p90_1
    ));
    // Local warps for the parallax the global model cannot explain.
    if project.output.local_alignment {
        progress(0.8, "Correcting parallax");
        let t2 = std::time::Instant::now();
        let warped = crate::local::fit_dense(project, &matches);
        if std::env::var_os("NODAL_PROFILE").is_some() {
            eprintln!("dense: register {t_register:.2}s polish {t_polish:.2}s warps {:.2}s", t2.elapsed().as_secs_f32());
        }
        report.images_warped = warped;
        let (rms2, med2, p90_2) = stats(matches.par_iter().map(|m| residual(project, m, true)).collect());
        report.rms_warped = rms2;
        report.median_warped = med2;
        report.p90_warped = p90_2;
        if warped > 0 {
            report.log.push(format!(
                "Local warps on {warped} photos: misregistration RMS {:.2} → {:.2} px (median {:.2}, 90 % under {:.2})",
                rms1, rms2, med2, p90_2
            ));
        }
    }
    progress(1.0, "Overlaps measured");
    report
}
