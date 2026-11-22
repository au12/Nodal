//! Nodal renderer: a wgpu panorama engine with virtual-texture tile streaming.
//!
//! * [`PanoEngine`] owns the GPU resources and draws a scene in one fullscreen pass.
//! * [`TileFetcher`] implementations load and decode tiles off the render thread.
//! * [`Streamer`] ties them together each frame: plan → request → upload → flush.

pub mod cache;
pub mod compose;
pub mod engine;
pub mod fetch;
#[cfg(not(target_arch = "wasm32"))]
pub mod headless;
pub mod stream;

pub use compose::{ComposeFrame, ComposePhoto, Composer};
pub use engine::{EngineStats, OverlayUniforms, PanoEngine, PanoUniforms};
pub use fetch::{TileFetcher, TileImage, TileKey, TileRequest, Waker};
pub use stream::Want;
pub use wgpu;

use nodal_core::pyramid::{Pyramid, TileId};
use nodal_core::Projector;
use std::collections::HashSet;

#[derive(Clone, Copy, Debug, Default)]
pub struct StreamStatus {
    pub uploaded: usize,
    pub pending: usize,
    pub missing_on_screen: usize,
    pub wanted_on_screen: usize,
    pub base_ready: bool,
    pub failed: usize,
}

impl StreamStatus {
    /// 0..1 progress of the tiles needed for the current view.
    pub fn progress(&self) -> f32 {
        if self.wanted_on_screen == 0 {
            return 1.0;
        }
        1.0 - self.missing_on_screen as f32 / self.wanted_on_screen as f32
    }

    pub fn is_complete(&self) -> bool {
        self.base_ready && self.missing_on_screen <= self.failed
    }
}

/// Per-scene streaming state.
pub struct Streamer {
    pub generation: u32,
    /// Directory or URL that tile paths are relative to.
    pub base_url: String,
    pub pyramid: Pyramid,
    failed: HashSet<TileId>,
}

impl Streamer {
    pub fn new(generation: u32, base_url: String, pyramid: Pyramid) -> Self {
        Self { generation, base_url, pyramid, failed: HashSet::new() }
    }

    pub fn tile_url(&self, t: TileId) -> String {
        nodal_core::tour::join_url(&self.base_url, &self.pyramid.tile_path(t))
    }

    #[allow(clippy::too_many_arguments)]
    /// One streaming step. `max_uploads` bounds GPU uploads per call to avoid frame hitches.
    pub fn step(
        &mut self,
        engine: &mut PanoEngine,
        fetcher: &mut dyn TileFetcher,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        proj: &Projector,
        lod_bias: f32,
        max_uploads: usize,
    ) -> StreamStatus {
        let mut status = StreamStatus::default();
        if engine.generation() != Some(self.generation) {
            return status;
        }
        engine.plan(proj, lod_bias);
        for (key, res) in fetcher.poll(max_uploads) {
            if key.generation != self.generation {
                continue;
            }
            match res {
                Ok(img) => {
                    if engine.accept_tile(device, queue, key.generation, key.tile, img) {
                        status.uploaded += 1;
                    }
                }
                Err(e) => {
                    log::warn!("tile failed: {e}");
                    self.failed.insert(key.tile);
                }
            }
        }
        engine.flush(queue);
        let scene = engine.scene.as_ref().expect("scene");
        let reqs: Vec<TileRequest> = engine
            .wants
            .iter()
            .filter(|w| !scene.is_resident(&w.tile) && !self.failed.contains(&w.tile))
            .map(|w| TileRequest {
                key: TileKey { generation: self.generation, tile: w.tile },
                url: self.tile_url(w.tile),
                priority: w.priority,
            })
            .collect();
        let failed_on_screen = engine.wants.iter().filter(|w| w.on_screen() && self.failed.contains(&w.tile)).count();
        fetcher.set_queue(reqs);
        status.pending = fetcher.pending();
        status.missing_on_screen = engine.stats.missing_on_screen;
        status.wanted_on_screen = engine.stats.wanted_on_screen;
        status.base_ready = scene.base_ready;
        status.failed = failed_on_screen;
        status
    }
}
