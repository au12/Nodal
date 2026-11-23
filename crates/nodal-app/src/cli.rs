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
        /// Never use the learned matcher for photos ordinary matching cannot connect (it downloads
        /// its 65 MB of models the first time it is needed).
        #[arg(long)]
        no_learned: bool,
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
    /// Measure stitching speed on this machine with a built-in synthetic photo set.
    Bench {
        /// Megapixels per synthetic photo (8 fisheye photos covering the sphere).
        #[arg(long, default_value_t = 12.0)]
        megapixels: f32,
        /// Width of the stitched panorama.
        #[arg(long, default_value_t = 8000)]
        width: u32,
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
            no_learned,
            no_level,
            no_trim,
            project,
            tour,
            title,
        } => {
            use nodal_stitch::{features::DetectOptions, project::PoleFill, AlignOptions, LensKind, RenderOptions};
            let ext = out.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
            anyhow::ensure!(
                matches!(ext.as_str(), "jpg" | "jpeg" | "png" | "tif" | "tiff" | "psd" | "psb"),
                "output must be a .jpg (8-bit), .tif, .png or .psd (16-bit) file"
            );
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
            #[allow(unused_mut)]
            let mut align_opts = AlignOptions { lens, optimize_lens: !no_lens_optimization, level: !no_level, ..Default::default() };
            #[cfg(feature = "ml")]
            if !no_learned {
                align_opts.rescue =
                    Some(nodal_stitch::align::RescueHandle(std::sync::Arc::new(nodal_stitch::ml::SourceRescue::new(&sources))));
            }
            #[cfg(not(feature = "ml"))]
            let _ = no_learned;
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
            let pano = nodal_stitch::render(&proj, &sources, &RenderOptions::default(), &|f, s| show(0.5 + f * 0.5, s))?;
            end_line();
            nodal_stitch::io::save_panorama(&pano, &out, quality)?;

            let aligned = proj.images.iter().filter(|i| i.aligned).count();
            println!("Stitched {aligned} of {} photos → {} ({}×{})", proj.images.len(), out.display(), pano.width, pano.height);
            for (i, l) in proj.lenses.iter().enumerate() {
                let n = proj.images.iter().filter(|img| img.lens == i).count();
                println!(
                    "  lens: {} {:.1}° ({:.0} mm equiv.), distortion a {:+.4} b {:+.4} c {:+.4}  [{n} photos]",
                    l.projection_label().to_lowercase(),
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
            let learned = report.pairs.iter().filter(|p| p.learned).count();
            if learned > 0 {
                println!("  learned matcher: connected {learned} pairs ordinary matching could not");
            }
            for line in report.log.iter().filter(|l| l.starts_with("Learned matching unavailable")) {
                println!("  {line}");
            }
            if let Some(local) = report.local.filter(|l| l.images_warped > 0) {
                println!("  local alignment: RMS {:.2} → {:.2} px", local.before, local.after);
            }
            if let Some(d) = &report.dense {
                println!(
                    "  dense registration: {} pairs, misregistration median {:.2} px, 90 % under {:.2} px{}",
                    d.pairs,
                    if d.images_warped > 0 { d.median_warped } else { d.median_after },
                    if d.images_warped > 0 { d.p90_warped } else { d.p90_after },
                    if d.images_warped > 0 { format!(" (local warps on {} photos)", d.images_warped) } else { String::new() }
                );
            }
            if let Some(ph) = &report.photometric {
                println!("  {}", ph.summary().to_lowercase());
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
        Command::Bench { megapixels, width } => bench(megapixels, width)?,
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

/// Stitches a synthetic photo set with known ground truth and reports timings and accuracy.
fn bench(megapixels: f32, width: u32) -> anyhow::Result<()> {
    use nodal_stitch::features::DetectOptions;
    use nodal_stitch::project::{rotation_from_ypr, PoleFill};
    use nodal_stitch::synth::{procedural_pano, render_shot, ShotOptions};
    use nodal_stitch::{AlignOptions, Lens, LensKind, RenderOptions, Source};
    use rayon::prelude::*;
    use std::sync::Arc;

    anyhow::ensure!((1.0..=64.0).contains(&megapixels), "--megapixels must be between 1 and 64");
    anyhow::ensure!((1024..=32768).contains(&width), "--width must be between 1024 and 32768");
    let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
    println!("Nodal {} benchmark", env!("CARGO_PKG_VERSION"));
    println!("  machine: {} {}, {cores} cores", std::env::consts::OS, std::env::consts::ARCH);
    match nodal_render::headless::Headless::new() {
        Ok(h) => println!("  gpu: {}", h.adapter),
        Err(e) => println!("  gpu: none ({e})"),
    }

    // Portrait 2:3 fisheye photos: six around, one up, one down, hand-held wobble included.
    let sw = ((megapixels as f64 * 1e6 / 1.5).sqrt().round() as u32).max(400);
    let sh = sw * 3 / 2;
    let true_fov = 150.0;
    let mut lens = Lens::new(LensKind::Fisheye, true_fov, sw, sh);
    lens.b = -0.03;
    lens.a = 0.01;
    let poses = [
        (0.0, 2.0, 1.0),
        (61.0, -1.0, -2.0),
        (119.0, 3.0, 0.5),
        (182.0, 0.0, 1.5),
        (240.0, -2.0, -1.0),
        (301.0, 1.0, 0.0),
        (10.0, 88.0, 0.0),
        (30.0, -86.0, 5.0),
    ];
    eprintln!("  preparing {} synthetic photos of {sw}×{sh}…", poses.len());
    // Source detail of about one panorama pixel per photo pixel.
    let pano_w = ((sw as f64 * 360.0 / true_fov) as u32).clamp(4096, 16384).next_multiple_of(2);
    let pano = procedural_pano(pano_w, 7);
    let shots: Vec<Arc<image::RgbImage>> = poses
        .par_iter()
        .enumerate()
        .map(|(i, &(y, p, r))| {
            let opts = ShotOptions { gain: 1.0 + 0.15 * ((i % 3) as f32 - 1.0), vignette: 0.25, noise: 2.0, seed: i as u64 };
            Arc::new(render_shot(&pano, &lens, &rotation_from_ypr(y, p, r), &opts))
        })
        .collect();
    drop(pano);

    let t0 = Instant::now();
    let sources: Vec<Source> = shots
        .par_iter()
        .enumerate()
        .map(|(i, s)| Source::from_rgb8(format!("bench{i}.jpg"), s.clone(), Default::default(), &DetectOptions::default()))
        .collect();
    let t_features = t0.elapsed().as_secs_f32();

    let t = Instant::now();
    let (mut proj, report) = nodal_stitch::align_sources(&sources, &AlignOptions::default(), &|_, _| {})?;
    let t_align = t.elapsed().as_secs_f32();
    anyhow::ensure!(report.unaligned.is_empty(), "benchmark photos failed to align");

    proj.output.width = width;
    proj.output.pole_fill = PoleFill::Auto;
    proj.output.nadir_patch = 20.0; // exercise the pole patcher too
    let t = Instant::now();
    let pano = nodal_stitch::render(&proj, &sources, &RenderOptions::default(), &|_, _| {})?;
    let t_render = t.elapsed().as_secs_f32();

    let total_mp = poses.len() as f32 * (sw * sh) as f32 / 1e6;
    let out_mp = (pano.width * pano.height) as f32 / 1e6;
    let est = proj.lenses[0];
    println!("  input: {} photos × {:.1} MP = {total_mp:.0} MP", poses.len(), (sw * sh) as f32 / 1e6);
    println!("  analyse (features):  {t_features:6.2} s   {:.0} MP/s", total_mp / t_features);
    println!("  align + optimise:    {t_align:6.2} s   {} control points, RMS {:.2} px", report.control_points, report.rms);
    println!(
        "  render {}×{}:  {t_render:6.2} s   {:.1} MP/s, nadir patched: {}",
        pano.width,
        pano.height,
        out_mp / t_render,
        pano.stats.filled_nadir
    );
    for (stage, secs) in &pano.stats.timings {
        println!("      {stage:<22} {secs:6.2} s");
    }
    println!("  total:               {:6.2} s", t_features + t_align + t_render);
    println!(
        "  accuracy: lens {} {:.2}° (true: fisheye {true_fov:.2}°), {} photos aligned",
        est.projection_label().to_lowercase(),
        est.hfov,
        proj.images.iter().filter(|i| i.aligned).count()
    );
    Ok(())
}
