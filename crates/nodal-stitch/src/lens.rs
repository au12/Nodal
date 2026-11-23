//! Lens models: ideal projection + radial distortion + principal-point shift.
//!
//! Camera frame: +X right, +Y up, +Z forward (the optical axis). Image pixels: x right, y down,
//! with pixel centres at half-integers (so the image centre is `(w/2, h/2)`).
//!
//! The radial polynomial follows the PanoTools convention so numbers look familiar to panorama
//! photographers: with `r` the *ideal* radius normalised by half the shorter image side,
//! `r_src = r · (a·r³ + b·r² + c·r + 1 − a − b − c)`. It maps ideal → actual, which is the cheap
//! direction for rendering (output direction → source pixel); the inverse uses Newton iterations.

use nalgebra::Vector3;
use serde::{Deserialize, Serialize};

pub type V3 = Vector3<f64>;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum LensKind {
    /// Ordinary lens: r = f·tan θ.
    #[default]
    Rectilinear,
    /// Generic fisheye (equidistant): r = f·θ. The radial polynomial absorbs other fisheye mappings.
    Fisheye,
    /// Equisolid-angle fisheye: r = 2f·sin(θ/2).
    FisheyeEquisolid,
    /// Stereographic fisheye: r = 2f·tan(θ/2).
    FisheyeStereographic,
}

impl LensKind {
    pub fn is_fisheye(self) -> bool {
        self != LensKind::Rectilinear
    }

    pub fn label(self) -> &'static str {
        match self {
            LensKind::Rectilinear => "Rectilinear",
            LensKind::Fisheye => "Fisheye",
            LensKind::FisheyeEquisolid => "Fisheye (equisolid)",
            LensKind::FisheyeStereographic => "Fisheye (stereographic)",
        }
    }

    /// The projection factor `k` of this family (see [`projection`]): 1 rectilinear, 0.5
    /// stereographic, 0 equidistant, −0.5 equisolid.
    pub fn default_k(self) -> f64 {
        match self {
            LensKind::Rectilinear => 1.0,
            LensKind::Fisheye => 0.0,
            LensKind::FisheyeEquisolid => -0.5,
            LensKind::FisheyeStereographic => 0.5,
        }
    }

    /// The named family closest to a projection factor.
    pub fn nearest(k: f64) -> LensKind {
        [LensKind::Rectilinear, LensKind::FisheyeStereographic, LensKind::Fisheye, LensKind::FisheyeEquisolid]
            .into_iter()
            .min_by(|a, b| (a.default_k() - k).abs().total_cmp(&(b.default_k() - k).abs()))
            .unwrap()
    }

    #[inline]
    fn g_inv(self, r: f64) -> f64 {
        projection::g_inv(self.default_k(), r)
    }

    /// Largest usable angle from the optical axis.
    #[inline]
    pub fn max_theta(self) -> f64 {
        projection::max_theta(self.default_k())
    }
}

/// One-parameter family of ideal lens projections (PTGui's "fisheye factor"):
/// `r = f·tan(kθ)/k` for k > 0, `r = f·θ` at k = 0 and `r = f·sin(kθ)/k` for k < 0. k = 1 is
/// rectilinear, 0.5 stereographic, 0 equidistant, −0.5 equisolid and −1 orthographic, and every
/// value in between is a valid lens, so the solver can fit the projection instead of guessing a
/// family.
pub mod projection {
    /// Ideal radius in focal lengths at angle `theta` from the axis.
    #[inline]
    pub fn g(k: f64, theta: f64) -> f64 {
        if k.abs() < 1e-6 {
            theta
        } else if k > 0.0 {
            (k * theta).tan() / k
        } else {
            (k * theta).sin() / k
        }
    }

    /// Angle from the axis for an ideal radius in focal lengths.
    #[inline]
    pub fn g_inv(k: f64, r: f64) -> f64 {
        if k.abs() < 1e-6 {
            r
        } else if k > 0.0 {
            (k * r).atan() / k
        } else {
            (k * r).clamp(-1.0, 1.0).asin() / k
        }
    }

    /// Derivative dg/dθ.
    #[inline]
    pub fn dg(k: f64, theta: f64) -> f64 {
        if k.abs() < 1e-6 {
            1.0
        } else if k > 0.0 {
            let c = (k * theta).cos();
            1.0 / (c * c)
        } else {
            (k * theta).cos()
        }
    }

    /// Largest usable angle: where the projection stops growing (or 175°).
    #[inline]
    pub fn max_theta(k: f64) -> f64 {
        let lim = if k.abs() < 1e-6 { f64::INFINITY } else { std::f64::consts::FRAC_PI_2 / k.abs() };
        // Rectilinear: 88°. Shrink a little below the asymptote everywhere.
        (lim - 2.0f64.to_radians() * k.abs().min(1.0)).min(175.0f64.to_radians())
    }
}

/// A lens (shared by every image shot with the same camera, lens and focal length).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Lens {
    pub kind: LensKind,
    /// Horizontal field of view in degrees across the stored image width (ideal, before distortion).
    pub hfov: f64,
    #[serde(default)]
    pub a: f64,
    #[serde(default)]
    pub b: f64,
    #[serde(default)]
    pub c: f64,
    /// Principal-point shift in pixels (x right, y down).
    #[serde(default)]
    pub d: f64,
    #[serde(default)]
    pub e: f64,
    /// Image size this lens describes, in pixels (EXIF orientation applied).
    pub width: u32,
    pub height: u32,
    /// Fitted projection factor (see [`projection`]); `None` uses the family's value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub k: Option<f64>,
    /// Vignetting: relative illumination `1 + v₁r² + v₂r⁴ + v₃r⁶`, with `r` the distance from the
    /// principal point over half the image diagonal.
    #[serde(default, skip_serializing_if = "is_zero3")]
    pub vignette: [f64; 3],
}

fn is_zero3(v: &[f64; 3]) -> bool {
    v.iter().all(|x| *x == 0.0)
}

impl Lens {
    pub fn new(kind: LensKind, hfov: f64, width: u32, height: u32) -> Self {
        Lens { kind, hfov, a: 0.0, b: 0.0, c: 0.0, d: 0.0, e: 0.0, width, height, k: None, vignette: [0.0; 3] }
    }

    /// Projection factor in use.
    pub fn k(&self) -> f64 {
        self.k.unwrap_or_else(|| self.kind.default_k())
    }

    /// Relative illumination at a pixel (1 at the centre).
    #[inline]
    pub fn vignetting_at(&self, x: f64, y: f64) -> f64 {
        let (w, h) = (self.width as f64, self.height as f64);
        let r2 = ((x - w * 0.5 - self.d).powi(2) + (y - h * 0.5 - self.e).powi(2)) / ((w * w + h * h) * 0.25);
        1.0 + r2 * (self.vignette[0] + r2 * (self.vignette[1] + r2 * self.vignette[2]))
    }

    /// Precomputes everything needed for fast projection.
    pub fn model(&self) -> LensModel {
        LensModel::new(self)
    }

    /// Focal length in pixels.
    pub fn focal_px(&self) -> f64 {
        let k = self.k();
        let half = (self.hfov.to_radians() * 0.5).min(projection::max_theta(k));
        self.width as f64 * 0.5 / projection::g(k, half)
    }

    /// Sets the field of view so the focal length becomes `f_px`.
    pub fn set_focal_px(&mut self, f_px: f64) {
        let k = self.k();
        self.hfov = (2.0 * projection::g_inv(k, self.width as f64 * 0.5 / f_px)).to_degrees();
    }

    /// 35 mm-equivalent focal length (by image diagonal), for display.
    pub fn focal_35mm(&self) -> f64 {
        let diag = ((self.width as f64).powi(2) + (self.height as f64).powi(2)).sqrt();
        self.focal_px() * 43.2666 / diag
    }

    /// Sets the field of view from a 35 mm-equivalent focal length.
    pub fn hfov_from_35mm(kind: LensKind, f35: f64, width: u32, height: u32) -> f64 {
        let diag = ((width as f64).powi(2) + (height as f64).powi(2)).sqrt();
        let f_px = f35 * diag / 43.2666;
        let half = kind.g_inv(width as f64 * 0.5 / f_px);
        (2.0 * half).to_degrees()
    }

    /// Whether the projection is a fisheye (anything well below rectilinear).
    pub fn is_fisheye(&self) -> bool {
        self.k() < 0.9
    }

    /// Field of view across the image diagonal in degrees (ideal projection).
    pub fn diagonal_fov(&self) -> f64 {
        let diag = ((self.width as f64).powi(2) + (self.height as f64).powi(2)).sqrt();
        (2.0 * projection::g_inv(self.k(), diag * 0.5 / self.focal_px()).min(projection::max_theta(self.k()))).to_degrees()
    }

    /// Display name of the projection: the family, or "k = …" for a fitted in-between value.
    pub fn projection_label(&self) -> String {
        match self.k {
            Some(k) if (k - self.kind.default_k()).abs() > 0.02 => format!("{} (k = {k:.2})", LensKind::nearest(k).label()),
            _ => self.kind.label().to_string(),
        }
    }
}

/// Precomputed lens for hot loops.
#[derive(Clone, Copy, Debug)]
pub struct LensModel {
    pub kind: LensKind,
    /// Projection factor.
    pub k: f64,
    pub f: f64,
    pub cx: f64,
    pub cy: f64,
    /// Normalisation radius (half the shorter side).
    pub r0: f64,
    pub a: f64,
    pub b: f64,
    pub c: f64,
    pub d0: f64,
    /// Largest normalised ideal radius for which the distortion polynomial is still monotonic.
    pub rho_max: f64,
    pub theta_max: f64,
    pub w: f64,
    pub h: f64,
}

impl LensModel {
    pub fn new(l: &Lens) -> Self {
        let (w, h) = (l.width as f64, l.height as f64);
        let r0 = w.min(h) * 0.5;
        let d0 = 1.0 - l.a - l.b - l.c;
        // Monotonic range: first positive root of d/dρ [ρ·poly(ρ)] = 4aρ³ + 3bρ² + 2cρ + d0.
        let mut rho_max = f64::INFINITY;
        if l.a != 0.0 || l.b != 0.0 || l.c != 0.0 {
            let deriv = |r: f64| 4.0 * l.a * r * r * r + 3.0 * l.b * r * r + 2.0 * l.c * r + d0;
            let mut prev = 0.0;
            let limit = 8.0;
            let steps = 400;
            for i in 1..=steps {
                let r = limit * i as f64 / steps as f64;
                if deriv(r) <= 0.0 {
                    // Bisect between prev and r.
                    let (mut lo, mut hi) = (prev, r);
                    for _ in 0..30 {
                        let mid = 0.5 * (lo + hi);
                        if deriv(mid) > 0.0 {
                            lo = mid
                        } else {
                            hi = mid
                        }
                    }
                    rho_max = lo * 0.999;
                    break;
                }
                prev = r;
            }
        }
        LensModel {
            kind: l.kind,
            k: l.k(),
            f: l.focal_px(),
            cx: w * 0.5 + l.d,
            cy: h * 0.5 + l.e,
            r0,
            a: l.a,
            b: l.b,
            c: l.c,
            d0,
            rho_max,
            theta_max: projection::max_theta(l.k()),
            w,
            h,
        }
    }

    /// Camera-frame ray → source pixel. `None` when the ray is outside the lens's usable field.
    /// The pixel may still be outside the image rectangle; check with [`LensModel::contains`].
    #[inline]
    pub fn ray_to_pixel(&self, ray: V3) -> Option<(f64, f64)> {
        let rxy = (ray.x * ray.x + ray.y * ray.y).sqrt();
        let theta = rxy.atan2(ray.z);
        if theta > self.theta_max {
            return None;
        }
        if rxy < 1e-12 {
            return Some((self.cx, self.cy));
        }
        let rho = self.f * projection::g(self.k, theta) / self.r0;
        if rho > self.rho_max {
            return None;
        }
        let r_src = rho * (((self.a * rho + self.b) * rho + self.c) * rho + self.d0) * self.r0;
        let s = r_src / rxy;
        Some((self.cx + ray.x * s, self.cy - ray.y * s))
    }

    /// Source pixel → unit ray in the camera frame.
    #[inline]
    pub fn pixel_to_ray(&self, x: f64, y: f64) -> V3 {
        let dx = x - self.cx;
        let dy = y - self.cy;
        let r_src = (dx * dx + dy * dy).sqrt();
        if r_src < 1e-9 {
            return V3::new(0.0, 0.0, 1.0);
        }
        let target = r_src / self.r0;
        // Newton: solve rho·poly(rho) = target.
        let mut rho = target;
        if self.a != 0.0 || self.b != 0.0 || self.c != 0.0 {
            for _ in 0..12 {
                let p = ((self.a * rho + self.b) * rho + self.c) * rho + self.d0;
                let fv = rho * p - target;
                let dv = ((4.0 * self.a * rho + 3.0 * self.b) * rho + 2.0 * self.c) * rho + self.d0;
                if dv.abs() < 1e-9 {
                    break;
                }
                let step = fv / dv;
                rho -= step;
                if step.abs() < 1e-12 {
                    break;
                }
            }
            rho = rho.clamp(0.0, self.rho_max.min(1e6));
        }
        let theta = projection::g_inv(self.k, rho * self.r0 / self.f).min(self.theta_max);
        let (st, ct) = theta.sin_cos();
        V3::new(st * dx / r_src, -st * dy / r_src, ct)
    }

    #[inline]
    pub fn contains(&self, x: f64, y: f64) -> bool {
        x >= 0.0 && y >= 0.0 && x <= self.w && y <= self.h
    }

    /// Whether the projection is a fisheye.
    #[inline]
    pub fn is_fisheye(&self) -> bool {
        self.k < 0.9
    }

    /// Angular size of one pixel at the image centre, in radians.
    pub fn pixel_angle(&self) -> f64 {
        1.0 / (self.f * self.d0.max(0.2))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_all_kinds() {
        for kind in [LensKind::Rectilinear, LensKind::Fisheye, LensKind::FisheyeEquisolid, LensKind::FisheyeStereographic] {
            let mut lens = Lens::new(kind, if kind.is_fisheye() { 170.0 } else { 75.0 }, 4000, 3000);
            lens.a = 0.01;
            lens.b = -0.04;
            lens.c = 0.02;
            lens.d = 12.0;
            lens.e = -7.0;
            let m = lens.model();
            for &(x, y) in &[(10.0, 20.0), (2000.0, 1500.0), (3990.0, 2990.0), (500.0, 2500.0), (2012.0, 1493.0)] {
                let ray = m.pixel_to_ray(x, y);
                assert!((ray.norm() - 1.0).abs() < 1e-9);
                let (px, py) = m.ray_to_pixel(ray).unwrap();
                assert!((px - x).abs() < 1e-5 && (py - y).abs() < 1e-5, "{kind:?} ({x},{y}) -> ({px},{py})");
            }
        }
    }

    #[test]
    fn focal_conversions() {
        // A 24 mm lens on full frame sees 73.7° horizontally.
        let hfov = Lens::hfov_from_35mm(LensKind::Rectilinear, 24.0, 6000, 4000);
        assert!((hfov - 73.74).abs() < 0.05, "{hfov}");
        let lens = Lens::new(LensKind::Rectilinear, hfov, 6000, 4000);
        assert!((lens.focal_35mm() - 24.0).abs() < 1e-6);
        // Edge of frame maps to half the field of view.
        let m = lens.model();
        let ray = m.pixel_to_ray(6000.0, 2000.0);
        assert!((ray.x.atan2(ray.z).to_degrees() - hfov / 2.0).abs() < 1e-6);
    }

    #[test]
    fn axes_follow_conventions() {
        let m = Lens::new(LensKind::Rectilinear, 60.0, 1000, 800).model();
        assert!(m.pixel_to_ray(900.0, 400.0).x > 0.0); // right of centre → +X
        assert!(m.pixel_to_ray(500.0, 100.0).y > 0.0); // above centre → +Y
    }
}
