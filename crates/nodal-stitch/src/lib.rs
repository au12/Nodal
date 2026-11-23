//! Nodal's panorama stitching engine.
//!
//! Pipeline: decode photos (RAW, TIFF, PSD/PSB, JPEG …) to scene-linear working copies → detect
//! features → match and verify pairs → estimate the lens and optimise orientations and distortion
//! → measure the overlaps densely and correct residual misalignment → calibrate exposure and
//! vignetting → (render) warp at full resolution, place seams, blend in a perceptual space, fill
//! voids, encode 8- or 16-bit output.

// Numeric kernels index several parallel buffers at once; iterator chains would obscure them.
#![allow(clippy::needless_range_loop, clippy::too_many_arguments)]

pub mod align;
pub mod bench;
pub mod blend;
pub mod decode;
pub mod dense;
pub mod features;
pub mod fill;
pub mod global;
pub mod io;
pub mod lens;
pub mod local;
pub mod matching;
#[cfg(feature = "ml")]
pub mod ml;
pub mod optimize;
pub mod photometric;
pub mod pixels;
pub mod project;
pub mod psd;
pub mod refine;
pub mod render;
pub mod seam;
pub mod synth;
pub mod warp;

pub use align::{AlignOptions, AlignReport};
pub use lens::{Lens, LensKind};
pub use nalgebra;
pub use pixels::LinearImage;
pub use project::{ControlPoint, OutputSettings, Project, SourceImage};
pub use render::{render, Panorama, RenderOptions};

use anyhow::Result;
use features::{DetectOptions, FeatureSet};
use image::RgbImage;
use rayon::prelude::*;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

/// Progress callback: fraction in [0, 1] and a short description of the current stage.
pub type Progress<'a> = &'a (dyn Fn(f32, &str) + Sync);

pub fn no_progress(_: f32, _: &str) {}

/// Longest side of the in-memory working copy of each photo. Alignment, dense registration,
/// photometric calibration and previews use working copies; the final render decodes the files
/// again at full resolution.
pub const WORK_DIM: u32 = 2800;

/// A loaded source photo: a reduced working copy, its features and how to get the full pixels.
#[derive(Clone)]
pub struct Source {
    pub path: PathBuf,
    /// Working copy (8-bit sRGB), at most [`WORK_DIM`] on the long side.
    pub image: Arc<RgbImage>,
    /// Full-resolution size, orientation applied.
    pub width: u32,
    pub height: u32,
    pub exif: project::ExifInfo,
    /// Format description for the UI ("CR3 14-bit", "TIFF 16-bit" …).
    pub format: String,
    pub kind: decode::SourceKind,
    /// Features in full-resolution pixel coordinates.
    pub features: Arc<FeatureSet>,
    /// Full-resolution pixels kept in memory (synthetic and in-memory sources). File sources leave
    /// this empty and are decoded again when rendering.
    pub full: Option<Arc<LinearImage>>,
    /// RAW white balance used for this set (shared by every RAW frame).
    pub white_balance: Option<[f32; 3]>,
}

impl Source {
    /// Working-copy pixels per full-resolution pixel.
    pub fn work_scale(&self) -> f32 {
        self.image.width() as f32 / self.width.max(1) as f32
    }

    /// Wraps in-memory pixels (tests, benchmarks, generated shots).
    pub fn from_memory(path: impl Into<PathBuf>, full: LinearImage, exif: project::ExifInfo, detect: &DetectOptions) -> Source {
        let (w, h) = (full.width as u32, full.height as u32);
        let work = full.downscale_to(WORK_DIM as usize).to_srgb8(1.0);
        let features = detect_scaled(&work, w, detect);
        Source {
            path: path.into(),
            image: Arc::new(work),
            width: w,
            height: h,
            exif,
            format: "memory".into(),
            kind: decode::SourceKind::Other,
            features: Arc::new(features),
            full: Some(Arc::new(full)),
            white_balance: None,
        }
    }

    /// Wraps an 8-bit sRGB image that is both working copy and full resolution.
    pub fn from_rgb8(path: impl Into<PathBuf>, img: Arc<RgbImage>, exif: project::ExifInfo, detect: &DetectOptions) -> Source {
        let (w, h) = (img.width(), img.height());
        let full = LinearImage::from_srgb8(&img);
        let work = if w.max(h) > WORK_DIM { Arc::new(full.downscale_to(WORK_DIM as usize).to_srgb8(1.0)) } else { img };
        let features = detect_scaled(&work, w, detect);
        Source {
            path: path.into(),
            image: work,
            width: w,
            height: h,
            exif,
            format: "memory".into(),
            kind: decode::SourceKind::Other,
            features: Arc::new(features),
            full: Some(Arc::new(full)),
            white_balance: None,
        }
    }
}

/// Detects features on a working copy and scales them to full-resolution coordinates.
fn detect_scaled(work: &RgbImage, full_width: u32, detect: &DetectOptions) -> FeatureSet {
    let mut fs = features::detect(work.as_raw(), work.width() as usize, work.height() as usize, detect);
    let s = full_width as f32 / work.width().max(1) as f32;
    if (s - 1.0).abs() > 1e-6 {
        for f in &mut fs.feats {
            f.x *= s;
            f.y *= s;
            f.scale *= s;
        }
    }
    fs
}

/// Loads photos and detects features, in parallel. RAW files in one set share a white balance
/// (the median as-shot balance) so the frames match; it is returned for the project.
pub fn load_sources(paths: &[PathBuf], detect: &DetectOptions, progress: Progress) -> Result<Vec<Source>> {
    Ok(load_sources_with(paths, detect, None, progress)?.0)
}

/// Like [`load_sources`], also returning the white balance used for RAW files. `white_balance`
/// overrides it.
pub fn load_sources_with(
    paths: &[PathBuf],
    detect: &DetectOptions,
    white_balance: Option<[f32; 3]>,
    progress: Progress,
) -> Result<(Vec<Source>, Option<[f32; 3]>)> {
    let total = paths.len().max(1);
    progress(0.0, "Reading images");
    let infos: Vec<decode::ImageInfo> = paths
        .par_iter()
        .map(|p| decode::probe(p).map_err(|e| e.context(format!("could not read {}", p.display()))))
        .collect::<Result<_>>()?;
    let wb = white_balance.or_else(|| decode::common_white_balance(&infos));
    let done = AtomicUsize::new(0);
    let settings = decode::DecodeSettings { max_dim: WORK_DIM, white_balance: wb };
    // Two decodes per core at most: RAW decoding is itself parallel.
    let pool = rayon::ThreadPoolBuilder::new().num_threads(rayon::current_num_threads().clamp(1, 8)).build()?;
    let sources = pool.install(|| {
        paths
            .par_iter()
            .zip(infos.par_iter())
            .map(|(path, info)| {
                let (work, _) =
                    decode::decode_linear(path, &settings).map_err(|e| e.context(format!("could not load {}", path.display())))?;
                let work = work.to_srgb8(1.0);
                let features = detect_scaled(&work, info.width, detect);
                let k = done.fetch_add(1, Ordering::Relaxed) + 1;
                progress(k as f32 / total as f32, "Analysing images");
                Ok(Source {
                    path: path.clone(),
                    image: Arc::new(work),
                    width: info.width,
                    height: info.height,
                    exif: info.exif.clone(),
                    format: info.format.clone(),
                    kind: info.kind,
                    features: Arc::new(features),
                    full: None,
                    white_balance: if info.kind == decode::SourceKind::Raw { wb } else { None },
                })
            })
            .collect::<Result<Vec<_>>>()
    })?;
    Ok((sources, wb))
}

/// Supplies pixels for rendering.
pub trait PixelProvider: Sync {
    /// Working copy (8-bit sRGB) of project image `i`.
    fn working(&self, i: usize) -> Arc<RgbImage>;
    /// Full-resolution scene-linear pixels of project image `i`.
    fn full(&self, i: usize, develop: &project::Develop) -> Result<Arc<LinearImage>>;
    /// Scene-linear pixels at about half resolution (RAW files decode several times faster this
    /// way). Used when the output is not fine enough to need every photo pixel.
    fn half(&self, i: usize, develop: &project::Develop) -> Result<Arc<LinearImage>> {
        self.full(i, develop)
    }
    /// Path of image `i` (for messages).
    fn path(&self, i: usize) -> &Path;
}

impl PixelProvider for [Source] {
    fn working(&self, i: usize) -> Arc<RgbImage> {
        self[i].image.clone()
    }

    fn full(&self, i: usize, develop: &project::Develop) -> Result<Arc<LinearImage>> {
        let s = &self[i];
        if let Some(f) = &s.full {
            return Ok(f.clone());
        }
        let settings = decode::DecodeSettings { max_dim: 0, white_balance: develop.white_balance.or(s.white_balance) };
        let (img, _) = decode::decode_linear(&s.path, &settings).map_err(|e| e.context(format!("could not load {}", s.path.display())))?;
        anyhow::ensure!(
            img.width as u32 == s.width && img.height as u32 == s.height,
            "{} changed size since it was loaded",
            s.path.display()
        );
        Ok(Arc::new(img))
    }

    fn half(&self, i: usize, develop: &project::Develop) -> Result<Arc<LinearImage>> {
        let s = &self[i];
        if s.full.is_some() || s.kind != decode::SourceKind::Raw {
            return self.full(i, develop);
        }
        let long = s.width.max(s.height);
        let settings = decode::DecodeSettings { max_dim: long / 2, white_balance: develop.white_balance.or(s.white_balance) };
        let (img, _) = decode::decode_linear(&s.path, &settings).map_err(|e| e.context(format!("could not load {}", s.path.display())))?;
        Ok(Arc::new(img))
    }

    fn path(&self, i: usize) -> &Path {
        &self[i].path
    }
}

impl PixelProvider for Vec<Source> {
    fn working(&self, i: usize) -> Arc<RgbImage> {
        self.as_slice().working(i)
    }
    fn full(&self, i: usize, develop: &project::Develop) -> Result<Arc<LinearImage>> {
        self.as_slice().full(i, develop)
    }
    fn half(&self, i: usize, develop: &project::Develop) -> Result<Arc<LinearImage>> {
        self.as_slice().half(i, develop)
    }
    fn path(&self, i: usize) -> &Path {
        self.as_slice().path(i)
    }
}

/// In-memory 8-bit images (each both working copy and full resolution).
impl PixelProvider for [Arc<RgbImage>] {
    fn working(&self, i: usize) -> Arc<RgbImage> {
        self[i].clone()
    }
    fn full(&self, i: usize, _: &project::Develop) -> Result<Arc<LinearImage>> {
        Ok(Arc::new(LinearImage::from_srgb8(&self[i])))
    }
    fn path(&self, _: usize) -> &Path {
        Path::new("")
    }
}

impl PixelProvider for Vec<Arc<RgbImage>> {
    fn working(&self, i: usize) -> Arc<RgbImage> {
        self[i].clone()
    }
    fn full(&self, i: usize, d: &project::Develop) -> Result<Arc<LinearImage>> {
        self.as_slice().full(i, d)
    }
    fn path(&self, _: usize) -> &Path {
        Path::new("")
    }
}

/// In-memory 8-bit images (each both working copy and full resolution).
pub struct MemoryPixels(pub Vec<Arc<RgbImage>>);

impl PixelProvider for MemoryPixels {
    fn working(&self, i: usize) -> Arc<RgbImage> {
        self.0[i].clone()
    }
    fn full(&self, i: usize, _: &project::Develop) -> Result<Arc<LinearImage>> {
        Ok(Arc::new(LinearImage::from_srgb8(&self.0[i])))
    }
    fn path(&self, _: usize) -> &Path {
        Path::new("")
    }
}

/// Creates a project from loaded sources and aligns it: global alignment, dense overlap
/// registration feeding a second adjustment, local warps where parallax remains, and photometric
/// calibration.
pub fn align_sources(sources: &[Source], opts: &AlignOptions, progress: Progress) -> Result<(Project, AlignReport)> {
    let meta: Vec<_> = sources.iter().map(|s| (s.path.clone(), s.width, s.height, s.exif.clone())).collect();
    let mut project = align::new_project(&meta);
    project.develop.white_balance = sources.iter().find_map(|s| s.white_balance);
    let feats: Vec<FeatureSet> = sources.iter().map(|s| (*s.features).clone()).collect();
    let mut clock = std::time::Instant::now();
    let mut lap = |report: &mut AlignReport, name: &str| {
        report.timings.push((name.to_string(), clock.elapsed().as_secs_f32()));
        clock = std::time::Instant::now();
    };
    let mut report = align::align(&mut project, &feats, &AlignOptions { level: false, ..opts.clone() }, &|f, s| progress(f * 0.6, s))?;
    lap(&mut report, "match + global alignment");
    // Polish control points on the working copies, then re-optimise with the sharper measurements.
    // Detector positions carry several pixels of noise on large frames; dense registration needs a
    // sub-pixel model to start from (it measures against the current model, so a rough start biases
    // what it sees).
    if project.control_points.len() >= 12 {
        let lumas: Vec<refine::Luma> = sources.par_iter().map(|s| refine::Luma::from_rgb_scaled(&s.image, s.work_scale())).collect();
        progress(0.62, "Refining control points");
        let before = report.rms;
        let search = ((report.rms * 2.5).ceil() as i32).clamp(3, 12);
        let refined = refine::refine_control_points(&mut project, &lumas, search);
        if refined.refined > 0 {
            progress(0.66, "Optimising");
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
        // Photos with nothing to match (clear sky) still belong in the panorama: place them from
        // the shooting pattern where they fill a gap, and say so.
        if !report.unaligned.is_empty() {
            let featureless: Vec<bool> = sources.iter().map(|s| pixels::textured_share(&s.image, 4.0) < 0.1).collect();
            let placed = align::place_by_pattern(&mut project, &featureless);
            if !placed.is_empty() {
                report.log.push(format!(
                    "Placed {} featureless photo(s) from the shooting pattern: {}",
                    placed.len(),
                    placed.iter().map(|&i| project.images[i].name.clone()).collect::<Vec<_>>().join(", ")
                ));
                report.unaligned.retain(|i| !placed.contains(i));
                report.estimated = placed;
            }
        }
    }
    lap(&mut report, "control point refinement");
    // Dense registration of every overlap: thousands of evenly spread measurements for a second
    // adjustment, and the parallax that remains for the local warps and seams.
    if opts.dense && project.active().count() >= 2 {
        progress(0.7, "Measuring overlaps");
        let dense_report = dense::refine_project(&mut project, sources, opts, &|f, s| progress(0.7 + 0.2 * f, s));
        report.log.extend(dense_report.log.iter().cloned());
        report.rms = dense_report.rms_after.max(0.0);
        report.dense = Some(dense_report);
    } else if project.output.local_alignment {
        let local = local::fit(&mut project);
        if local.images_warped > 0 {
            report.log.push(format!(
                "Local alignment: RMS {:.2} → {:.2} px (largest correction {:.1} px)",
                local.before, local.after, local.max_shift
            ));
        }
        report.local = Some(local);
    }
    lap(&mut report, "dense registration + warps");
    progress(0.92, "Calibrating exposure");
    let photo = photometric::calibrate(&mut project, sources);
    lap(&mut report, "photometric calibration");
    report.log.push(photo.summary());
    report.photometric = Some(photo);
    progress(1.0, "Aligned");
    Ok((project, report))
}
