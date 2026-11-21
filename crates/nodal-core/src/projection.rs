//! Screen projections.
//!
//! Every projection here is evaluated per pixel on the GPU (see `nodal-render/src/pano.wgsl`)
//! and mirrored on the CPU for picking, hotspot placement and tile visibility. Keep both in sync.
//!
//! Screen coordinates are normalised by half of the *larger* viewport dimension, so the
//! field of view always describes the long edge and rotating a phone keeps the scene scale.

use crate::math::{camera_basis, dir_from_yaw_pitch, smoothstep, wrap_pi, yaw_pitch_from_dir, DEG};
use crate::view::View;
use glam::{Mat3, Vec2, Vec3};
use serde::{Deserialize, Serialize};
use std::f32::consts::{FRAC_PI_2, PI};

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Default, Hash)]
#[serde(rename_all = "snake_case")]
pub enum ProjMode {
    /// General perspective family: rectilinear (`distortion = 0`) morphing into
    /// stereographic (`distortion = 1`, "little planet" when looking down).
    #[default]
    Perspective,
    /// General Pannini: straight verticals, compressed horizontals. Best for wide interiors.
    Pannini,
    /// Flat longitude/latitude overview.
    Equirect,
}

#[derive(Copy, Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Projection {
    #[serde(default)]
    pub mode: ProjMode,
    /// Perspective: 0 rectilinear … 1 stereographic. Pannini: 0 rectilinear … 1 classic Pannini.
    #[serde(default)]
    pub distortion: f32,
    /// 0..1 — keeps verticals vertical when pitching (perspective mode, fades out near the poles).
    #[serde(default)]
    pub architectural: f32,
}

impl Default for Projection {
    fn default() -> Self {
        Self::RECTILINEAR
    }
}

impl Projection {
    pub const RECTILINEAR: Self = Self { mode: ProjMode::Perspective, distortion: 0.0, architectural: 0.0 };
    pub const FISHEYE: Self = Self { mode: ProjMode::Perspective, distortion: 0.55, architectural: 0.0 };
    pub const STEREOGRAPHIC: Self = Self { mode: ProjMode::Perspective, distortion: 1.0, architectural: 0.0 };
    pub const PANNINI: Self = Self { mode: ProjMode::Pannini, distortion: 1.0, architectural: 0.0 };
    pub const EQUIRECT: Self = Self { mode: ProjMode::Equirect, distortion: 0.0, architectural: 0.0 };

    /// Largest usable field of view in degrees for this projection.
    pub fn max_fov(&self) -> f32 {
        match self.mode {
            ProjMode::Perspective | ProjMode::Pannini => {
                let d = self.distortion.clamp(0.0, 1.0);
                let theta_max = (-d).acos() / DEG; // asymptote of the projection
                (2.0 * theta_max - 6.0).min(if self.mode == ProjMode::Pannini { 300.0 } else { 340.0 })
            }
            ProjMode::Equirect => 360.0,
        }
    }

    /// Smoothly interpolates two projections (used for animated projection changes).
    /// Mixing across modes snaps at the midpoint.
    pub fn lerp(&self, other: &Projection, t: f32) -> Projection {
        let mode = if self.mode == other.mode || t < 0.5 { self.mode } else { other.mode };
        let pick = |a: &Projection, b: &Projection, f: fn(&Projection) -> f32| {
            if a.mode == b.mode {
                f(a) + (f(b) - f(a)) * t
            } else if t < 0.5 {
                f(a)
            } else {
                f(b)
            }
        };
        Projection {
            mode,
            distortion: pick(self, other, |p| p.distortion),
            architectural: self.architectural + (other.architectural - self.architectural) * t,
        }
    }

    pub fn label(&self) -> &'static str {
        match self.mode {
            ProjMode::Equirect => "Equirectangular",
            ProjMode::Pannini => "Pannini",
            ProjMode::Perspective if self.distortion < 0.05 => "Rectilinear",
            ProjMode::Perspective if self.distortion > 0.95 => "Stereographic",
            ProjMode::Perspective => "Fisheye",
        }
    }
}

/// Projected radius of an off-axis angle θ for the general perspective projection.
#[inline]
pub fn persp_radius(theta: f32, d: f32) -> f32 {
    (d + 1.0) * theta.sin() / (d + theta.cos())
}

/// Inverse of [`persp_radius`]: angle for a projected radius.
#[inline]
pub fn persp_theta(r: f32, d: f32) -> f32 {
    let s = d + 1.0;
    r.atan2(s) + ((r * d) / (s * s + r * r).sqrt()).clamp(-1.0, 1.0).asin()
}

/// Per-frame projection state: maps screen pixels to world directions and back.
#[derive(Clone, Copy, Debug)]
pub struct Projector {
    pub width: f32,
    pub height: f32,
    /// Half of the larger viewport dimension in pixels.
    pub half: f32,
    pub mode: ProjMode,
    pub d: f32,
    /// Plane-units at normalised screen radius 1.
    pub k: f32,
    /// Architectural lens shift in plane units.
    pub shift: f32,
    pub basis: Mat3,
    pub yaw: f32,
    pub pitch: f32,
    pub half_fov: f32,
}

impl Projector {
    pub fn new(view: &View, width: f32, height: f32) -> Self {
        let width = width.max(1.0);
        let height = height.max(1.0);
        let proj = view.projection;
        let max_fov = proj.max_fov();
        let half_fov = (view.fov.clamp(0.01, max_fov) * DEG) * 0.5;
        let yaw = view.yaw * DEG;
        let pitch = view.pitch.clamp(-90.0, 90.0) * DEG;
        let roll = view.roll * DEG;
        let d = proj.distortion.clamp(0.0, 1.0);
        let (k, shift, rot_pitch) = match proj.mode {
            ProjMode::Perspective => {
                let a = proj.architectural.clamp(0.0, 1.0) * smoothstep(80.0 * DEG, 45.0 * DEG, pitch.abs());
                let residual = pitch * a;
                (persp_radius(half_fov, d), persp_radius(residual, d), pitch - residual)
            }
            ProjMode::Pannini => {
                let k = if width >= height { persp_radius(half_fov, d) } else { half_fov.min(88.0 * DEG).tan() };
                (k, 0.0, pitch)
            }
            ProjMode::Equirect => (1.0, 0.0, pitch),
        };
        Self {
            width,
            height,
            half: width.max(height) * 0.5,
            mode: proj.mode,
            d,
            k,
            shift,
            basis: camera_basis(yaw, rot_pitch, roll),
            yaw,
            pitch,
            half_fov,
        }
    }

    #[inline]
    pub fn normalized(&self, p: Vec2) -> Vec2 {
        Vec2::new((p.x - self.width * 0.5) / self.half, -(p.y - self.height * 0.5) / self.half)
    }

    #[inline]
    pub fn denormalized(&self, n: Vec2) -> Vec2 {
        Vec2::new(self.width * 0.5 + n.x * self.half, self.height * 0.5 - n.y * self.half)
    }

    /// World direction (unit) seen at pixel `p`, or `None` outside the projection's domain.
    pub fn screen_to_dir(&self, p: Vec2) -> Option<Vec3> {
        let n = self.normalized(p);
        match self.mode {
            ProjMode::Equirect => {
                let lat = n.y * self.half_fov + self.pitch;
                if lat.abs() > FRAC_PI_2 {
                    return None;
                }
                Some(dir_from_yaw_pitch(self.yaw + n.x * self.half_fov, lat))
            }
            ProjMode::Perspective => {
                let q = Vec2::new(n.x * self.k, n.y * self.k + self.shift);
                let r = q.length();
                let ray = if r < 1e-9 {
                    Vec3::Z
                } else {
                    let theta = persp_theta(r, self.d);
                    let (st, ct) = theta.sin_cos();
                    Vec3::new(st * q.x / r, st * q.y / r, ct)
                };
                Some(self.basis * ray)
            }
            ProjMode::Pannini => {
                let q = n * self.k;
                let phi = persp_theta(q.x, self.d);
                let s = (self.d + 1.0) / (self.d + phi.cos());
                let t = q.y / s;
                Some(self.basis * Vec3::new(phi.sin(), t, phi.cos()).normalize())
            }
        }
    }

    /// Pixel position of a world direction, or `None` if it is not visible in this projection.
    pub fn dir_to_screen(&self, dir: Vec3) -> Option<Vec2> {
        let n = match self.mode {
            ProjMode::Equirect => {
                let (y, p) = yaw_pitch_from_dir(dir);
                Vec2::new(wrap_pi(y - self.yaw) / self.half_fov, (p - self.pitch) / self.half_fov)
            }
            ProjMode::Perspective => {
                let v = (self.basis.transpose() * dir).normalize();
                let ct = v.z;
                if self.d + ct <= 1e-4 {
                    return None;
                }
                let st = Vec2::new(v.x, v.y).length();
                let q = if st < 1e-7 { Vec2::ZERO } else { Vec2::new(v.x, v.y) / st * ((self.d + 1.0) * st / (self.d + ct)) };
                Vec2::new(q.x / self.k, (q.y - self.shift) / self.k)
            }
            ProjMode::Pannini => {
                let v = self.basis.transpose() * dir;
                let horiz = (v.x * v.x + v.z * v.z).sqrt();
                if horiz < 1e-6 {
                    return None;
                }
                let cphi = v.z / horiz;
                if self.d + cphi <= 1e-4 {
                    return None;
                }
                let s = (self.d + 1.0) / (self.d + cphi);
                Vec2::new(s * (v.x / horiz) / self.k, s * (v.y / horiz) / self.k)
            }
        };
        if !n.is_finite() {
            return None;
        }
        Some(self.denormalized(n))
    }

    /// Whether a direction is in front of the viewer for this projection (cheap check).
    pub fn is_visible(&self, dir: Vec3) -> bool {
        match self.dir_to_screen(dir) {
            Some(p) => p.x >= -1.0 && p.y >= -1.0 && p.x <= self.width + 1.0 && p.y <= self.height + 1.0,
            None => false,
        }
    }

    /// Approximate angular size (radians) of one pixel at screen position `p`.
    pub fn pixel_angle(&self, p: Vec2) -> Option<f32> {
        let d0 = self.screen_to_dir(p)?;
        let dx = self.screen_to_dir(p + Vec2::X)?;
        let dy = self.screen_to_dir(p + Vec2::Y)?;
        Some(d0.angle_between(dx).max(d0.angle_between(dy)))
    }

    /// The world direction at the centre of the screen.
    pub fn center_dir(&self) -> Vec3 {
        self.screen_to_dir(Vec2::new(self.width * 0.5, self.height * 0.5)).unwrap_or(Vec3::Z)
    }

    /// Angular radius (radians) beyond which nothing can be visible. Used for clipping overlays.
    pub fn visible_cone(&self) -> f32 {
        match self.mode {
            ProjMode::Equirect => PI,
            ProjMode::Perspective | ProjMode::Pannini => (-self.d).acos(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn views() -> Vec<View> {
        let mut v = vec![];
        for proj in [
            Projection::RECTILINEAR,
            Projection::FISHEYE,
            Projection::STEREOGRAPHIC,
            Projection::PANNINI,
            Projection { architectural: 1.0, ..Projection::RECTILINEAR },
        ] {
            for (yaw, pitch, fov) in [(0.0, 0.0, 90.0), (37.0, 25.0, 70.0), (-120.0, -60.0, 110.0)] {
                v.push(View { yaw, pitch, roll: 0.0, fov, projection: proj });
            }
        }
        v.push(View { yaw: 10.0, pitch: -90.0, roll: 0.0, fov: 280.0, projection: Projection::STEREOGRAPHIC });
        v.push(View { yaw: 10.0, pitch: 20.0, roll: 0.0, fov: 200.0, projection: Projection::EQUIRECT });
        v
    }

    #[test]
    fn screen_dir_roundtrip() {
        for view in views() {
            for (w, h) in [(1600.0, 900.0), (390.0, 844.0)] {
                let p = Projector::new(&view, w, h);
                for i in 0..9 {
                    for j in 0..9 {
                        let px = Vec2::new(w * (0.05 + 0.9 * i as f32 / 8.0), h * (0.05 + 0.9 * j as f32 / 8.0));
                        let Some(d) = p.screen_to_dir(px) else { continue };
                        assert!((d.length() - 1.0).abs() < 1e-4);
                        let back = p.dir_to_screen(d).expect("visible");
                        assert!((back - px).length() < 0.05, "{view:?} {w}x{h} {px:?} -> {back:?}");
                    }
                }
            }
        }
    }

    #[test]
    fn centre_looks_at_view_direction() {
        for view in views() {
            if view.projection.mode == ProjMode::Equirect {
                continue;
            }
            let p = Projector::new(&view, 800.0, 600.0);
            let c = p.center_dir();
            let want = dir_from_yaw_pitch(view.yaw * DEG, view.pitch * DEG);
            assert!(c.angle_between(want) < 1e-3, "{view:?}");
        }
    }

    #[test]
    fn fov_spans_long_edge() {
        let view = View { fov: 90.0, ..View::default() };
        let p = Projector::new(&view, 1000.0, 500.0);
        let l = p.screen_to_dir(Vec2::new(0.0, 250.0)).unwrap();
        let r = p.screen_to_dir(Vec2::new(1000.0, 250.0)).unwrap();
        assert!((l.angle_between(r) / DEG - 90.0).abs() < 0.01);
        let p = Projector::new(&view, 500.0, 1000.0);
        let t = p.screen_to_dir(Vec2::new(250.0, 0.0)).unwrap();
        let b = p.screen_to_dir(Vec2::new(250.0, 1000.0)).unwrap();
        assert!((t.angle_between(b) / DEG - 90.0).abs() < 0.01);
    }

    #[test]
    fn architectural_keeps_verticals_vertical() {
        let view =
            View { pitch: 30.0, fov: 90.0, projection: Projection { architectural: 1.0, ..Projection::RECTILINEAR }, ..View::default() };
        let p = Projector::new(&view, 800.0, 800.0);
        // Two points on the same world vertical line must share the same screen x.
        let a = p.dir_to_screen(dir_from_yaw_pitch(20.0 * DEG, 10.0 * DEG)).unwrap();
        let b = p.dir_to_screen(dir_from_yaw_pitch(20.0 * DEG, 40.0 * DEG)).unwrap();
        assert!((a.x - b.x).abs() < 0.01, "{a:?} {b:?}");
    }

    #[test]
    fn rectilinear_hides_behind() {
        let p = Projector::new(&View::default(), 800.0, 600.0);
        assert!(p.dir_to_screen(-Vec3::Z).is_none());
        let s = Projector::new(&View { projection: Projection::STEREOGRAPHIC, fov: 300.0, ..View::default() }, 800.0, 600.0);
        assert!(s.dir_to_screen(dir_from_yaw_pitch(150.0 * DEG, 0.0)).is_some());
    }
}
