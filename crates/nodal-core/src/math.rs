use glam::{Mat3, Vec3};
use std::f32::consts::{PI, TAU};

pub const DEG: f32 = PI / 180.0;

/// Unit direction for a yaw/pitch pair (radians).
#[inline]
pub fn dir_from_yaw_pitch(yaw: f32, pitch: f32) -> Vec3 {
    let (sy, cy) = yaw.sin_cos();
    let (sp, cp) = pitch.sin_cos();
    Vec3::new(sy * cp, sp, cy * cp)
}

/// Yaw/pitch (radians) of a direction. The direction does not need to be normalised.
#[inline]
pub fn yaw_pitch_from_dir(d: Vec3) -> (f32, f32) {
    let horiz = (d.x * d.x + d.z * d.z).sqrt();
    (d.x.atan2(d.z), d.y.atan2(horiz))
}

/// Camera basis as a matrix whose columns are `right`, `up`, `forward` in world space.
/// Multiplying a view-space ray by it gives the world direction.
pub fn camera_basis(yaw: f32, pitch: f32, roll: f32) -> Mat3 {
    let forward = dir_from_yaw_pitch(yaw, pitch);
    let right0 = Vec3::new(yaw.cos(), 0.0, -yaw.sin());
    let up0 = forward.cross(right0);
    let (sr, cr) = roll.sin_cos();
    let right = right0 * cr + up0 * sr;
    let up = up0 * cr - right0 * sr;
    Mat3::from_cols(right, up, forward)
}

/// Wraps an angle into `(-π, π]`.
#[inline]
pub fn wrap_pi(a: f32) -> f32 {
    let mut x = (a + PI).rem_euclid(TAU) - PI;
    if x <= -PI {
        x += TAU;
    }
    x
}

/// Wraps degrees into `(-180, 180]`.
#[inline]
pub fn wrap_180(a: f32) -> f32 {
    wrap_pi(a * DEG) / DEG
}

/// Signed shortest difference `to - from` for angles in radians.
#[inline]
pub fn angle_delta(from: f32, to: f32) -> f32 {
    wrap_pi(to - from)
}

#[inline]
pub fn smoothstep(e0: f32, e1: f32, x: f32) -> f32 {
    let t = ((x - e0) / (e1 - e0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// Spherical linear interpolation between unit vectors.
pub fn slerp(a: Vec3, b: Vec3, t: f32) -> Vec3 {
    let dot = a.dot(b).clamp(-1.0, 1.0);
    let theta = dot.acos();
    if theta < 1e-5 {
        return a.lerp(b, t).normalize();
    }
    let s = theta.sin();
    if s.abs() < 1e-5 {
        // Antipodal: pick any orthogonal path.
        let ortho = if a.y.abs() < 0.9 { Vec3::Y } else { Vec3::X };
        let axis = a.cross(ortho).normalize();
        return (a * (t * PI).cos() + axis * (t * PI).sin()).normalize();
    }
    (a * ((1.0 - t) * theta).sin() / s + b * (t * theta).sin() / s).normalize()
}

/// Easing used for camera animations.
#[inline]
pub fn ease_in_out_cubic(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    if t < 0.5 {
        4.0 * t * t * t
    } else {
        1.0 - (-2.0 * t + 2.0).powi(3) / 2.0
    }
}

#[inline]
pub fn ease_out_cubic(t: f32) -> f32 {
    1.0 - (1.0 - t.clamp(0.0, 1.0)).powi(3)
}

/// sRGB transfer helpers (used by the tiler and the CPU reference renderer).
#[inline]
pub fn srgb_to_linear(c: f32) -> f32 {
    if c <= 0.04045 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

#[inline]
pub fn linear_to_srgb(c: f32) -> f32 {
    if c <= 0.0031308 {
        c * 12.92
    } else {
        1.055 * c.powf(1.0 / 2.4) - 0.055
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn yaw_pitch_roundtrip() {
        for yaw in [-170.0f32, -90.0, 0.0, 33.0, 179.0] {
            for pitch in [-89.0f32, -30.0, 0.0, 45.0, 89.0] {
                let d = dir_from_yaw_pitch(yaw * DEG, pitch * DEG);
                let (y, p) = yaw_pitch_from_dir(d);
                assert!((wrap_pi(y - yaw * DEG)).abs() < 1e-4, "yaw {yaw} {pitch}");
                assert!((p - pitch * DEG).abs() < 1e-4);
            }
        }
    }

    #[test]
    fn basis_is_orthonormal_and_forward_matches() {
        let m = camera_basis(0.7, -0.3, 0.2);
        let (r, u, f) = (m.x_axis, m.y_axis, m.z_axis);
        assert!((r.length() - 1.0).abs() < 1e-5);
        assert!((u.length() - 1.0).abs() < 1e-5);
        assert!(r.dot(u).abs() < 1e-5 && r.dot(f).abs() < 1e-5 && u.dot(f).abs() < 1e-5);
        assert!((f - dir_from_yaw_pitch(0.7, -0.3)).length() < 1e-5);
        // Looking straight ahead, right is +X and up is +Y.
        let m = camera_basis(0.0, 0.0, 0.0);
        assert!((m.x_axis - Vec3::X).length() < 1e-6);
        assert!((m.y_axis - Vec3::Y).length() < 1e-6);
    }

    #[test]
    fn wrap() {
        assert!((wrap_180(190.0) + 170.0).abs() < 1e-3);
        assert!((wrap_180(-190.0) - 170.0).abs() < 1e-3);
        assert!((angle_delta(170.0 * DEG, -170.0 * DEG) - 20.0 * DEG).abs() < 1e-4);
    }
}
