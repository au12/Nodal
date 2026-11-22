//! Equirectangular source sampling with adaptive anti-aliasing.

use glam::Vec3;
use std::f32::consts::{FRAC_PI_2, PI};

/// Fast `atan2` (max error ≈ 1e-6 rad, i.e. < 0.01 px even on a 32k panorama).
#[inline]
pub fn fast_atan2(y: f32, x: f32) -> f32 {
    let ax = x.abs();
    let ay = y.abs();
    let (mn, mx) = if ax < ay { (ax, ay) } else { (ay, ax) };
    if mx == 0.0 {
        return 0.0;
    }
    let a = mn / mx;
    let s = a * a;
    // Minimax polynomial from the Cg standard library reference implementation.
    let mut r = a * (0.999_995_6 + s * (-0.332_994_6 + s * (0.195_635_9 + s * (-0.121_239_07 + s * (0.057_477_314 + s * -0.013_480_47)))));
    if ay > ax {
        r = FRAC_PI_2 - r;
    }
    if x < 0.0 {
        r = PI - r;
    }
    if y < 0.0 {
        -r
    } else {
        r
    }
}

/// Geometry of the source image on the sphere (full 360×180 by default).
#[derive(Clone, Copy, Debug)]
pub struct Coverage {
    /// Horizontal field of view in radians (≤ 2π).
    pub hfov: f32,
    /// Vertical field of view in radians (≤ π).
    pub vfov: f32,
    /// Pitch of the image's vertical centre in radians.
    pub voffset: f32,
}

impl Coverage {
    pub const FULL: Coverage = Coverage { hfov: 2.0 * PI, vfov: PI, voffset: 0.0 };

    pub fn is_full(&self) -> bool {
        self.hfov >= 2.0 * PI - 1e-4 && self.vfov >= PI - 1e-4
    }

    /// Reads Photo Sphere (GPano) metadata from an image file's bytes, if present: it records which
    /// part of the full sphere a cropped panorama covers. Nodal's stitcher and most panorama tools
    /// write it.
    pub fn from_gpano(bytes: &[u8]) -> Option<Coverage> {
        // The XMP packet sits in the file header; attribute and element forms are both common.
        let head = &bytes[..bytes.len().min(256 * 1024)];
        let text = String::from_utf8_lossy(head);
        let field = |name: &str| -> Option<f32> {
            let key = format!("GPano:{name}");
            let at = text.find(&key)? + key.len();
            let rest = &text[at..];
            let rest = rest.trim_start_matches(|c: char| c == '=' || c == '"' || c == '\'' || c == '>' || c.is_whitespace());
            let end = rest.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(rest.len());
            rest[..end].parse().ok()
        };
        let (full_w, full_h) = (field("FullPanoWidthPixels")?, field("FullPanoHeightPixels")?);
        let (w, h) = (field("CroppedAreaImageWidthPixels")?, field("CroppedAreaImageHeightPixels")?);
        let top = field("CroppedAreaTopPixels").unwrap_or(0.0);
        if full_w < 1.0 || full_h < 1.0 || w < 1.0 || h < 1.0 || w > full_w + 0.5 || h + top > full_h + 0.5 {
            return None;
        }
        let centre = (top + h * 0.5) / full_h;
        Some(Coverage { hfov: (w / full_w * 2.0 * PI).min(2.0 * PI), vfov: (h / full_h * PI).min(PI), voffset: (0.5 - centre) * PI })
    }

    /// Guesses coverage from the aspect ratio: 2:1 is a full sphere, wider images are
    /// treated as 360° panoramas with a reduced vertical field centred on the horizon.
    pub fn guess(width: u32, height: u32) -> Coverage {
        let aspect = width as f32 / height as f32;
        if (aspect - 2.0).abs() < 0.02 || aspect < 2.0 {
            Coverage::FULL
        } else {
            Coverage { hfov: 2.0 * PI, vfov: 2.0 * PI / aspect, voffset: 0.0 }
        }
    }
}

pub struct Equirect {
    pub width: usize,
    pub height: usize,
    data: Vec<u8>,
    pub coverage: Coverage,
    /// Precomputed scale factors: pixels per radian.
    sx: f32,
    sy: f32,
    pitch_top: f32,
    wrap: bool,
    pub background: [u8; 3],
}

impl Equirect {
    pub fn new(img: image::RgbImage, coverage: Coverage) -> Self {
        let (w, h) = img.dimensions();
        let wrap = coverage.hfov >= 2.0 * PI - 1e-4;
        Self {
            width: w as usize,
            height: h as usize,
            data: img.into_raw(),
            coverage,
            sx: w as f32 / coverage.hfov,
            sy: h as f32 / coverage.vfov,
            pitch_top: coverage.voffset + coverage.vfov * 0.5,
            wrap,
            background: [0, 0, 0],
        }
    }

    /// Pixels per radian along the equator.
    pub fn density(&self) -> f32 {
        self.sx
    }

    #[inline]
    fn texel(&self, x: i32, y: i32) -> [f32; 3] {
        let w = self.width as i32;
        let x = if self.wrap { x.rem_euclid(w) } else { x.clamp(0, w - 1) };
        let y = y.clamp(0, self.height as i32 - 1);
        let i = (y as usize * self.width + x as usize) * 3;
        [self.data[i] as f32, self.data[i + 1] as f32, self.data[i + 2] as f32]
    }

    /// Source pixel coordinates (continuous, texel centres at .5) for a direction, or `None`
    /// when the direction falls outside a partial panorama.
    #[inline]
    pub fn pixel_of(&self, d: Vec3) -> Option<(f32, f32)> {
        let yaw = fast_atan2(d.x, d.z);
        let pitch = fast_atan2(d.y, (d.x * d.x + d.z * d.z).sqrt());
        let x = (yaw + self.coverage.hfov * 0.5) * self.sx;
        let y = (self.pitch_top - pitch) * self.sy;
        if !self.wrap && (x < 0.0 || x > self.width as f32) {
            return None;
        }
        if y < -0.5 || y > self.height as f32 + 0.5 {
            return None;
        }
        Some((x, y))
    }

    #[inline]
    pub fn bilinear(&self, x: f32, y: f32) -> [f32; 3] {
        let fx = x - 0.5;
        let fy = y - 0.5;
        let x0 = fx.floor();
        let y0 = fy.floor();
        let tx = fx - x0;
        let ty = fy - y0;
        let (x0, y0) = (x0 as i32, y0 as i32);
        let a = self.texel(x0, y0);
        let b = self.texel(x0 + 1, y0);
        let c = self.texel(x0, y0 + 1);
        let d = self.texel(x0 + 1, y0 + 1);
        let mut out = [0.0; 3];
        for k in 0..3 {
            let top = a[k] + (b[k] - a[k]) * tx;
            let bot = c[k] + (d[k] - c[k]) * tx;
            out[k] = top + (bot - top) * ty;
        }
        out
    }

    /// Catmull-Rom bicubic — crisper than bilinear near 1:1 scale.
    #[inline]
    pub fn bicubic(&self, x: f32, y: f32) -> [f32; 3] {
        let fx = x - 0.5;
        let fy = y - 0.5;
        let x0 = fx.floor();
        let y0 = fy.floor();
        let wx = catmull_rom(fx - x0);
        let wy = catmull_rom(fy - y0);
        let (x0, y0) = (x0 as i32, y0 as i32);
        let mut out = [0.0f32; 3];
        let interior = x0 >= 1 && y0 >= 1 && x0 + 2 < self.width as i32 && y0 + 2 < self.height as i32;
        if interior {
            // Fast path: direct slice access, no wrapping or clamping.
            let stride = self.width * 3;
            for (j, wyj) in wy.iter().enumerate() {
                let base = (y0 as usize - 1 + j) * stride + (x0 as usize - 1) * 3;
                let px = &self.data[base..base + 12];
                let mut row = [0.0f32; 3];
                for (i, wxi) in wx.iter().enumerate() {
                    row[0] += px[i * 3] as f32 * wxi;
                    row[1] += px[i * 3 + 1] as f32 * wxi;
                    row[2] += px[i * 3 + 2] as f32 * wxi;
                }
                out[0] += row[0] * wyj;
                out[1] += row[1] * wyj;
                out[2] += row[2] * wyj;
            }
            return out;
        }
        for (j, wyj) in wy.iter().enumerate() {
            let mut row = [0.0f32; 3];
            for (i, wxi) in wx.iter().enumerate() {
                let t = self.texel(x0 - 1 + i as i32, y0 - 1 + j as i32);
                row[0] += t[0] * wxi;
                row[1] += t[1] * wxi;
                row[2] += t[2] * wxi;
            }
            out[0] += row[0] * wyj;
            out[1] += row[1] * wyj;
            out[2] += row[2] * wyj;
        }
        out
    }

    /// Samples the colour seen along direction `d` for a target pixel that covers roughly
    /// `pixel_angle` radians.
    ///
    /// An isotropic footprint on the sphere maps to an axis-aligned ellipse in equirect
    /// space (the metric is `cos²(lat)·dlon² + dlat²`), so oversampled regions near the
    /// poles are filtered with a source-space box of the right extent — one direction→pixel
    /// conversion per target pixel, no matter how wide the footprint. Near 1:1 the sample is
    /// a single Catmull-Rom tap.
    #[inline]
    pub fn sample_adaptive(&self, d: Vec3, pixel_angle: f32) -> [u8; 3] {
        let Some((x, y)) = self.pixel_of(d) else { return self.background };
        let cos_lat = ((d.x * d.x + d.z * d.z).sqrt() / d.length()).max(1e-5);
        let rh = (pixel_angle * self.sx / cos_lat).min(self.width as f32);
        let rv = pixel_angle * self.sy;
        if rh <= 1.3 && rv <= 1.3 {
            return to_u8(self.bicubic(x, y));
        }
        let (nx, ex) = if rh > 1.0 { ((rh.ceil() as usize).min(64), rh) } else { (1, 0.0) };
        let (ny, ey) = if rv > 1.0 { ((rv.ceil() as usize).min(16), rv) } else { (1, 0.0) };
        let mut acc = [0.0f32; 3];
        for j in 0..ny {
            let yy = y + ((j as f32 + 0.5) / ny as f32 - 0.5) * ey;
            for i in 0..nx {
                let xx = x + ((i as f32 + 0.5) / nx as f32 - 0.5) * ex;
                let c = self.bilinear(xx, yy);
                acc[0] += c[0];
                acc[1] += c[1];
                acc[2] += c[2];
            }
        }
        let inv = 1.0 / (nx * ny) as f32;
        to_u8([acc[0] * inv, acc[1] * inv, acc[2] * inv])
    }

    /// A box-downscaled copy (for thumbnails), keeping coverage.
    pub fn downscaled(&self, max_width: usize) -> Equirect {
        let mut factor = 1;
        while self.width / (factor * 2) >= max_width && self.height / (factor * 2) >= 2 {
            factor *= 2;
        }
        if factor == 1 {
            let img = image::RgbImage::from_raw(self.width as u32, self.height as u32, self.data.clone()).unwrap();
            let mut e = Equirect::new(img, self.coverage);
            e.background = self.background;
            return e;
        }
        let (w, h) = (self.width / factor, self.height / factor);
        let mut out = vec![0u8; w * h * 3];
        for y in 0..h {
            for x in 0..w {
                let mut acc = [0u32; 3];
                for dy in 0..factor {
                    for dx in 0..factor {
                        let i = ((y * factor + dy) * self.width + x * factor + dx) * 3;
                        acc[0] += self.data[i] as u32;
                        acc[1] += self.data[i + 1] as u32;
                        acc[2] += self.data[i + 2] as u32;
                    }
                }
                let n = (factor * factor) as u32;
                let o = (y * w + x) * 3;
                out[o] = ((acc[0] + n / 2) / n) as u8;
                out[o + 1] = ((acc[1] + n / 2) / n) as u8;
                out[o + 2] = ((acc[2] + n / 2) / n) as u8;
            }
        }
        let mut e = Equirect::new(image::RgbImage::from_raw(w as u32, h as u32, out).unwrap(), self.coverage);
        e.background = self.background;
        e
    }
}

#[inline]
fn catmull_rom(t: f32) -> [f32; 4] {
    let t2 = t * t;
    let t3 = t2 * t;
    [0.5 * (-t3 + 2.0 * t2 - t), 0.5 * (3.0 * t3 - 5.0 * t2 + 2.0), 0.5 * (-3.0 * t3 + 4.0 * t2 + t), 0.5 * (t3 - t2)]
}

#[inline]
pub fn to_u8(c: [f32; 3]) -> [u8; 3] {
    [(c[0] + 0.5).clamp(0.0, 255.0) as u8, (c[1] + 0.5).clamp(0.0, 255.0) as u8, (c[2] + 0.5).clamp(0.0, 255.0) as u8]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gpano_metadata_gives_coverage() {
        let xmp = br#"....<rdf:Description GPano:ProjectionType="equirectangular" GPano:CroppedAreaImageWidthPixels="4000" GPano:CroppedAreaImageHeightPixels="1000" GPano:FullPanoWidthPixels="8000" GPano:FullPanoHeightPixels="4000" GPano:CroppedAreaLeftPixels="2000" GPano:CroppedAreaTopPixels="1000"/>"#;
        let c = Coverage::from_gpano(xmp).unwrap();
        assert!((c.hfov.to_degrees() - 180.0).abs() < 1e-3);
        assert!((c.vfov.to_degrees() - 45.0).abs() < 1e-3);
        assert!((c.voffset.to_degrees() - 22.5).abs() < 1e-3); // centred above the horizon
        assert!(Coverage::from_gpano(b"no metadata here").is_none());
    }

    #[test]
    fn fast_atan2_is_accurate() {
        let mut worst = 0.0f32;
        for i in 0..2000 {
            for j in 0..50 {
                let a = i as f32 / 2000.0 * std::f32::consts::TAU;
                let r = 0.01 + j as f32 * 0.7;
                let (y, x) = (a.sin() * r, a.cos() * r);
                let e = (fast_atan2(y, x) - y.atan2(x)).abs();
                let e = e.min((e - std::f32::consts::TAU).abs());
                worst = worst.max(e);
            }
        }
        eprintln!("fast_atan2 worst error: {worst:e} rad");
        assert!(worst < 2e-5, "worst error {worst}");
    }

    #[test]
    fn coverage_guess() {
        assert!(Coverage::guess(8000, 4000).is_full());
        let c = Coverage::guess(12000, 3000);
        assert!((c.vfov.to_degrees() - 90.0).abs() < 0.01);
    }
}
