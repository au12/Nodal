//! Output geometry and remapping of source photos onto the panorama canvas.
//!
//! Each photo is rendered into a *layer*: a rectangle of the canvas (which may wrap around the
//! 360° seam) holding colour, coverage and a seam-placement weight. The pixel mapping is evaluated
//! exactly on an 8-pixel grid and interpolated in between (with an error check), which makes
//! remapping several times faster than evaluating trigonometry per pixel without visible loss.

use crate::lens::{LensModel, V3};
use crate::project::{OutputProjection, Project, WarpField};
use image::RgbImage;
use nalgebra::Rotation3;
use rayon::prelude::*;
use std::f64::consts::{PI, TAU};

/// The panorama canvas: projection and size.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Canvas {
    pub projection: OutputProjection,
    pub w: usize,
    pub h: usize,
    /// Horizontal field of view of the canvas in radians (2π for a full equirectangular image).
    pub hfov: f64,
}

impl Canvas {
    pub fn equirect(w: usize) -> Canvas {
        Canvas { projection: OutputProjection::Equirectangular, w, h: w / 2, hfov: TAU }
    }

    /// Same projection at a different width.
    pub fn scaled(&self, w: usize) -> Canvas {
        let h = ((self.h as f64 * w as f64 / self.w as f64).round() as usize).max(1);
        Canvas { w, h, ..*self }
    }

    /// Whether the canvas is horizontally periodic.
    pub fn wraps(&self) -> bool {
        self.projection == OutputProjection::Equirectangular && self.hfov >= TAU - 1e-9
    }

    /// World direction at continuous canvas coordinates (pixel centres at half-integers).
    #[inline]
    pub fn dir(&self, x: f64, y: f64) -> V3 {
        match self.projection {
            OutputProjection::Equirectangular | OutputProjection::Cylindrical | OutputProjection::Rectilinear => {
                let yaw = (x / self.w as f64 - 0.5) * TAU;
                let pitch = (0.5 - y / self.h as f64) * PI;
                let (sy, cy) = yaw.sin_cos();
                let (sp, cp) = pitch.sin_cos();
                V3::new(sy * cp, sp, cy * cp)
            }
        }
    }

    /// Canvas coordinates of a world direction.
    #[inline]
    pub fn pixel(&self, d: V3) -> (f64, f64) {
        let yaw = d.x.atan2(d.z);
        let pitch = (d.y / d.norm()).clamp(-1.0, 1.0).asin();
        ((yaw / TAU + 0.5) * self.w as f64, (0.5 - pitch / PI) * self.h as f64)
    }

    /// Angular size of a canvas pixel at the equator, in radians.
    pub fn pixel_angle(&self) -> f64 {
        TAU / self.w as f64
    }
}

/// A rectangle in *unwrapped* canvas coordinates. On a 360° canvas columns are periodic, so `x0`
/// may be negative or the rectangle may run past the canvas width; both simply continue around.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Roi {
    pub x0: i64,
    pub y0: i64,
    pub w: usize,
    pub h: usize,
}

impl Roi {
    pub fn full(canvas: &Canvas) -> Roi {
        Roi { x0: 0, y0: 0, w: canvas.w, h: canvas.h }
    }

    /// Layer-local x of a canvas column on a periodic canvas of width `canvas_w`.
    #[inline]
    pub fn local_x(&self, x: usize, canvas_w: usize) -> Option<usize> {
        let lx = (x as i64 - self.x0).rem_euclid(canvas_w as i64) as usize;
        (lx < self.w).then_some(lx)
    }

    #[inline]
    pub fn local_y(&self, y: usize) -> Option<usize> {
        let ly = y as i64 - self.y0;
        (ly >= 0 && (ly as usize) < self.h).then_some(ly as usize)
    }

    /// Intersection with `[x_min, x_max) × [y_min, y_max)`, then grown so edges fall on multiples of
    /// `align` (relative to `x_min`/`y_min`). Returns `None` when the intersection is empty.
    pub fn clipped_aligned(&self, x_min: i64, x_max: i64, y_min: i64, y_max: i64, align: usize, pad: usize) -> Option<Roi> {
        let a = align as i64;
        let p = pad as i64;
        let x0 = (self.x0 - p).max(x_min);
        let x1 = (self.x0 + self.w as i64 + p).min(x_max);
        let y0 = (self.y0 - p).max(y_min);
        let y1 = (self.y0 + self.h as i64 + p).min(y_max);
        if x1 <= x0 || y1 <= y0 {
            return None;
        }
        let ax0 = x_min + (x0 - x_min).div_euclid(a) * a;
        let ay0 = y_min + (y0 - y_min).div_euclid(a) * a;
        let ax1 = x_min + (x1 - x_min + a - 1).div_euclid(a) * a;
        let ay1 = y_min + (y1 - y_min + a - 1).div_euclid(a) * a;
        Some(Roi { x0: ax0, y0: ay0, w: (ax1 - ax0) as usize, h: (ay1 - ay0) as usize })
    }
}

/// Canvas rectangle that can contain an image: the bounding box of its spherical cap.
pub fn image_roi(canvas: &Canvas, model: &LensModel, rot: &Rotation3<f64>, margin_px: f64) -> Roi {
    // Angular radius of the image around its optical axis, from points along the border.
    let mut rho = 0.0f64;
    let n = 24;
    for i in 0..=n {
        let t = i as f64 / n as f64;
        for (x, y) in [(t * model.w, 0.0), (t * model.w, model.h), (0.0, t * model.h), (model.w, t * model.h)] {
            let ray = model.pixel_to_ray(x, y);
            rho = rho.max(ray.x.hypot(ray.y).atan2(ray.z));
        }
    }
    rho = (rho + margin_px * model.pixel_angle() + 2.0 * canvas.pixel_angle()).min(PI);
    let axis = rot * V3::z();
    let pitch = axis.y.clamp(-1.0, 1.0).asin();
    let yaw = axis.x.atan2(axis.z);
    let top = (pitch + rho).min(PI / 2.0);
    let bottom = (pitch - rho).max(-PI / 2.0);
    let y0 = (((0.5 - top / PI) * canvas.h as f64).floor().max(0.0)) as usize;
    let y1 = (((0.5 - bottom / PI) * canvas.h as f64).ceil() as usize).min(canvas.h);
    let covers_pole = pitch + rho >= PI / 2.0 || pitch - rho <= -PI / 2.0;
    let sin_ratio = rho.sin() / pitch.cos().max(1e-9);
    if covers_pole || rho >= PI / 2.0 || sin_ratio >= 1.0 {
        return Roi { x0: 0, y0: y0 as i64, w: canvas.w, h: y1.saturating_sub(y0).max(1) };
    }
    let dl = sin_ratio.asin();
    let xc = (yaw / TAU + 0.5) * canvas.w as f64;
    let half = dl / TAU * canvas.w as f64 + 1.0;
    let w = ((2.0 * half).ceil() as usize + 1).min(canvas.w);
    if w >= canvas.w {
        return Roi { x0: 0, y0: y0 as i64, w: canvas.w, h: y1.saturating_sub(y0).max(1) };
    }
    let x0 = ((xc - half).floor() as i64).rem_euclid(canvas.w as i64);
    Roi { x0, y0: y0 as i64, w, h: y1.saturating_sub(y0).max(1) }
}

/// A warped photo on the canvas.
pub struct Layer {
    pub roi: Roi,
    /// RGB in 8-bit units (0–255, may exceed after exposure gain), 3 floats per pixel.
    pub rgb: Vec<f32>,
    /// 255 where the photo covers the pixel.
    pub alpha: Vec<u8>,
    /// Seam-placement weight: 1 at the photo's centre falling to 0 at its edge. Empty if not requested.
    pub weight: Vec<f32>,
}

impl Layer {
    #[inline]
    pub fn index(&self, x: usize, y: usize, canvas_w: usize) -> Option<usize> {
        let ly = self.roi.local_y(y)?;
        let lx = self.roi.local_x(x, canvas_w)?;
        Some(ly * self.roi.w + lx)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Filter {
    Bilinear,
    Bicubic,
}

struct Mip {
    w: usize,
    h: usize,
    data: Vec<u8>,
}

fn halve(src: &[u8], w: usize, h: usize) -> Mip {
    let (nw, nh) = ((w / 2).max(1), (h / 2).max(1));
    let mut data = vec![0u8; nw * nh * 3];
    data.par_chunks_mut(nw * 3).enumerate().for_each(|(y, row)| {
        let y0 = (y * 2).min(h - 1);
        let y1 = (y * 2 + 1).min(h - 1);
        for x in 0..nw {
            let x0 = (x * 2).min(w - 1);
            let x1 = (x * 2 + 1).min(w - 1);
            for c in 0..3 {
                let s = src[(y0 * w + x0) * 3 + c] as u32
                    + src[(y0 * w + x1) * 3 + c] as u32
                    + src[(y1 * w + x0) * 3 + c] as u32
                    + src[(y1 * w + x1) * 3 + c] as u32;
                row[x * 3 + c] = ((s + 2) / 4) as u8;
            }
        }
    });
    Mip { w: nw, h: nh, data }
}

/// Everything needed to remap one photo.
pub struct ImageWarper<'a> {
    src: &'a RgbImage,
    mips: Vec<Mip>,
    pub model: LensModel,
    world_to_cam: Rotation3<f64>,
    cam_to_world: Rotation3<f64>,
    field: Option<&'a WarpField>,
    /// Gamma-space multiplier per channel (linear gain ^ 1/2.2).
    pub gain: [f32; 3],
    /// Pixels trimmed from the photo's border (hides lens-edge artefacts and interpolation fringes).
    pub inset: f32,
}

impl<'a> ImageWarper<'a> {
    pub fn new(project: &'a Project, index: usize, src: &'a RgbImage) -> Self {
        let img = &project.images[index];
        let rot = img.rot();
        let g = img.gain;
        ImageWarper {
            src,
            mips: Vec::new(),
            model: project.lens_of(index).model(),
            world_to_cam: rot.inverse(),
            cam_to_world: rot,
            field: img.warp.as_ref(),
            gain: [g[0].powf(1.0 / 2.2), g[1].powf(1.0 / 2.2), g[2].powf(1.0 / 2.2)],
            inset: 1.0,
        }
    }

    pub fn rotation(&self) -> &Rotation3<f64> {
        &self.cam_to_world
    }

    /// Source pixel for a world direction (before the local warp), or `None` outside the lens field.
    #[inline]
    pub fn map(&self, dir: V3) -> Option<(f64, f64)> {
        self.model.ray_to_pixel(self.world_to_cam * dir)
    }

    fn level_for(&self, canvas: &Canvas) -> usize {
        let scale = canvas.pixel_angle() / self.model.pixel_angle();
        if scale <= 1.5 {
            0
        } else {
            (scale / 1.5).log2().ceil() as usize
        }
    }

    fn ensure_mips(&mut self, level: usize) {
        while self.mips.len() < level {
            let next = match self.mips.last() {
                None => halve(self.src.as_raw(), self.src.width() as usize, self.src.height() as usize),
                Some(m) => halve(&m.data, m.w, m.h),
            };
            self.mips.push(next);
        }
    }

    /// Renders the photo into `roi` of `canvas`.
    pub fn render(&mut self, canvas: &Canvas, roi: Roi, filter: Filter, want_weight: bool) -> Layer {
        let level = self.level_for(canvas);
        self.ensure_mips(level);
        let (pix, pw, ph): (&[u8], usize, usize) = if level == 0 {
            (self.src.as_raw(), self.src.width() as usize, self.src.height() as usize)
        } else {
            let m = &self.mips[level - 1];
            (&m.data, m.w, m.h)
        };
        let lscale = 1.0 / (1u32 << level) as f32;
        let (sw, sh) = (self.model.w as f32, self.model.h as f32);
        let inset = self.inset.max(0.5 * (1u32 << level) as f32);
        let half_min = sw.min(sh) * 0.5;
        let half_diag2 = (sw * sw + sh * sh) * 0.25;
        let fisheye = self.model.kind.is_fisheye();

        const B: usize = 8;
        let gw = roi.w.div_ceil(B) + 1;
        let gh = roi.h.div_ceil(B) + 1;
        // Exact mapping at grid nodes (canvas pixel corners).
        let mut nodes = vec![(f32::NAN, f32::NAN); gw * gh];
        nodes.par_chunks_mut(gw).enumerate().for_each(|(gy, row)| {
            let y = (roi.y0 + (gy * B) as i64) as f64;
            for (gx, n) in row.iter_mut().enumerate() {
                let x = (roi.x0 + (gx * B) as i64) as f64;
                if let Some((sx, sy)) = self.map(canvas.dir(x, y)) {
                    *n = (sx as f32, sy as f32);
                }
            }
        });

        let mut rgb = vec![0.0f32; roi.w * roi.h * 3];
        let mut alpha = vec![0u8; roi.w * roi.h];
        let mut weight = if want_weight { vec![0.0f32; roi.w * roi.h] } else { Vec::new() };
        let rows_per_band = B;
        let band_iter = rgb.par_chunks_mut(roi.w * 3 * rows_per_band).zip(alpha.par_chunks_mut(roi.w * rows_per_band)).enumerate();
        let weight_bands: Vec<&mut [f32]> = if want_weight { weight.chunks_mut(roi.w * rows_per_band).collect() } else { Vec::new() };
        let weight_bands = std::sync::Mutex::new(weight_bands.into_iter().map(Some).collect::<Vec<_>>());

        band_iter.for_each(|(by, (rgb_band, alpha_band))| {
            let mut wband: Option<&mut [f32]> = if want_weight { weight_bands.lock().unwrap()[by].take() } else { None };
            let rows = alpha_band.len() / roi.w;
            for bx in 0..roi.w.div_ceil(B) {
                let n00 = nodes[by * gw + bx];
                let n10 = nodes[by * gw + bx + 1];
                let n01 = nodes[(by + 1) * gw + bx];
                let n11 = nodes[(by + 1) * gw + bx + 1];
                let finite = n00.0.is_finite() && n10.0.is_finite() && n01.0.is_finite() && n11.0.is_finite();
                // Skip blocks that are clearly outside the photo.
                if finite {
                    let minx = n00.0.min(n10.0).min(n01.0).min(n11.0);
                    let maxx = n00.0.max(n10.0).max(n01.0).max(n11.0);
                    let miny = n00.1.min(n10.1).min(n01.1).min(n11.1);
                    let maxy = n00.1.max(n10.1).max(n01.1).max(n11.1);
                    let span = (maxx - minx).max(maxy - miny);
                    if span < sw.max(sh) * 0.25
                        && (maxx < -span - 8.0 || maxy < -span - 8.0 || minx > sw + span + 8.0 || miny > sh + span + 8.0)
                    {
                        continue;
                    }
                }
                // Interpolation is trusted when it reproduces the exact mapping at the block centre.
                let mut interp = finite;
                if interp {
                    let cx = (roi.x0 + (bx * B) as i64) as f64 + B as f64 * 0.5;
                    let cy = (roi.y0 + (by * B) as i64) as f64 + B as f64 * 0.5;
                    match self.map(canvas.dir(cx, cy)) {
                        Some((ex, ey)) => {
                            let px = 0.25 * (n00.0 + n10.0 + n01.0 + n11.0);
                            let py = 0.25 * (n00.1 + n10.1 + n01.1 + n11.1);
                            if (px - ex as f32).abs() > 0.04 || (py - ey as f32).abs() > 0.04 {
                                interp = false;
                            }
                        }
                        None => interp = false,
                    }
                }
                let x_end = ((bx + 1) * B).min(roi.w);
                for ry in 0..rows {
                    let cy = roi.y0 + (by * B + ry) as i64;
                    if cy < 0 || cy >= canvas.h as i64 {
                        continue;
                    }
                    let fy = (ry as f32 + 0.5) / B as f32;
                    for lx in bx * B..x_end {
                        let (mut sx, mut sy);
                        if interp {
                            let fx = ((lx - bx * B) as f32 + 0.5) / B as f32;
                            let top = (n00.0 + (n10.0 - n00.0) * fx, n00.1 + (n10.1 - n00.1) * fx);
                            let bot = (n01.0 + (n11.0 - n01.0) * fx, n01.1 + (n11.1 - n01.1) * fx);
                            sx = top.0 + (bot.0 - top.0) * fy;
                            sy = top.1 + (bot.1 - top.1) * fy;
                        } else {
                            let x = (roi.x0 + lx as i64) as f64 + 0.5;
                            let y = cy as f64 + 0.5;
                            match self.map(canvas.dir(x, y)) {
                                Some((ex, ey)) => {
                                    sx = ex as f32;
                                    sy = ey as f32;
                                }
                                None => continue,
                            }
                        }
                        if let Some(f) = self.field {
                            let (dx, dy) = f.sample(sx, sy, sw, sh);
                            sx += dx;
                            sy += dy;
                        }
                        if sx < inset || sy < inset || sx > sw - inset || sy > sh - inset {
                            continue;
                        }
                        let o = ry * roi.w + lx;
                        let c = match filter {
                            Filter::Bicubic => crate::synth::sample_bicubic(pix, pw, ph, sx * lscale, sy * lscale, false),
                            Filter::Bilinear => sample_bilinear(pix, pw, ph, sx * lscale, sy * lscale),
                        };
                        rgb_band[o * 3] = c[0] * self.gain[0];
                        rgb_band[o * 3 + 1] = c[1] * self.gain[1];
                        rgb_band[o * 3 + 2] = c[2] * self.gain[2];
                        alpha_band[o] = 255;
                        if let Some(wb) = wband.as_deref_mut() {
                            let edge = sx.min(sw - sx).min(sy).min(sh - sy) / half_min;
                            let wv = if fisheye {
                                let r2 = ((sx - sw * 0.5).powi(2) + (sy - sh * 0.5).powi(2)) / half_diag2;
                                edge.min(1.0 - r2)
                            } else {
                                edge
                            };
                            wb[o] = wv.clamp(1e-4, 1.0);
                        }
                    }
                }
            }
        });
        drop(weight_bands);
        Layer { roi, rgb, alpha, weight }
    }
}

#[inline]
pub fn sample_bilinear(data: &[u8], w: usize, h: usize, x: f32, y: f32) -> [f32; 3] {
    let fx = (x - 0.5).clamp(0.0, (w - 1) as f32);
    let fy = (y - 0.5).clamp(0.0, (h - 1) as f32);
    let (x0, y0) = (fx as usize, fy as usize);
    let (x1, y1) = ((x0 + 1).min(w - 1), (y0 + 1).min(h - 1));
    let (tx, ty) = (fx - x0 as f32, fy - y0 as f32);
    let p = |xx: usize, yy: usize, c: usize| data[(yy * w + xx) * 3 + c] as f32;
    let mut out = [0.0; 3];
    for (c, o) in out.iter_mut().enumerate() {
        let top = p(x0, y0, c) + (p(x1, y0, c) - p(x0, y0, c)) * tx;
        let bot = p(x0, y1, c) + (p(x1, y1, c) - p(x0, y1, c)) * tx;
        *o = top + (bot - top) * ty;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lens::{Lens, LensKind};
    use crate::project::rotation_from_ypr;

    #[test]
    fn canvas_round_trip() {
        let c = Canvas::equirect(4096);
        for &(x, y) in &[(10.5, 20.5), (2048.0, 1024.0), (4000.2, 2000.7), (100.0, 1900.0)] {
            let (px, py) = c.pixel(c.dir(x, y));
            assert!((px - x).abs() < 1e-6 && (py - y).abs() < 1e-6);
        }
        // Centre of the canvas looks forward, left edge looks backwards.
        assert!((c.dir(2048.0, 1024.0) - V3::z()).norm() < 1e-9);
    }

    #[test]
    fn roi_contains_every_projected_pixel() {
        let canvas = Canvas::equirect(2048);
        for (kind, fov, yaw, pitch) in [
            (LensKind::Rectilinear, 70.0, 0.0, 0.0),
            (LensKind::Rectilinear, 70.0, 175.0, 20.0),
            (LensKind::Rectilinear, 90.0, -100.0, 80.0),
            (LensKind::Fisheye, 180.0, 30.0, -10.0),
            (LensKind::Rectilinear, 40.0, -179.0, -60.0),
        ] {
            let lens = Lens::new(kind, fov, 600, 400);
            let m = lens.model();
            let rot = rotation_from_ypr(yaw, pitch, 7.0);
            let roi = image_roi(&canvas, &m, &rot, 2.0);
            for sy in (0..=400).step_by(20) {
                for sx in (0..=600).step_by(20) {
                    let d = rot * m.pixel_to_ray(sx as f64, sy as f64);
                    let (cx, cy) = canvas.pixel(d);
                    let (cx, cy) = ((cx as usize).min(canvas.w - 1), (cy as usize).min(canvas.h - 1));
                    assert!(
                        roi.local_x(cx, canvas.w).is_some() && roi.local_y(cy).is_some(),
                        "{kind:?} yaw {yaw} pitch {pitch}: ({cx},{cy}) outside {roi:?}"
                    );
                }
            }
        }
    }
}
