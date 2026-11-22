//! Filling holes between photos (voids that are not pole caps).

use crate::render::Panorama;
use crate::Progress;

/// Fills interior holes; returns how many were filled.
pub fn fill_holes(_pano: &mut Panorama, _progress: Progress) -> usize {
    0
}
