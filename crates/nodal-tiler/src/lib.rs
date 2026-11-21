//! Nodal tiler: turns equirectangular panoramas into multi-resolution cube tile pyramids.
//!
//! Pipeline per scene: decode → render each cube face once at full resolution (with a
//! margin ring so every level's tile borders hold real neighbouring pixels) → box-filter
//! down the pyramid in linear light → crop bordered tiles → encode in parallel.

pub mod face;
pub mod sampler;

use anyhow::{bail, Context, Result};
use nodal_core::cube::Face;
use nodal_core::math::DEG;
use nodal_core::pyramid::{Pyramid, TileId};
use nodal_core::tour::{slugify, Scene, SourceInfo, Tour, TourSettings};
use nodal_core::view::{View, ViewLimits};
use nodal_core::Projector;
use rayon::prelude::*;
use sampler::{Coverage, Equirect};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

pub use sampler::Coverage as SourceCoverage;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TileFormat {
    Jpeg,
    Png,
}

impl TileFormat {
    pub fn ext(self) -> &'static str {
        match self {
            TileFormat::Jpeg => "jpg",
            TileFormat::Png => "png",
        }
    }
}

#[derive(Clone, Debug)]
pub struct TileOptions {
    /// Largest tile content size when planning automatically.
    pub max_tile: u32,
    /// Force a tile size (content pixels, excluding the border).
    pub tile_size: Option<u32>,
    /// Force a number of levels.
    pub levels: Option<u32>,
    pub format: TileFormat,
    /// JPEG quality 1–100.
    pub quality: u8,
    pub thumb_width: u32,
    pub thumb_height: u32,
    /// Override the guessed coverage for partial panoramas.
    pub coverage: Option<Coverage>,
}

impl Default for TileOptions {
    fn default() -> Self {
        Self {
            max_tile: 510,
            tile_size: None,
            levels: None,
            format: TileFormat::Jpeg,
            quality: 86,
            thumb_width: 640,
            thumb_height: 360,
            coverage: None,
        }
    }
}

/// Progress callback: fraction in `0..=1` and a short stage description.
pub type Progress<'a> = &'a (dyn Fn(f32, &str) + Sync);

pub fn no_progress(_: f32, _: &str) {}

/// Decodes an image file without the `image` crate's default allocation limits
/// (large panoramas easily exceed 512 MB).
pub fn load_image(path: &Path) -> Result<image::RgbImage> {
    let mut reader = image::ImageReader::open(path).with_context(|| format!("opening {}", path.display()))?.with_guessed_format()?;
    reader.no_limits();
    let img = reader.decode().with_context(|| format!("decoding {}", path.display()))?;
    Ok(img.into_rgb8())
}

pub fn load_image_bytes(bytes: &[u8]) -> Result<image::RgbImage> {
    let mut reader = image::ImageReader::new(std::io::Cursor::new(bytes)).with_guessed_format()?;
    reader.no_limits();
    Ok(reader.decode().context("decoding uploaded image")?.into_rgb8())
}

/// Tiles one panorama into `tour_dir/scenes/<id>/` and returns the scene entry
/// (with paths relative to `tour_dir`).
pub fn tile_scene(
    img: image::RgbImage,
    tour_dir: &Path,
    id: &str,
    title: &str,
    file_name: &str,
    opts: &TileOptions,
    progress: Progress,
) -> Result<Scene> {
    let (w, h) = img.dimensions();
    if w < 64 || h < 32 {
        bail!("image is too small ({w}×{h})");
    }
    let coverage = opts.coverage.unwrap_or_else(|| Coverage::guess(w, h));
    let eq = Equirect::new(img, coverage);

    // Face resolution matching the source's equatorial density: face/2 px per radian.
    let ideal_face = 2.0 * eq.density();
    let (tile_size, levels) = match (opts.tile_size, opts.levels) {
        (Some(t), Some(l)) => (t, l),
        (Some(t), None) => {
            let l = ((ideal_face / t as f32).log2().ceil().max(0.0) as u32) + 1;
            (t, l)
        }
        (None, Some(l)) => {
            let t = (ideal_face / (1u32 << (l - 1)) as f32).ceil() as u32;
            (t + t % 2, l)
        }
        (None, None) => Pyramid::plan_for_equirect((std::f32::consts::PI * ideal_face) as u32, opts.max_tile),
    };
    if !(16..=4094).contains(&tile_size) || levels == 0 || levels > 10 {
        bail!("unsupported pyramid: tile {tile_size}, levels {levels}");
    }

    let scene_rel = format!("scenes/{id}");
    let scene_dir = tour_dir.join(&scene_rel);
    if scene_dir.exists() {
        std::fs::remove_dir_all(&scene_dir).with_context(|| format!("clearing {}", scene_dir.display()))?;
    }
    let pyramid = Pyramid { tile_size, border: 1, levels, url: format!("{scene_rel}/tiles/{{f}}/{{l}}/{{y}}_{{x}}.{}", opts.format.ext()) };
    for l in 0..levels {
        for f in Face::ALL {
            std::fs::create_dir_all(scene_dir.join("tiles").join(f.letter().to_string()).join(l.to_string()))?;
        }
    }

    let top = levels - 1;
    let top_size = pyramid.face_size(top);
    let margin = pyramid.border << top;
    let total_tiles = pyramid.tile_count() as usize;
    let done = AtomicUsize::new(0);
    let report = |stage: &str| {
        let d = done.load(Ordering::Relaxed);
        progress(0.04 + 0.92 * d as f32 / total_tiles as f32, stage);
    };

    for (fi, face) in Face::ALL.into_iter().enumerate() {
        progress(0.04 + 0.92 * done.load(Ordering::Relaxed) as f32 / total_tiles as f32, &format!("rendering {} face", face.name()));
        let mut img = face::render_face(&eq, face, top_size, margin);
        for level in (0..levels).rev() {
            if level != top {
                img = img.downsample();
            }
            let n = pyramid.tiles_per_side(level);
            let coords: Vec<(u32, u32)> = (0..n).flat_map(|y| (0..n).map(move |x| (x, y))).collect();
            coords.par_iter().try_for_each(|&(x, y)| -> Result<()> {
                let t = TileId::new(face, level, x, y);
                let slot = pyramid.slot_size();
                let px = img.crop((x * tile_size) as i64 - pyramid.border as i64, (y * tile_size) as i64 - pyramid.border as i64, slot);
                let bytes = encode(&px, slot, slot, opts)?;
                std::fs::write(tour_dir.join(pyramid.tile_path(t)), bytes)?;
                done.fetch_add(1, Ordering::Relaxed);
                Ok(())
            })?;
            report(&format!("encoding {} face", face.name()));
        }
        let _ = fi;
    }

    // Start view and thumbnail.
    let mut limits = ViewLimits::default();
    if !coverage.is_full() {
        let half = coverage.vfov * 0.5 / DEG;
        limits.min_pitch = (coverage.voffset / DEG - half).max(-90.0);
        limits.max_pitch = (coverage.voffset / DEG + half).min(90.0);
    }
    let view = View { fov: 100.0, ..View::default() };
    progress(0.97, "thumbnail");
    let small = eq.downscaled(2048);
    let thumb = render_view(&small, &View { fov: 105.0, ..view }, opts.thumb_width, opts.thumb_height, 3);
    let thumb_rel = format!("{scene_rel}/thumb.jpg");
    std::fs::write(
        tour_dir.join(&thumb_rel),
        encode(thumb.as_raw(), thumb.width(), thumb.height(), &TileOptions { format: TileFormat::Jpeg, quality: 82, ..opts.clone() })?,
    )?;
    progress(1.0, "done");

    Ok(Scene {
        id: id.to_string(),
        title: title.to_string(),
        description: String::new(),
        pyramid,
        thumb: Some(thumb_rel),
        view,
        limits,
        north: 0.0,
        hotspots: vec![],
        source: Some(SourceInfo { width: w, height: h, file: file_name.to_string() }),
    })
}

/// Encodes an RGB8 buffer as JPEG or PNG.
pub fn encode(rgb: &[u8], w: u32, h: u32, opts: &TileOptions) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(rgb.len() / 6);
    match opts.format {
        TileFormat::Jpeg => {
            let mut enc = jpeg_encoder::Encoder::new(&mut out, opts.quality.clamp(1, 100));
            enc.set_sampling_factor(jpeg_encoder::SamplingFactor::R_4_2_0);
            // Do NOT enable optimized Huffman tables together with 4:2:0: zune-jpeg (used by
            // the `image` crate) misdecodes those files even though libjpeg accepts them.
            // Standard tables decode correctly everywhere; the size cost is a few percent.
            enc.encode(rgb, w as u16, h as u16, jpeg_encoder::ColorType::Rgb)?;
        }
        TileFormat::Png => {
            use image::ImageEncoder;
            image::codecs::png::PngEncoder::new_with_quality(
                &mut out,
                image::codecs::png::CompressionType::Fast,
                image::codecs::png::FilterType::Adaptive,
            )
            .write_image(rgb, w, h, image::ExtendedColorType::Rgb8)?;
        }
    }
    Ok(out)
}

/// CPU reference renderer: renders any view of an equirectangular source with
/// `ss × ss` supersampling. Used for thumbnails and to validate the GPU renderer.
pub fn render_view(eq: &Equirect, view: &View, w: u32, h: u32, ss: u32) -> image::RgbImage {
    let proj = Projector::new(view, w as f32, h as f32);
    let mut data = vec![0u8; (w * h * 3) as usize];
    let bg = [eq.background[0] as f32, eq.background[1] as f32, eq.background[2] as f32];
    data.par_chunks_mut((w * 3) as usize).enumerate().for_each(|(y, row)| {
        for x in 0..w as usize {
            let mut acc = [0.0f32; 3];
            for sy in 0..ss {
                for sx in 0..ss {
                    let p = glam::Vec2::new(x as f32 + (sx as f32 + 0.5) / ss as f32, y as f32 + (sy as f32 + 0.5) / ss as f32);
                    let c = proj.screen_to_dir(p).and_then(|d| eq.pixel_of(d)).map(|(u, v)| eq.bilinear(u, v)).unwrap_or(bg);
                    acc[0] += c[0];
                    acc[1] += c[1];
                    acc[2] += c[2];
                }
            }
            let n = (ss * ss) as f32;
            let px = sampler::to_u8([acc[0] / n, acc[1] / n, acc[2] / n]);
            row[x * 3..x * 3 + 3].copy_from_slice(&px);
        }
    });
    image::RgbImage::from_raw(w, h, data).unwrap()
}

/// `"venice_sunset_8k"` → `"Venice Sunset 8k"`.
pub fn title_from_file(path: &Path) -> String {
    let stem = path.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| "Scene".into());
    stem.split(['_', '-', ' '])
        .filter(|w| !w.is_empty())
        .map(|w| {
            let mut c = w.chars();
            match c.next() {
                Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Builds (or extends) a tour in `tour_dir` from image files. Existing scenes and hotspots
/// are kept; new images are appended as scenes. Writes `tour.json` atomically.
pub fn build_tour(inputs: &[PathBuf], tour_dir: &Path, title: Option<&str>, opts: &TileOptions, progress: Progress) -> Result<Tour> {
    std::fs::create_dir_all(tour_dir)?;
    let tour_path = tour_dir.join("tour.json");
    let mut tour = if tour_path.exists() {
        Tour::from_json(&std::fs::read_to_string(&tour_path)?)?
    } else {
        Tour {
            version: nodal_core::tour::FORMAT_VERSION,
            title: title.unwrap_or("Untitled tour").to_string(),
            description: String::new(),
            start_scene: None,
            settings: TourSettings::default(),
            scenes: vec![],
        }
    };
    if let Some(t) = title {
        tour.title = t.to_string();
    }
    let n = inputs.len().max(1) as f32;
    for (i, input) in inputs.iter().enumerate() {
        let scene_title = title_from_file(input);
        let id = tour.unique_scene_id(&slugify(&scene_title));
        let sub = |f: f32, stage: &str| progress((i as f32 + f) / n, &format!("{scene_title}: {stage}"));
        sub(0.0, "decoding");
        let img = load_image(input)?;
        let file_name = input.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
        let scene = tile_scene(img, tour_dir, &id, &scene_title, &file_name, opts, &sub)?;
        tour.scenes.push(scene);
    }
    if tour.start_scene.is_none() {
        tour.start_scene = tour.scenes.first().map(|s| s.id.clone());
    }
    write_tour(&tour, tour_dir)?;
    Ok(tour)
}

/// Writes `tour.json` via a temporary file + rename so readers never see a partial file.
pub fn write_tour(tour: &Tour, tour_dir: &Path) -> Result<()> {
    tour.validate()?;
    let tmp = tour_dir.join(".tour.json.tmp");
    std::fs::write(&tmp, tour.to_json())?;
    std::fs::rename(&tmp, tour_dir.join("tour.json"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A synthetic equirect: smooth colour field plus a fine lat/long grid.
    pub fn synthetic(w: u32, h: u32) -> image::RgbImage {
        image::RgbImage::from_fn(w, h, |x, y| {
            let u = x as f32 / w as f32;
            let v = y as f32 / h as f32;
            let grid = ((u * 72.0).fract() < 0.03) || ((v * 36.0).fract() < 0.03);
            if grid {
                image::Rgb([250, 250, 250])
            } else {
                image::Rgb([(u * 255.0) as u8, (v * 255.0) as u8, (((u * std::f32::consts::TAU).sin() * 0.5 + 0.5) * 255.0) as u8])
            }
        })
    }

    #[test]
    fn tiles_a_small_pano_and_borders_match_neighbours() {
        let dir = tempfile::tempdir().unwrap();
        let tour = build_tour_from_image(synthetic(2048, 1024), dir.path());
        let s = &tour.scenes[0];
        assert_eq!(s.pyramid.levels, 2);
        let p = &s.pyramid;
        // Every tile exists and has the slot size.
        for t in p.all_tiles() {
            let path = dir.path().join(p.tile_path(t));
            let img = image::open(&path).unwrap();
            assert_eq!(img.width(), p.slot_size());
        }
        // The right border column of front/1/0_1 equals (≈) the first content column of right/1/0_0.
        let a = image::open(dir.path().join(p.tile_path(TileId::new(Face::Front, 1, 1, 0)))).unwrap().into_rgb8();
        let b = image::open(dir.path().join(p.tile_path(TileId::new(Face::Right, 1, 0, 0)))).unwrap().into_rgb8();
        let slot = p.slot_size();
        let mut err = 0.0;
        for y in 1..slot - 1 {
            let pa = a.get_pixel(slot - 1, y);
            let pb = b.get_pixel(1, y);
            for c in 0..3 {
                err += (pa[c] as f32 - pb[c] as f32).abs();
            }
        }
        let mean = err / ((slot - 2) * 3) as f32;
        assert!(mean < 6.0, "border mismatch {mean}");
        assert!(dir.path().join("tour/tour.json").exists());
        assert!(dir.path().join(s.thumb.as_ref().unwrap()).exists());
    }

    fn build_tour_from_image(img: image::RgbImage, dir: &Path) -> Tour {
        let path = dir.join("synthetic_test.png");
        img.save(&path).unwrap();
        let out = dir.join("tour");
        build_tour(&[path], &out, Some("Test"), &TileOptions::default(), &no_progress).unwrap();
        let t = Tour::from_json(&std::fs::read_to_string(out.join("tour.json")).unwrap()).unwrap();
        // Re-root paths at `dir` for the assertions above.
        std::fs::rename(out.join("scenes"), dir.join("scenes")).unwrap();
        t
    }

    #[test]
    fn titles() {
        assert_eq!(title_from_file(Path::new("/x/venice_sunset-8k.jpg")), "Venice Sunset 8k");
    }
}
