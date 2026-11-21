//! Which tiles does the current view need, and in what order?
//!
//! The screen (plus a prefetch margin) is sampled on a coarse grid. For each sample the
//! same level-of-detail formula as the shader picks the finest level needed; that tile and
//! all its ancestors are wanted. Coarse levels and on-screen tiles near the centre come first,
//! so the view sharpens progressively from the middle outwards.

use glam::Vec2;
use nodal_core::cube::{dir_to_face_uv, face_uv};
use nodal_core::pyramid::{Pyramid, TileId};
use nodal_core::{Face, Projector};
use std::collections::HashMap;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Want {
    pub tile: TileId,
    /// Higher is more urgent. On-screen tiles are ≥ 0, prefetch tiles < 0.
    pub priority: f32,
}

impl Want {
    pub fn on_screen(&self) -> bool {
        self.priority >= 0.0
    }
}

/// Grid spacing in physical pixels. Tiles are never smaller than ~tile_size/2 on screen
/// at the levels the shader blends, so this cannot miss one.
const STEP: f32 = 40.0;

/// Computes the wanted tiles for a view, sorted by descending priority.
pub fn compute_wants(proj: &Projector, pyr: &Pyramid, lod_bias: f32, scratch: &mut HashMap<TileId, f32>, out: &mut Vec<Want>) {
    scratch.clear();
    out.clear();
    for f in Face::ALL {
        scratch.insert(TileId::new(f, 0, 0, 0), 1000.0);
    }
    if pyr.levels > 1 {
        let (w, h) = (proj.width, proj.height);
        let margin = 0.15 * w.max(h);
        let nx = ((w + 2.0 * margin) / STEP).ceil().max(1.0) as i32;
        let ny = ((h + 2.0 * margin) / STEP).ceil().max(1.0) as i32;
        let sx = (w + 2.0 * margin) / nx as f32;
        let sy = (h + 2.0 * margin) / ny as f32;
        let centre = Vec2::new(w * 0.5, h * 0.5);
        let max_level = (pyr.levels - 1) as f32;
        for j in 0..=ny {
            for i in 0..=nx {
                let p = Vec2::new(-margin + i as f32 * sx, -margin + j as f32 * sy);
                let on_screen = p.x >= -1.0 && p.y >= -1.0 && p.x <= w + 1.0 && p.y <= h + 1.0;
                let Some(d) = proj.screen_to_dir(p) else { continue };
                let (Some(dx), Some(dy)) = (proj.screen_to_dir(p + Vec2::X), proj.screen_to_dir(p + Vec2::Y)) else { continue };
                let (face, uv) = dir_to_face_uv(d);
                let gx = face_uv(face, dx) - uv;
                let gy = face_uv(face, dy) - uv;
                let fp = gx.length().max(gy.length()) * pyr.tile_size as f32;
                let lf = -(fp.max(1e-8)).log2() + lod_bias;
                if !lf.is_finite() {
                    continue;
                }
                let finest = lf.clamp(0.0, max_level).ceil() as u32;
                let dist = (p - centre).length() / w.max(h);
                let base = if on_screen { 100.0 } else { -100.0 };
                for level in 1..=finest {
                    let n = 1u32 << level;
                    let tx = ((uv.x * n as f32).floor() as i64).clamp(0, n as i64 - 1) as u32;
                    let ty = ((uv.y * n as f32).floor() as i64).clamp(0, n as i64 - 1) as u32;
                    let t = TileId::new(face, level, tx, ty);
                    let prio = base - level as f32 * 10.0 - dist;
                    let e = scratch.entry(t).or_insert(f32::MIN);
                    if prio > *e {
                        *e = prio;
                    }
                }
            }
        }
    }
    out.extend(scratch.iter().map(|(t, p)| Want { tile: *t, priority: *p }));
    out.sort_by(|a, b| b.priority.total_cmp(&a.priority).then(a.tile.cmp(&b.tile)));
}

#[cfg(test)]
mod tests {
    use super::*;
    use nodal_core::{Projection, View};

    fn pyr() -> Pyramid {
        Pyramid { tile_size: 326, border: 1, levels: 4, url: String::new() }
    }

    #[test]
    fn wide_view_needs_coarse_tiles_only() {
        let view = View { fov: 120.0, ..View::default() };
        let proj = Projector::new(&view, 800.0, 450.0);
        let (mut s, mut out) = (HashMap::new(), vec![]);
        compute_wants(&proj, &pyr(), 0.0, &mut s, &mut out);
        let max = out.iter().map(|w| w.tile.level).max().unwrap();
        assert!(max <= 2, "max level {max}");
        assert!(out[..6].iter().all(|w| w.tile.level == 0), "base first");
    }

    #[test]
    fn zoomed_view_needs_finest_level_near_centre() {
        let view = View { fov: 20.0, ..View::default() };
        let proj = Projector::new(&view, 1600.0, 900.0);
        let (mut s, mut out) = (HashMap::new(), vec![]);
        compute_wants(&proj, &pyr(), 0.0, &mut s, &mut out);
        let finest: Vec<_> = out.iter().filter(|w| w.tile.level == 3 && w.on_screen()).collect();
        assert!(!finest.is_empty());
        assert!(finest.len() < 20, "only a handful of top-level tiles: {}", finest.len());
        // All on the front face.
        assert!(finest.iter().all(|w| w.tile.face == Face::Front as u8));
        // Priorities: on-screen coarse before fine.
        let first_l3 = out.iter().position(|w| w.tile.level == 3).unwrap();
        let last_l1_on = out.iter().rposition(|w| w.tile.level == 1 && w.on_screen()).unwrap();
        assert!(last_l1_on < first_l3);
    }

    #[test]
    fn little_planet_covers_all_faces() {
        let view = View { pitch: -90.0, fov: 300.0, projection: Projection::STEREOGRAPHIC, ..View::default() };
        let proj = Projector::new(&view, 1000.0, 1000.0);
        let (mut s, mut out) = (HashMap::new(), vec![]);
        compute_wants(&proj, &pyr(), 0.0, &mut s, &mut out);
        for f in Face::ALL {
            assert!(out.iter().any(|w| w.tile.face == f as u8), "{f:?}");
        }
    }
}
