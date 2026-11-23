//! Scene-linear float images, transfer curves, resampling and orientation.
//!
//! The stitcher works in scene-linear light (Rec. 709 / sRGB primaries) from decoding to the final
//! encode. Values are not clipped: RAW highlights can exceed 1.0 and saturated colours can go
//! slightly negative, and both survive until the output is written.

use image::RgbImage;
use rayon::prelude::*;
use std::sync::OnceLock;

/// Scene-linear RGB, interleaved `f32`, row-major.
#[derive(Clone, Debug, Default)]
pub struct LinearImage {
    pub width: usize,
    pub height: usize,
    pub data: Vec<f32>,
}

impl LinearImage {
    pub fn new(width: usize, height: usize) -> Self {
        LinearImage { width, height, data: vec![0.0; width * height * 3] }
    }

    pub fn from_data(width: usize, height: usize, data: Vec<f32>) -> Self {
        debug_assert_eq!(data.len(), width * height * 3);
        LinearImage { width, height, data }
    }

    /// Decodes an 8-bit sRGB image to linear light.
    pub fn from_srgb8(img: &RgbImage) -> Self {
        let lut = srgb8_to_linear_lut();
        let data: Vec<f32> = img.as_raw().par_iter().map(|&v| lut[v as usize]).collect();
        LinearImage { width: img.width() as usize, height: img.height() as usize, data }
    }

    /// Encodes to 8-bit sRGB (clipped), multiplying by `gain` first.
    pub fn to_srgb8(&self, gain: f32) -> RgbImage {
        let mut out = vec![0u8; self.data.len()];
        out.par_chunks_mut(4096).zip(self.data.par_chunks(4096)).for_each(|(o, s)| {
            for (o, &v) in o.iter_mut().zip(s) {
                *o = encode_srgb8(v * gain);
            }
        });
        RgbImage::from_raw(self.width as u32, self.height as u32, out).expect("buffer size")
    }

    pub fn len_pixels(&self) -> usize {
        self.width * self.height
    }

    #[inline]
    pub fn pixel(&self, x: usize, y: usize) -> [f32; 3] {
        let i = (y * self.width + x) * 3;
        [self.data[i], self.data[i + 1], self.data[i + 2]]
    }

    pub fn scale(&mut self, k: [f32; 3]) {
        self.data.par_chunks_mut(3 * 1024).for_each(|c| {
            for p in c.as_chunks_mut::<3>().0 {
                p[0] *= k[0];
                p[1] *= k[1];
                p[2] *= k[2];
            }
        });
    }

    /// Area-averaging downscale so the longer side is at most `max_dim` (no-op if already smaller).
    pub fn downscale_to(&self, max_dim: usize) -> LinearImage {
        let long = self.width.max(self.height);
        if long <= max_dim || max_dim == 0 {
            return self.clone();
        }
        let s = max_dim as f64 / long as f64;
        let nw = ((self.width as f64 * s).round() as usize).max(1);
        let nh = ((self.height as f64 * s).round() as usize).max(1);
        self.resize_area(nw, nh)
    }

    /// Exact area resampling (box filter with fractional coverage); only for shrinking.
    pub fn resize_area(&self, nw: usize, nh: usize) -> LinearImage {
        let data = resize_area(&self.data, self.width, self.height, 3, nw, nh);
        LinearImage { width: nw, height: nh, data }
    }

    /// 2×2 box reduction.
    pub fn halve(&self) -> LinearImage {
        let (w, h) = (self.width, self.height);
        let (nw, nh) = ((w / 2).max(1), (h / 2).max(1));
        let mut data = vec![0.0f32; nw * nh * 3];
        data.par_chunks_mut(nw * 3).enumerate().for_each(|(y, row)| {
            let y0 = (2 * y).min(h - 1);
            let y1 = (2 * y + 1).min(h - 1);
            for x in 0..nw {
                let x0 = (2 * x).min(w - 1);
                let x1 = (2 * x + 1).min(w - 1);
                for c in 0..3 {
                    row[x * 3 + c] = 0.25
                        * (self.data[(y0 * w + x0) * 3 + c]
                            + self.data[(y0 * w + x1) * 3 + c]
                            + self.data[(y1 * w + x0) * 3 + c]
                            + self.data[(y1 * w + x1) * 3 + c]);
                }
            }
        });
        LinearImage { width: nw, height: nh, data }
    }

    pub fn crop(&self, x0: usize, y0: usize, w: usize, h: usize) -> LinearImage {
        let w = w.min(self.width.saturating_sub(x0));
        let h = h.min(self.height.saturating_sub(y0));
        let mut data = vec![0.0f32; w * h * 3];
        data.par_chunks_mut(w * 3).enumerate().for_each(|(y, row)| {
            let s = ((y0 + y) * self.width + x0) * 3;
            row.copy_from_slice(&self.data[s..s + w * 3]);
        });
        LinearImage { width: w, height: h, data }
    }

    /// Applies an EXIF orientation (1–8) so the image is upright.
    pub fn oriented(self, orientation: u32) -> LinearImage {
        if orientation <= 1 || orientation > 8 {
            return self;
        }
        let (w, h, data) = orient(&self.data, self.width, self.height, 3, orientation);
        LinearImage { width: w, height: h, data }
    }

    /// Rec. 709 luma of every pixel.
    pub fn luma(&self) -> Vec<f32> {
        self.data.par_chunks(3).map(|p| 0.2126 * p[0] + 0.7152 * p[1] + 0.0722 * p[2]).collect()
    }

    /// Mean of each channel.
    pub fn mean(&self) -> [f64; 3] {
        let n = self.len_pixels().max(1) as f64;
        let s = self.data.par_chunks(3 * 4096).map(|c| {
            let mut s = [0.0f64; 3];
            for p in c.as_chunks::<3>().0 {
                s[0] += p[0] as f64;
                s[1] += p[1] as f64;
                s[2] += p[2] as f64;
            }
            s
        });
        let s = s.reduce(|| [0.0; 3], |a, b| [a[0] + b[0], a[1] + b[1], a[2] + b[2]]);
        [s[0] / n, s[1] / n, s[2] / n]
    }
}

/// Share of 8×8 blocks of an 8-bit photo whose luma varies by at least `min_contrast` levels
/// (standard deviation). Clear sky, fog or a blank wall score near zero.
pub fn textured_share(img: &image::RgbImage, min_contrast: f32) -> f32 {
    let (w, h) = (img.width() as usize, img.height() as usize);
    let raw = img.as_raw();
    let (bw, bh) = (w / 8, h / 8);
    if bw == 0 || bh == 0 {
        return 0.0;
    }
    let mut textured = 0usize;
    for by in 0..bh {
        for bx in 0..bw {
            let (mut s, mut s2) = (0.0f32, 0.0f32);
            for y in by * 8..by * 8 + 8 {
                for x in bx * 8..bx * 8 + 8 {
                    let i = (y * w + x) * 3;
                    let l = 0.299 * raw[i] as f32 + 0.587 * raw[i + 1] as f32 + 0.114 * raw[i + 2] as f32;
                    s += l;
                    s2 += l * l;
                }
            }
            let m = s / 64.0;
            if (s2 / 64.0 - m * m).max(0.0).sqrt() >= min_contrast {
                textured += 1;
            }
        }
    }
    textured as f32 / (bw * bh) as f32
}

/// Separable exact-area resampling of an interleaved `c`-channel image (shrinking only).
pub fn resize_area(src: &[f32], w: usize, h: usize, c: usize, nw: usize, nh: usize) -> Vec<f32> {
    // Horizontal pass.
    let wx = area_weights(w, nw);
    let mut tmp = vec![0.0f32; nw * h * c];
    tmp.par_chunks_mut(nw * c).enumerate().for_each(|(y, row)| {
        let srow = &src[y * w * c..(y + 1) * w * c];
        for (ox, (start, weights)) in wx.iter().enumerate() {
            for ch in 0..c {
                let mut acc = 0.0f32;
                for (k, &wt) in weights.iter().enumerate() {
                    acc += srow[(start + k) * c + ch] * wt;
                }
                row[ox * c + ch] = acc;
            }
        }
    });
    // Vertical pass.
    let wy = area_weights(h, nh);
    let mut out = vec![0.0f32; nw * nh * c];
    out.par_chunks_mut(nw * c).enumerate().for_each(|(oy, row)| {
        let (start, weights) = &wy[oy];
        for (k, &wt) in weights.iter().enumerate() {
            let srow = &tmp[(start + k) * nw * c..(start + k + 1) * nw * c];
            for (o, &s) in row.iter_mut().zip(srow) {
                *o += s * wt;
            }
        }
    });
    out
}

/// For each output sample: first input index and normalised coverage weights.
fn area_weights(n: usize, m: usize) -> Vec<(usize, Vec<f32>)> {
    let scale = n as f64 / m as f64;
    (0..m)
        .map(|o| {
            let a = o as f64 * scale;
            let b = ((o + 1) as f64 * scale).min(n as f64);
            let i0 = a.floor() as usize;
            let i1 = (b.ceil() as usize).min(n).max(i0 + 1);
            let mut ws = Vec::with_capacity(i1 - i0);
            let mut total = 0.0;
            for i in i0..i1 {
                let lo = (i as f64).max(a);
                let hi = ((i + 1) as f64).min(b);
                let wgt = (hi - lo).max(0.0);
                ws.push(wgt as f32);
                total += wgt;
            }
            let inv = if total > 0.0 { 1.0 / total as f32 } else { 0.0 };
            ws.iter_mut().for_each(|v| *v *= inv);
            (i0, ws)
        })
        .collect()
}

/// Applies an EXIF orientation to an interleaved buffer. Returns the new size and data.
pub fn orient<T: Copy + Default + Send + Sync>(src: &[T], w: usize, h: usize, c: usize, orientation: u32) -> (usize, usize, Vec<T>) {
    let swap = matches!(orientation, 5..=8);
    let (nw, nh) = if swap { (h, w) } else { (w, h) };
    let mut out = vec![T::default(); nw * nh * c];
    out.par_chunks_mut(nw * c).enumerate().for_each(|(y, row)| {
        for x in 0..nw {
            // Source pixel shown at (x, y) of the upright image.
            let (sx, sy) = match orientation {
                2 => (w - 1 - x, y),
                3 => (w - 1 - x, h - 1 - y),
                4 => (x, h - 1 - y),
                5 => (y, x),
                6 => (y, h - 1 - x),
                7 => (w - 1 - y, h - 1 - x),
                8 => (w - 1 - y, x),
                _ => (x, y),
            };
            let s = (sy * w + sx) * c;
            row[x * c..x * c + c].copy_from_slice(&src[s..s + c]);
        }
    });
    (nw, nh, out)
}

// ---- Transfer curves ---------------------------------------------------------------------------

#[inline]
pub fn srgb_to_linear(v: f32) -> f32 {
    let a = v.abs();
    let l = if a <= 0.04045 { a / 12.92 } else { ((a + 0.055) / 1.055).powf(2.4) };
    l.copysign(v)
}

#[inline]
pub fn linear_to_srgb(v: f32) -> f32 {
    let a = v.abs();
    let e = if a <= 0.0031308 { a * 12.92 } else { 1.055 * a.powf(1.0 / 2.4) - 0.055 };
    e.copysign(v)
}

pub fn srgb8_to_linear_lut() -> &'static [f32; 256] {
    static LUT: OnceLock<[f32; 256]> = OnceLock::new();
    LUT.get_or_init(|| {
        let mut t = [0.0f32; 256];
        for (i, v) in t.iter_mut().enumerate() {
            *v = srgb_to_linear(i as f32 / 255.0);
        }
        t
    })
}

pub fn srgb16_to_linear_lut() -> &'static [f32] {
    static LUT: OnceLock<Vec<f32>> = OnceLock::new();
    LUT.get_or_init(|| (0..65536).map(|i| srgb_to_linear(i as f32 / 65535.0)).collect())
}

const ENC_N: usize = 8192;

/// sRGB encoding table indexed by `sqrt(linear)`, which spends its resolution where the curve is
/// steep (shadows). Linear interpolation between entries is accurate to well under one 16-bit step.
fn encode_table() -> &'static [f32] {
    static LUT: OnceLock<Vec<f32>> = OnceLock::new();
    LUT.get_or_init(|| {
        (0..=ENC_N)
            .map(|i| {
                let s = i as f32 / ENC_N as f32;
                linear_to_srgb(s * s)
            })
            .collect()
    })
}

/// Fast sRGB encode of a linear value, clipped to [0, 1].
#[inline]
pub fn encode_srgb(v: f32) -> f32 {
    if v <= 0.0 || v.is_nan() {
        return 0.0;
    }
    if v >= 1.0 {
        return 1.0;
    }
    let t = encode_table();
    let p = v.sqrt() * ENC_N as f32;
    let i = (p as usize).min(ENC_N - 1);
    let f = p - i as f32;
    t[i] + (t[i + 1] - t[i]) * f
}

#[inline]
pub fn encode_srgb8(v: f32) -> u8 {
    (encode_srgb(v) * 255.0 + 0.5) as u8
}

#[inline]
pub fn encode_srgb16(v: f32) -> u16 {
    (encode_srgb(v) * 65535.0 + 0.5) as u16
}

// ---- Perceptual working space ------------------------------------------------------------------
//
// Seams and multi-band blending work on perceptually spaced values so that a difference of 0.01
// means about the same visible step in shadows and highlights, and blending bright skies does not
// leave dark halos. The curve is a pure power (no toe), so it extends smoothly past 1.0 (HDR) and,
// sign-preserving, to negative out-of-gamut values.

pub const PERCEPTUAL_GAMMA: f32 = 1.0 / 2.2;

#[inline]
pub fn to_perceptual(v: f32) -> f32 {
    v.abs().powf(PERCEPTUAL_GAMMA).copysign(v)
}

#[inline]
pub fn from_perceptual(v: f32) -> f32 {
    v.abs().powf(1.0 / PERCEPTUAL_GAMMA).copysign(v)
}

/// Table-driven `to_perceptual` for values in [0, 4]; falls back to `powf` outside.
#[inline]
pub fn to_perceptual_fast(v: f32) -> f32 {
    static LUT: OnceLock<Vec<f32>> = OnceLock::new();
    const N: usize = 16384;
    const MAX: f32 = 4.0;
    if !(0.0..MAX).contains(&v) {
        return to_perceptual(v);
    }
    let t = LUT.get_or_init(|| {
        (0..=N)
            .map(|i| {
                let s = i as f32 / N as f32;
                (s * s * MAX).powf(PERCEPTUAL_GAMMA)
            })
            .collect()
    });
    let p = (v / MAX).sqrt() * N as f32;
    let i = (p as usize).min(N - 1);
    let f = p - i as f32;
    t[i] + (t[i + 1] - t[i]) * f
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn srgb_round_trip_and_encode_accuracy() {
        for i in 0..=255u32 {
            let v = i as f32 / 255.0;
            assert!((linear_to_srgb(srgb_to_linear(v)) - v).abs() < 1e-5);
            assert_eq!(encode_srgb8(srgb_to_linear(v)), i as u8);
        }
        for i in 0..=1000u32 {
            let l = (i as f32 / 1000.0).powi(3);
            let exact = linear_to_srgb(l);
            assert!((encode_srgb(l) - exact).abs() < 1.0 / 65535.0 * 2.0, "{l}");
        }
    }

    #[test]
    fn perceptual_fast_matches() {
        for i in 0..2000 {
            let v = i as f32 / 500.0;
            assert!((to_perceptual_fast(v) - to_perceptual(v)).abs() < 2e-4, "{v}");
        }
        assert!((from_perceptual(to_perceptual(-0.3)) + 0.3).abs() < 1e-6);
    }

    #[test]
    fn orientation_round_trips() {
        // 3×2 image with distinct values.
        let src: Vec<u8> = (0..6).collect();
        for o in 1..=8u32 {
            let (w, h, d) = orient(&src, 3, 2, 1, o);
            assert_eq!(w * h, 6);
            let mut sorted = d.clone();
            sorted.sort();
            assert_eq!(sorted, src);
        }
        // Orientation 6 (rotate 90° clockwise to display): top-left of the upright image is the
        // stored bottom-left.
        let (w, h, d) = orient(&src, 3, 2, 1, 6);
        assert_eq!((w, h), (2, 3));
        assert_eq!(d[0], 3);
        // Orientation 8 (rotate 90° counter-clockwise): top-left is the stored top-right.
        let (_, _, d) = orient(&src, 3, 2, 1, 8);
        assert_eq!(d[0], 2);
    }

    #[test]
    fn area_resize_preserves_mean() {
        let w = 37;
        let h = 23;
        let data: Vec<f32> = (0..w * h * 3).map(|i| (i % 17) as f32).collect();
        let img = LinearImage::from_data(w, h, data);
        let small = img.resize_area(11, 7);
        let (a, b) = (img.mean(), small.mean());
        for c in 0..3 {
            assert!((a[c] - b[c]).abs() < 0.05 * a[c].abs().max(1.0), "{a:?} {b:?}");
        }
    }
}
