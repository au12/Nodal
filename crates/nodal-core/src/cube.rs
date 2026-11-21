use glam::{Vec2, Vec3};

/// The six cube faces. The discriminant is the index used in URLs, GPU layers and the page table.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(u8)]
pub enum Face {
    Front = 0,
    Right = 1,
    Back = 2,
    Left = 3,
    Up = 4,
    Down = 5,
}

impl Face {
    pub const ALL: [Face; 6] = [Face::Front, Face::Right, Face::Back, Face::Left, Face::Up, Face::Down];

    #[inline]
    pub fn from_index(i: usize) -> Face {
        Face::ALL[i]
    }

    #[inline]
    pub fn index(self) -> usize {
        self as usize
    }

    /// Single letter used in tile URLs (`{f}` placeholder).
    pub fn letter(self) -> char {
        ['f', 'r', 'b', 'l', 'u', 'd'][self as usize]
    }

    pub fn name(self) -> &'static str {
        ["front", "right", "back", "left", "up", "down"][self as usize]
    }

    /// `(centre, right axis, down axis)` in world space. A point on the face with
    /// face coordinates `u, v ∈ [0,1]` lies in direction `c + (2u-1)·r + (2v-1)·d`.
    #[inline]
    pub fn basis(self) -> (Vec3, Vec3, Vec3) {
        match self {
            Face::Front => (Vec3::Z, Vec3::X, Vec3::NEG_Y),
            Face::Right => (Vec3::X, Vec3::NEG_Z, Vec3::NEG_Y),
            Face::Back => (Vec3::NEG_Z, Vec3::NEG_X, Vec3::NEG_Y),
            Face::Left => (Vec3::NEG_X, Vec3::Z, Vec3::NEG_Y),
            Face::Up => (Vec3::Y, Vec3::X, Vec3::Z),
            Face::Down => (Vec3::NEG_Y, Vec3::X, Vec3::NEG_Z),
        }
    }
}

/// Picks the face for a direction. Must stay in sync with `face_of` in the WGSL shader.
#[inline]
pub fn face_of(d: Vec3) -> Face {
    let a = d.abs();
    if a.x >= a.y && a.x >= a.z {
        if d.x > 0.0 {
            Face::Right
        } else {
            Face::Left
        }
    } else if a.y >= a.z {
        if d.y > 0.0 {
            Face::Up
        } else {
            Face::Down
        }
    } else if d.z > 0.0 {
        Face::Front
    } else {
        Face::Back
    }
}

/// Face and face coordinates (`[0,1]²`) for a direction.
#[inline]
pub fn dir_to_face_uv(d: Vec3) -> (Face, Vec2) {
    let face = face_of(d);
    (face, face_uv(face, d))
}

/// Coordinates of `d` projected onto the plane of `face`. Values outside `[0,1]`
/// are valid and continue the face plane (used for tile borders).
#[inline]
pub fn face_uv(face: Face, d: Vec3) -> Vec2 {
    let (c, r, dn) = face.basis();
    let p = d / d.dot(c);
    Vec2::new((p.dot(r) + 1.0) * 0.5, (p.dot(dn) + 1.0) * 0.5)
}

/// Unit direction for face coordinates. Coordinates outside `[0,1]` extend the face plane.
#[inline]
pub fn face_uv_to_dir(face: Face, uv: Vec2) -> Vec3 {
    let (c, r, dn) = face.basis();
    (c + r * (2.0 * uv.x - 1.0) + dn * (2.0 * uv.y - 1.0)).normalize()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::math::{dir_from_yaw_pitch, DEG};

    #[test]
    fn roundtrip_all_faces() {
        for f in Face::ALL {
            for (u, v) in [(0.5, 0.5), (0.1, 0.9), (0.99, 0.01), (0.3, 0.6)] {
                let d = face_uv_to_dir(f, Vec2::new(u, v));
                let (f2, uv) = dir_to_face_uv(d);
                assert_eq!(f, f2, "face {:?} {u} {v}", f);
                assert!((uv - Vec2::new(u, v)).length() < 1e-5);
            }
        }
    }

    #[test]
    fn orientation_conventions() {
        // Yaw 90 looks at the right face centre.
        let (f, uv) = dir_to_face_uv(dir_from_yaw_pitch(90.0 * DEG, 0.0));
        assert_eq!(f, Face::Right);
        assert!((uv - Vec2::splat(0.5)).length() < 1e-5);
        // Slightly right of straight ahead is u > 0.5 on the front face.
        let (_, uv) = dir_to_face_uv(dir_from_yaw_pitch(10.0 * DEG, 10.0 * DEG));
        assert!(uv.x > 0.5 && uv.y < 0.5);
        // Looking up, "forward" is at the bottom of the up face.
        let (f, uv) = dir_to_face_uv(dir_from_yaw_pitch(0.0, 80.0 * DEG));
        assert_eq!(f, Face::Up);
        assert!(uv.y > 0.5 && (uv.x - 0.5).abs() < 1e-5);
        // Looking down, "forward" is at the top of the down face.
        let (f, uv) = dir_to_face_uv(dir_from_yaw_pitch(0.0, -80.0 * DEG));
        assert_eq!(f, Face::Down);
        assert!(uv.y < 0.5);
    }

    #[test]
    fn borders_continue_into_neighbours() {
        // Just beyond the right edge of the front face is the left edge of the right face.
        let d = face_uv_to_dir(Face::Front, Vec2::new(1.01, 0.5));
        let (f, uv) = dir_to_face_uv(d);
        assert_eq!(f, Face::Right);
        assert!(uv.x < 0.02);
    }
}
