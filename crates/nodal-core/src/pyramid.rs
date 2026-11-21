//! Multi-resolution cube tile pyramid.
//!
//! Level `L` has `2^L × 2^L` tiles per face, each `tile_size` content pixels plus a
//! `border` of real neighbouring pixels on every side (so tiles are stored as
//! `slot_size = tile_size + 2·border` squares). Borders are rendered by continuing the
//! face plane past its edge, which makes bilinear filtering seamless across tile *and*
//! cube-face boundaries without any special casing in the shader.

use crate::cube::Face;
use serde::{Deserialize, Serialize};

fn one() -> u32 {
    1
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Pyramid {
    /// Content pixels per tile edge (excluding borders).
    pub tile_size: u32,
    #[serde(default = "one")]
    pub border: u32,
    /// Number of levels, ≥ 1. Level 0 is one tile per face.
    pub levels: u32,
    /// URL template relative to the tour file. Placeholders: `{f}` face letter, `{l}` level, `{x}`, `{y}`.
    pub url: String,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TileId {
    pub face: u8,
    pub level: u8,
    pub x: u16,
    pub y: u16,
}

impl TileId {
    pub fn new(face: Face, level: u32, x: u32, y: u32) -> Self {
        Self { face: face as u8, level: level as u8, x: x as u16, y: y as u16 }
    }

    pub fn face(&self) -> Face {
        Face::from_index(self.face as usize)
    }

    pub fn parent(&self) -> Option<TileId> {
        (self.level > 0).then(|| TileId { face: self.face, level: self.level - 1, x: self.x / 2, y: self.y / 2 })
    }

    pub fn children(&self) -> [TileId; 4] {
        let (x, y, l) = (self.x * 2, self.y * 2, self.level + 1);
        [
            TileId { face: self.face, level: l, x, y },
            TileId { face: self.face, level: l, x: x + 1, y },
            TileId { face: self.face, level: l, x, y: y + 1 },
            TileId { face: self.face, level: l, x: x + 1, y: y + 1 },
        ]
    }

    /// Whether `self` covers `other` (is an ancestor of, or equal to, it).
    pub fn covers(&self, other: &TileId) -> bool {
        if self.face != other.face || self.level > other.level {
            return false;
        }
        let shift = other.level - self.level;
        (other.x >> shift) == self.x && (other.y >> shift) == self.y
    }
}

impl Pyramid {
    #[inline]
    pub fn slot_size(&self) -> u32 {
        self.tile_size + 2 * self.border
    }

    #[inline]
    pub fn tiles_per_side(&self, level: u32) -> u32 {
        1 << level
    }

    #[inline]
    pub fn face_size(&self, level: u32) -> u32 {
        self.tile_size << level
    }

    pub fn max_face_size(&self) -> u32 {
        self.face_size(self.levels - 1)
    }

    pub fn tile_count(&self) -> u32 {
        (0..self.levels).map(|l| 6 * (1u32 << (2 * l))).sum()
    }

    pub fn tile_path(&self, t: TileId) -> String {
        self.url
            .replace("{f}", &t.face().letter().to_string())
            .replace("{l}", &t.level.to_string())
            .replace("{x}", &t.x.to_string())
            .replace("{y}", &t.y.to_string())
    }

    pub fn all_tiles(&self) -> impl Iterator<Item = TileId> + '_ {
        (0..self.levels).flat_map(move |l| {
            let n = 1u32 << l;
            Face::ALL.into_iter().flat_map(move |f| (0..n).flat_map(move |y| (0..n).map(move |x| TileId::new(f, l, x, y))))
        })
    }

    /// Picks a tile size and level count for an equirectangular source of the given width,
    /// so that the top level's face resolution matches the source's equatorial density
    /// (`face = width / π`) with tiles no larger than `max_tile` content pixels.
    pub fn plan_for_equirect(width: u32, max_tile: u32) -> (u32, u32) {
        let ideal = width as f64 / std::f64::consts::PI;
        let max_tile = max_tile.max(64) as f64;
        let levels_minus_one = (ideal / max_tile).log2().ceil().max(0.0) as u32;
        let mut tile = (ideal / (1u64 << levels_minus_one) as f64).ceil() as u32;
        tile += tile % 2;
        (tile.max(16), levels_minus_one + 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan() {
        assert_eq!(Pyramid::plan_for_equirect(8192, 510), (326, 4));
        assert_eq!(Pyramid::plan_for_equirect(4096, 510), (326, 3));
        assert_eq!(Pyramid::plan_for_equirect(1000, 510), (320, 1));
        let (t, l) = Pyramid::plan_for_equirect(12000, 510);
        assert!(t <= 510 && (t << (l - 1)) as f64 >= 12000.0 / std::f64::consts::PI);
    }

    #[test]
    fn tile_relations() {
        let t = TileId::new(Face::Up, 3, 5, 6);
        let p = t.parent().unwrap();
        assert_eq!((p.level, p.x, p.y), (2, 2, 3));
        assert!(p.covers(&t));
        assert!(p.children().contains(&t));
        assert!(!TileId::new(Face::Up, 2, 0, 0).covers(&t));
    }

    #[test]
    fn counts_and_paths() {
        let p = Pyramid { tile_size: 326, border: 1, levels: 4, url: "s/{f}/{l}/{y}_{x}.jpg".into() };
        assert_eq!(p.tile_count(), 6 * (1 + 4 + 16 + 64));
        assert_eq!(p.all_tiles().count() as u32, p.tile_count());
        assert_eq!(p.tile_path(TileId::new(Face::Left, 2, 3, 1)), "s/l/2/1_3.jpg");
        assert_eq!(p.slot_size(), 328);
    }
}
