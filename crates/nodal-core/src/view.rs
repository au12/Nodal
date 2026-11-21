use crate::projection::Projection;
use serde::{Deserialize, Serialize};

fn default_fov() -> f32 {
    90.0
}

/// A camera pose. Angles in degrees; `fov` spans the longer edge of the viewport.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct View {
    #[serde(default)]
    pub yaw: f32,
    #[serde(default)]
    pub pitch: f32,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub roll: f32,
    #[serde(default = "default_fov")]
    pub fov: f32,
    #[serde(default, skip_serializing_if = "is_rectilinear")]
    pub projection: Projection,
}

fn is_zero(v: &f32) -> bool {
    *v == 0.0
}

fn is_rectilinear(p: &Projection) -> bool {
    *p == Projection::RECTILINEAR
}

impl Default for View {
    fn default() -> Self {
        Self { yaw: 0.0, pitch: 0.0, roll: 0.0, fov: default_fov(), projection: Projection::RECTILINEAR }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct ViewLimits {
    #[serde(default = "d_min_fov")]
    pub min_fov: f32,
    #[serde(default = "d_max_fov")]
    pub max_fov: f32,
    #[serde(default = "d_min_pitch")]
    pub min_pitch: f32,
    #[serde(default = "d_max_pitch")]
    pub max_pitch: f32,
    /// Maximum magnification of the source pixels (1.0 = one source pixel per screen pixel).
    #[serde(default = "d_max_zoom")]
    pub max_pixel_zoom: f32,
}

fn d_min_fov() -> f32 {
    8.0
}
fn d_max_fov() -> f32 {
    120.0
}
fn d_min_pitch() -> f32 {
    -90.0
}
fn d_max_pitch() -> f32 {
    90.0
}
fn d_max_zoom() -> f32 {
    2.5
}

impl Default for ViewLimits {
    fn default() -> Self {
        Self {
            min_fov: d_min_fov(),
            max_fov: d_max_fov(),
            min_pitch: d_min_pitch(),
            max_pitch: d_max_pitch(),
            max_pixel_zoom: d_max_zoom(),
        }
    }
}

impl View {
    /// Clamps pitch and fov to the limits and the projection's own maximum.
    pub fn clamp(&mut self, limits: &ViewLimits, extra_min_fov: f32) {
        let proj_max = self.projection.max_fov();
        let max_fov = match self.projection.mode {
            crate::ProjMode::Perspective if self.projection.distortion < 0.05 => limits.max_fov.min(proj_max),
            _ => proj_max,
        };
        let min_fov = limits.min_fov.max(extra_min_fov).min(max_fov);
        self.fov = self.fov.clamp(min_fov, max_fov);
        self.pitch = self.pitch.clamp(limits.min_pitch, limits.max_pitch);
        self.yaw = crate::math::wrap_180(self.yaw);
    }
}
