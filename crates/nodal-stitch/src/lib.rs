//! Nodal's panorama stitching engine.
//!
//! Pipeline: load photos and EXIF → detect features → find overlapping pairs and control points →
//! estimate the lens and optimise orientations and distortion → (render) warp, balance exposure,
//! find seams, blend, and fill pole gaps.

// Numeric kernels index several parallel buffers at once; iterator chains would obscure them.
#![allow(clippy::needless_range_loop, clippy::too_many_arguments)]

pub mod align;
pub mod blend;
pub mod features;
pub mod fill;
pub mod io;
pub mod lens;
pub mod local;
pub mod matching;
pub mod optimize;
pub mod photometric;
pub mod project;
pub mod refine;
pub mod render;
pub mod seam;
pub mod synth;
pub mod warp;

pub use align::{AlignOptions, AlignReport};
pub use lens::{Lens, LensKind};
pub use nalgebra;
pub use project::{ControlPoint, OutputSettings, Project, SourceImage};
pub use render::{render, Panorama, RenderOptions};

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
    let mut report = align::align(&mut project, &feats, &AlignOptions { level: false, ..opts.clone() }, &|f, s| progress(f * 0.8, s))?;
    // Polish control points at full resolution, then re-optimise with the sharper measurements.
    if project.control_points.len() >= 12 {
        progress(0.82, "Refining control points");
        let lumas: Vec<refine::Luma> = sources.par_iter().map(|s| refine::Luma::from_rgb(&s.image)).collect();
        let before = report.rms;
        let search = ((report.rms * 2.5).ceil() as i32).clamp(3, 12);
        let refined = refine::refine_control_points(&mut project, &lumas, search);
        if refined.refined > 0 {
            progress(0.9, "Optimising");
            let (rms, max) = align::polish(&mut project, opts);
            report.log.push(format!(
                "Sub-pixel refinement: {} points kept, {} dropped, RMS {:.2} → {:.2} px",
                refined.refined, refined.rejected, before, rms
            ));
            report.rms = rms;
            report.max = max;
            report.control_points = project.control_points.len();
        }
    }
    if opts.level {
        align::level(&mut project);
    }
    if project.output.local_alignment {
        let local = local::fit(&mut project);
        if local.images_warped > 0 {
            report.log.push(format!(
                "Local alignment: RMS {:.2} → {:.2} px (largest correction {:.1} px)",
                local.before, local.after, local.max_shift
            ));
        }
        report.local = Some(local);
    }
    Ok((project, report))
}
