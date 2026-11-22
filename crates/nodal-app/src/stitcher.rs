//! The stitching workspace: turn a set of overlapping photos into a panorama.
//!
//! Photos on the left, a live preview in the middle, lens / alignment / output settings on the
//! right. All heavy work runs on background threads; the UI only ever shows progress.

use crate::app::{NodalApp, Screen};
use crate::icons::{self, Icon};
use crate::theme;
use eframe::egui_wgpu;
use egui::{pos2, vec2, Align2, Color32, Rect, RichText, Sense, Stroke, TextureHandle, TextureOptions, Ui};
use image::RgbImage;
use nodal_render::{ComposeFrame, ComposePhoto, Composer};
use nodal_stitch::align::LensSource;
use nodal_stitch::features::DetectOptions;
use nodal_stitch::nalgebra::{Rotation3, Vector3};
use nodal_stitch::project::PoleFill;
use nodal_stitch::warp::Filter;
use nodal_stitch::{AlignOptions, AlignReport, LensKind, Panorama, Project, RenderOptions, Source};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Instant;

struct Photo {
    name: String,
    summary: String,
    /// Reduced copy used for previews.
    small: Arc<RgbImage>,
    thumb: TextureHandle,
    /// The photo resampled to the GPU preview's layer size (RGBA).
    layer: Arc<Vec<u8>>,
}

/// Size every photo is resampled to for the GPU preview (the shader works in normalised coordinates).
const LAYER_W: u32 = 1024;
const LAYER_H: u32 = 768;

enum Msg {
    Loaded { sources: Vec<Source>, small: Vec<Arc<RgbImage>>, layers: Vec<Arc<Vec<u8>>> },
    Aligned { project: Box<Project>, report: Option<Box<AlignReport>>, secs: f32 },
    Preview { pano: Box<Panorama>, generation: u64 },
    Saved { path: PathBuf, width: usize, height: usize, secs: f32, make_tour: bool },
    Failed(String),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum LensMode {
    Auto,
    Rectilinear,
    Fisheye,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Format {
    Jpeg,
    Tiff,
    Png,
}

impl Format {
    fn ext(self) -> &'static str {
        match self {
            Format::Jpeg => "jpg",
            Format::Tiff => "tif",
            Format::Png => "png",
        }
    }
}

struct Preview {
    tex: TextureHandle,
    full_sphere: bool,
    hfov: f32,
    vfov: f32,
    has_gaps: bool,
    filled: (bool, bool),
    /// Canvas window it shows: yaw at the left edge, yaw span, pitch at the top, pitch span (radians).
    window: [f32; 4],
}

struct Saved {
    path: PathBuf,
    width: usize,
    height: usize,
    secs: f32,
}

pub struct Stitcher {
    photos: Vec<Photo>,
    sources: Vec<Source>,
    project: Option<Project>,
    report: Option<AlignReport>,
    align_secs: f32,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    /// Title of the running job, if any, with shared progress.
    job: Option<&'static str>,
    progress: Arc<Mutex<(f32, String)>>,
    preview: Option<Preview>,
    preview_generation: u64,
    preview_running: bool,
    preview_stale: bool,
    lens_mode: LensMode,
    lens_fov: f64,
    optimize_lens: bool,
    width_choice: usize,
    custom_width: u32,
    format: Format,
    quality: u8,
    saved: Option<Saved>,
    error: Option<String>,
    /// Live horizontal drag of the preview, in fractions of its width.
    drag_x: f32,
    drag_y: f32,
    roll: f32,
    show_points: bool,
    grid: bool,
    /// Photos waiting to be loaded once a frame (and its context) is available.
    pending: Vec<PathBuf>,
    /// The GPU preview's photo textures need (re)uploading.
    gpu_dirty: bool,
    gpu_ready: bool,
    /// Exposure gains from the latest rendered preview, reused by the GPU preview.
    gains: Vec<[f32; 3]>,
    /// Show the GPU preview until the next rendered preview arrives (the panorama just moved).
    live: bool,
}

const WIDTHS: [(&str, u32); 7] =
    [("Native", 0), ("16384 px", 16384), ("12000 px", 12000), ("8192 px", 8192), ("6000 px", 6000), ("4096 px", 4096), ("Custom", 1)];

impl Default for Stitcher {
    fn default() -> Self {
        let (tx, rx) = channel();
        Stitcher {
            photos: Vec::new(),
            sources: Vec::new(),
            project: None,
            report: None,
            align_secs: 0.0,
            tx,
            rx,
            job: None,
            progress: Arc::new(Mutex::new((0.0, String::new()))),
            preview: None,
            preview_generation: 0,
            preview_running: false,
            preview_stale: false,
            lens_mode: LensMode::Auto,
            lens_fov: 0.0,
            optimize_lens: true,
            width_choice: 0,
            custom_width: 8192,
            format: Format::Jpeg,
            quality: 95,
            saved: None,
            error: None,
            drag_x: 0.0,
            drag_y: 0.0,
            roll: 0.0,
            show_points: false,
            grid: false,
            pending: Vec::new(),
            gpu_dirty: false,
            gpu_ready: false,
            gains: Vec::new(),
            live: false,
        }
    }
}

fn is_photo(p: &Path) -> bool {
    matches!(
        p.extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase()).as_deref(),
        Some("jpg" | "jpeg" | "png" | "tif" | "tiff" | "webp")
    )
}

/// True when every image looks like a finished equirectangular panorama (2:1) rather than a
/// photo to be stitched.
pub fn all_panoramas(paths: &[PathBuf]) -> bool {
    paths.iter().all(|p| match image::image_dimensions(p) {
        Ok((w, h)) => (w as f32 / h as f32 - 2.0).abs() < 0.03,
        Err(_) => true,
    })
}

impl Stitcher {
    pub fn busy(&self) -> bool {
        self.job.is_some()
    }

    fn start(&mut self, title: &'static str) -> (Sender<Msg>, Arc<Mutex<(f32, String)>>) {
        self.job = Some(title);
        self.error = None;
        *self.progress.lock().unwrap() = (0.0, title.to_string());
        (self.tx.clone(), self.progress.clone())
    }

    /// Loads photos (and analyses them) in the background and adds them to the set.
    pub fn add_photos(&mut self, ctx: &egui::Context, paths: Vec<PathBuf>) {
        let mut paths: Vec<PathBuf> = paths.into_iter().filter(|p| is_photo(p)).collect();
        paths.retain(|p| !self.sources.iter().any(|s| s.path == *p));
        paths.sort();
        if paths.is_empty() || self.busy() {
            return;
        }
        let (tx, progress) = self.start("Loading photos");
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let report = |f: f32, s: &str| {
                *progress.lock().unwrap() = (f, s.to_string());
                ctx.request_repaint();
            };
            let msg = match nodal_stitch::load_sources(&paths, &DetectOptions::default(), &report) {
                Ok(sources) => {
                    let small = sources
                        .iter()
                        .map(|s| {
                            let (w, h) = (s.image.width(), s.image.height());
                            let k = (1280.0 / w.max(h) as f32).min(1.0);
                            Arc::new(image::imageops::thumbnail(&*s.image, ((w as f32 * k) as u32).max(1), ((h as f32 * k) as u32).max(1)))
                        })
                        .collect::<Vec<Arc<RgbImage>>>();
                    let layers = small
                        .iter()
                        .map(|sm| {
                            let img = image::imageops::resize(&**sm, LAYER_W, LAYER_H, image::imageops::FilterType::Triangle);
                            let mut rgba = Vec::with_capacity((LAYER_W * LAYER_H * 4) as usize);
                            for p in img.pixels() {
                                rgba.extend_from_slice(&[p[0], p[1], p[2], 255]);
                            }
                            Arc::new(rgba)
                        })
                        .collect();
                    Msg::Loaded { sources, small, layers }
                }
                Err(e) => Msg::Failed(format!("{e:#}")),
            };
            let _ = tx.send(msg);
            ctx.request_repaint();
        });
    }

    fn align_options(&self) -> AlignOptions {
        let lens = match (self.lens_mode, self.lens_fov) {
            (LensMode::Auto, _) => None,
            (_, f) if f < 1.0 => None,
            (LensMode::Rectilinear, f) => Some((LensKind::Rectilinear, f)),
            (LensMode::Fisheye, f) => Some((LensKind::Fisheye, f)),
        };
        AlignOptions { lens, optimize_lens: self.optimize_lens, ..Default::default() }
    }

    fn align(&mut self, ctx: &egui::Context) {
        if self.busy() || self.sources.len() < 2 {
            return;
        }
        let (tx, progress) = self.start("Aligning");
        let ctx = ctx.clone();
        let sources = self.sources.clone();
        let opts = self.align_options();
        let output = self.project.as_ref().map(|p| p.output.clone());
        std::thread::spawn(move || {
            let t = Instant::now();
            let report = |f: f32, s: &str| {
                *progress.lock().unwrap() = (f, s.to_string());
                ctx.request_repaint();
            };
            let msg = match nodal_stitch::align_sources(&sources, &opts, &report) {
                Ok((mut project, rep)) => {
                    if let Some(o) = output {
                        project.output = o;
                    }
                    Msg::Aligned { project: Box::new(project), report: Some(Box::new(rep)), secs: t.elapsed().as_secs_f32() }
                }
                Err(e) => Msg::Failed(format!("{e:#}")),
            };
            let _ = tx.send(msg);
            ctx.request_repaint();
        });
    }

    /// Project scaled to the reduced preview images.
    fn preview_project(&self) -> Option<Project> {
        let mut p = self.project.clone()?;
        for (li, lens) in p.lenses.iter_mut().enumerate() {
            let Some(idx) = self.project.as_ref()?.images.iter().position(|i| i.lens == li) else { continue };
            let k = self.photos[idx].small.width() as f64 / lens.width as f64;
            lens.width = self.photos[idx].small.width();
            lens.height = self.photos[idx].small.height();
            lens.d *= k;
            lens.e *= k;
        }
        for (i, img) in p.images.iter_mut().enumerate() {
            let k = self.photos[i].small.width() as f32 / img.width as f32;
            img.width = self.photos[i].small.width();
            img.height = self.photos[i].small.height();
            if let Some(w) = &mut img.warp {
                w.dx.iter_mut().for_each(|v| *v *= k);
                w.dy.iter_mut().for_each(|v| *v *= k);
            }
        }
        Some(p)
    }

    fn request_preview(&mut self, ctx: &egui::Context) {
        if self.preview_running {
            self.preview_stale = true;
            return;
        }
        let Some(mut project) = self.preview_project() else { return };
        self.preview_running = true;
        self.preview_stale = false;
        self.preview_generation += 1;
        let generation = self.preview_generation;
        let images: Vec<Arc<RgbImage>> = self.photos.iter().map(|p| p.small.clone()).collect();
        let tx = self.tx.clone();
        let ctx = ctx.clone();
        project.output.width = 0;
        std::thread::spawn(move || {
            let opts = RenderOptions { width: 1600, seam_width: 800, filter: Filter::Bilinear, bands: Some(4), ..Default::default() };
            let msg = match nodal_stitch::render(&project, &images, &opts, &|_, _| {}) {
                Ok(pano) => Msg::Preview { pano: Box::new(pano), generation },
                Err(e) => Msg::Failed(format!("{e:#}")),
            };
            let _ = tx.send(msg);
            ctx.request_repaint();
        });
    }

    fn output_width(&self) -> u32 {
        match WIDTHS[self.width_choice].1 {
            1 => self.custom_width.clamp(512, 65536),
            w => w,
        }
    }

    fn export(&mut self, ctx: &egui::Context, path: PathBuf, make_tour: bool) {
        let Some(mut project) = self.project.clone() else { return };
        if self.busy() {
            return;
        }
        let (tx, progress) = self.start("Creating panorama");
        project.output.width = self.output_width();
        let quality = self.quality;
        let images: Vec<Arc<RgbImage>> = self.sources.iter().map(|s| s.image.clone()).collect();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let t = Instant::now();
            let report = |f: f32, s: &str| {
                *progress.lock().unwrap() = (f * 0.95, s.to_string());
                ctx.request_repaint();
            };
            let result = nodal_stitch::render(&project, &images, &RenderOptions::default(), &report).and_then(|pano| {
                report(1.0, "Saving");
                if let Some(dir) = path.parent() {
                    std::fs::create_dir_all(dir)?;
                }
                nodal_stitch::io::save_panorama(&pano, &path, quality)?;
                Ok((pano.width, pano.height))
            });
            let msg = match result {
                Ok((width, height)) => Msg::Saved { path, width, height, secs: t.elapsed().as_secs_f32(), make_tour },
                Err(e) => Msg::Failed(format!("{e:#}")),
            };
            let _ = tx.send(msg);
            ctx.request_repaint();
        });
    }

    /// Re-optimises after control points changed, without re-detecting anything.
    fn reoptimise(&mut self, ctx: &egui::Context) {
        let Some(mut project) = self.project.clone() else { return };
        if self.busy() {
            return;
        }
        let (tx, _) = self.start("Optimising");
        let opts = self.align_options();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let t = Instant::now();
            nodal_stitch::align::polish(&mut project, &opts);
            if project.output.local_alignment {
                nodal_stitch::local::fit(&mut project);
            }
            let _ = tx.send(Msg::Aligned { project: Box::new(project), report: None, secs: t.elapsed().as_secs_f32() });
            ctx.request_repaint();
        });
    }

    fn rotate(&mut self, ctx: &egui::Context, g: Rotation3<f64>) {
        if let Some(p) = &mut self.project {
            p.rotate_world(&g);
            self.live = self.gpu_ready;
            self.request_preview(ctx);
        }
    }

    /// Per-photo parameters for the GPU preview.
    fn compose_photos(&self) -> Vec<ComposePhoto> {
        let Some(p) = self.preview_project() else { return Vec::new() };
        p.images
            .iter()
            .enumerate()
            .map(|(i, img)| {
                let m = p.lenses[img.lens].model();
                let r = img.rot();
                let r = r.matrix();
                let kind = match m.kind {
                    LensKind::Rectilinear => 0.0,
                    LensKind::Fisheye => 1.0,
                    LensKind::FisheyeEquisolid => 2.0,
                    LensKind::FisheyeStereographic => 3.0,
                };
                let g = self.gains.get(i).copied().unwrap_or([1.0; 3]);
                ComposePhoto {
                    // World → camera is the transpose of camera → world: its rows are the columns of `r`.
                    r0: [r[(0, 0)] as f32, r[(1, 0)] as f32, r[(2, 0)] as f32, kind],
                    r1: [r[(0, 1)] as f32, r[(1, 1)] as f32, r[(2, 1)] as f32, m.f as f32],
                    r2: [r[(0, 2)] as f32, r[(1, 2)] as f32, r[(2, 2)] as f32, m.r0 as f32],
                    centre: [m.cx as f32, m.cy as f32, m.w as f32, m.h as f32],
                    poly: [m.a as f32, m.b as f32, m.c as f32, m.d0 as f32],
                    limits: [m.rho_max.min(1e9) as f32, m.theta_max as f32, 0.0, 0.0],
                    gain: [g[0], g[1], g[2], if img.aligned && img.enabled { 1.0 } else { 0.0 }],
                }
            })
            .collect()
    }

    fn default_name(&self) -> String {
        let stem = self
            .sources
            .first()
            .and_then(|s| s.path.file_stem())
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "panorama".into());
        format!("{stem} panorama")
    }

    fn reset(&mut self) {
        *self = Stitcher::default();
    }
}

/// Handles finished background work. Call once per frame.
pub fn poll(app: &mut NodalApp, ctx: &egui::Context) {
    if !app.stitcher.pending.is_empty() && !app.stitcher.busy() {
        let paths = std::mem::take(&mut app.stitcher.pending);
        app.stitcher.add_photos(ctx, paths);
    }
    while let Ok(msg) = app.stitcher.rx.try_recv() {
        let s = &mut app.stitcher;
        match msg {
            Msg::Loaded { sources, small, layers } => {
                s.job = None;
                s.gpu_dirty = true;
                for ((src, sm), layer) in sources.into_iter().zip(small).zip(layers) {
                    let thumb_img = image::imageops::thumbnail(&*sm, 160, (160 * sm.height() / sm.width().max(1)).max(1));
                    let color = egui::ColorImage::from_rgb([thumb_img.width() as usize, thumb_img.height() as usize], thumb_img.as_raw());
                    let name = src.path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                    let thumb = ctx.load_texture(format!("stitch-thumb-{name}"), color, TextureOptions::LINEAR);
                    let mut summary = src.exif.summary();
                    if summary.is_empty() {
                        summary = format!("{} × {}", src.image.width(), src.image.height());
                    }
                    s.photos.push(Photo { name, summary, small: sm, thumb, layer });
                    s.sources.push(src);
                }
                // New photos invalidate the alignment.
                s.project = None;
                s.report = None;
                s.preview = None;
                s.saved = None;
                if s.sources.len() >= 2 {
                    s.align(ctx);
                }
            }
            Msg::Aligned { project, report, secs } => {
                s.job = None;
                s.gains.clear();
                s.project = Some(*project);
                if let Some(r) = report {
                    s.report = Some(*r);
                    s.align_secs = secs;
                }
                s.saved = None;
                s.roll = 0.0;
                s.request_preview(ctx);
            }
            Msg::Preview { pano, generation } => {
                s.preview_running = false;
                if generation == s.preview_generation {
                    let mut rgba = Vec::with_capacity(pano.width * pano.height * 4);
                    for (p, a) in pano.rgb.as_chunks::<3>().0.iter().zip(&pano.alpha) {
                        rgba.extend_from_slice(&[p[0], p[1], p[2], *a]);
                    }
                    let img = egui::ColorImage::from_rgba_unmultiplied([pano.width, pano.height], &rgba);
                    let tex = ctx.load_texture("stitch-preview", img, TextureOptions::LINEAR);
                    let (hfov, vfov, _) = pano.coverage_degrees();
                    let (fw, fh) = (pano.full_width as f32, pano.full_height as f32);
                    let window = [
                        (pano.crop_x as f32 / fw - 0.5) * std::f32::consts::TAU,
                        pano.width as f32 / fw * std::f32::consts::TAU,
                        (0.5 - pano.crop_y as f32 / fh) * std::f32::consts::PI,
                        pano.height as f32 / fh * std::f32::consts::PI,
                    ];
                    s.gains = pano.stats.gains.clone();
                    s.live = false;
                    s.preview = Some(Preview {
                        tex,
                        full_sphere: pano.is_full_sphere(),
                        hfov,
                        vfov,
                        has_gaps: pano.has_gaps(),
                        filled: (pano.stats.filled_zenith, pano.stats.filled_nadir),
                        window,
                    });
                    s.drag_x = 0.0;
                    s.drag_y = 0.0;
                }
                if s.preview_stale {
                    s.request_preview(ctx);
                }
            }
            Msg::Saved { path, width, height, secs, make_tour } => {
                s.job = None;
                s.saved = Some(Saved { path: path.clone(), width, height, secs });
                if make_tour {
                    let title = path.file_stem().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "Panorama".into());
                    let dir = path.parent().map(|d| d.to_path_buf()).unwrap_or_default();
                    app.library.job = Some((0.0, format!("Tiling {title}…")));
                    app.loader.tile_local(vec![path], dir, Some(title), true);
                    app.screen = Screen::Library;
                } else {
                    app.toast(ctx, "Panorama saved", false);
                }
            }
            Msg::Failed(e) => {
                s.job = None;
                s.preview_running = false;
                log::error!("{e}");
                s.error = Some(e);
            }
        }
    }
}

/// Opens the workspace, adding these photos (possibly none) to it.
pub fn open(app: &mut NodalApp, paths: Vec<PathBuf>) {
    if app.screen != Screen::Stitch {
        app.stitcher.reset();
        app.screen = Screen::Stitch;
    }
    app.stitcher.pending.extend(paths);
}

fn section(ui: &mut Ui, title: &str) {
    ui.add_space(14.0);
    ui.label(RichText::new(title.to_uppercase()).font(theme::heading_font(11.0)).color(theme::MUTED).extra_letter_spacing(0.8));
    ui.add_space(4.0);
}

fn stat(ui: &mut Ui, label: &str, value: impl Into<String>) {
    ui.horizontal(|ui| {
        ui.label(RichText::new(label).color(theme::MUTED));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.label(RichText::new(value.into()).color(theme::TEXT));
        });
    });
}

fn reveal(path: &Path) {
    let dir = path.parent().unwrap_or(path);
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg("-R").arg(path).spawn();
    #[cfg(target_os = "windows")]
    let _ = std::process::Command::new("explorer").arg(format!("/select,{}", path.display())).spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    let _ = std::process::Command::new("xdg-open").arg(dir).spawn();
    let _ = dir;
}

/// Draws the GPU preview inside egui's render pass.
struct ComposeCallback {
    frame: ComposeFrame,
}

impl egui_wgpu::CallbackTrait for ComposeCallback {
    fn prepare(
        &self,
        _device: &eframe::wgpu::Device,
        queue: &eframe::wgpu::Queue,
        _screen: &egui_wgpu::ScreenDescriptor,
        _encoder: &mut eframe::wgpu::CommandEncoder,
        resources: &mut egui_wgpu::CallbackResources,
    ) -> Vec<eframe::wgpu::CommandBuffer> {
        if let Some(c) = resources.get::<Composer>() {
            c.write(queue, &self.frame);
        }
        vec![]
    }

    fn paint(
        &self,
        _info: egui::PaintCallbackInfo,
        rpass: &mut eframe::wgpu::RenderPass<'static>,
        resources: &egui_wgpu::CallbackResources,
    ) {
        if let Some(c) = resources.get::<Composer>() {
            c.paint(rpass);
        }
    }
}

pub fn show(app: &mut NodalApp, ui: &mut Ui, frame: &mut eframe::Frame) {
    let ctx = ui.ctx().clone();
    // Keep the GPU preview's photo textures in step with the photo set.
    if app.stitcher.gpu_dirty {
        if let Some(rs) = frame.wgpu_render_state() {
            let s = &mut app.stitcher;
            let mut renderer = rs.renderer.write();
            if let Some(c) = renderer.callback_resources.get_mut::<Composer>() {
                if !s.photos.is_empty() && s.photos.len() <= nodal_render::compose::MAX_PHOTOS {
                    let layers: Vec<&[u8]> = s.photos.iter().map(|p| p.layer.as_slice()).collect();
                    c.set_layers(&rs.device, &rs.queue, LAYER_W, LAYER_H, &layers);
                    s.gpu_ready = true;
                } else {
                    c.clear();
                    s.gpu_ready = false;
                }
            }
            s.gpu_dirty = false;
        }
    }
    top_bar(app, ui);
    photo_panel(app, ui);
    settings_panel(app, ui);
    egui::CentralPanel::default().frame(egui::Frame::new().fill(theme::BG)).show(ui, |ui| centre(app, ui));
    if ctx.input(|i| i.key_pressed(egui::Key::Escape)) && !app.stitcher.busy() {
        app.screen = Screen::Library;
    }
    if app.stitcher.busy() || app.stitcher.preview_running {
        ctx.request_repaint_after(std::time::Duration::from_millis(100));
    }
}

fn top_bar(app: &mut NodalApp, ui: &mut Ui) {
    egui::Panel::top("stitch_bar")
        .exact_size(56.0)
        .resizable(false)
        .frame(egui::Frame::new().fill(theme::PANEL).inner_margin(egui::Margin::symmetric(10, 8)).stroke(theme::hairline()))
        .show(ui, |ui| {
            ui.horizontal_centered(|ui| {
                if icons::button(ui, Icon::Back, "Back to the library (Esc)", false).clicked() && !app.stitcher.busy() {
                    app.screen = Screen::Library;
                }
                ui.label(RichText::new("Stitch").font(theme::heading_font(18.0)));
                let s = &app.stitcher;
                let status = match (&s.project, s.photos.len()) {
                    (_, 0) => "Add overlapping photos to begin".to_string(),
                    (None, n) => format!("{n} photo{}", if n == 1 { "" } else { "s" }),
                    (Some(p), n) => {
                        let aligned = p.images.iter().filter(|i| i.aligned).count();
                        format!("{aligned} of {n} photos aligned")
                    }
                };
                ui.label(RichText::new(status).color(theme::MUTED));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let ready = app.stitcher.project.is_some() && !app.stitcher.busy();
                    ui.add_enabled_ui(ready, |ui| {
                        if icons::pill(ui, Icon::Play, "Create tour", false)
                            .on_hover_text("Stitch at full resolution and open the result as a virtual tour")
                            .clicked()
                        {
                            let name = app.stitcher.default_name();
                            let slug = nodal_core::tour::slugify(&name);
                            let base = crate::platform::tours_dir();
                            let mut dir = base.join(&slug);
                            let mut n = 2;
                            while dir.exists() {
                                dir = base.join(format!("{slug}-{n}"));
                                n += 1;
                            }
                            let path = dir.join(format!("{name}.jpg"));
                            app.stitcher.export(ui.ctx(), path, true);
                        }
                        if icons::pill(ui, Icon::Check, "Save panorama…", true).clicked() {
                            let ext = app.stitcher.format.ext();
                            let mut dialog = rfd::FileDialog::new()
                                .set_title("Save panorama")
                                .set_file_name(format!("{}.{ext}", app.stitcher.default_name()))
                                .add_filter("Image", &[ext]);
                            if let Some(dir) = app.stitcher.sources.first().and_then(|s| s.path.parent()) {
                                dialog = dialog.set_directory(dir);
                            }
                            if let Some(mut path) = dialog.save_file() {
                                if path.extension().is_none() {
                                    path.set_extension(ext);
                                }
                                app.stitcher.export(ui.ctx(), path, false);
                            }
                        }
                    });
                });
            });
        });
}

fn photo_panel(app: &mut NodalApp, ui: &mut Ui) {
    egui::Panel::left("stitch_photos")
        .exact_size(264.0)
        .resizable(false)
        .frame(egui::Frame::new().fill(theme::PANEL).inner_margin(12).stroke(theme::hairline()))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("PHOTOS").font(theme::heading_font(11.0)).color(theme::MUTED).extra_letter_spacing(0.8));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.add_enabled(!app.stitcher.busy(), egui::Button::new("Add…").small()).clicked() {
                        if let Some(files) =
                            rfd::FileDialog::new().add_filter("Photos", &["jpg", "jpeg", "png", "tif", "tiff", "webp"]).pick_files()
                        {
                            app.stitcher.add_photos(ui.ctx(), files);
                        }
                    }
                });
            });
            ui.add_space(6.0);
            let s = &mut app.stitcher;
            let mut toggled = false;
            // Control points per photo, for the status line.
            let mut counts = vec![0usize; s.photos.len()];
            if let Some(p) = &s.project {
                for cp in &p.control_points {
                    counts[cp.a as usize] += 1;
                    counts[cp.b as usize] += 1;
                }
            }
            egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
                for (i, photo) in s.photos.iter().enumerate() {
                    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 54.0), Sense::hover());
                    let t = ui.ctx().animate_bool(resp.id, resp.hovered());
                    ui.painter().rect_filled(rect, 10.0, icons::lerp_color(Color32::TRANSPARENT, Color32::from_rgb(32, 35, 42), t));
                    let thumb = Rect::from_min_size(rect.min + vec2(6.0, 6.0), vec2(60.0, 42.0));
                    // Centre-crop the thumbnail into its slot.
                    let ts = photo.thumb.size_vec2();
                    let slot_aspect = thumb.width() / thumb.height();
                    let uv = if ts.x / ts.y > slot_aspect {
                        let k = slot_aspect / (ts.x / ts.y);
                        Rect::from_min_max(pos2(0.5 - k / 2.0, 0.0), pos2(0.5 + k / 2.0, 1.0))
                    } else {
                        let k = (ts.x / ts.y) / slot_aspect;
                        Rect::from_min_max(pos2(0.0, 0.5 - k / 2.0), pos2(1.0, 0.5 + k / 2.0))
                    };
                    egui::Image::new(&photo.thumb).uv(uv).corner_radius(6).paint_at(ui, thumb);
                    let painter = ui.painter().with_clip_rect(Rect::from_min_max(rect.min, pos2(rect.right() - 34.0, rect.bottom())));
                    let x = thumb.right() + 10.0;
                    painter.text(pos2(x, rect.top() + 9.0), Align2::LEFT_TOP, &photo.name, theme::body_font(13.0), theme::TEXT);
                    let (line, colour) = match s.project.as_ref().map(|p| &p.images[i]) {
                        Some(img) if !img.aligned => ("Not aligned — no overlap found".to_string(), theme::DANGER),
                        Some(img) if !img.enabled => ("Excluded".to_string(), theme::MUTED),
                        Some(_) => (format!("{} · {} points", photo.summary, counts[i]), theme::MUTED),
                        None => (photo.summary.clone(), theme::MUTED),
                    };
                    painter.text(pos2(x, rect.top() + 29.0), Align2::LEFT_TOP, line, theme::body_font(11.5), colour);
                    // Include / exclude toggle.
                    if let Some(p) = &mut s.project {
                        if p.images[i].aligned {
                            let b = Rect::from_center_size(pos2(rect.right() - 16.0, rect.center().y), vec2(22.0, 22.0));
                            let bresp = ui.interact(b, resp.id.with("on"), Sense::click()).on_hover_text(if p.images[i].enabled {
                                "Leave this photo out"
                            } else {
                                "Use this photo"
                            });
                            let on = p.images[i].enabled;
                            ui.painter().circle(
                                b.center(),
                                8.0,
                                if on { theme::ACCENT } else { Color32::TRANSPARENT },
                                Stroke::new(1.5, if on { theme::ACCENT } else { theme::MUTED }),
                            );
                            if on {
                                icons::paint(ui.painter(), b.shrink(5.0), Icon::Check, Color32::from_rgb(25, 20, 10));
                            }
                            if bresp.clicked() {
                                p.images[i].enabled = !on;
                                toggled = true;
                            }
                        }
                    }
                }
                if s.photos.is_empty() {
                    ui.add_space(20.0);
                    ui.label(RichText::new("Drop photos here, or use Add… above.").color(theme::MUTED));
                }
            });
            if toggled {
                s.request_preview(ui.ctx());
            }
        });
}

fn settings_panel(app: &mut NodalApp, ui: &mut Ui) {
    egui::Panel::right("stitch_settings")
        .exact_size(318.0)
        .resizable(false)
        .frame(egui::Frame::new().fill(theme::PANEL).inner_margin(14).stroke(theme::hairline()))
        .show(ui, |ui| {
            let ctx = ui.ctx().clone();
            let s = &mut app.stitcher;
            let busy = s.busy();
            egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
                ui.spacing_mut().slider_width = 150.0;
                // ---- Lens ----
                ui.label(RichText::new("LENS").font(theme::heading_font(11.0)).color(theme::MUTED).extra_letter_spacing(0.8));
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    for (mode, label) in
                        [(LensMode::Auto, "Automatic"), (LensMode::Rectilinear, "Rectilinear"), (LensMode::Fisheye, "Fisheye")]
                    {
                        ui.selectable_value(&mut s.lens_mode, mode, label);
                    }
                });
                if s.lens_mode != LensMode::Auto {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("Field of view").color(theme::MUTED));
                        ui.add(egui::DragValue::new(&mut s.lens_fov).range(0.0..=300.0).speed(0.5).suffix("°"))
                            .on_hover_text("Horizontal field of view across the photo's width. 0 = estimate it.");
                    });
                }
                ui.checkbox(&mut s.optimize_lens, "Calibrate distortion from the photos");
                if let Some(p) = &s.project {
                    for (i, l) in p.lenses.iter().enumerate() {
                        let source = s.report.as_ref().and_then(|r| r.lens_source.get(i)).map(|src| match src {
                            LensSource::Exif => "from EXIF, refined",
                            LensSource::Estimated => "estimated from the photos",
                            LensSource::User => "as entered",
                        });
                        theme::card_frame().show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            ui.label(
                                RichText::new(format!("{} · {:.1}° · {:.0} mm equiv.", l.kind.label(), l.hfov, l.focal_35mm()))
                                    .color(theme::TEXT),
                            );
                            ui.label(
                                RichText::new(format!("distortion a {:+.4}  b {:+.4}  c {:+.4}", l.a, l.b, l.c))
                                    .color(theme::MUTED)
                                    .small(),
                            );
                            if let Some(src) = source {
                                ui.label(RichText::new(src).color(theme::MUTED).small());
                            }
                        });
                    }
                }

                // ---- Alignment ----
                section(ui, "Alignment");
                if let (Some(p), Some(r)) = (&s.project, &s.report) {
                    let rms = p.control_points.iter().map(|c| (c.error as f64).powi(2)).sum::<f64>() / p.control_points.len().max(1) as f64;
                    let rms = rms.sqrt();
                    let (verdict, colour) = if rms < 1.0 {
                        ("Excellent", theme::OK)
                    } else if rms < 2.5 {
                        ("Good", theme::OK)
                    } else if rms < 5.0 {
                        ("Fair", theme::ACCENT)
                    } else {
                        ("Poor", theme::DANGER)
                    };
                    theme::card_frame().show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(verdict).font(theme::heading_font(15.0)).color(colour));
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                ui.label(RichText::new(format!("{:.1} s", s.align_secs)).color(theme::MUTED).small());
                            });
                        });
                        stat(ui, "Control points", format!("{} in {} pairs", p.control_points.len(), r.pairs.len()));
                        stat(ui, "Average distance", format!("{rms:.2} px"));
                        if let Some(local) = r.local.filter(|l| l.images_warped > 0 && p.output.local_alignment) {
                            stat(ui, "After local alignment", format!("{:.2} px", local.after));
                        }
                    });
                    if !r.unaligned.is_empty() {
                        ui.label(
                            RichText::new(format!(
                                "{} photo(s) could not be placed: they need more overlap with the others.",
                                r.unaligned.len()
                            ))
                            .color(theme::DANGER)
                            .small(),
                        );
                    }
                    ui.horizontal(|ui| {
                        if ui.add_enabled(!busy, egui::Button::new("Align again")).clicked() {
                            s.align(&ctx);
                        }
                        ui.toggle_value(&mut s.show_points, "Control points");
                    });
                    if s.show_points {
                        points_table(s, ui, &ctx);
                    }
                } else if s.photos.len() >= 2 {
                    if ui.add_enabled(!busy, egui::Button::new("Align photos")).clicked() {
                        s.align(&ctx);
                    }
                } else {
                    ui.label(RichText::new("Add at least two overlapping photos.").color(theme::MUTED));
                }

                // ---- Panorama ----
                let Some(p) = &mut s.project else { return };
                let mut changed = false;
                section(ui, "Panorama");
                changed |= ui.checkbox(&mut p.output.exposure_compensation, "Balance exposure and colour").changed();
                changed |= ui
                    .checkbox(&mut p.output.seam_optimization, "Find the best seams")
                    .on_hover_text("Routes seams around moving objects and parallax")
                    .changed();
                changed |= ui.checkbox(&mut p.output.multiband, "Multi-band blending").changed();
                changed |= ui
                    .checkbox(&mut p.output.local_alignment, "Local alignment")
                    .on_hover_text("Small per-photo corrections so matched points coincide exactly")
                    .changed();
                changed |= ui
                    .checkbox(&mut p.output.trim_edges, "Trim uneven edges")
                    .on_hover_text("Crop a partial panorama to a clean rectangle")
                    .changed();
                let mut fill = p.output.pole_fill == PoleFill::Auto;
                if ui.checkbox(&mut fill, "Patch zenith and nadir gaps").changed() {
                    p.output.pole_fill = if fill { PoleFill::Auto } else { PoleFill::Off };
                    changed = true;
                }
                if fill {
                    ui.spacing_mut().slider_width = ui.available_width() - 150.0;
                    for (value, label, tip) in [
                        (&mut p.output.nadir_patch, "Nadir patch", "Paint out this much around the nadir, for example to remove a tripod"),
                        (&mut p.output.zenith_patch, "Zenith patch", "Paint out this much around the zenith"),
                    ] {
                        ui.horizontal(|ui| {
                            ui.add_sized(
                                [86.0, 20.0],
                                egui::Label::new(RichText::new(label).color(theme::MUTED)).halign(egui::Align::LEFT),
                            );
                            let r = ui.add(egui::Slider::new(value, 0.0..=45.0).suffix("°").max_decimals(0)).on_hover_text(tip);
                            changed |= r.drag_stopped() || (r.changed() && !r.dragged());
                        });
                    }
                }

                section(ui, "Orientation");
                ui.horizontal(|ui| {
                    if ui.button("Level horizon").clicked() {
                        nodal_stitch::align::level(p);
                        s.roll = 0.0;
                        changed = true;
                    }
                    let before = s.roll;
                    let r = ui.add(
                        egui::DragValue::new(&mut s.roll).range(-180.0..=180.0).speed(0.1).max_decimals(1).prefix("Roll ").suffix("°"),
                    );
                    if r.changed() {
                        p.rotate_world(&Rotation3::from_axis_angle(&Vector3::z_axis(), ((s.roll - before) as f64).to_radians()));
                    }
                    changed |= r.drag_stopped() || (r.changed() && !r.dragged());
                });
                ui.label(RichText::new("Drag the preview to re-centre the panorama.").color(theme::MUTED).small());

                // ---- Output ----
                section(ui, "Output");
                let native = p.native_width();
                egui::ComboBox::from_id_salt("stitch_width")
                    .width(ui.available_width() - 8.0)
                    .selected_text(if s.width_choice == 0 {
                        format!("Native resolution ({native} px for 360°)")
                    } else {
                        WIDTHS[s.width_choice].0.to_string()
                    })
                    .show_ui(ui, |ui| {
                        for (i, (label, _)) in WIDTHS.iter().enumerate() {
                            ui.selectable_value(
                                &mut s.width_choice,
                                i,
                                if i == 0 { format!("Native resolution ({native} px for 360°)") } else { label.to_string() },
                            );
                        }
                    });
                if WIDTHS[s.width_choice].1 == 1 {
                    ui.add(egui::DragValue::new(&mut s.custom_width).range(512..=65536).speed(16).suffix(" px wide"));
                }
                ui.horizontal(|ui| {
                    for (f, label) in [(Format::Jpeg, "JPEG"), (Format::Tiff, "TIFF"), (Format::Png, "PNG")] {
                        ui.selectable_value(&mut s.format, f, label);
                    }
                });
                if s.format == Format::Jpeg {
                    ui.spacing_mut().slider_width = ui.available_width() - 150.0;
                    ui.horizontal(|ui| {
                        ui.add_sized(
                            [86.0, 20.0],
                            egui::Label::new(RichText::new("Quality").color(theme::MUTED)).halign(egui::Align::LEFT),
                        );
                        ui.add(egui::Slider::new(&mut s.quality, 60..=100));
                    });
                }
                if let Some(saved) = &s.saved {
                    ui.add_space(8.0);
                    theme::card_frame().show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        let name = saved.path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                        ui.label(RichText::new(name).color(theme::TEXT));
                        ui.label(
                            RichText::new(format!("{} × {} · {:.1} s", saved.width, saved.height, saved.secs)).color(theme::MUTED).small(),
                        );
                        if ui.small_button("Show in folder").clicked() {
                            reveal(&saved.path);
                        }
                    });
                }
                if changed {
                    s.request_preview(&ctx);
                }
            });
        });
}

fn points_table(s: &mut Stitcher, ui: &mut Ui, ctx: &egui::Context) {
    let Some(p) = &mut s.project else { return };
    // Per-pair statistics, worst first.
    let mut pairs: std::collections::BTreeMap<(u32, u32), (usize, f64, f32)> = Default::default();
    for cp in &p.control_points {
        let e = pairs.entry((cp.a, cp.b)).or_insert((0, 0.0, 0.0));
        e.0 += 1;
        e.1 += (cp.error as f64).powi(2);
        e.2 = e.2.max(cp.error);
    }
    let mut rows: Vec<_> = pairs.into_iter().map(|(k, v)| (k, v.0, (v.1 / v.0 as f64).sqrt(), v.2)).collect();
    rows.sort_by(|a, b| b.2.total_cmp(&a.2));
    theme::card_frame().show(ui, |ui| {
        ui.set_width(ui.available_width());
        egui::Grid::new("stitch_pairs").num_columns(4).spacing(vec2(14.0, 3.0)).show(ui, |ui| {
            for h in ["Pair", "Points", "Avg", "Max"] {
                ui.label(RichText::new(h).color(theme::MUTED).small());
            }
            ui.end_row();
            for ((a, b), n, rms, max) in rows.iter().take(12) {
                ui.label(format!("{} – {}", a + 1, b + 1));
                ui.label(n.to_string());
                ui.label(format!("{rms:.2}"));
                ui.label(RichText::new(format!("{max:.1}")).color(if *max > 6.0 { theme::ACCENT } else { theme::TEXT }));
                ui.end_row();
            }
        });
        if rows.len() > 12 {
            ui.label(RichText::new(format!("…and {} more pairs", rows.len() - 12)).color(theme::MUTED).small());
        }
    });
    let mut reopt = false;
    if ui.add_enabled(p.control_points.len() > 40, egui::Button::new("Remove the worst 10 % and optimise")).clicked() {
        let mut errs: Vec<f32> = p.control_points.iter().map(|c| c.error).collect();
        let k = errs.len() * 9 / 10;
        errs.select_nth_unstable_by(k, |a, b| a.total_cmp(b));
        let limit = errs[k];
        p.control_points.retain(|c| c.error < limit);
        reopt = true;
    }
    if reopt {
        s.reoptimise(ctx);
    }
}

fn centre(app: &mut NodalApp, ui: &mut Ui) {
    let ctx = ui.ctx().clone();
    let rect = ui.max_rect();
    let s = &mut app.stitcher;
    let hovering_files = ctx.input(|i| !i.raw.hovered_files.is_empty());

    if s.photos.is_empty() && !s.busy() {
        // Empty state.
        let card = Rect::from_center_size(rect.center(), vec2(480.0f32.min(rect.width() - 40.0), 230.0));
        ui.painter().rect_filled(card, 18.0, Color32::from_rgb(20, 22, 27));
        ui.painter().rect_stroke(
            card,
            18.0,
            Stroke::new(1.5, if hovering_files { theme::ACCENT } else { Color32::from_white_alpha(28) }),
            egui::StrokeKind::Inside,
        );
        ui.scope_builder(egui::UiBuilder::new().max_rect(card.shrink(24.0)), |ui| {
            ui.vertical_centered(|ui| {
                ui.add_space(18.0);
                ui.label(RichText::new("Drop overlapping photos here").font(theme::heading_font(19.0)));
                ui.add_space(2.0);
                ui.label(RichText::new("Any lens, any order — single row, multi-row or a full sphere.\nNodal finds the overlaps, calibrates the lens and blends the result.").color(theme::MUTED));
                ui.add_space(14.0);
                ui.horizontal(|ui| {
                    icons::center_row(ui, &["Choose photos…"]);
                    if icons::pill(ui, Icon::Plus, "Choose photos…", true).clicked() {
                        if let Some(files) = rfd::FileDialog::new().add_filter("Photos", &["jpg", "jpeg", "png", "tif", "tiff", "webp"]).pick_files() {
                            s.add_photos(&ctx, files);
                        }
                    }
                });
            });
        });
        return;
    }

    let area = rect.shrink(22.0);
    if let Some(prev) = &s.preview {
        // Fit the preview, keeping its aspect.
        let size = prev.tex.size_vec2();
        let k = (area.width() / size.x).min((area.height() - 34.0) / size.y);
        let img = Rect::from_center_size(area.center() - vec2(0.0, 12.0), size * k);
        let resp = ui.interact(img, ui.scope_id().with("stitch_preview"), Sense::drag());
        if resp.dragged() && s.project.is_some() {
            s.drag_x += resp.drag_delta().x / img.width();
            s.drag_y += resp.drag_delta().y / img.height();
            ctx.set_cursor_icon(egui::CursorIcon::Grabbing);
        } else if resp.hovered() {
            ctx.set_cursor_icon(egui::CursorIcon::Grab);
        }
        let painter = ui.painter().with_clip_rect(img);
        ui.painter().rect_filled(img.expand(1.0), 6.0, Color32::from_rgb(6, 7, 9));
        let moving = resp.dragged() || s.drag_x != 0.0 || s.drag_y != 0.0;
        if s.gpu_ready && s.project.is_some() && (moving || s.live) {
            // Live GPU composite: the photos are re-projected every frame, so the panorama follows
            // the pointer in yaw and pitch while the full-quality preview catches up.
            let (yaw, pitch) =
                if prev.full_sphere { (s.drag_x * 360.0, s.drag_y * 180.0) } else { (s.drag_x * prev.hfov, s.drag_y * prev.vfov) };
            let g = Rotation3::from_axis_angle(&Vector3::x_axis(), (pitch as f64).to_radians())
                * Rotation3::from_axis_angle(&Vector3::y_axis(), (yaw as f64).to_radians());
            let inv = g.inverse();
            let m = inv.matrix();
            let ppp = ctx.pixels_per_point();
            let frame = ComposeFrame {
                world: [
                    [m[(0, 0)] as f32, m[(0, 1)] as f32, m[(0, 2)] as f32],
                    [m[(1, 0)] as f32, m[(1, 1)] as f32, m[(1, 2)] as f32],
                    [m[(2, 0)] as f32, m[(2, 1)] as f32, m[(2, 2)] as f32],
                ],
                rect: [img.left() * ppp, img.top() * ppp, img.width() * ppp, img.height() * ppp],
                crop: prev.window,
                photos: s.compose_photos(),
            };
            ui.painter().add(egui_wgpu::Callback::new_paint_callback(img, ComposeCallback { frame }));
        } else if prev.full_sphere {
            // A full panorama wraps: show the horizontal drag live by sliding it around.
            let off = s.drag_x.rem_euclid(1.0);
            for dx in [off - 1.0, off] {
                let r = img.translate(vec2(dx * img.width(), 0.0));
                painter.image(prev.tex.id(), r, Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)), Color32::WHITE);
            }
        } else {
            let r = img.translate(vec2(s.drag_x * img.width(), s.drag_y * img.height()));
            painter.image(prev.tex.id(), r, Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)), Color32::WHITE);
        }
        if s.grid || resp.dragged() {
            let line = Stroke::new(1.0, Color32::from_white_alpha(70));
            if prev.full_sphere {
                painter.hline(img.x_range(), img.center().y, Stroke::new(1.0, theme::ACCENT.gamma_multiply(0.8)));
                for f in [0.25, 0.75] {
                    painter.hline(img.x_range(), img.top() + img.height() * f, line);
                }
                for f in [0.25, 0.5, 0.75] {
                    painter.vline(img.left() + img.width() * f, img.y_range(), line);
                }
            }
        }
        if resp.drag_stopped() {
            // Apply the drag as a rotation of the whole panorama.
            let (yaw, pitch) =
                if prev.full_sphere { (s.drag_x * 360.0, s.drag_y * 180.0) } else { (s.drag_x * prev.hfov, s.drag_y * prev.vfov) };
            let g = Rotation3::from_axis_angle(&Vector3::x_axis(), (pitch as f64).to_radians())
                * Rotation3::from_axis_angle(&Vector3::y_axis(), (yaw as f64).to_radians());
            let gpu = s.gpu_ready;
            if !prev.full_sphere || gpu {
                s.drag_x = 0.0;
            }
            s.drag_y = 0.0;
            s.rotate(&ctx, g);
        }
        // Caption.
        let prev = s.preview.as_ref().unwrap();
        let mut caption = if prev.full_sphere { "360° × 180°".to_string() } else { format!("{:.0}° × {:.0}°", prev.hfov, prev.vfov) };
        match prev.filled {
            (true, true) => caption += " · zenith and nadir patched",
            (true, false) => caption += " · zenith patched",
            (false, true) => caption += " · nadir patched",
            _ => {}
        }
        if prev.has_gaps {
            caption += " · has uncovered areas";
        }
        if s.preview_running {
            caption += " · updating…";
        }
        ui.painter().text(pos2(img.center().x, img.bottom() + 12.0), Align2::CENTER_TOP, caption, theme::body_font(12.5), theme::MUTED);
        let g = Rect::from_min_size(pos2(img.right() - 64.0, img.bottom() + 6.0), vec2(64.0, 24.0));
        ui.scope_builder(egui::UiBuilder::new().max_rect(g), |ui| {
            ui.toggle_value(&mut s.grid, "Grid");
        });
    } else if !s.photos.is_empty() {
        // Contact sheet while there is no panorama yet.
        let n = s.photos.len();
        let cols = ((n as f32).sqrt().ceil() as usize).clamp(1, 8);
        let rows = n.div_ceil(cols);
        let cell = ((area.width() - 12.0 * (cols - 1) as f32) / cols as f32)
            .min((area.height() - 12.0 * (rows - 1) as f32) / rows as f32 * 1.5)
            .min(260.0);
        let (cw, chh) = (cell, cell / 1.5);
        let total = vec2(cols as f32 * cw + (cols - 1) as f32 * 12.0, rows as f32 * chh + (rows - 1) as f32 * 12.0);
        let origin = area.center() - total / 2.0;
        for (i, photo) in s.photos.iter().enumerate() {
            let r = Rect::from_min_size(origin + vec2((i % cols) as f32 * (cw + 12.0), (i / cols) as f32 * (chh + 12.0)), vec2(cw, chh));
            let ts = photo.thumb.size_vec2();
            let k = (r.width() / ts.x).min(r.height() / ts.y);
            egui::Image::new(&photo.thumb).corner_radius(6).paint_at(ui, Rect::from_center_size(r.center(), ts * k));
        }
    }

    // Progress and errors float over the centre.
    if let Some(title) = s.job {
        let (f, stage) = s.progress.lock().unwrap().clone();
        let card = Rect::from_center_size(pos2(rect.center().x, rect.bottom() - 70.0), vec2(420.0f32.min(rect.width() - 40.0), 76.0));
        ui.painter().rect_filled(card, 14.0, theme::glass());
        ui.painter().rect_stroke(card, 14.0, theme::hairline(), egui::StrokeKind::Inside);
        ui.painter().text(card.left_top() + vec2(18.0, 14.0), Align2::LEFT_TOP, title, theme::heading_font(14.0), theme::TEXT);
        ui.painter().text(card.right_top() + vec2(-18.0, 15.0), Align2::RIGHT_TOP, stage, theme::body_font(12.5), theme::MUTED);
        let bar = Rect::from_min_size(card.left_bottom() + vec2(18.0, -24.0), vec2(card.width() - 36.0, 6.0));
        ui.painter().rect_filled(bar, 3.0, Color32::from_rgb(40, 44, 52));
        let fill = Rect::from_min_size(bar.min, vec2(bar.width() * f.clamp(0.02, 1.0), bar.height()));
        ui.painter().rect_filled(fill, 3.0, theme::ACCENT);
    } else if let Some(err) = s.error.clone() {
        let card = Rect::from_center_size(pos2(rect.center().x, rect.bottom() - 70.0), vec2(560.0f32.min(rect.width() - 40.0), 84.0));
        ui.painter().rect_filled(card, 14.0, theme::glass());
        ui.painter().rect_stroke(card, 14.0, Stroke::new(1.0, theme::DANGER.gamma_multiply(0.7)), egui::StrokeKind::Inside);
        ui.scope_builder(egui::UiBuilder::new().max_rect(card.shrink2(vec2(18.0, 12.0))), |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("Something went wrong").font(theme::heading_font(14.0)).color(theme::DANGER));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.small_button("Dismiss").clicked() {
                        s.error = None;
                    }
                });
            });
            ui.label(RichText::new(err).color(theme::TEXT).small());
        });
    }
}
