//! The stitching project: source images, lenses, orientations, control points and output settings.

use crate::lens::{Lens, LensKind, V3};
use nalgebra::{Quaternion, Rotation3, UnitQuaternion};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ExifInfo {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub make: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lens: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub focal_mm: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub focal_35mm: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exposure_time: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub f_number: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub iso: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub taken: Option<String>,
}

impl ExifInfo {
    /// Exposure value at ISO 100 (higher = darker capture settings), when the EXIF data allows.
    pub fn ev100(&self) -> Option<f64> {
        let (t, n) = (self.exposure_time?, self.f_number?);
        if t <= 0.0 || n <= 0.0 {
            return None;
        }
        Some((n * n / t).log2() - (self.iso.unwrap_or(100.0) / 100.0).log2())
    }

    /// One-line summary such as "24 mm · f/8 · 1/250 s · ISO 100".
    pub fn summary(&self) -> String {
        let mut parts = Vec::new();
        if let Some(f) = self.focal_mm {
            parts.push(format!("{} mm", trim_float(f)));
        }
        if let Some(n) = self.f_number {
            parts.push(format!("f/{}", trim_float(n)));
        }
        if let Some(t) = self.exposure_time {
            parts.push(if t >= 0.3 { format!("{} s", trim_float(t)) } else { format!("1/{:.0} s", 1.0 / t) });
        }
        if let Some(iso) = self.iso {
            parts.push(format!("ISO {iso:.0}"));
        }
        parts.join(" · ")
    }
}

fn trim_float(v: f64) -> String {
    let s = format!("{v:.1}");
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

/// A smooth per-image displacement field that removes residual misalignment (parallax, lens quirks)
/// left after global optimisation. Sampled bilinearly over the image; values are pixel offsets
/// added to the source position during rendering.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WarpField {
    pub cols: usize,
    pub rows: usize,
    /// `(cols × rows)` offsets in full-resolution pixels, row-major.
    pub dx: Vec<f32>,
    pub dy: Vec<f32>,
}

impl WarpField {
    #[inline]
    pub fn sample(&self, x: f32, y: f32, w: f32, h: f32) -> (f32, f32) {
        let gx = (x / w * (self.cols - 1) as f32).clamp(0.0, (self.cols - 1) as f32);
        let gy = (y / h * (self.rows - 1) as f32).clamp(0.0, (self.rows - 1) as f32);
        let (x0, y0) = ((gx as usize).min(self.cols - 2), (gy as usize).min(self.rows - 2));
        let (fx, fy) = (gx - x0 as f32, gy - y0 as f32);
        let i = y0 * self.cols + x0;
        let lerp = |v: &[f32]| {
            let top = v[i] + (v[i + 1] - v[i]) * fx;
            let bot = v[i + self.cols] + (v[i + self.cols + 1] - v[i + self.cols]) * fx;
            top + (bot - top) * fy
        };
        (lerp(&self.dx), lerp(&self.dy))
    }

    pub fn max_shift(&self) -> f32 {
        self.dx.iter().zip(&self.dy).map(|(x, y)| (x * x + y * y).sqrt()).fold(0.0, f32::max)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SourceImage {
    pub path: PathBuf,
    pub name: String,
    pub width: u32,
    pub height: u32,
    #[serde(default)]
    pub exif: ExifInfo,
    /// Index into [`Project::lenses`].
    pub lens: usize,
    /// Camera → world rotation as a unit quaternion `[x, y, z, w]`.
    pub rotation: [f64; 4],
    /// Whether alignment placed this image.
    #[serde(default)]
    pub aligned: bool,
    #[serde(default = "yes")]
    pub enabled: bool,
    /// Linear-light RGB gain from exposure and white-balance compensation.
    #[serde(default = "unit_gain")]
    pub gain: [f32; 3],
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warp: Option<WarpField>,
    /// Parts of the photo that must not appear in the panorama (people, the photographer's
    /// shadow, a tripod leg).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mask: Option<ImageMask>,
}

/// A low-resolution exclusion mask over a photo, stored as run lengths so projects stay small.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ImageMask {
    pub w: u32,
    pub h: u32,
    /// Row-major run lengths alternating keep / exclude, starting with keep.
    pub runs: Vec<u32>,
    #[serde(skip)]
    bits: std::sync::OnceLock<Vec<bool>>,
}

impl PartialEq for ImageMask {
    fn eq(&self, other: &Self) -> bool {
        self.w == other.w && self.h == other.h && self.runs == other.runs
    }
}

impl ImageMask {
    /// Builds a mask from `w × h` flags (true = exclude).
    pub fn from_bits(w: u32, h: u32, bits: &[bool]) -> ImageMask {
        let mut runs = Vec::new();
        let mut cur = false;
        let mut len = 0u32;
        for &b in bits {
            if b == cur {
                len += 1;
            } else {
                runs.push(len);
                cur = b;
                len = 1;
            }
        }
        runs.push(len);
        ImageMask { w, h, runs, bits: std::sync::OnceLock::new() }
    }

    pub fn bits(&self) -> &[bool] {
        self.bits.get_or_init(|| {
            let mut out = Vec::with_capacity((self.w * self.h) as usize);
            let mut v = false;
            for &r in &self.runs {
                out.extend(std::iter::repeat_n(v, r as usize));
                v = !v;
            }
            out.resize((self.w * self.h) as usize, false);
            out
        })
    }

    pub fn is_empty(&self) -> bool {
        self.runs.len() <= 1
    }

    /// Whether full-resolution pixel `(x, y)` of a `sw × sh` photo is excluded.
    #[inline]
    pub fn excluded(&self, x: f32, y: f32, sw: f32, sh: f32) -> bool {
        let mx = ((x / sw * self.w as f32) as i64).clamp(0, self.w as i64 - 1) as usize;
        let my = ((y / sh * self.h as f32) as i64).clamp(0, self.h as i64 - 1) as usize;
        self.bits()[my * self.w as usize + mx]
    }

    /// Fraction of the photo excluded.
    pub fn coverage(&self) -> f32 {
        let b = self.bits();
        b.iter().filter(|x| **x).count() as f32 / b.len().max(1) as f32
    }
}

/// How RAW files are developed. Every frame of a panorama uses the same settings so colours match.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Develop {
    /// White balance multipliers (R, G, B) for RAW files; `None` uses each file's as-shot balance.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub white_balance: Option<[f32; 3]>,
    /// Exposure adjustment in stops applied to the whole panorama.
    #[serde(default)]
    pub exposure: f32,
}

fn yes() -> bool {
    true
}

fn unit_gain() -> [f32; 3] {
    [1.0; 3]
}

impl SourceImage {
    pub fn rot(&self) -> Rotation3<f64> {
        let [x, y, z, w] = self.rotation;
        UnitQuaternion::from_quaternion(Quaternion::new(w, x, y, z)).to_rotation_matrix()
    }

    pub fn set_rot(&mut self, r: &Rotation3<f64>) {
        let q = UnitQuaternion::from_rotation_matrix(r);
        self.rotation = [q.i, q.j, q.k, q.w];
    }

    /// Where the optical axis points: (yaw, pitch) in degrees, plus roll about the axis.
    pub fn yaw_pitch_roll(&self) -> (f64, f64, f64) {
        let r = self.rot();
        let fwd: V3 = r * V3::z();
        let yaw = fwd.x.atan2(fwd.z);
        let pitch = fwd.y.clamp(-1.0, 1.0).asin();
        // Roll: angle of the camera's right vector against the horizon at that heading.
        let level_right = V3::new(yaw.cos(), 0.0, -yaw.sin());
        let level_up = fwd.cross(&level_right);
        let right: V3 = r * V3::x();
        let roll = right.dot(&level_up).atan2(right.dot(&level_right));
        (yaw.to_degrees(), pitch.to_degrees(), roll.to_degrees())
    }
}

/// Camera → world rotation for a camera looking at (yaw, pitch) with roll, in degrees.
pub fn rotation_from_ypr(yaw: f64, pitch: f64, roll: f64) -> Rotation3<f64> {
    let (y, p, r) = (yaw.to_radians(), pitch.to_radians(), roll.to_radians());
    // Yaw about +Y (turning right), then pitch about the camera's X (looking up), then roll about Z.
    Rotation3::from_axis_angle(&V3::y_axis(), y)
        * Rotation3::from_axis_angle(&V3::x_axis(), -p)
        * Rotation3::from_axis_angle(&V3::z_axis(), r)
}

/// A pair of pixels in two images that show the same scene point.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct ControlPoint {
    pub a: u32,
    pub b: u32,
    pub ax: f32,
    pub ay: f32,
    pub bx: f32,
    pub by: f32,
    /// Distance between the two projections after optimisation, in source pixels.
    #[serde(default)]
    pub error: f32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum OutputProjection {
    #[default]
    Equirectangular,
    Cylindrical,
    Rectilinear,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PoleFill {
    /// Fill voids: zenith/nadir gaps of full spheres and holes between photos.
    #[default]
    Auto,
    /// Leave voids empty (transparent in PNG/TIFF output).
    Off,
}

/// File format of the stitched panorama.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum OutputFormat {
    #[default]
    Jpeg,
    /// 16-bit TIFF (with alpha where there are voids).
    Tiff16,
    /// 16-bit PNG.
    Png16,
    /// 16-bit Photoshop (PSD, or PSB for very large panoramas).
    Psd16,
}

impl OutputFormat {
    pub fn extension(self) -> &'static str {
        match self {
            OutputFormat::Jpeg => "jpg",
            OutputFormat::Tiff16 => "tif",
            OutputFormat::Png16 => "png",
            OutputFormat::Psd16 => "psd",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            OutputFormat::Jpeg => "JPEG (8-bit)",
            OutputFormat::Tiff16 => "TIFF (16-bit)",
            OutputFormat::Png16 => "PNG (16-bit)",
            OutputFormat::Psd16 => "Photoshop (16-bit)",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct OutputSettings {
    #[serde(default)]
    pub projection: OutputProjection,
    /// Output width in pixels; 0 picks the native resolution of the sources.
    #[serde(default)]
    pub width: u32,
    #[serde(default = "yes")]
    pub exposure_compensation: bool,
    #[serde(default = "yes")]
    pub seam_optimization: bool,
    #[serde(default = "yes")]
    pub multiband: bool,
    #[serde(default = "yes")]
    pub local_alignment: bool,
    #[serde(default)]
    pub pole_fill: PoleFill,
    /// Extra nadir patch radius in degrees (e.g. to paint out a tripod); 0 = only fill real gaps.
    #[serde(default)]
    pub nadir_patch: f32,
    #[serde(default)]
    pub zenith_patch: f32,
    #[serde(default = "default_quality")]
    pub jpeg_quality: u8,
    /// Crop partial panoramas to the largest rectangle with no uncovered corners.
    #[serde(default = "yes")]
    pub trim_edges: bool,
    /// Correct lens vignetting (estimated from the overlaps).
    #[serde(default = "yes")]
    pub vignetting_correction: bool,
    #[serde(default)]
    pub format: OutputFormat,
}

fn default_quality() -> u8 {
    95
}

impl Default for OutputSettings {
    fn default() -> Self {
        OutputSettings {
            projection: OutputProjection::Equirectangular,
            width: 0,
            exposure_compensation: true,
            seam_optimization: true,
            multiband: true,
            local_alignment: true,
            pole_fill: PoleFill::Auto,
            nadir_patch: 0.0,
            zenith_patch: 0.0,
            jpeg_quality: 95,
            trim_edges: true,
            vignetting_correction: true,
            format: OutputFormat::Jpeg,
        }
    }
}

pub const PROJECT_VERSION: u32 = 1;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Project {
    pub version: u32,
    pub images: Vec<SourceImage>,
    pub lenses: Vec<Lens>,
    #[serde(default)]
    pub control_points: Vec<ControlPoint>,
    #[serde(default)]
    pub output: OutputSettings,
    #[serde(default)]
    pub develop: Develop,
}

impl Default for Project {
    fn default() -> Self {
        Project {
            version: PROJECT_VERSION,
            images: Vec::new(),
            lenses: Vec::new(),
            control_points: Vec::new(),
            output: OutputSettings::default(),
            develop: Develop::default(),
        }
    }
}

impl Project {
    pub fn lens_of(&self, image: usize) -> &Lens {
        &self.lenses[self.images[image].lens]
    }

    /// Images that take part in rendering.
    pub fn active(&self) -> impl Iterator<Item = usize> + '_ {
        (0..self.images.len()).filter(|&i| self.images[i].aligned && self.images[i].enabled)
    }

    /// Angular size of a source pixel (radians) for the sharpest active image; decides native output size.
    pub fn finest_pixel_angle(&self) -> f64 {
        self.active().map(|i| self.lens_of(i).model().pixel_angle()).fold(f64::INFINITY, f64::min)
    }

    /// Output width that preserves source detail for a 360° equirectangular panorama.
    pub fn native_width(&self) -> u32 {
        let a = self.finest_pixel_angle();
        if !a.is_finite() {
            return 4096;
        }
        let w = (std::f64::consts::TAU / a).round() as u32;
        (w.clamp(1024, 32768) / 64).max(1) * 64
    }

    /// Applies a world rotation to every image (re-centring or levelling the panorama).
    pub fn rotate_world(&mut self, g: &Rotation3<f64>) {
        for img in &mut self.images {
            let r = g * img.rot();
            img.set_rot(&r);
        }
    }

    pub fn default_lens_kind(&self) -> LensKind {
        self.lenses.first().map(|l| l.kind).unwrap_or_default()
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).expect("project serialises")
    }

    pub fn from_json(s: &str) -> anyhow::Result<Project> {
        let p: Project = serde_json::from_str(s)?;
        anyhow::ensure!(p.version <= PROJECT_VERSION, "project was saved by a newer version of Nodal");
        for img in &p.images {
            anyhow::ensure!(img.lens < p.lenses.len(), "project references a missing lens");
        }
        Ok(p)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ypr_round_trip() {
        for &(y, p, r) in &[(0.0, 0.0, 0.0), (40.0, 10.0, 0.0), (-120.0, -35.0, 12.0), (170.0, 60.0, -80.0)] {
            let mut img = SourceImage {
                path: PathBuf::new(),
                name: String::new(),
                width: 1,
                height: 1,
                exif: ExifInfo::default(),
                lens: 0,
                rotation: [0.0, 0.0, 0.0, 1.0],
                aligned: true,
                enabled: true,
                gain: [1.0; 3],
                warp: None,
                mask: None,
            };
            img.set_rot(&rotation_from_ypr(y, p, r));
            let (yy, pp, rr) = img.yaw_pitch_roll();
            assert!((yy - y).abs() < 1e-6 && (pp - p).abs() < 1e-6 && (rr - r).abs() < 1e-6, "{y} {p} {r} -> {yy} {pp} {rr}");
        }
    }

    #[test]
    fn yaw_turns_right_pitch_looks_up() {
        let r = rotation_from_ypr(90.0, 0.0, 0.0);
        assert!((r * V3::z() - V3::x()).norm() < 1e-9);
        let r = rotation_from_ypr(0.0, 90.0, 0.0);
        assert!((r * V3::z() - V3::y()).norm() < 1e-9);
    }
}
