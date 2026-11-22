//! Renders an aligned project into a panorama: remap, balance exposure, place seams, blend.

use crate::blend::Blender;
use crate::pixels::{self, LinearImage};
use crate::project::Project;
use crate::seam;
use crate::warp::{image_roi, Canvas, Filter, ImageWarper, Layer, Roi, SourcePixels};
use crate::{PixelProvider, Progress};
use anyhow::{ensure, Result};
use image::RgbImage;
use rayon::prelude::*;
use std::sync::Arc;
use std::time::Instant;

#[derive(Clone, Debug)]
pub struct RenderOptions {
    /// Output width in pixels for the full 360°; 0 uses the project's setting or the native size.
    pub width: u32,
    /// Largest automatic width (explicit widths are not capped).
    pub max_auto_width: u32,
    /// Width of the reduced canvas used for exposure analysis and seam placement.
    pub seam_width: usize,
    pub filter: Filter,
    /// Number of blend bands; `None` picks from the output size.
    pub bands: Option<usize>,
}

impl Default for RenderOptions {
    fn default() -> Self {
        RenderOptions { width: 0, max_auto_width: 16384, seam_width: 1536, filter: Filter::Bicubic, bands: None }
    }
}

#[derive(Clone, Debug, Default)]
pub struct RenderStats {
    /// Linear RGB gain applied to each project image (1.0 for unused images).
    pub gains: Vec<[f32; 3]>,
    /// Images that ended up contributing pixels (others were fully covered by better ones).
    pub used_images: Vec<usize>,
    /// Images decoded at full resolution (the rest used working copies).
    pub full_resolution_images: usize,
    /// Interior holes and pole gaps filled, in percent of the image.
    pub filled_percent: f32,
    /// Reduced-canvas pixels dropped as ghosts (moving people, shadows).
    pub ghost_pixels: usize,
    pub bands: usize,
    pub seam_pairs: usize,
    /// (stage, seconds)
    pub timings: Vec<(String, f32)>,
    pub filled_zenith: bool,
    pub filled_nadir: bool,
}

/// A rendered panorama, possibly cropped out of the full 360° × 180° canvas.
pub struct Panorama {
    pub width: usize,
    pub height: usize,
    /// 16-bit sRGB-encoded RGB, `width × height × 3`.
    pub rgb: Vec<u16>,
    /// 255 where the panorama has image data, 0 in gaps.
    pub alpha: Vec<u8>,
    /// Size of the full equirectangular canvas this image was cut from.
    pub full_width: usize,
    pub full_height: usize,
    /// Position of this image inside the full canvas.
    pub crop_x: usize,
    pub crop_y: usize,
    pub stats: RenderStats,
}

impl Panorama {
    /// True when the image spans the whole sphere (no crop).
    pub fn is_full_sphere(&self) -> bool {
        self.width == self.full_width && self.height == self.full_height
    }

    /// Horizontal and vertical field of view in degrees, and the vertical centre offset.
    pub fn coverage_degrees(&self) -> (f32, f32, f32) {
        let hfov = self.width as f32 / self.full_width as f32 * 360.0;
        let vfov = self.height as f32 / self.full_height as f32 * 180.0;
        let centre = (self.crop_y as f32 + self.height as f32 * 0.5) / self.full_height as f32;
        (hfov, vfov, (0.5 - centre) * 180.0)
    }

    pub fn has_gaps(&self) -> bool {
        self.alpha.contains(&0)
    }

    /// 8-bit sRGB copy of the pixels.
    pub fn rgb8(&self) -> Vec<u8> {
        self.rgb.par_iter().map(|&v| ((v as u32 * 255 + 32767) / 65535) as u8).collect()
    }

    pub fn to_image(&self) -> RgbImage {
        RgbImage::from_raw(self.width as u32, self.height as u32, self.rgb8()).expect("buffer size")
    }

    /// Builds a panorama from 8-bit pixels (tests and tools).
    pub fn from_rgb8(width: usize, height: usize, rgb: &[u8], alpha: Vec<u8>) -> Panorama {
        Panorama {
            width,
            height,
            rgb: rgb.iter().map(|&v| v as u16 * 257).collect(),
            alpha,
            full_width: width,
            full_height: height,
            crop_x: 0,
            crop_y: 0,
            stats: RenderStats::default(),
        }
    }
}

/// Picks the width of the full 360° canvas. `covered` is the fraction of the canvas width the
/// panorama actually spans (1.0 for a full sphere): requested widths refer to the output image.
pub fn canvas_width(project: &Project, opts: &RenderOptions, covered: f64) -> usize {
    let covered = covered.clamp(0.01, 1.0);
    let requested = if opts.width > 0 { opts.width } else { project.output.width };
    let w = if requested > 0 {
        requested as f64 / covered
    } else {
        // Native resolution of the sharpest photo, limited so the output stays a sensible size.
        (project.native_width() as f64).min(opts.max_auto_width as f64 / covered)
    };
    (w.clamp(256.0, 131072.0) as usize).div_ceil(2) * 2
}

fn default_bands(width: usize) -> usize {
    ((width as f32).log2().round() as isize - 6).clamp(3, 8) as usize
}

/// Seam mask for one layer: bilinear upsampling of "label == index" from the reduced canvas,
/// sharpened to a few pixels, limited to the layer's own coverage.
fn layer_mask(layer: &Layer, canvas: &Canvas, labels: &[u16], lw: usize, lh: usize, index: u16) -> Vec<f32> {
    let roi = layer.roi;
    let sx = lw as f32 / canvas.w as f32;
    let sy = lh as f32 / canvas.h as f32;
    let mut mask = vec![0.0f32; roi.w * roi.h];
    mask.par_chunks_mut(roi.w).enumerate().for_each(|(ly, row)| {
        let cy = roi.y0 + ly as i64;
        if cy < 0 || cy >= canvas.h as i64 {
            return;
        }
        let v = ((cy as f32 + 0.5) * sy - 0.5).clamp(0.0, (lh - 1) as f32);
        let v0 = (v as usize).min(lh - 1);
        let v1 = (v0 + 1).min(lh - 1);
        let fv = v - v0 as f32;
        let (r0, r1) = (&labels[v0 * lw..(v0 + 1) * lw], &labels[v1 * lw..(v1 + 1) * lw]);
        for (lx, m) in row.iter_mut().enumerate() {
            if layer.alpha[ly * roi.w + lx] == 0 {
                continue;
            }
            let cx = (roi.x0 + lx as i64).rem_euclid(canvas.w as i64);
            let u = (cx as f32 + 0.5) * sx - 0.5;
            let u0f = u.floor();
            let fu = u - u0f;
            let u0 = (u0f as i64).rem_euclid(lw as i64) as usize;
            let u1 = (u0 + 1) % lw;
            let ind = |l: u16| (l == index) as u8 as f32;
            let top = ind(r0[u0]) + (ind(r0[u1]) - ind(r0[u0])) * fu;
            let bot = ind(r1[u0]) + (ind(r1[u1]) - ind(r1[u0])) * fu;
            let val = top + (bot - top) * fv;
            *m = ((val - 0.5) * 2.5 + 0.5).clamp(0.0, 1.0);
        }
    });
    mask
}

/// Disjoint horizontal intervals `[a, b)` of the padded canvas that an ROI (periodic with the
/// canvas width) touches.
fn padded_intervals(roi: &Roi, canvas_w: usize, pad: usize, align: usize) -> Vec<(i64, i64)> {
    let wp = (canvas_w + 2 * pad) as i64;
    let mut out: Vec<(i64, i64)> = Vec::new();
    for k in -1i64..=1 {
        let a = (roi.x0 + k * canvas_w as i64 + pad as i64).max(0);
        let b = (roi.x0 + roi.w as i64 + k * canvas_w as i64 + pad as i64).min(wp);
        if b <= a {
            continue;
        }
        let a = a / align as i64 * align as i64;
        let b = (b + align as i64 - 1) / align as i64 * align as i64;
        out.push((a, b));
    }
    out.sort();
    let mut merged: Vec<(i64, i64)> = Vec::new();
    for (a, b) in out {
        match merged.last_mut() {
            Some(last) if a <= last.1 => last.1 = last.1.max(b),
            _ => merged.push((a, b)),
        }
    }
    merged
}

/// Ghost masks: where at least three photos overlap and one of them disagrees clearly with the
/// others (a person who moved, the photographer's shadow, a passing car), that photo's pixels are
/// marked so seams avoid them and blending ignores them. Works on reduced-size layers in
/// perceptual units (0–255), on blurred luminance so texture misalignment does not count.
/// Returns one ROI-local mask per layer (true = ghost).
pub fn ghost_masks(layers: &[Layer], cw: usize, ch: usize) -> Vec<Vec<bool>> {
    const K: usize = 12;
    const THRESHOLD: f32 = 20.0;
    // Blurred luminance per layer (5×5 box over covered pixels).
    let lumas: Vec<Vec<f32>> = layers
        .par_iter()
        .map(|l| {
            let (w, h) = (l.roi.w, l.roi.h);
            let y: Vec<f32> = (0..w * h).map(|i| 0.3 * l.rgb[i * 3] + 0.59 * l.rgb[i * 3 + 1] + 0.11 * l.rgb[i * 3 + 2]).collect();
            let mut out = vec![f32::NAN; w * h];
            for py in 0..h {
                for px in 0..w {
                    if l.alpha[py * w + px] == 0 {
                        continue;
                    }
                    let (mut s, mut n) = (0.0f32, 0.0f32);
                    for dy in -2i32..=2 {
                        let yy = py as i32 + dy;
                        if yy < 0 || yy >= h as i32 {
                            continue;
                        }
                        for dx in -2i32..=2 {
                            let xx = px as i32 + dx;
                            if xx < 0 || xx >= w as i32 {
                                continue;
                            }
                            let j = yy as usize * w + xx as usize;
                            if l.alpha[j] != 0 {
                                s += y[j];
                                n += 1.0;
                            }
                        }
                    }
                    out[py * w + px] = s / n;
                }
            }
            out
        })
        .collect();
    // Gather the values every layer has at each canvas pixel.
    let mut count = vec![0u8; cw * ch];
    let mut vals = vec![0.0f32; cw * ch * K];
    let mut owners = vec![0u16; cw * ch * K];
    for (k, l) in layers.iter().enumerate() {
        let roi = l.roi;
        for ly in 0..roi.h {
            let cy = roi.y0 + ly as i64;
            if cy < 0 || cy >= ch as i64 {
                continue;
            }
            for lx in 0..roi.w {
                let v = lumas[k][ly * roi.w + lx];
                if v.is_nan() {
                    continue;
                }
                let cx = (roi.x0 + lx as i64).rem_euclid(cw as i64) as usize;
                let c = cy as usize * cw + cx;
                let n = count[c] as usize;
                if n < K {
                    vals[c * K + n] = v;
                    owners[c * K + n] = k as u16;
                    count[c] += 1;
                }
            }
        }
    }
    // Flag outliers against the median.
    let mut flags: Vec<Vec<bool>> = layers.iter().map(|l| vec![false; l.roi.w * l.roi.h]).collect();
    let mut flagged_any = false;
    for c in 0..cw * ch {
        let n = count[c] as usize;
        if n < 3 {
            continue;
        }
        let mut v: Vec<f32> = vals[c * K..c * K + n].to_vec();
        v.sort_by(|a, b| a.total_cmp(b));
        let med = v[n / 2];
        let mut out = Vec::new();
        let mut best = (f32::MAX, 0usize);
        for t in 0..n {
            let d = (vals[c * K + t] - med).abs();
            if d < best.0 {
                best = (d, t);
            }
            if d > THRESHOLD {
                out.push(t);
            }
        }
        if out.len() == n {
            out.retain(|&t| t != best.1);
        }
        let (cx, cy) = (c % cw, c / cw);
        for t in out {
            let k = owners[c * K + t] as usize;
            if let Some(i) = layers[k].index(cx, cy, cw) {
                flags[k][i] = true;
                flagged_any = true;
            }
        }
    }
    if !flagged_any {
        return flags;
    }
    // Clean up: drop isolated flags, then grow a little to cover soft edges.
    flags
        .par_iter()
        .zip(layers.par_iter())
        .map(|(f, l)| {
            let (w, h) = (l.roi.w, l.roi.h);
            let at = |m: &[bool], x: i32, y: i32| x >= 0 && y >= 0 && x < w as i32 && y < h as i32 && m[y as usize * w + x as usize];
            let mut opened = vec![false; w * h];
            for y in 0..h as i32 {
                for x in 0..w as i32 {
                    if !at(f, x, y) {
                        continue;
                    }
                    let mut n = 0;
                    for dy in -1..=1 {
                        for dx in -1..=1 {
                            n += at(f, x + dx, y + dy) as i32;
                        }
                    }
                    opened[y as usize * w + x as usize] = n >= 5;
                }
            }
            let mut grown = vec![false; w * h];
            for y in 0..h as i32 {
                for x in 0..w as i32 {
                    let mut any = false;
                    'n: for dy in -2..=2 {
                        for dx in -2..=2 {
                            if at(&opened, x + dx, y + dy) {
                                any = true;
                                break 'n;
                            }
                        }
                    }
                    grown[y as usize * w + x as usize] = any && l.alpha[y as usize * w + x as usize] != 0;
                }
            }
            grown
        })
        .collect()
}

/// Applies ghost masks to layers (the photo no longer covers those pixels).
fn mask_ghosts(layers: &mut [Layer], ghosts: &[Vec<bool>]) {
    for (l, g) in layers.iter_mut().zip(ghosts) {
        for (i, &gh) in g.iter().enumerate() {
            if gh {
                l.alpha[i] = 0;
            }
        }
    }
}

/// Render-time copy of the project with the output switches applied.
fn prepared(project: &Project) -> Project {
    let mut proj = project.clone();
    if !proj.output.exposure_compensation {
        proj.images.iter_mut().for_each(|i| i.gain = [1.0; 3]);
    }
    if !proj.output.vignetting_correction {
        proj.lenses.iter_mut().for_each(|l| l.vignette = [0.0; 3]);
    }
    if !proj.output.local_alignment {
        proj.images.iter_mut().for_each(|i| i.warp = None);
    }
    let ev = 2f32.powf(proj.develop.exposure);
    if ev != 1.0 {
        proj.images.iter_mut().for_each(|i| i.gain = [i.gain[0] * ev, i.gain[1] * ev, i.gain[2] * ev]);
    }
    proj
}

/// Converts a linear layer to perceptual values in 8-bit units, as the seam finder expects.
fn perceptual_255(layer: &mut Layer) {
    layer.rgb.par_chunks_mut(4096).for_each(|c| {
        for v in c {
            *v = pixels::to_perceptual_fast(*v) * 255.0;
        }
    });
}

/// Renders the project. Pixels come from `pixels` (working copies for reduced-size passes and
/// previews, full-resolution decodes for the final output).
pub fn render(project: &Project, pixels: &dyn PixelProvider, opts: &RenderOptions, progress: Progress) -> Result<Panorama> {
    let active: Vec<usize> = project.active().collect();
    ensure!(!active.is_empty(), "No aligned images to render. Align the images first.");
    let mut stats = RenderStats { gains: vec![[1.0; 3]; project.images.len()], ..Default::default() };
    let mut clock = Instant::now();
    let mut lap = |stats: &mut RenderStats, name: &str| {
        stats.timings.push((name.to_string(), clock.elapsed().as_secs_f32()));
        clock = Instant::now();
    };
    let proj = prepared(project);
    for &i in &active {
        stats.gains[i] = proj.images[i].gain;
    }
    let working: Vec<Option<Arc<RgbImage>>> =
        (0..proj.images.len()).map(|i| active.contains(&i).then(|| pixels.working(i))).collect();

    // ---- Reduced-size pass: seams ------------------------------------------------------------
    progress(0.0, "Analysing overlaps");
    let lw = (opts.seam_width / 2 * 2).max(64);
    let low = Canvas::equirect(lw);
    let lh = low.h;
    let low_layer = |canvas: &Canvas, i: usize| {
        let work = working[i].as_ref().expect("active image");
        let mut warper = ImageWarper::new(&proj, i, SourcePixels::Srgb8(work));
        // Stay clear of the photo's border so seams never sit on its very edge.
        warper.inset = (2.0 * canvas.pixel_angle() / warper.model.pixel_angle()).max(2.0) as f32;
        let roi = image_roi(canvas, &warper.model, warper.rotation(), 2.0);
        let mut layer = warper.render(canvas, roi, Filter::Bilinear, true);
        perceptual_255(&mut layer);
        layer
    };
    let mut low_layers: Vec<Layer> = active.par_iter().map(|&i| low_layer(&low, i)).collect();
    lap(&mut stats, "preview warp");
    // Moving people and shadows: drop the photos that disagree with the consensus.
    let ghosts: Vec<(Roi, Vec<bool>)> = if proj.output.seam_optimization && active.len() > 2 {
        let g = ghost_masks(&low_layers, lw, lh);
        mask_ghosts(&mut low_layers, &g);
        low_layers.iter().map(|l| l.roi).zip(g).collect()
    } else {
        Vec::new()
    };
    stats.ghost_pixels = ghosts.iter().map(|(_, g)| g.iter().filter(|x| **x).count()).sum();

    progress(0.12, "Placing seams");
    let mut labels = seam::initial_labels(lw, lh, &low_layers);
    if proj.output.seam_optimization && active.len() > 1 {
        // Coarse-to-fine: solve the seams freely on a quarter-size canvas, then polish them in a
        // narrow band at the working size.
        let coarse = Canvas::equirect((lw / 4 / 2 * 2).max(32));
        let mut coarse_layers: Vec<Layer> = active.par_iter().map(|&i| low_layer(&coarse, i)).collect();
        if !ghosts.is_empty() {
            // Carry the ghost masks down to the coarse canvas.
            let f = lw as f64 / coarse.w as f64;
            for (k, layer) in coarse_layers.iter_mut().enumerate() {
                let (groi, gm) = &ghosts[k];
                let roi = layer.roi;
                for ly in 0..roi.h {
                    for lx in 0..roi.w {
                        let cx = ((roi.x0 + lx as i64) as f64 + 0.5) * f;
                        let cy = ((roi.y0 + ly as i64) as f64 + 0.5) * f;
                        let gx = (cx as i64 - groi.x0).rem_euclid(lw as i64) as usize;
                        let gy = cy as i64 - groi.y0;
                        if gy >= 0 && (gy as usize) < groi.h && gx < groi.w && gm[gy as usize * groi.w + gx] {
                            layer.alpha[ly * roi.w + lx] = 0;
                        }
                    }
                }
            }
        }
        let mut coarse_labels = seam::initial_labels(coarse.w, coarse.h, &coarse_layers);
        stats.seam_pairs = seam::refine_seams(&mut coarse_labels, coarse.w, coarse.h, &coarse_layers, true, 3, coarse.w / 8);
        seam::upsample_labels(&coarse_labels, coarse.w, coarse.h, &mut labels, lw, lh, &low_layers);
        seam::refine_seams(&mut labels, lw, lh, &low_layers, true, 1, 10);
    }
    seam::dilate_labels(&mut labels, lw, lh, true, 4);
    // Where each photo actually ends up in the panorama (in its own unwrapped low-res coordinates).
    let used: Vec<Option<Roi>> = low_layers
        .iter()
        .enumerate()
        .map(|(k, layer)| {
            let roi = layer.roi;
            let (mut x0, mut x1, mut y0, mut y1) = (usize::MAX, 0usize, usize::MAX, 0usize);
            for ly in 0..roi.h {
                let cy = roi.y0 + ly as i64;
                if cy < 0 || cy >= lh as i64 {
                    continue;
                }
                let row = &labels[cy as usize * lw..(cy as usize + 1) * lw];
                for lx in 0..roi.w {
                    if row[(roi.x0 + lx as i64).rem_euclid(lw as i64) as usize] == k as u16 {
                        x0 = x0.min(lx);
                        x1 = x1.max(lx + 1);
                        y0 = y0.min(ly);
                        y1 = y1.max(ly + 1);
                    }
                }
            }
            (x1 > x0).then(|| Roi { x0: roi.x0 + x0 as i64, y0: roi.y0 + y0 as i64, w: x1 - x0, h: y1 - y0 })
        })
        .collect();
    drop(low_layers);
    stats.used_images = used.iter().enumerate().filter(|(_, u)| u.is_some()).map(|(k, _)| active[k]).collect();
    lap(&mut stats, "seams");

    // ---- Full-resolution pass --------------------------------------------------------------
    // How much of the 360° the panorama spans decides the canvas size and whether it is periodic.
    let mut col_used = vec![false; lw];
    for y in 0..lh {
        for x in 0..lw {
            if labels[y * lw + x] != seam::NONE {
                col_used[x] = true;
            }
        }
    }
    let covered = col_used.iter().filter(|c| **c).count() as f64 / lw as f64;
    let width = canvas_width(project, opts, covered);
    let canvas = Canvas::equirect(width);
    let h = canvas.h;
    let bands =
        if proj.output.multiband { opts.bands.unwrap_or_else(|| default_bands(((width as f64 * covered) as usize).max(256))) } else { 0 };
    stats.bands = bands;
    let align = 1usize << bands;
    let scale = width as f64 / lw as f64;
    // Context around each photo's share: the mask of the coarsest band spreads about two of its
    // pixels beyond the seam.
    let ctx = (2 * align + 8) as i64;
    // Full-resolution rectangle each photo is needed in (unwrapped canvas coordinates).
    let rois: Vec<Option<Roi>> = used
        .iter()
        .map(|u| {
            let u = (*u)?;
            let x0 = (u.x0 as f64 * scale).floor() as i64 - ctx;
            let w = (((u.w as f64 + 1.0) * scale).ceil() as i64 + 2 * ctx).min(width as i64) as usize;
            let y0 = ((u.y0 as f64 * scale).floor() as i64 - ctx).max(0);
            let y1 = ((((u.y0 + u.h as i64) as f64 + 1.0) * scale).ceil() as i64 + ctx).min(h as i64);
            Some(if w >= width {
                Roi { x0: 0, y0, w: width, h: (y1 - y0) as usize }
            } else {
                Roi { x0: x0.rem_euclid(width as i64), y0, w, h: (y1 - y0) as usize }
            })
        })
        .collect();
    // A panorama that crosses the ±180° seam or spans most of the circle is blended on a periodic
    // canvas (padded on both sides so every band is continuous across the seam); a partial one only
    // needs its own bounding box.
    let periodic = covered > 0.7 || rois.iter().flatten().any(|r| r.x0 + r.w as i64 > width as i64);
    let pad = if periodic && bands > 0 { 4 * align } else { 0 };
    let (origin_x, origin_y, bw, bh) = if periodic {
        (-(pad as i64), 0i64, width + 2 * pad, h)
    } else {
        let a = align as i64;
        let x0 = rois.iter().flatten().map(|r| r.x0).min().unwrap_or(0) / a * a;
        let y0 = rois.iter().flatten().map(|r| r.y0).min().unwrap_or(0) / a * a;
        let x1 = rois.iter().flatten().map(|r| r.x0 + r.w as i64).max().unwrap_or(a).min(width as i64);
        let y1 = rois.iter().flatten().map(|r| r.y0 + r.h as i64).max().unwrap_or(a).min(h as i64);
        (x0, y0, (x1 - x0) as usize, (y1 - y0) as usize)
    };
    let profile = std::env::var_os("NODAL_PROFILE").is_some();
    let mut blender = Blender::new(bw, bh, bands);
    let a = align as i64;
    // Which images need decoding, and at what resolution: only where the canvas is finer than
    // the working copy, and at half resolution when that is still at least as fine as the canvas.
    let needs_full = |i: usize| {
        let lens = proj.lens_of(i);
        let work_w = working[i].as_ref().map(|w| w.width()).unwrap_or(1) as f64;
        let work_angle = lens.model().pixel_angle() * proj.images[i].width as f64 / work_w;
        canvas.pixel_angle() < work_angle * 1.3
    };
    let half_is_enough = |i: usize| canvas.pixel_angle() >= 1.9 * proj.lens_of(i).model().pixel_angle();
    let order: Vec<(usize, usize)> = active.iter().enumerate().filter(|(k, _)| rois[*k].is_some()).map(|(k, &i)| (k, i)).collect();
    let load = |i: usize| -> Result<Option<Arc<LinearImage>>> {
        if !needs_full(i) {
            Ok(None)
        } else if half_is_enough(i) {
            pixels.half(i, &proj.develop).map(Some)
        } else {
            pixels.full(i, &proj.develop).map(Some)
        }
    };
    // Decode the next photo on another thread while the current one is warped and blended.
    let mut next = order.first().map(|&(_, i)| load(i)).transpose()?.flatten();
    for (n, &(k, i)) in order.iter().enumerate() {
        progress(0.2 + 0.65 * n as f32 / order.len() as f32, "Blending");
        let roi = rois[k].expect("filtered");
        let current = next.take();
        let following = order.get(n + 1).map(|&(_, j)| j);
        let work = working[i].clone().expect("active image");
        let (layers_done, prefetched) = std::thread::scope(|scope| -> Result<(Vec<(i64, i64, usize, usize, Layer)>, Option<Arc<LinearImage>>)> {
            let handle = following.map(|j| scope.spawn(move || load(j)));
            let src = match &current {
                Some(full) => SourcePixels::Linear(full),
                None => SourcePixels::Srgb8(&work),
            };
            let mut warper = ImageWarper::new(&proj, i, src);
            // Vertical extent in blender coordinates, aligned.
            let by0 = (roi.y0 - origin_y).max(0) / a * a;
            let by1 = (roi.y0 + roi.h as i64 - origin_y + a - 1) / a * a;
            let spans: Vec<(i64, i64)> = if periodic {
                padded_intervals(&roi, width, pad, align)
            } else {
                vec![((roi.x0 - origin_x).max(0) / a * a, (roi.x0 + roi.w as i64 - origin_x + a - 1) / a * a)]
            };
            let mut out = Vec::new();
            for (bx0, bx1) in spans {
                let (lw_px, lh_px) = ((bx1 - bx0) as usize, (by1 - by0) as usize);
                let mut layer = warper.render(&canvas, Roi { x0: bx0 + origin_x, y0: by0 + origin_y, w: lw_px, h: lh_px }, opts.filter, false);
                layer.rgb.par_chunks_mut(4096).for_each(|c| c.iter_mut().for_each(|v| *v = pixels::to_perceptual_fast(*v)));
                out.push((bx0, by0, lw_px, lh_px, layer));
            }
            let pre = match handle {
                Some(hd) => hd.join().map_err(|_| anyhow::anyhow!("decoder thread panicked"))??,
                None => None,
            };
            Ok((out, pre))
        })?;
        if current.is_some() {
            stats.full_resolution_images += 1;
        }
        drop(current);
        next = prefetched;
        for (bx0, by0, lw_px, lh_px, mut layer) in layers_done {
            let t1 = Instant::now();
            if let Some((groi, gm)) = ghosts.get(k) {
                if gm.iter().any(|g| *g) {
                    let roi = layer.roi;
                    let f = lw as f64 / canvas.w as f64;
                    layer.alpha.par_chunks_mut(roi.w).zip(layer.rgb.par_chunks_mut(roi.w * 3)).enumerate().for_each(|(ly, (arow, crow))| {
                        let cy = ((roi.y0 + ly as i64) as f64 + 0.5) * f;
                        let gy = cy as i64 - groi.y0;
                        if gy < 0 || gy as usize >= groi.h {
                            return;
                        }
                        for lx in 0..roi.w {
                            let cx = ((roi.x0 + lx as i64) as f64 + 0.5) * f;
                            let gx = (cx as i64 - groi.x0).rem_euclid(lw as i64) as usize;
                            if gx < groi.w && gm[gy as usize * groi.w + gx] {
                                arow[lx] = 0;
                                crow[lx * 3..lx * 3 + 3].fill(0.0);
                            }
                        }
                    });
                }
            }
            let mask = layer_mask(&layer, &canvas, &labels, lw, lh, k as u16);
            let alpha: Vec<f32> = layer.alpha.par_iter().map(|&a| if a > 0 { 1.0 } else { 0.0 }).collect();
            let t2 = Instant::now();
            blender.feed(bx0, by0, lw_px, lh_px, layer.rgb, alpha, mask);
            if profile {
                eprintln!("layer {k}: {lw_px}x{lh_px} mask {:.2?} blend {:.2?}", t2 - t1, t2.elapsed());
            }
        }
    }
    lap(&mut stats, "warp + blend");

    progress(0.88, "Finishing");
    let (out, cov) = blender.finish();
    // The part of the blend canvas that becomes the image.
    let (ow, oh, skip_x) = if periodic { (width, h, pad) } else { (bw, bh, 0) };
    let mut rgb = vec![0u16; ow * oh * 3];
    let mut alpha = vec![0u8; ow * oh];
    rgb.par_chunks_mut(ow * 3).zip(alpha.par_chunks_mut(ow)).enumerate().for_each(|(y, (row, arow))| {
        let src = &out[(y * bw + skip_x) * 3..(y * bw + skip_x + ow) * 3];
        let c = &cov[y * bw + skip_x..y * bw + skip_x + ow];
        for x in 0..ow {
            if c[x] > 1e-3 {
                arow[x] = 255;
                for k in 0..3 {
                    row[x * 3 + k] = pixels::encode_srgb16(pixels::from_perceptual(src[x * 3 + k]));
                }
            }
        }
    });
    drop(out);
    drop(cov);
    lap(&mut stats, "collapse");

    let mut pano = Panorama {
        width: ow,
        height: oh,
        rgb,
        alpha,
        full_width: width,
        full_height: h,
        crop_x: if periodic { 0 } else { origin_x as usize },
        crop_y: if periodic { 0 } else { origin_y as usize },
        stats,
    };
    crop_to_coverage(&mut pano);
    if proj.output.trim_edges && !pano.is_full_sphere() {
        trim_to_inner_rectangle(&mut pano);
    }
    if proj.output.pole_fill == crate::project::PoleFill::Auto {
        let t = Instant::now();
        let fill =
            crate::fill::FillOptions { zenith_patch: proj.output.zenith_patch, nadir_patch: proj.output.nadir_patch, ..Default::default() };
        let report = crate::fill::fill_voids(&mut pano, &fill, &|f, s| progress(0.9 + 0.1 * f, s));
        pano.stats.filled_percent = report.filled_percent;
        pano.stats.timings.push(("void fill".to_string(), t.elapsed().as_secs_f32()));
    }
    progress(1.0, "Done");
    Ok(pano)
}

/// Crops partial panoramas to the bounding box of their coverage. Full 360° panoramas keep the
/// whole canvas (pole gaps stay inside the image).
pub fn crop_to_coverage(pano: &mut Panorama) {
    let (w, h) = (pano.width, pano.height);
    let mut col = vec![false; w];
    let mut row = vec![false; h];
    for y in 0..h {
        let a = &pano.alpha[y * w..(y + 1) * w];
        for x in 0..w {
            if a[x] != 0 {
                col[x] = true;
                row[y] = true;
            }
        }
    }
    let covered_cols = col.iter().filter(|c| **c).count();
    if covered_cols == 0 {
        return;
    }
    // A panorama that wraps (nearly) all the way round stays a full sphere.
    if w == pano.full_width && covered_cols as f32 >= w as f32 * 0.995 {
        return;
    }
    let x0 = col.iter().position(|c| *c).unwrap();
    let x1 = col.iter().rposition(|c| *c).unwrap() + 1;
    let y0 = row.iter().position(|c| *c).unwrap();
    let y1 = row.iter().rposition(|c| *c).unwrap() + 1;
    let (nw, nh) = (x1 - x0, y1 - y0);
    let mut rgb = vec![0u16; nw * nh * 3];
    let mut alpha = vec![0u8; nw * nh];
    for y in 0..nh {
        let s = (y + y0) * w + x0;
        rgb[y * nw * 3..(y + 1) * nw * 3].copy_from_slice(&pano.rgb[s * 3..(s + nw) * 3]);
        alpha[y * nw..(y + 1) * nw].copy_from_slice(&pano.alpha[s..s + nw]);
    }
    pano.rgb = rgb;
    pano.alpha = alpha;
    pano.width = nw;
    pano.height = nh;
    pano.crop_x += x0;
    pano.crop_y += y0;
}

/// Crops a partial panorama to the largest axis-aligned rectangle that is completely covered,
/// removing the ragged, empty corners left by the photos' outlines. Does nothing if that would
/// throw away most of the image.
pub fn trim_to_inner_rectangle(pano: &mut Panorama) {
    let (w, h) = (pano.width, pano.height);
    if w == 0 || h == 0 || !pano.has_gaps() {
        return;
    }
    // Largest rectangle of covered pixels: histogram of column heights, row by row.
    let mut heights = vec![0u32; w];
    let mut best = (0u64, 0usize, 0usize, 0usize, 0usize); // area, x0, y0, x1, y1
    let mut stack: Vec<usize> = Vec::with_capacity(w);
    for y in 0..h {
        let row = &pano.alpha[y * w..(y + 1) * w];
        for x in 0..w {
            heights[x] = if row[x] != 0 { heights[x] + 1 } else { 0 };
        }
        stack.clear();
        for x in 0..=w {
            let cur = if x < w { heights[x] } else { 0 };
            while let Some(&top) = stack.last() {
                if heights[top] < cur {
                    break;
                }
                stack.pop();
                let height = heights[top] as usize;
                let left = stack.last().map_or(0, |&l| l + 1);
                let area = (height * (x - left)) as u64;
                if area > best.0 {
                    best = (area, left, y + 1 - height, x, y + 1);
                }
            }
            stack.push(x);
        }
    }
    let (area, x0, y0, x1, y1) = best;
    if (area as f64) < 0.5 * (w * h) as f64 {
        return;
    }
    let (nw, nh) = (x1 - x0, y1 - y0);
    let mut rgb = vec![0u16; nw * nh * 3];
    for y in 0..nh {
        let s = (y + y0) * w + x0;
        rgb[y * nw * 3..(y + 1) * nw * 3].copy_from_slice(&pano.rgb[s * 3..(s + nw) * 3]);
    }
    pano.rgb = rgb;
    pano.alpha = vec![255u8; nw * nh];
    pano.width = nw;
    pano.height = nh;
    pano.crop_x += x0;
    pano.crop_y += y0;
}
