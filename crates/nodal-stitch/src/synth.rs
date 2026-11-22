//! Synthetic camera shots rendered from an equirectangular panorama, with known lens and pose.
//! Used by tests and benchmarks to measure the stitcher against ground truth.

use crate::lens::Lens;
use crate::matching::Rng;
use image::RgbImage;
use nalgebra::Rotation3;
use rayon::prelude::*;

#[inline]
fn cubic(t: f32) -> [f32; 4] {
    // Catmull-Rom weights.
    let t2 = t * t;
    let t3 = t2 * t;
    [-0.5 * t3 + t2 - 0.5 * t, 1.5 * t3 - 2.5 * t2 + 1.0, -1.5 * t3 + 2.0 * t2 + 0.5 * t, 0.5 * t3 - 0.5 * t2]
}

/// Bicubic sample of an RGB8 image at continuous pixel coordinates (pixel centres at half-integers).
/// `wrap_x` treats the image as horizontally periodic (equirectangular panoramas).
#[inline]
pub fn sample_bicubic(data: &[u8], w: usize, h: usize, x: f32, y: f32, wrap_x: bool) -> [f32; 3] {
    let fx = x - 0.5;
    let fy = y - 0.5;
    let x0 = fx.floor();
    let y0 = fy.floor();
    let wx = cubic(fx - x0);
    let wy = cubic(fy - y0);
    let (x0, y0) = (x0 as isize, y0 as isize);
    let mut out = [0.0f32; 3];
    for (j, wyj) in wy.iter().enumerate() {
        let yy = (y0 - 1 + j as isize).clamp(0, h as isize - 1) as usize;
        let mut row = [0.0f32; 3];
        for (i, wxi) in wx.iter().enumerate() {
            let xi = x0 - 1 + i as isize;
            let xx = if wrap_x { xi.rem_euclid(w as isize) as usize } else { xi.clamp(0, w as isize - 1) as usize };
            let p = &data[(yy * w + xx) * 3..(yy * w + xx) * 3 + 3];
            row[0] += wxi * p[0] as f32;
            row[1] += wxi * p[1] as f32;
            row[2] += wxi * p[2] as f32;
        }
        out[0] += wyj * row[0];
        out[1] += wyj * row[1];
        out[2] += wyj * row[2];
    }
    out
}

#[derive(Clone, Copy, Debug)]
pub struct ShotOptions {
    /// Linear-light exposure multiplier.
    pub gain: f32,
    /// Corner darkening: brightness at the image corner is `1 − vignette`.
    pub vignette: f32,
    /// Gaussian-ish noise amplitude in 8-bit levels.
    pub noise: f32,
    pub seed: u64,
}

impl Default for ShotOptions {
    fn default() -> Self {
        ShotOptions { gain: 1.0, vignette: 0.0, noise: 0.0, seed: 1 }
    }
}

/// Renders what a camera with `lens`, oriented by `rot` (camera → world), sees of `pano`.
pub fn render_shot(pano: &RgbImage, lens: &Lens, rot: &Rotation3<f64>, opts: &ShotOptions) -> RgbImage {
    let (pw, ph) = (pano.width() as usize, pano.height() as usize);
    let (w, h) = (lens.width as usize, lens.height as usize);
    let model = lens.model();
    let data = pano.as_raw();
    let mut out = vec![0u8; w * h * 3];
    let half_diag2 = ((w * w + h * h) as f32) * 0.25;
    out.par_chunks_mut(w * 3).enumerate().for_each(|(y, row)| {
        let mut rng = Rng::new(opts.seed.wrapping_mul(1_000_003).wrapping_add(y as u64));
        for x in 0..w {
            let ray = rot * model.pixel_to_ray(x as f64 + 0.5, y as f64 + 0.5);
            let yaw = ray.x.atan2(ray.z);
            let pitch = ray.y.clamp(-1.0, 1.0).asin();
            let u = (0.5 + yaw / std::f64::consts::TAU) * pw as f64;
            let v = (0.5 - pitch / std::f64::consts::PI) * ph as f64;
            let c = sample_bicubic(data, pw, ph, u as f32, v as f32, true);
            let r2 = ((x as f32 + 0.5 - w as f32 * 0.5).powi(2) + (y as f32 + 0.5 - h as f32 * 0.5).powi(2)) / half_diag2;
            let k = opts.gain * (1.0 - opts.vignette * r2);
            for ch in 0..3 {
                let mut val = c[ch].clamp(0.0, 255.0);
                if k != 1.0 {
                    let lin = (val / 255.0).powf(2.2) * k;
                    val = lin.powf(1.0 / 2.2) * 255.0;
                }
                if opts.noise > 0.0 {
                    let nz = (rng.unit() + rng.unit() + rng.unit() - 1.5) as f32 * 2.0;
                    val += nz * opts.noise;
                }
                row[x * 3 + ch] = val.round().clamp(0.0, 255.0) as u8;
            }
        }
    });
    RgbImage::from_raw(w as u32, h as u32, out).expect("buffer size")
}
