//! Nodal's panorama stitching engine.
//!
//! Pipeline: load photos and EXIF → detect features → find overlapping pairs and control points →
//! estimate the lens and optimise orientations and distortion → (render) warp, balance exposure,
//! find seams, blend, and fill pole gaps.

pub mod align;
pub mod features;
pub mod io;
pub mod lens;
pub mod matching;
pub mod optimize;
pub mod project;
pub mod synth;

pub use align::{AlignOptions, AlignReport};
pub use lens::{Lens, LensKind};
pub use project::{ControlPoint, OutputSettings, Project, SourceImage};

use anyhow::Result;
use features::{DetectOptions, FeatureSet};
use image::RgbImage;
use rayon::prelude::*;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

/// Progress callback: fraction in [0, 1] and a short description of the current stage.
pub type Progress<'a> = &'a (dyn Fn(f32, &str) + Sync);

pub fn no_progress(_: f32, _: &str) {}

/// A loaded source photo with its features.
#[derive(Clone)]
pub struct Source {
    pub path: PathBuf,
    pub image: Arc<RgbImage>,
    pub exif: project::ExifInfo,
    pub features: Arc<FeatureSet>,
}

/// Loads photos and detects features, in parallel.
pub fn load_sources(paths: &[PathBuf], detect: &DetectOptions, progress: Progress) -> Result<Vec<Source>> {
    let done = AtomicUsize::new(0);
    let total = paths.len().max(1);
    progress(0.0, "Loading images");
    paths
        .par_iter()
        .map(|path| {
            let (image, exif) = io::load(path)?;
            let features = features::detect(image.as_raw(), image.width() as usize, image.height() as usize, detect);
            let k = done.fetch_add(1, Ordering::Relaxed) + 1;
            progress(k as f32 / total as f32, "Analysing images");
            Ok(Source { path: path.clone(), image: Arc::new(image), exif, features: Arc::new(features) })
        })
        .collect()
}

/// Creates a project from loaded sources and aligns it.
pub fn align_sources(sources: &[Source], opts: &AlignOptions, progress: Progress) -> Result<(Project, AlignReport)> {
    let meta: Vec<_> = sources.iter().map(|s| (s.path.clone(), s.image.width(), s.image.height(), s.exif.clone())).collect();
    let mut project = align::new_project(&meta);
    let feats: Vec<FeatureSet> = sources.iter().map(|s| (*s.features).clone()).collect();
    let report = align::align(&mut project, &feats, opts, progress)?;
    Ok((project, report))
}
