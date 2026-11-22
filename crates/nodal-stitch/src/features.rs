//! Scale- and rotation-invariant features (SIFT detector, RootSIFT descriptors), multi-threaded.
//!
//! Detection runs on a downscaled grey copy of each photo; coordinates are reported in
//! full-resolution continuous pixels (pixel centres at half-integers).

use rayon::prelude::*;
use std::f32::consts::PI;

/// A single-channel float image in [0, 1].
#[derive(Clone)]
pub struct Gray {
    pub w: usize,
    pub h: usize,
    pub data: Vec<f32>,
}

impl Gray {
    pub fn new(w: usize, h: usize) -> Self {
        Gray { w, h, data: vec![0.0; w * h] }
    }

    #[inline]
    pub fn at(&self, x: usize, y: usize) -> f32 {
        self.data[y * self.w + x]
    }

    #[inline]
    pub fn row(&self, y: usize) -> &[f32] {
        &self.data[y * self.w..(y + 1) * self.w]
    }

    /// Area-averaged grey downscale of an RGB8 image so its longer side is at most `max_dim`.
    /// Returns the image and the factor full-res / working-res.
    pub fn from_rgb8(rgb: &[u8], w: usize, h: usize, max_dim: usize) -> (Gray, f32) {
        let scale = (w.max(h) as f32 / max_dim as f32).max(1.0);
        let ow = ((w as f32 / scale).round() as usize).max(1);
        let oh = ((h as f32 / scale).round() as usize).max(1);
        let sx = w as f32 / ow as f32;
        let sy = h as f32 / oh as f32;
        let mut out = Gray::new(ow, oh);
        out.data.par_chunks_mut(ow).enumerate().for_each(|(oy, row)| {
            let y0 = (oy as f32 * sy) as usize;
            let y1 = (((oy + 1) as f32 * sy).ceil() as usize).clamp(y0 + 1, h);
            for (ox, o) in row.iter_mut().enumerate() {
                let x0 = (ox as f32 * sx) as usize;
                let x1 = (((ox + 1) as f32 * sx).ceil() as usize).clamp(x0 + 1, w);
                let mut sum = 0u32;
                for y in y0..y1 {
                    let line = &rgb[(y * w + x0) * 3..(y * w + x1) * 3];
                    for px in line.chunks_exact(3) {
                        // Rec. 601 luma in 8.8 fixed point.
                        sum += 77 * px[0] as u32 + 150 * px[1] as u32 + 29 * px[2] as u32;
                    }
                }
                let n = ((y1 - y0) * (x1 - x0)) as f32;
                *o = sum as f32 / (n * 256.0 * 255.0);
            }
        });
        (out, (sx + sy) * 0.5)
    }

    fn half(&self) -> Gray {
        let (w, h) = ((self.w / 2).max(1), (self.h / 2).max(1));
        let mut out = Gray::new(w, h);
        for y in 0..h {
            let src = self.row((y * 2).min(self.h - 1));
            let dst = &mut out.data[y * w..(y + 1) * w];
            for (x, d) in dst.iter_mut().enumerate() {
                *d = src[(x * 2).min(self.w - 1)];
            }
        }
        out
    }
}

fn gaussian_kernel(sigma: f32) -> Vec<f32> {
    let r = ((sigma * 3.5).ceil() as usize).max(1);
    let mut k: Vec<f32> = (0..=2 * r).map(|i| (-((i as f32 - r as f32).powi(2)) / (2.0 * sigma * sigma)).exp()).collect();
    let s: f32 = k.iter().sum();
    k.iter_mut().for_each(|v| *v /= s);
    k
}

/// Separable Gaussian blur with clamped borders.
pub fn gaussian_blur(src: &Gray, sigma: f32) -> Gray {
    let k = gaussian_kernel(sigma);
    let r = k.len() / 2;
    let (w, h) = (src.w, src.h);
    let mut tmp = Gray::new(w, h);
    tmp.data.par_chunks_mut(w).enumerate().for_each(|(y, out)| {
        let row = src.row(y);
        if w > 2 * r {
            for (i, &kv) in k.iter().enumerate() {
                let src_part = &row[i..i + w - 2 * r];
                for (o, &s) in out[r..w - r].iter_mut().zip(src_part) {
                    *o += kv * s;
                }
            }
        }
        let edge = r.min(w);
        for x in (0..edge).chain(w.saturating_sub(r).max(edge)..w) {
            let mut acc = 0.0;
            for (i, &kv) in k.iter().enumerate() {
                let sx = (x as isize + i as isize - r as isize).clamp(0, w as isize - 1) as usize;
                acc += kv * row[sx];
            }
            out[x] = acc;
        }
    });
    let mut out = Gray::new(w, h);
    out.data.par_chunks_mut(w).enumerate().for_each(|(y, o)| {
        for (i, &kv) in k.iter().enumerate() {
            let sy = (y as isize + i as isize - r as isize).clamp(0, h as isize - 1) as usize;
            for (d, &s) in o.iter_mut().zip(tmp.row(sy)) {
                *d += kv * s;
            }
        }
    });
    out
}

#[derive(Clone, Copy, Debug)]
pub struct Feature {
    /// Full-resolution continuous pixel position.
    pub x: f32,
    pub y: f32,
    /// Feature scale (σ) in full-resolution pixels.
    pub scale: f32,
    /// Dominant gradient orientation in radians.
    pub angle: f32,
    /// Contrast of the difference-of-Gaussians extremum.
    pub response: f32,
}

pub const DESC_LEN: usize = 128;

#[derive(Clone, Default)]
pub struct FeatureSet {
    pub feats: Vec<Feature>,
    /// `feats.len() × 128` RootSIFT descriptors.
    pub desc: Vec<u8>,
}

impl FeatureSet {
    pub fn len(&self) -> usize {
        self.feats.len()
    }

    pub fn is_empty(&self) -> bool {
        self.feats.is_empty()
    }

    #[inline]
    pub fn descriptor(&self, i: usize) -> &[u8] {
        &self.desc[i * DESC_LEN..(i + 1) * DESC_LEN]
    }

    /// The first `n` features (they are sorted strongest first, spatially balanced).
    pub fn head(&self, n: usize) -> FeatureSet {
        let n = n.min(self.len());
        FeatureSet { feats: self.feats[..n].to_vec(), desc: self.desc[..n * DESC_LEN].to_vec() }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct DetectOptions {
    /// Longer side of the working image in pixels.
    pub max_dim: usize,
    /// Maximum number of features kept per image.
    pub max_features: usize,
    pub contrast_threshold: f32,
    pub edge_threshold: f32,
}

impl Default for DetectOptions {
    fn default() -> Self {
        DetectOptions { max_dim: 1600, max_features: 4000, contrast_threshold: 0.03, edge_threshold: 10.0 }
    }
}

const S: usize = 3; // intervals per octave
const SIGMA0: f32 = 1.6;
const BORDER: usize = 5;

struct Octave {
    gauss: Vec<Gray>,
    dog: Vec<Gray>,
}

fn build_pyramid(base: Gray) -> Vec<Octave> {
    let n_oct = (((base.w.min(base.h)) as f32).log2().floor() as isize - 3).clamp(1, 8) as usize;
    let k = 2f32.powf(1.0 / S as f32);
    let sig: Vec<f32> = (1..S + 3)
        .map(|i| {
            let prev = SIGMA0 * k.powi(i as i32 - 1);
            let total = prev * k;
            (total * total - prev * prev).sqrt()
        })
        .collect();
    let mut octaves = Vec::with_capacity(n_oct);
    let mut first = base;
    for _ in 0..n_oct {
        let mut gauss = Vec::with_capacity(S + 3);
        gauss.push(first);
        for s in &sig {
            let next = gaussian_blur(gauss.last().unwrap(), *s);
            gauss.push(next);
        }
        let dog: Vec<Gray> = (0..S + 2)
            .map(|i| {
                let (a, b) = (&gauss[i], &gauss[i + 1]);
                let mut d = Gray::new(a.w, a.h);
                d.data.par_iter_mut().zip(a.data.par_iter().zip(b.data.par_iter())).for_each(|(o, (x, y))| *o = y - x);
                d
            })
            .collect();
        first = gauss[S].half();
        octaves.push(Octave { gauss, dog });
        if first.w < 2 * BORDER + 3 || first.h < 2 * BORDER + 3 {
            break;
        }
    }
    octaves
}

struct Keypoint {
    octave: usize,
    layer: usize,
    /// Position in octave pixels (integer-centred).
    x: f32,
    y: f32,
    /// Scale relative to the octave.
    scl: f32,
    response: f32,
    angle: f32,
}

fn refine(oct: &Octave, mut s: usize, mut y: usize, mut x: usize, opts: &DetectOptions) -> Option<(usize, f32, f32, f32, f32)> {
    let (w, h) = (oct.dog[0].w, oct.dog[0].h);
    for _ in 0..5 {
        let (p, c, n) = (&oct.dog[s - 1], &oct.dog[s], &oct.dog[s + 1]);
        let v = c.at(x, y);
        let dx = (c.at(x + 1, y) - c.at(x - 1, y)) * 0.5;
        let dy = (c.at(x, y + 1) - c.at(x, y - 1)) * 0.5;
        let ds = (n.at(x, y) - p.at(x, y)) * 0.5;
        let dxx = c.at(x + 1, y) + c.at(x - 1, y) - 2.0 * v;
        let dyy = c.at(x, y + 1) + c.at(x, y - 1) - 2.0 * v;
        let dss = n.at(x, y) + p.at(x, y) - 2.0 * v;
        let dxy = (c.at(x + 1, y + 1) - c.at(x - 1, y + 1) - c.at(x + 1, y - 1) + c.at(x - 1, y - 1)) * 0.25;
        let dxs = (n.at(x + 1, y) - n.at(x - 1, y) - p.at(x + 1, y) + p.at(x - 1, y)) * 0.25;
        let dys = (n.at(x, y + 1) - n.at(x, y - 1) - p.at(x, y + 1) + p.at(x, y - 1)) * 0.25;
        // Solve H·X = −g by Cramer's rule.
        let det = dxx * (dyy * dss - dys * dys) - dxy * (dxy * dss - dys * dxs) + dxs * (dxy * dys - dyy * dxs);
        if det.abs() < 1e-18 {
            return None;
        }
        let (gx, gy, gs) = (-dx, -dy, -ds);
        let xc = (gx * (dyy * dss - dys * dys) - dxy * (gy * dss - dys * gs) + dxs * (gy * dys - dyy * gs)) / det;
        let xr = (dxx * (gy * dss - gs * dys) - gx * (dxy * dss - dys * dxs) + dxs * (dxy * gs - gy * dxs)) / det;
        let xi = (dxx * (dyy * gs - dys * gy) - dxy * (dxy * gs - gy * dxs) + gx * (dxy * dys - dyy * dxs)) / det;
        if xc.abs() < 0.5 && xr.abs() < 0.5 && xi.abs() < 0.5 {
            // Contrast and edge tests at the refined location.
            let contrast = v + 0.5 * (dx * xc + dy * xr + ds * xi);
            if contrast.abs() * (S as f32) < opts.contrast_threshold {
                return None;
            }
            let tr = dxx + dyy;
            let det2 = dxx * dyy - dxy * dxy;
            let r = opts.edge_threshold;
            if det2 <= 0.0 || tr * tr * r >= (r + 1.0) * (r + 1.0) * det2 {
                return None;
            }
            return Some((s, x as f32 + xc, y as f32 + xr, xi, contrast.abs()));
        }
        if xc.abs() > 1e6 || xr.abs() > 1e6 || xi.abs() > 1e6 {
            return None;
        }
        let nx = x as isize + xc.round() as isize;
        let ny = y as isize + xr.round() as isize;
        let ns = s as isize + xi.round() as isize;
        if ns < 1 || ns > S as isize || nx < BORDER as isize || ny < BORDER as isize || nx >= (w - BORDER) as isize || ny >= (h - BORDER) as isize {
            return None;
        }
        (x, y, s) = (nx as usize, ny as usize, ns as usize);
    }
    None
}

/// Dominant gradient orientations around a keypoint (up to 3).
fn orientations(img: &Gray, x: f32, y: f32, scl: f32, out: &mut Vec<f32>) {
    const BINS: usize = 36;
    let radius = (3.0 * 1.5 * scl).round() as isize;
    let sigma = 1.5 * scl;
    let expf = -1.0 / (2.0 * sigma * sigma);
    let (cx, cy) = (x.round() as isize, y.round() as isize);
    let mut hist = [0.0f32; BINS];
    for i in -radius..=radius {
        let yy = cy + i;
        if yy <= 0 || yy >= img.h as isize - 1 {
            continue;
        }
        for j in -radius..=radius {
            let xx = cx + j;
            if xx <= 0 || xx >= img.w as isize - 1 {
                continue;
            }
            let (ux, uy) = (xx as usize, yy as usize);
            let dx = img.at(ux + 1, uy) - img.at(ux - 1, uy);
            let dy = img.at(ux, uy - 1) - img.at(ux, uy + 1);
            let mag = (dx * dx + dy * dy).sqrt();
            let ang = dy.atan2(dx);
            let wgt = ((i * i + j * j) as f32 * expf).exp();
            let mut bin = ((ang + PI) / (2.0 * PI) * BINS as f32).floor() as isize;
            bin = bin.rem_euclid(BINS as isize);
            hist[bin as usize] += wgt * mag;
        }
    }
    // Smooth the histogram.
    let mut sm = [0.0f32; BINS];
    for i in 0..BINS {
        let g = |k: isize| hist[(i as isize + k).rem_euclid(BINS as isize) as usize];
        sm[i] = (g(-2) + g(2)) / 16.0 + (g(-1) + g(1)) * 4.0 / 16.0 + g(0) * 6.0 / 16.0;
    }
    let max = sm.iter().cloned().fold(0.0f32, f32::max);
    if max <= 0.0 {
        return;
    }
    let thr = max * 0.8;
    let start = out.len();
    for i in 0..BINS {
        let l = sm[(i + BINS - 1) % BINS];
        let r = sm[(i + 1) % BINS];
        if sm[i] > l && sm[i] > r && sm[i] >= thr {
            let off = 0.5 * (l - r) / (l - 2.0 * sm[i] + r);
            let bin = i as f32 + 0.5 + off;
            out.push(bin / BINS as f32 * 2.0 * PI - PI);
            if out.len() - start >= 3 {
                break;
            }
        }
    }
}

fn descriptor(img: &Gray, x: f32, y: f32, scl: f32, ori: f32, dst: &mut [u8]) {
    const D: usize = 4;
    const N: usize = 8;
    let hist_width = 3.0 * scl;
    let diag = ((img.w * img.w + img.h * img.h) as f32).sqrt();
    let radius = ((hist_width * std::f32::consts::SQRT_2 * (D as f32 + 1.0) * 0.5).round().min(diag)) as isize;
    let (sin_o, cos_o) = ori.sin_cos();
    let cos_t = cos_o / hist_width;
    let sin_t = sin_o / hist_width;
    let bins_per_rad = N as f32 / (2.0 * PI);
    let exp_scale = -1.0 / (D as f32 * D as f32 * 0.5);
    let mut hist = [0.0f32; (D + 2) * (D + 2) * (N + 2)];
    let (cx, cy) = (x.round() as isize, y.round() as isize);
    for i in -radius..=radius {
        let r = cy + i;
        if r <= 0 || r >= img.h as isize - 1 {
            continue;
        }
        for j in -radius..=radius {
            let c = cx + j;
            if c <= 0 || c >= img.w as isize - 1 {
                continue;
            }
            let c_rot = j as f32 * cos_t - i as f32 * sin_t;
            let r_rot = j as f32 * sin_t + i as f32 * cos_t;
            let rbin = r_rot + D as f32 / 2.0 - 0.5;
            let cbin = c_rot + D as f32 / 2.0 - 0.5;
            if rbin <= -1.0 || rbin >= D as f32 || cbin <= -1.0 || cbin >= D as f32 {
                continue;
            }
            let (ux, uy) = (c as usize, r as usize);
            let dx = img.at(ux + 1, uy) - img.at(ux - 1, uy);
            let dy = img.at(ux, uy - 1) - img.at(ux, uy + 1);
            let mag = (dx * dx + dy * dy).sqrt() * ((c_rot * c_rot + r_rot * r_rot) * exp_scale).exp();
            let mut obin = (dy.atan2(dx) - ori) * bins_per_rad;
            let r0 = rbin.floor();
            let c0 = cbin.floor();
            let o0 = obin.floor();
            let (fr, fc, fo) = (rbin - r0, cbin - c0, obin - o0);
            obin = o0;
            let mut o0i = obin as isize;
            o0i = o0i.rem_euclid(N as isize);
            let (r0i, c0i) = (r0 as isize + 1, c0 as isize + 1);
            let v_r1 = mag * fr;
            let v_r0 = mag - v_r1;
            let v_rc11 = v_r1 * fc;
            let v_rc10 = v_r1 - v_rc11;
            let v_rc01 = v_r0 * fc;
            let v_rc00 = v_r0 - v_rc01;
            let idx = ((r0i as usize) * (D + 2) + c0i as usize) * (N + 2) + o0i as usize;
            let mut add = |off: usize, v: f32| {
                hist[idx + off] += v * (1.0 - fo);
                hist[idx + off + 1] += v * fo;
            };
            add(0, v_rc00);
            add(N + 2, v_rc01);
            add((D + 2) * (N + 2), v_rc10);
            add((D + 3) * (N + 2), v_rc11);
        }
    }
    // Collapse the circular orientation bins and drop the border cells.
    let mut raw = [0.0f32; DESC_LEN];
    for i in 0..D {
        for j in 0..D {
            let idx = ((i + 1) * (D + 2) + (j + 1)) * (N + 2);
            hist[idx] += hist[idx + N];
            hist[idx + 1] += hist[idx + N + 1];
            raw[(i * D + j) * N..(i * D + j) * N + N].copy_from_slice(&hist[idx..idx + N]);
        }
    }
    // SIFT normalisation (L2, clamp, L2), then RootSIFT (L1, square root).
    let norm = raw.iter().map(|v| v * v).sum::<f32>().sqrt().max(1e-12);
    let clamp = 0.2 * norm;
    raw.iter_mut().for_each(|v| *v = v.min(clamp));
    let l1 = raw.iter().sum::<f32>().max(1e-12);
    for (d, v) in dst.iter_mut().zip(raw.iter()) {
        *d = ((v / l1).sqrt() * 512.0).min(255.0) as u8;
    }
}

/// Detects features in an RGB8 image.
pub fn detect(rgb: &[u8], w: usize, h: usize, opts: &DetectOptions) -> FeatureSet {
    let (gray, scale) = Gray::from_rgb8(rgb, w, h, opts.max_dim);
    detect_gray(gray, scale, opts)
}

pub fn detect_gray(gray: Gray, scale: f32, opts: &DetectOptions) -> FeatureSet {
    if gray.w < 32 || gray.h < 32 {
        return FeatureSet::default();
    }
    let (work_w, work_h) = (gray.w, gray.h);
    // The camera's own blur is taken as σ = 0.5; bring the base image to σ0.
    let base = gaussian_blur(&gray, (SIGMA0 * SIGMA0 - 0.25).sqrt());
    drop(gray);
    let octaves = build_pyramid(base);
    let prelim = 0.5 * opts.contrast_threshold / S as f32;

    // Scale-space extrema.
    let mut jobs = Vec::new();
    for (o, oct) in octaves.iter().enumerate() {
        for s in 1..=S {
            for y in BORDER..oct.dog[0].h.saturating_sub(BORDER) {
                jobs.push((o, s, y));
            }
        }
    }
    let candidates: Vec<(usize, usize, usize, usize)> = jobs
        .par_iter()
        .flat_map_iter(|&(o, s, y)| {
            let oct = &octaves[o];
            let (p, c, n) = (&oct.dog[s - 1], &oct.dog[s], &oct.dog[s + 1]);
            let w = c.w;
            let mut found = Vec::new();
            for x in BORDER..w.saturating_sub(BORDER) {
                let v = c.at(x, y);
                if v.abs() <= prelim {
                    continue;
                }
                let mut is_max = true;
                let mut is_min = true;
                'scan: for img in [p, c, n] {
                    for yy in y - 1..=y + 1 {
                        let row = &img.data[yy * w + x - 1..yy * w + x + 2];
                        for &q in row {
                            if q > v {
                                is_max = false;
                            }
                            if q < v {
                                is_min = false;
                            }
                            if !is_max && !is_min {
                                break 'scan;
                            }
                        }
                    }
                }
                if (v > 0.0 && is_max) || (v < 0.0 && is_min) {
                    found.push((o, s, y, x));
                }
            }
            found
        })
        .collect();

    // Sub-pixel refinement, orientation assignment.
    let mut kps: Vec<Keypoint> = candidates
        .par_iter()
        .flat_map_iter(|&(o, s, y, x)| {
            let oct = &octaves[o];
            let mut out = Vec::new();
            if let Some((layer, fx, fy, xi, response)) = refine(oct, s, y, x, opts) {
                let scl = SIGMA0 * 2f32.powf((layer as f32 + xi) / S as f32);
                let mut angles = Vec::new();
                orientations(&oct.gauss[layer], fx, fy, scl, &mut angles);
                for angle in angles {
                    out.push(Keypoint { octave: o, layer, x: fx, y: fy, scl, response, angle });
                }
            }
            out
        })
        .collect();

    // Keep the strongest features, balanced across the frame so control points cover every overlap.
    kps.sort_by(|a, b| b.response.total_cmp(&a.response));
    if kps.len() > opts.max_features {
        const GRID: usize = 10;
        let cap = (opts.max_features as f32 * 2.0 / (GRID * GRID) as f32).ceil() as usize;
        let mut counts = [0usize; GRID * GRID];
        let mut keep = Vec::with_capacity(opts.max_features);
        let mut rest = Vec::new();
        for kp in kps {
            let f = (1usize << kp.octave) as f32;
            let gx = ((kp.x * f / work_w as f32 * GRID as f32) as usize).min(GRID - 1);
            let gy = ((kp.y * f / work_h as f32 * GRID as f32) as usize).min(GRID - 1);
            if counts[gy * GRID + gx] < cap && keep.len() < opts.max_features {
                counts[gy * GRID + gx] += 1;
                keep.push(kp);
            } else {
                rest.push(kp);
            }
        }
        let missing = opts.max_features.saturating_sub(keep.len());
        keep.extend(rest.into_iter().take(missing));
        keep.sort_by(|a, b| b.response.total_cmp(&a.response));
        kps = keep;
    }

    let mut desc = vec![0u8; kps.len() * DESC_LEN];
    desc.par_chunks_mut(DESC_LEN).zip(kps.par_iter()).for_each(|(d, kp)| {
        descriptor(&octaves[kp.octave].gauss[kp.layer], kp.x, kp.y, kp.scl, kp.angle, d);
    });
    let feats = kps
        .iter()
        .map(|kp| {
            let f = (1usize << kp.octave) as f32 * scale;
            Feature { x: (kp.x + 0.5) * f, y: (kp.y + 0.5) * f, scale: kp.scl * f, angle: kp.angle, response: kp.response }
        })
        .collect();
    FeatureSet { feats, desc }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blur_preserves_mean_and_smooths() {
        let mut g = Gray::new(64, 48);
        for (i, v) in g.data.iter_mut().enumerate() {
            *v = ((i * 7919) % 97) as f32 / 97.0;
        }
        let b = gaussian_blur(&g, 2.0);
        let mean = |x: &Gray| x.data.iter().sum::<f32>() / x.data.len() as f32;
        assert!((mean(&g) - mean(&b)).abs() < 0.01);
        let var = |x: &Gray| {
            let m = mean(x);
            x.data.iter().map(|v| (v - m) * (v - m)).sum::<f32>()
        };
        assert!(var(&b) < var(&g) * 0.2);
    }
}
