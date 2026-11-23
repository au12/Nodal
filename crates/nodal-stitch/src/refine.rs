//! Sub-pixel control point refinement.
//!
//! Feature detection runs on reduced-size images, so matched positions are only good to about one
//! working pixel. Once the geometry is known, each control point is polished at full resolution:
//! the neighbourhood from one photo is resampled into the other's frame and slid over it to find
//! the correlation peak to a fraction of a pixel.

use crate::lens::LensModel;
use crate::project::Project;
use image::RgbImage;
use nalgebra::Rotation3;
use rayon::prelude::*;

/// Luma plane of a photo (possibly a reduced working copy).
pub struct Luma {
    pub w: usize,
    pub h: usize,
    pub data: Vec<u8>,
    /// Plane pixels per full-resolution pixel.
    pub scale: f32,
}

impl Luma {
    /// Luma of a working copy that is `scale` times the photo's full size.
    pub fn from_rgb_scaled(img: &RgbImage, scale: f32) -> Luma {
        let mut l = Luma::from_rgb(img);
        l.scale = scale;
        l
    }

    pub fn from_rgb(img: &RgbImage) -> Luma {
        let (w, h) = (img.width() as usize, img.height() as usize);
        let mut data = vec![0u8; w * h];
        data.par_chunks_mut(w).zip(img.as_raw().par_chunks(w * 3)).for_each(|(o, row)| {
            for (d, p) in o.iter_mut().zip(row.as_chunks::<3>().0) {
                *d = ((77 * p[0] as u32 + 150 * p[1] as u32 + 29 * p[2] as u32) >> 8) as u8;
            }
        });
        Luma { w, h, data, scale: 1.0 }
    }

    #[inline]
    fn sample(&self, x: f32, y: f32) -> Option<f32> {
        let (fx, fy) = (x * self.scale - 0.5, y * self.scale - 0.5);
        if fx < 0.0 || fy < 0.0 || fx >= (self.w - 1) as f32 || fy >= (self.h - 1) as f32 {
            return None;
        }
        let (x0, y0) = (fx as usize, fy as usize);
        let (tx, ty) = (fx - x0 as f32, fy - y0 as f32);
        let i = y0 * self.w + x0;
        let p = |o: usize| self.data[i + o] as f32;
        let top = p(0) + (p(1) - p(0)) * tx;
        let bot = p(self.w) + (p(self.w + 1) - p(self.w)) * tx;
        Some(top + (bot - top) * ty)
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct RefineReport {
    pub refined: usize,
    pub rejected: usize,
    /// Mean distance control points moved, in source pixels.
    pub mean_shift: f32,
}

/// Refines every control point in place. `lumas[i]` must belong to `project.images[i]`.
/// Points whose neighbourhoods do not correlate (repetition, motion, blur) are removed.
pub fn refine_control_points(project: &mut Project, lumas: &[Luma], search: i32) -> RefineReport {
    let models: Vec<LensModel> = project.lenses.iter().map(|l| l.model()).collect();
    let rots: Vec<Rotation3<f64>> = project.images.iter().map(|i| i.rot()).collect();
    let lens: Vec<usize> = project.images.iter().map(|i| i.lens).collect();
    const R: i32 = 9; // template half-size
    let big = R + search;
    let side = (2 * big + 1) as usize;
    let results: Vec<Option<(f32, f32, f32)>> = project
        .control_points
        .par_iter()
        .map(|cp| {
            let (a, b) = (cp.a as usize, cp.b as usize);
            let (ma, mb) = (&models[lens[a]], &models[lens[b]]);
            let b_to_a = rots[a].inverse() * rots[b];
            let map = |x: f32, y: f32| -> Option<(f32, f32)> {
                let ray = b_to_a * mb.pixel_to_ray(x as f64, y as f64);
                let (px, py) = ma.ray_to_pixel(ray)?;
                Some((px as f32, py as f32))
            };
            // Photo a resampled into photo b's frame around the point, on a pixel grid centred on it.
            let (cx, cy) = (cp.bx.floor() + 0.5, cp.by.floor() + 0.5);
            let mut warped = vec![0.0f32; side * side];
            for j in 0..side {
                for i in 0..side {
                    let (x, y) = (cx + (i as i32 - big) as f32, cy + (j as i32 - big) as f32);
                    let (ax, ay) = map(x, y)?;
                    warped[j * side + i] = lumas[a].sample(ax, ay)?;
                }
            }
            // Template from photo b.
            let t_side = (2 * R + 1) as usize;
            let mut tmpl = vec![0.0f32; t_side * t_side];
            for j in 0..t_side {
                for i in 0..t_side {
                    tmpl[j * t_side + i] = lumas[b].sample(cx + (i as i32 - R) as f32, cy + (j as i32 - R) as f32)?;
                }
            }
            let n = (t_side * t_side) as f32;
            let t_mean = tmpl.iter().sum::<f32>() / n;
            let t_var = tmpl.iter().map(|v| (v - t_mean).powi(2)).sum::<f32>();
            if t_var / n < 9.0 {
                return None; // featureless
            }
            // Normalised cross-correlation over the search window.
            let s_side = (2 * search + 1) as usize;
            let mut ncc = vec![-1.0f32; s_side * s_side];
            for dv in 0..s_side {
                for du in 0..s_side {
                    let (mut sum, mut sum2, mut cross) = (0.0f32, 0.0f32, 0.0f32);
                    for j in 0..t_side {
                        let row = &warped[(j + dv) * side + du..(j + dv) * side + du + t_side];
                        let trow = &tmpl[j * t_side..(j + 1) * t_side];
                        for (wv, tv) in row.iter().zip(trow) {
                            sum += wv;
                            sum2 += wv * wv;
                            cross += wv * tv;
                        }
                    }
                    let var = sum2 - sum * sum / n;
                    if var > 1e-3 {
                        ncc[dv * s_side + du] = (cross - sum * t_mean) / (var * t_var).sqrt();
                    }
                }
            }
            let (best_i, best) = ncc.iter().enumerate().max_by(|x, y| x.1.total_cmp(y.1))?;
            let (bu, bv) = ((best_i % s_side) as i32, (best_i / s_side) as i32);
            // The peak must be clear and inside the window.
            if *best < 0.8 || bu == 0 || bv == 0 || bu == s_side as i32 - 1 || bv == s_side as i32 - 1 {
                return None;
            }
            let at = |u: i32, v: i32| ncc[v as usize * s_side + u as usize];
            let sub = |m: f32, c: f32, p: f32| {
                let d = m - 2.0 * c + p;
                if d < -1e-6 {
                    (0.5 * (m - p) / d).clamp(-0.5, 0.5)
                } else {
                    0.0
                }
            };
            let su = sub(at(bu - 1, bv), *best, at(bu + 1, bv));
            let sv = sub(at(bu, bv - 1), *best, at(bu, bv + 1));
            // Photo a's content at window offset (du, dv) matches photo b's point: its true
            // counterpart in a is where that offset maps to.
            let off_x = (bu - search) as f32 + su;
            let off_y = (bv - search) as f32 + sv;
            let (ax, ay) = map(cp.bx + off_x, cp.by + off_y)?;
            Some((ax, ay, *best))
        })
        .collect();
    let mut report = RefineReport::default();
    let mut shift_sum = 0.0f32;
    let mut kept = Vec::with_capacity(project.control_points.len());
    for (cp, r) in project.control_points.iter().zip(results) {
        match r {
            Some((ax, ay, _)) => {
                shift_sum += (ax - cp.ax).hypot(ay - cp.ay);
                let mut c = *cp;
                c.ax = ax;
                c.ay = ay;
                kept.push(c);
                report.refined += 1;
            }
            None => report.rejected += 1,
        }
    }
    // Keep the original set if refinement would leave too little to work with.
    if kept.len() >= 12 && kept.len() * 4 >= project.control_points.len() {
        project.control_points = kept;
        report.mean_shift = shift_sum / report.refined.max(1) as f32;
    } else {
        report = RefineReport { rejected: project.control_points.len(), ..Default::default() };
    }
    report
}
