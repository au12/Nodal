//! Command-line tools: tiling, headless rendering, tour inspection.

use clap::{Parser, Subcommand};
use nodal_core::{Projection, Tour, View};
use std::io::IsTerminal as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

#[derive(Parser)]
#[command(name = "nodal", version, about = "Fast GPU panorama viewer, tiler and tour editor")]
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
