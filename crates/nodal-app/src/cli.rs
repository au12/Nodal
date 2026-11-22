//! Command-line tools: tiling, headless rendering, tour inspection.

use clap::{Parser, Subcommand};
use nodal_core::{Projection, Tour, View};
use std::io::IsTerminal as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

#[derive(Parser)]
#[command(name = "nodal", version, about = "Panorama stitcher, viewer and virtual tour editor")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
    /// Tour folder, tour.json, or panoramas to open in the viewer.
    pub paths: Vec<PathBuf>,
}

#[derive(Subcommand)]
pub enum Command {
    /// Tile equirectangular panoramas into a tour (creates or extends OUT/tour.json).
    Tile {
        #[arg(required = true)]
        images: Vec<PathBuf>,
        /// Tour folder to write.
        #[arg(short, long)]
        out: PathBuf,
        #[arg(long)]
        title: Option<String>,
        /// JPEG quality (1-100).
        #[arg(long, default_value_t = 86)]
        quality: u8,
        /// Tile content size in pixels (default: planned automatically, ≤ 510).
        #[arg(long)]
        tile_size: Option<u32>,
        /// Write lossless PNG tiles instead of JPEG.
        #[arg(long)]
        png: bool,
        /// Horizontal field of view of a partial panorama, in degrees.
        #[arg(long)]
        hfov: Option<f32>,
        /// Vertical field of view of a partial panorama, in degrees.
        #[arg(long)]
        vfov: Option<f32>,
        /// Vertical offset (pitch of the image centre) of a partial panorama, in degrees.
        #[arg(long, default_value_t = 0.0)]
        voffset: f32,
    },
    /// Render a still of a scene with the GPU renderer (works headless).
    Render {
        /// Tour folder or tour.json.
        tour: PathBuf,
        #[arg(long)]
        scene: Option<String>,
        #[arg(long)]
        yaw: Option<f32>,
        #[arg(long)]
        pitch: Option<f32>,
        #[arg(long)]
        fov: Option<f32>,
        /// rectilinear | fisheye | stereographic | little-planet | pannini | equirect
        #[arg(long)]
        projection: Option<String>,
        /// Output size, e.g. 3840x2160.
        #[arg(long, default_value = "1920x1080")]
        size: String,
        #[arg(short, long)]
        out: PathBuf,
    },
    /// Print a summary of a tour and any problems.
    Info { tour: PathBuf },
    /// Stitch overlapping photos into a panorama.
    Stitch {
        #[arg(required = true)]
        images: Vec<PathBuf>,
        /// Output panorama (.jpg, .png or .tif).
        #[arg(short, long)]
        out: PathBuf,
        /// Output width in pixels (default: the native resolution of the photos).
        #[arg(long)]
        width: Option<u32>,
        /// Lens type when it cannot be read from EXIF: rectilinear | fisheye (default: detected).
        #[arg(long)]
        lens: Option<String>,
        /// Horizontal field of view of the lens in degrees (with --lens; default: EXIF or estimated).
        #[arg(long)]
        hfov: Option<f64>,
        /// JPEG quality (1-100).
        #[arg(long, default_value_t = 95)]
        quality: u8,
        /// Paint out this many degrees around the nadir (e.g. a tripod), on top of filling real gaps.
        #[arg(long, default_value_t = 0.0)]
        nadir_patch: f32,
        /// Same for the zenith.
        #[arg(long, default_value_t = 0.0)]
        zenith_patch: f32,
        /// Leave zenith and nadir gaps unfilled.
        #[arg(long)]
        no_fill: bool,
        /// Skip exposure and white-balance compensation.
        #[arg(long)]
        no_exposure: bool,
        /// Skip local alignment (per-photo corrections for parallax and lens quirks).
        #[arg(long)]
        no_local: bool,
        /// Keep the lens exactly as given: do not calibrate field of view and distortion.
        #[arg(long)]
        no_lens_optimization: bool,
        /// Do not level the horizon.
        #[arg(long)]
        no_level: bool,
        /// Keep the uneven outline of a partial panorama instead of trimming to a clean rectangle.
        #[arg(long)]
        no_trim: bool,
        /// Also save the stitching project (orientations, lens, control points) to this file.
        #[arg(long)]
        project: Option<PathBuf>,
        /// Also build a tour from the panorama in this folder (open it with `nodal FOLDER`).
        #[arg(long)]
        tour: Option<PathBuf>,
        #[arg(long)]
        title: Option<String>,
    },
}

/// Terminal progress: a live line on a TTY, one line per stage in logs.
fn progress_printer() -> impl Fn(f32, &str) + Sync {
    let tty = std::io::stderr().is_terminal();
    let last_stage = std::sync::Mutex::new(String::new());
    move |f: f32, stage: &str| {
        if tty {
            eprint!("\r\x1b[2K{:>5.1}%  {stage}", f * 100.0);
        } else if let Ok(mut last) = last_stage.lock() {
            if *last != stage {
                eprintln!("{:>5.1}%  {stage}", f * 100.0);
                *last = stage.to_string();
            }
        }
    }
}

fn tour_json(p: &Path) -> PathBuf {
    if p.is_dir() {
        p.join("tour.json")
    } else {
        p.to_path_buf()
    }
}

pub fn run(cmd: Command) -> ExitCode {
    match run_inner(cmd) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn run_inner(cmd: Command) -> anyhow::Result<()> {
    match cmd {
        Command::Tile { images, out, title, quality, tile_size, png, hfov, vfov, voffset } => {
            let coverage = if hfov.is_some() || vfov.is_some() {
                Some(nodal_tiler::sampler::Coverage {
                    hfov: hfov.unwrap_or(360.0).to_radians(),
                    vfov: vfov.unwrap_or(180.0).to_radians(),
                    voffset: voffset.to_radians(),
                })
            } else {
                None
            };
            let opts = nodal_tiler::TileOptions {
                quality,
                tile_size,
                format: if png { nodal_tiler::TileFormat::Png } else { nodal_tiler::TileFormat::Jpeg },
                coverage,
                ..Default::default()
            };
            let t0 = Instant::now();
            // Live single-line progress on a terminal; one line per stage in logs (CI, pipes).
            let tty = std::io::stderr().is_terminal();
            let last_stage = std::sync::Mutex::new(String::new());
            let tour = nodal_tiler::build_tour(&images, &out, title.as_deref(), &opts, &|f, stage| {
                if tty {
                    eprint!("\r\x1b[2K{:>5.1}%  {stage}", f * 100.0);
                } else if let Ok(mut last) = last_stage.lock() {
                    if *last != stage {
                        eprintln!("{:>5.1}%  {stage}", f * 100.0);
                        *last = stage.to_string();
                    }
                }
            })?;
            if tty {
                eprintln!();
            }
            let tiles: u32 = tour.scenes.iter().map(|s| s.pyramid.tile_count()).sum();
            println!("{} scene(s), {} tiles in {:.2?} → {}", tour.scenes.len(), tiles, t0.elapsed(), out.join("tour.json").display());
        }
        Command::Stitch {
            images,
            out,
            width,
            lens,
            hfov,
            quality,
            nadir_patch,
            zenith_patch,
            no_fill,
            no_exposure,
            no_local,
            no_lens_optimization,
            no_level,
            no_trim,
            project,
            tour,
            title,
        } => {
            use nodal_stitch::{features::DetectOptions, project::PoleFill, AlignOptions, LensKind, RenderOptions};
            let ext = out.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
            anyhow::ensure!(matches!(ext.as_str(), "jpg" | "jpeg" | "png" | "tif" | "tiff"), "output must be a .jpg, .png or .tif file");
            let lens = match (lens.as_deref(), hfov) {
                (None, None) => None,
                (kind, fov) => {
                    let kind = match kind.unwrap_or("rectilinear") {
                        "rectilinear" | "rect" | "normal" => LensKind::Rectilinear,
                        "fisheye" => LensKind::Fisheye,
                        "fisheye-equisolid" | "equisolid" => LensKind::FisheyeEquisolid,
                        "fisheye-stereographic" => LensKind::FisheyeStereographic,
                        other => anyhow::bail!("unknown lens type '{other}' (use rectilinear or fisheye)"),
                    };
                    let fov = fov.ok_or_else(|| anyhow::anyhow!("--lens needs --hfov (the lens's horizontal field of view in degrees)"))?;
                    anyhow::ensure!(fov > 1.0 && fov < 300.0, "--hfov must be between 1 and 300 degrees");
                    Some((kind, fov))
                }
            };
            let t0 = Instant::now();
            let show = progress_printer();
            let tty = std::io::stderr().is_terminal();
            let end_line = || {
                if tty {
                    eprintln!();
                }
            };
            let sources = nodal_stitch::load_sources(&images, &DetectOptions::default(), &|f, s| show(f * 0.25, s))?;
            let t_load = t0.elapsed();
            let align_opts = AlignOptions { lens, optimize_lens: !no_lens_optimization, level: !no_level, ..Default::default() };
            let (mut proj, report) = nodal_stitch::align_sources(&sources, &align_opts, &|f, s| show(0.25 + f * 0.25, s))?;
            let t_align = t0.elapsed();
            proj.output.width = width.unwrap_or(0);
            proj.output.jpeg_quality = quality;
            proj.output.exposure_compensation = !no_exposure;
            proj.output.local_alignment = !no_local;
            proj.output.trim_edges = !no_trim;
            proj.output.pole_fill = if no_fill { PoleFill::Off } else { PoleFill::Auto };
            proj.output.nadir_patch = nadir_patch;
            proj.output.zenith_patch = zenith_patch;
            let pixels: Vec<_> = sources.iter().map(|s| s.image.clone()).collect();
            let pano = nodal_stitch::render(&proj, &pixels, &RenderOptions::default(), &|f, s| show(0.5 + f * 0.5, s))?;
            end_line();
            nodal_stitch::io::save_panorama(&pano, &out, quality)?;

            let aligned = proj.images.iter().filter(|i| i.aligned).count();
            println!("Stitched {aligned} of {} photos → {} ({}×{})", proj.images.len(), out.display(), pano.width, pano.height);
            for (i, l) in proj.lenses.iter().enumerate() {
                let n = proj.images.iter().filter(|img| img.lens == i).count();
                println!(
                    "  lens: {} {:.1}° ({:.0} mm equiv.), distortion a {:+.4} b {:+.4} c {:+.4}  [{n} photos]",
                    l.kind.label().to_lowercase(),
                    l.hfov,
                    l.focal_35mm(),
                    l.a,
                    l.b,
                    l.c
                );
            }
            println!(
                "  control points: {} in {} pairs, RMS {:.2} px, max {:.2} px",
                report.control_points,
                report.pairs.len(),
                report.rms,
                report.max
            );
            if let Some(local) = report.local.filter(|l| l.images_warped > 0) {
                println!("  local alignment: RMS {:.2} → {:.2} px", local.before, local.after);
            }
            let (hf, vf, _) = pano.coverage_degrees();
            if pano.is_full_sphere() {
                let filled: Vec<&str> = [(pano.stats.filled_zenith, "zenith"), (pano.stats.filled_nadir, "nadir")]
                    .iter()
                    .filter(|f| f.0)
                    .map(|f| f.1)
                    .collect();
                println!(
                    "  coverage: full 360° × 180°{}",
                    if filled.is_empty() { String::new() } else { format!(", patched {}", filled.join(" and ")) }
                );
            } else {
                println!("  coverage: {hf:.0}° × {vf:.0}°");
            }
            if pano.has_gaps() {
                println!(
                    "  note: the panorama has uncovered areas{}",
                    if ext == "jpg" || ext == "jpeg" { " (black in JPEG; use .png or .tif to keep them transparent)" } else { "" }
                );
            }
            for i in &report.unaligned {
                println!("  warning: {} does not overlap the others enough and was left out", proj.images[*i].name);
            }
            println!(
                "  time: load {:.1}s, align {:.1}s, render {:.1}s",
                t_load.as_secs_f32(),
                (t_align - t_load).as_secs_f32(),
                (t0.elapsed() - t_align).as_secs_f32()
            );
            if let Some(path) = project {
                std::fs::write(&path, proj.to_json())?;
                println!("  project saved to {}", path.display());
            }
            if let Some(dir) = tour {
                let (hf, vf, voff) = pano.coverage_degrees();
                let coverage = (!pano.is_full_sphere()).then(|| nodal_tiler::sampler::Coverage {
                    hfov: hf.to_radians(),
                    vfov: vf.to_radians(),
                    voffset: voff.to_radians(),
                });
                let opts = nodal_tiler::TileOptions { coverage, ..Default::default() };
                let t = nodal_tiler::build_tour(std::slice::from_ref(&out), &dir, title.as_deref(), &opts, &show)?;
                end_line();
                println!("  tour with {} scene(s) → {}", t.scenes.len(), dir.join("tour.json").display());
            }
        }
        Command::Render { tour, scene, yaw, pitch, fov, projection, size, out } => {
            let json = tour_json(&tour);
            let t = Tour::from_json(&std::fs::read_to_string(&json)?)?;
            let s = match &scene {
                Some(id) => t.scene(id).ok_or_else(|| anyhow::anyhow!("no scene '{id}'"))?,
                None => &t.scenes[t.start_index()],
            };
            let (w, h) = size.split_once('x').ok_or_else(|| anyhow::anyhow!("size must look like 1920x1080"))?;
            let (w, h): (u32, u32) = (w.parse()?, h.parse()?);
            let mut view: View = s.view;
            let preset = projection.as_deref().map(|p| match p {
                "rectilinear" => Ok((Projection::RECTILINEAR, None)),
                "fisheye" => Ok((Projection::FISHEYE, Some(150.0))),
                "stereographic" => Ok((Projection::STEREOGRAPHIC, Some(200.0))),
                "little-planet" => Ok((Projection::STEREOGRAPHIC, Some(290.0))),
                "pannini" => Ok((Projection::PANNINI, Some(130.0))),
                "equirect" => Ok((Projection::EQUIRECT, Some(360.0))),
                other => Err(anyhow::anyhow!("unknown projection '{other}'")),
            });
            if let Some(p) = preset {
                let (proj, default_fov) = p?;
                view.projection = proj;
                if let Some(f) = default_fov {
                    view.fov = f;
                }
                if projection.as_deref() == Some("little-planet") {
                    view.pitch = -90.0;
                }
            }
            if let Some(v) = yaw {
                view.yaw = v;
            }
            if let Some(v) = pitch {
                view.pitch = v;
            }
            if let Some(v) = fov {
                view.fov = v;
            }
            let base = json.parent().map(|p| p.to_string_lossy().to_string()).unwrap_or_default();
            let mut hl = nodal_render::headless::Headless::new().map_err(|e| anyhow::anyhow!(e))?;
            let t0 = Instant::now();
            let img = hl
                .render(&base, s, t.settings.background, &view, w, h, std::time::Duration::from_secs(120))
                .map_err(|e| anyhow::anyhow!(e))?;
            image::DynamicImage::ImageRgba8(img).to_rgb8().save(&out)?;
            println!("rendered {}×{} on {} in {:.2?} → {}", w, h, hl.adapter, t0.elapsed(), out.display());
        }
        Command::Info { tour } => {
            let json = tour_json(&tour);
            let t = Tour::from_json(&std::fs::read_to_string(&json)?)?;
            println!("{} — {} scene(s)", if t.title.is_empty() { "(untitled)" } else { &t.title }, t.scenes.len());
            for s in &t.scenes {
                let src = s.source.as_ref().map(|s| format!("{}×{}", s.width, s.height)).unwrap_or_default();
                println!(
                    "  {:<24} {:<28} {:>2} levels, {:>5}px faces, {:>4} tiles, {} hotspots {}",
                    s.id,
                    s.title,
                    s.pyramid.levels,
                    s.pyramid.max_face_size(),
                    s.pyramid.tile_count(),
                    s.hotspots.len(),
                    src
                );
            }
            for w in t.warnings() {
                println!("warning: {w}");
            }
        }
    }
    Ok(())
}
