//! The Nodal application: tour state, streaming, camera, transitions and input.

use crate::camera::Camera;
use crate::editor::Editor;
use crate::hotspots::{self, DrawState, ScreenMap};
use crate::library::Library;
use crate::loader::{Event, LoadedTour, Loader, Origin, ServerConfig};
use crate::{chrome, editor, library, platform, theme};
use eframe::egui_wgpu;
use egui::{CursorIcon, Key, Pos2, Sense};
use glam::Vec2;
use nodal_core::math::{ease_in_out_cubic, DEG};
use nodal_core::tour::{Action, HotspotKind, TransitionKind};
use nodal_core::{Projection, Projector, Scene, View};
use nodal_render::{OverlayUniforms, PanoEngine, PanoUniforms, StreamStatus, Streamer, TileFetcher};
use serde::{Deserialize, Serialize};

const MAX_UPLOADS_PER_FRAME: usize = if cfg!(target_arch = "wasm32") { 8 } else { 16 };

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Screen {
    Library,
    Viewer,
    /// The stitching workspace (native app).
    Stitch,
}

pub enum Cmd {
    Goto { scene: usize, view: Option<View>, focus: Option<Pos2> },
    Open(LoadedTour),
    BackToLibrary,
    Fullscreen,
    Projection(Projection, Option<(f32, f32)>),
    LookAt(View),
    Link(String),
    Info { title: String, text: String },
    Save,
    Reload,
}

struct Transition {
    start: Option<f64>,
    waited: f32,
    duration: f32,
    kind: TransitionKind,
    focus: Vec2,
    arrive: Option<View>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct RecentTour {
    pub title: String,
    pub path: String,
    pub thumb: Option<String>,
    pub scenes: usize,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Persisted {
    pub recent: Vec<RecentTour>,
    pub token: String,
    pub show_labels: bool,
    /// 0 = full detail, 1 = save bandwidth (one level coarser).
    pub saver: bool,
}

pub struct Toast {
    pub text: String,
    pub until: f64,
    pub error: bool,
}

#[derive(Default)]
pub struct UiState {
    pub hovered: Option<usize>,
    pub info: Option<(String, String)>,
    pub last_activity: f64,
    pub ui_alpha: f32,
    pub show_projection: bool,
    pub show_help: bool,
    pub show_stats: bool,
    pub drag_last: Option<Pos2>,
    pub scene_started: f64,
    /// Where the panorama is drawn (overlays anchor to it, not to the window).
    pub pano_rect: Option<egui::Rect>,
}

pub struct NodalApp {
    pub loader: Loader,
    pub fetcher: Box<dyn TileFetcher>,
    pub screen: Screen,
    pub tour: Option<LoadedTour>,
    pub scene: usize,
    generation: u32,
    streamer: Option<Streamer>,
    pub camera: Camera,
    transition: Option<Transition>,
    pub cmds: Vec<Cmd>,
    pub ui: UiState,
    pub editor: Editor,
    pub library: Library,
    #[cfg(not(target_arch = "wasm32"))]
    pub stitcher: crate::stitcher::Stitcher,
    pub status: StreamStatus,
    pub debug_mode: u32,
    pub server: Option<ServerConfig>,
    pub origin: String,
    pub persisted: Persisted,
    pub toasts: Vec<Toast>,
    pub gpu_info: String,
    pub last_proj: Option<Projector>,
    pub last_map: Option<ScreenMap>,
    hash_timer: f32,
    last_hash: String,
    pub initial_view: Option<(String, View)>,
    pub static_mode: bool,
}

/// Paint callback: writes this frame's uniforms and draws the panorama inside egui's pass.
struct PanoCallback {
    uniforms: PanoUniforms,
    overlay: Option<OverlayUniforms>,
}

impl egui_wgpu::CallbackTrait for PanoCallback {
    fn prepare(
        &self,
        _device: &eframe::wgpu::Device,
        queue: &eframe::wgpu::Queue,
        _screen: &egui_wgpu::ScreenDescriptor,
        _encoder: &mut eframe::wgpu::CommandEncoder,
        resources: &mut egui_wgpu::CallbackResources,
    ) -> Vec<eframe::wgpu::CommandBuffer> {
        if let Some(engine) = resources.get::<PanoEngine>() {
            engine.write_uniforms(queue, &self.uniforms, self.overlay);
        }
        vec![]
    }

    fn paint(
        &self,
        _info: egui::PaintCallbackInfo,
        rpass: &mut eframe::wgpu::RenderPass<'static>,
        resources: &egui_wgpu::CallbackResources,
    ) {
        if let Some(engine) = resources.get::<PanoEngine>() {
            engine.paint(rpass);
        }
    }
}

impl NodalApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        theme::install(&cc.egui_ctx);
        egui_extras::install_image_loaders(&cc.egui_ctx);
        let mut gpu_info = String::from("no GPU");
        if let Some(rs) = cc.wgpu_render_state.as_ref() {
            let engine = PanoEngine::new(&rs.device, rs.target_format);
            rs.renderer.write().callback_resources.insert(engine);
            let info = rs.adapter.get_info();
            gpu_info = format!("{} · {:?}", info.name, info.backend);
            log::info!("GPU: {gpu_info}, target {:?}", rs.target_format);
        }
        let ctx = cc.egui_ctx.clone();
        let waker: nodal_render::Waker = std::sync::Arc::new(move || ctx.request_repaint());
        #[cfg(not(target_arch = "wasm32"))]
        let fetcher: Box<dyn TileFetcher> = Box::new(nodal_render::fetch::ThreadFetcher::new(
            std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).clamp(3, 8),
            Some(waker),
        ));
        #[cfg(target_arch = "wasm32")]
        let fetcher: Box<dyn TileFetcher> = Box::new(nodal_render::fetch::WebFetcher::new(8, Some(waker)));
        let persisted: Persisted = cc.storage.and_then(|s| eframe::get_value(s, "nodal")).unwrap_or_default();
        let mut loader = Loader::new(cc.egui_ctx.clone());
        loader.token = persisted.token.clone();
        Self {
            loader,
            fetcher,
            screen: Screen::Library,
            tour: None,
            scene: 0,
            generation: 0,
            streamer: None,
            camera: Camera::default(),
            transition: None,
            cmds: vec![],
            ui: UiState { ui_alpha: 1.0, ..Default::default() },
            editor: Editor::default(),
            library: Library::default(),
            #[cfg(not(target_arch = "wasm32"))]
            stitcher: Default::default(),
            status: StreamStatus::default(),
            debug_mode: 0,
            server: None,
            origin: platform::origin(),
            persisted,
            toasts: vec![],
            gpu_info,
            last_proj: None,
            last_map: None,
            hash_timer: 0.0,
            last_hash: String::new(),
            initial_view: None,
            static_mode: false,
        }
    }

    pub fn toast(&mut self, ctx: &egui::Context, text: impl Into<String>, error: bool) {
        let now = ctx.input(|i| i.time);
        self.toasts.push(Toast { text: text.into(), until: now + if error { 6.0 } else { 2.6 }, error });
    }

    pub fn current_scene(&self) -> Option<&Scene> {
        self.tour.as_ref().and_then(|t| t.tour.scenes.get(self.scene))
    }

    pub fn can_edit(&self) -> bool {
        match self.tour.as_ref().map(|t| &t.origin) {
            #[cfg(not(target_arch = "wasm32"))]
            Some(Origin::Local(_)) => true,
            Some(Origin::Server(_)) => self.server.as_ref().map(|s| s.writable).unwrap_or(false),
            _ => false,
        }
    }

    pub fn lod_bias(&self, ppp: f32) -> f32 {
        let dpi = if ppp > 1.5 { -0.35 } else { 0.0 };
        dpi - if self.persisted.saver { 1.0 } else { 0.0 }
    }

    fn on_event(&mut self, ev: Event, ctx: &egui::Context) {
        match ev {
            Event::TourLoaded(t) => {
                for w in t.tour.warnings() {
                    log::warn!("{w}");
                }
                self.cmds.push(Cmd::Open(t));
            }
            Event::TourList(list) => self.library.tours = Some(list),
            Event::Config(c) => self.server = Some(c),
            Event::Uploading { done, total, progress, stage } => {
                self.library.job = Some(((done as f32 + progress) / total.max(1) as f32, stage));
            }
            Event::UploadFinished { tour } => {
                self.library.job = None;
                self.loader.server_list(&self.origin);
                let reopen = matches!(self.tour.as_ref().map(|t| &t.origin), Some(Origin::Server(id)) if *id == tour);
                if reopen || self.screen == Screen::Library {
                    self.loader.server_open(&self.origin, &tour);
                }
                self.toast(ctx, "Panorama ready", false);
            }
            Event::Tiling { progress, stage } => self.library.job = Some((progress, stage)),
            #[cfg(not(target_arch = "wasm32"))]
            Event::Tiled { dir, open } => {
                self.library.job = None;
                let path = dir.join("tour.json");
                if open || matches!(self.tour.as_ref().map(|t| &t.origin), Some(Origin::Local(d)) if *d == dir) {
                    self.loader.open_url(path.to_string_lossy().to_string(), Origin::Local(dir));
                }
                self.toast(ctx, "Tour ready", false);
            }
            Event::Saved => {
                self.editor.dirty = false;
                self.editor.saving = false;
                self.toast(ctx, "Saved", false);
            }
            #[cfg(target_arch = "wasm32")]
            Event::Dropped(files) => library::upload(self, ctx, files),
            Event::Deleted(id) => {
                if let Some(list) = &mut self.library.tours {
                    list.retain(|t| t.id != id);
                }
                self.toast(ctx, "Tour deleted", false);
            }
            Event::Error(e) => {
                log::error!("{e}");
                self.library.job = None;
                self.editor.saving = false;
                self.toast(ctx, e, true);
            }
        }
    }

    fn open_tour(&mut self, t: LoadedTour, frame: &mut eframe::Frame) {
        let reload_same = self.tour.as_ref().map(|old| old.origin == t.origin).unwrap_or(false);
        let keep_scene = if reload_same { self.current_scene().map(|s| s.id.clone()) } else { None };
        let keep_view = self.camera.view;
        self.camera.autorotate = t.tour.settings.autorotate.clone();
        self.camera.controls = t.tour.settings.controls.clone();
        // Remember native tours in the recent list.
        #[cfg(not(target_arch = "wasm32"))]
        if let Origin::Local(dir) = &t.origin {
            let path = dir.to_string_lossy().to_string();
            let start = &t.tour.scenes[t.tour.start_index()];
            let thumb = start.thumb.as_ref().map(|th| crate::loader::asset_uri(&t.base_url, th));
            self.persisted.recent.retain(|r| r.path != path);
            self.persisted.recent.insert(0, RecentTour { title: t.tour.title.clone(), path, thumb, scenes: t.tour.scenes.len() });
            self.persisted.recent.truncate(12);
        }
        let mut start = t.tour.start_index();
        let mut view = None;
        if let Some(id) = keep_scene {
            if let Some(i) = t.tour.scene_index(&id) {
                start = i;
                view = Some(keep_view);
            }
        } else if let Some((id, v)) = self.initial_view.take() {
            if let Some(i) = t.tour.scene_index(&id) {
                start = i;
                view = Some(v);
            }
        }
        self.editor.reset(&t.tour, reload_same);
        self.tour = Some(t);
        self.screen = Screen::Viewer;
        if !reload_same || self.streamer.is_none() {
            self.goto_scene(frame, start, view, None, false);
        }
    }

    fn goto_scene(&mut self, frame: &mut eframe::Frame, idx: usize, view: Option<View>, focus: Option<Pos2>, animate: bool) {
        let Some(rs) = frame.wgpu_render_state() else { return };
        let Some(t) = &self.tour else { return };
        let Some(scene) = t.tour.scenes.get(idx).cloned() else { return };
        let settings = t.tour.settings.clone();
        let base_url = t.base_url.clone();
        let mut renderer = rs.renderer.write();
        let Some(engine) = renderer.callback_resources.get_mut::<PanoEngine>() else { return };
        let ppp = self.last_map.map(|m| m.ppp).unwrap_or(1.0);
        let kind = if animate { settings.transition.kind } else { TransitionKind::None };
        let had_scene = engine.scene.is_some();
        let use_transition = had_scene && kind != TransitionKind::None;
        if use_transition {
            if let Some(proj) = &self.last_proj {
                engine.capture_snapshot(&rs.device, &rs.queue, proj, self.lod_bias(ppp));
            }
        }
        self.generation += 1;
        engine.set_scene(&rs.device, self.generation, scene.pyramid.clone(), settings.background);
        self.streamer = Some(Streamer::new(self.generation, base_url, scene.pyramid.clone()));
        self.scene = idx;
        self.camera.limits = scene.limits;
        let target = view.unwrap_or(scene.view);
        let focus_px = match (focus, self.last_map) {
            (Some(f), Some(m)) => m.to_px(f),
            _ => self.last_proj.map(|p| Vec2::new(p.width / 2.0, p.height / 2.0)).unwrap_or(Vec2::ZERO),
        };
        if use_transition {
            let start = if kind == TransitionKind::ZoomFade { View { fov: target.fov * 0.82, ..target } } else { target };
            self.camera.jump_to(start);
            self.transition = Some(Transition {
                start: None,
                waited: 0.0,
                duration: settings.transition.duration.max(0.1),
                kind,
                focus: focus_px,
                arrive: (kind == TransitionKind::ZoomFade).then_some(target),
            });
        } else {
            engine.drop_snapshot();
            self.camera.jump_to(target);
            self.transition = None;
        }
        self.camera.touch();
        self.ui.scene_started = -1.0;
        self.ui.hovered = None;
        self.editor.selected = None;
        self.editor.placing = None;
    }

    fn process_cmds(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        let cmds = std::mem::take(&mut self.cmds);
        for cmd in cmds {
            match cmd {
                Cmd::Goto { scene, view, focus } => self.goto_scene(frame, scene, view, focus, true),
                Cmd::Open(t) => self.open_tour(t, frame),
                Cmd::BackToLibrary => {
                    self.screen = Screen::Library;
                    self.editor.open = false;
                    if self.server.is_some() {
                        self.loader.server_list(&self.origin);
                    }
                }
                Cmd::Fullscreen => platform::toggle_fullscreen(ctx),
                Cmd::Projection(p, hint) => self.camera.set_projection(p, hint),
                Cmd::LookAt(v) => self.camera.animate_to(v, None),
                Cmd::Link(url) => ctx.open_url(egui::OpenUrl::new_tab(url)),
                Cmd::Info { title, text } => self.ui.info = Some((title, text)),
                Cmd::Save => self.save(),
                Cmd::Reload => {
                    if let Some(t) = &self.tour {
                        match &t.origin {
                            #[cfg(not(target_arch = "wasm32"))]
                            Origin::Local(dir) => {
                                self.loader.open_url(dir.join("tour.json").to_string_lossy().to_string(), t.origin.clone())
                            }
                            Origin::Server(id) => self.loader.server_open(&self.origin, id),
                            Origin::Remote(url) => self.loader.open_url(url.clone(), t.origin.clone()),
                        }
                    }
                }
            }
        }
    }

    pub fn save(&mut self) {
        let Some(t) = &self.tour else { return };
        if let Err(e) = t.tour.validate() {
            self.loader.report(Event::Error(format!("Cannot save: {e}")));
            return;
        }
        self.editor.saving = true;
        match &t.origin {
            #[cfg(not(target_arch = "wasm32"))]
            Origin::Local(dir) => self.loader.save_local(dir, &t.tour),
            Origin::Server(id) => self.loader.server_save(&self.origin, id, &t.tour),
            Origin::Remote(_) => {}
        }
    }

    /// Runs a hotspot's action.
    pub fn activate(&mut self, idx: usize, focus: Option<Pos2>) {
        let Some(t) = &self.tour else { return };
        let Some(scene) = t.tour.scenes.get(self.scene) else { return };
        let Some(h) = scene.hotspots.get(idx) else { return };
        match h.action.clone() {
            Action::None => {}
            Action::Goto { scene: target, yaw, pitch, fov } => {
                if let Some(i) = t.tour.scene_index(&target) {
                    let mut v = t.tour.scenes[i].view;
                    if let Some(y) = yaw {
                        v.yaw = y;
                    }
                    if let Some(p) = pitch {
                        v.pitch = p;
                    }
                    if let Some(f) = fov {
                        v.fov = f;
                    }
                    self.cmds.push(Cmd::Goto { scene: i, view: Some(v), focus });
                }
            }
            Action::Info { title, text } => self.cmds.push(Cmd::Info { title, text }),
            Action::Link { url } => self.cmds.push(Cmd::Link(url)),
            Action::LookAt { yaw, pitch, fov } => {
                let v = View { yaw, pitch, fov: fov.unwrap_or(self.camera.view.fov), ..self.camera.view };
                self.cmds.push(Cmd::LookAt(v));
            }
        }
    }

    fn transition_overlay(&mut self, engine: &mut PanoEngine, dt: f32, now: f64, vp: [f32; 4]) -> Option<OverlayUniforms> {
        let tr = self.transition.as_mut()?;
        if !engine.has_snapshot() {
            self.transition = None;
            return None;
        }
        let ready = engine.scene.as_ref().map(|s| s.base_ready).unwrap_or(false);
        tr.waited += dt;
        if tr.start.is_none() && (ready || tr.waited > 4.0) {
            tr.start = Some(now);
            if let Some(v) = tr.arrive.take() {
                self.camera.animate_to(v, Some(tr.duration * 1.25));
            }
        }
        let t = tr.start.map(|s| ((now - s) as f32 / tr.duration).clamp(0.0, 1.0)).unwrap_or(0.0);
        if t >= 1.0 {
            engine.drop_snapshot();
            self.transition = None;
            return None;
        }
        let e = ease_in_out_cubic(t);
        let anticipation = 0.04 * (tr.waited.min(1.0));
        let (scale, alpha) = match tr.kind {
            TransitionKind::ZoomFade => (1.0 + anticipation + 0.7 * e, 1.0 - e),
            _ => (1.0, 1.0 - e),
        };
        Some(OverlayUniforms { rect: vp, xf: [tr.focus.x, tr.focus.y, scale, alpha] })
    }

    fn viewer(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        if self.editor.open && self.can_edit() {
            editor::side_panel(self, ui);
        }
        egui::CentralPanel::no_frame().show(ui, |ui| self.pano_view(ui, frame));
        chrome::overlays(self, &ctx);
    }

    fn pano_view(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let rect = ui.max_rect();
        let resp = ui.interact(rect, ui.scope_id().with("pano"), Sense::click_and_drag());
        let ppp = ctx.pixels_per_point();
        let ox = (rect.min.x * ppp).round();
        let oy = (rect.min.y * ppp).round();
        let w = ((rect.max.x * ppp).round() - ox).max(1.0);
        let h = ((rect.max.y * ppp).round() - oy).max(1.0);
        let map = ScreenMap { rect, ppp };
        self.ui.pano_rect = Some(rect);
        let (dt, now) = ctx.input(|i| (i.stable_dt.min(0.1), i.time));
        if self.ui.scene_started < 0.0 {
            self.ui.scene_started = now;
        }

        // Resolution limit: don't magnify source pixels beyond `max_pixel_zoom`.
        if let Some(s) = self.current_scene() {
            let face = s.pyramid.max_face_size() as f32;
            let long_edge = rect.width().max(rect.height());
            self.camera.min_fov_res = (2.0 * long_edge / (face * s.limits.max_pixel_zoom.max(0.5))) / DEG;
        }

        let proj0 = Projector::new(&self.camera.view, w, h);
        let consumed = if self.editor.open && self.can_edit() { editor::viewport_input(self, &ctx, &resp, &proj0, &map) } else { false };
        if !consumed {
            self.pointer_input(&ctx, &resp, &proj0, &map, dt, now);
        }
        if !ctx.egui_wants_keyboard_input() {
            self.keyboard_input(&ctx);
        }
        let moving = self.camera.update(dt, w, h);
        let proj = Projector::new(&self.camera.view, w, h);

        let mut paint = None;
        if let Some(rs) = frame.wgpu_render_state() {
            let mut renderer = rs.renderer.write();
            if let Some(engine) = renderer.callback_resources.get_mut::<PanoEngine>() {
                engine.debug_mode = self.debug_mode;
                let lod = self.lod_bias(ppp);
                if let Some(st) = &mut self.streamer {
                    self.status = st.step(engine, self.fetcher.as_mut(), &rs.device, &rs.queue, &proj, lod, MAX_UPLOADS_PER_FRAME);
                }
                let overlay = self.transition_overlay(engine, dt, now, [ox, oy, w, h]);
                engine.set_overlay_active(overlay.is_some());
                paint = Some((engine.uniforms(&proj, [ox, oy], lod), overlay));
            }
        }
        if let Some((uniforms, overlay)) = paint {
            ui.painter().add(egui_wgpu::Callback::new_paint_callback(rect, PanoCallback { uniforms, overlay }));
        }

        let pulsing = now - self.ui.scene_started < 12.0;
        if let Some(scene) = self.current_scene() {
            let st = DrawState {
                hovered: self.ui.hovered,
                selected: if self.editor.open { self.editor.selected } else { None },
                editing: self.editor.open,
                show_labels: self.persisted.show_labels || self.editor.open,
                time: if pulsing { now } else { 0.95 },
            };
            hotspots::draw(ui.painter(), &proj, &map, scene, &st);
        }
        if self.editor.open {
            editor::viewport_overlay(self, ui, &proj, &map, &resp);
        }
        self.last_proj = Some(proj);
        self.last_map = Some(map);
        self.update_hash(dt, moving);

        if moving || self.transition.is_some() || !self.status.is_complete() || self.status.pending > 0 {
            ctx.request_repaint();
        } else if pulsing && self.current_scene().map(|s| !s.hotspots.is_empty()).unwrap_or(false) {
            ctx.request_repaint_after(std::time::Duration::from_millis(33));
        }
    }

    fn pointer_input(&mut self, ctx: &egui::Context, resp: &egui::Response, proj: &Projector, map: &ScreenMap, dt: f32, now: f64) {
        let hover = resp.hover_pos();
        self.ui.hovered = match (hover, self.current_scene()) {
            (Some(p), Some(scene)) if !self.camera.dragging => hotspots::hit_test(proj, map, scene, p),
            _ => None,
        };
        if resp.drag_started() {
            if let Some(p) = resp.interact_pointer_pos() {
                self.camera.begin_drag(map.to_px(p));
                self.ui.drag_last = Some(p);
            }
        }
        if resp.dragged() {
            if let (Some(last), Some(p)) = (self.ui.drag_last, resp.interact_pointer_pos()) {
                if last != p {
                    self.camera.drag(proj, map.to_px(last), map.to_px(p), dt, now);
                }
                self.ui.drag_last = Some(p);
            }
        }
        if resp.drag_stopped() {
            self.camera.end_drag(now);
            self.ui.drag_last = None;
        }
        if resp.clicked() {
            self.camera.touch();
            if let Some(i) = self.ui.hovered {
                self.activate(i, hover);
            }
        }
        if resp.double_clicked() && self.ui.hovered.is_none() {
            if let Some(p) = hover {
                let px = map.to_px(p);
                self.camera.zoom(0.5, proj.screen_to_dir(px).map(|d| (px, d)));
            }
        }
        if resp.hovered() {
            let (scroll, zoom, touch) = ctx.input(|i| (i.smooth_scroll_delta.y, i.zoom_delta(), i.multi_touch()));
            let anchor_at = |p: Pos2| {
                let px = map.to_px(p);
                proj.screen_to_dir(px).map(|d| (px, d))
            };
            if scroll.abs() > 0.01 {
                let anchor = hover.and_then(anchor_at);
                self.camera.zoom((-scroll * 0.0022).exp(), anchor);
            }
            if (zoom - 1.0).abs() > 1e-4 {
                let at = touch.map(|t| t.center_pos).or(hover);
                self.camera.zoom(1.0 / zoom, at.and_then(anchor_at));
            }
        }
        let icon = if self.camera.dragging {
            CursorIcon::Grabbing
        } else if self.ui.hovered.is_some() {
            CursorIcon::PointingHand
        } else {
            CursorIcon::Grab
        };
        if resp.hovered() || self.camera.dragging {
            ctx.set_cursor_icon(icon);
        }
    }

    fn keyboard_input(&mut self, ctx: &egui::Context) {
        let mut keys = Vec2::ZERO;
        let mut zoom = 1.0;
        let mut cmds = vec![];
        let n_scenes = self.tour.as_ref().map(|t| t.tour.scenes.len()).unwrap_or(0);
        ctx.input(|i| {
            if i.key_down(Key::ArrowLeft) || i.key_down(Key::A) {
                keys.x -= 1.0;
            }
            if i.key_down(Key::ArrowRight) || i.key_down(Key::D) && !i.modifiers.command {
                keys.x += 1.0;
            }
            if i.key_down(Key::ArrowUp) || i.key_down(Key::W) {
                keys.y += 1.0;
            }
            if i.key_down(Key::ArrowDown) || i.key_down(Key::S) && !i.modifiers.command {
                keys.y -= 1.0;
            }
            if i.key_down(Key::Plus) || i.key_down(Key::Equals) {
                zoom *= (-1.2 * i.stable_dt).exp();
            }
            if i.key_down(Key::Minus) {
                zoom *= (1.2 * i.stable_dt).exp();
            }
            if i.key_pressed(Key::F) {
                cmds.push(Cmd::Fullscreen);
            }
            for (k, idx) in [
                (Key::Num1, 0),
                (Key::Num2, 1),
                (Key::Num3, 2),
                (Key::Num4, 3),
                (Key::Num5, 4),
                (Key::Num6, 5),
                (Key::Num7, 6),
                (Key::Num8, 7),
                (Key::Num9, 8),
            ] {
                if i.key_pressed(k) && idx < n_scenes {
                    cmds.push(Cmd::Goto { scene: idx, view: None, focus: None });
                }
            }
        });
        let pressed = |k| ctx.input(|i| i.key_pressed(k));
        if pressed(Key::Space) {
            self.camera.autorotate_on = !self.camera.autorotate_on;
            self.camera.idle = if self.camera.autorotate_on { 1e3 } else { 0.0 };
        }
        if pressed(Key::P) {
            let next = match (self.camera.view.projection.mode, self.camera.view.projection.distortion > 0.5) {
                (nodal_core::ProjMode::Perspective, false) => (Projection::PANNINI, None),
                (nodal_core::ProjMode::Pannini, _) => (Projection::STEREOGRAPHIC, Some((-90.0, 290.0))),
                (nodal_core::ProjMode::Perspective, true) => (Projection::EQUIRECT, Some((0.0, 360.0))),
                (nodal_core::ProjMode::Equirect, _) => (Projection::RECTILINEAR, Some((0.0, 90.0))),
            };
            cmds.push(Cmd::Projection(next.0, next.1));
        }
        if pressed(Key::L) {
            self.debug_mode = (self.debug_mode + 1) % 4;
        }
        if pressed(Key::I) {
            self.ui.show_stats = !self.ui.show_stats;
        }
        if pressed(Key::Questionmark) || pressed(Key::H) {
            self.ui.show_help = !self.ui.show_help;
        }
        if pressed(Key::E) && self.can_edit() {
            self.editor.open = !self.editor.open;
        }
        if pressed(Key::Escape) {
            if self.ui.info.is_some() {
                self.ui.info = None;
            } else if self.ui.show_help {
                self.ui.show_help = false;
            } else if self.editor.placing.is_some() {
                self.editor.placing = None;
            } else if self.editor.open {
                self.editor.open = false;
            }
        }
        self.camera.keys = keys;
        if zoom != 1.0 {
            self.camera.zoom(zoom, None);
        }
        self.cmds.extend(cmds);
    }

    fn update_hash(&mut self, dt: f32, moving: bool) {
        if !cfg!(target_arch = "wasm32") {
            return;
        }
        self.hash_timer = if moving { 0.0 } else { self.hash_timer + dt };
        if self.hash_timer < 0.5 {
            return;
        }
        let Some(scene) = self.current_scene() else { return };
        let v = self.camera.view;
        let id = match self.tour.as_ref().map(|t| &t.origin) {
            Some(Origin::Server(id)) => format!("id={id}&"),
            _ => String::new(),
        };
        let proj = match (v.projection.mode, v.projection.distortion) {
            (nodal_core::ProjMode::Equirect, _) => "&proj=flat",
            (nodal_core::ProjMode::Pannini, _) => "&proj=pannini",
            (nodal_core::ProjMode::Perspective, d) if d > 0.9 => "&proj=planet",
            (nodal_core::ProjMode::Perspective, d) if d > 0.2 => "&proj=fisheye",
            _ => "",
        };
        let hash = format!("{id}scene={}&yaw={:.1}&pitch={:.1}&fov={:.1}{proj}", scene.id, v.yaw, v.pitch, v.fov);
        if hash != self.last_hash {
            platform::set_hash(&hash);
            self.last_hash = hash;
        }
    }

    pub fn ui_visible(&self) -> bool {
        self.ui.ui_alpha > 0.01
    }

    fn handle_dropped_files(&mut self, ctx: &egui::Context) {
        let files = ctx.input(|i| i.raw.dropped_files.clone());
        if files.is_empty() {
            return;
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            let paths: Vec<std::path::PathBuf> = files.iter().map(|f| f.path().to_path_buf()).collect();
            if self.screen == Screen::Stitch {
                self.stitcher.add_photos(ctx, paths);
            } else {
                library::open_paths(self, paths);
            }
        }
        #[cfg(target_arch = "wasm32")]
        {
            let _ = ctx;
            self.loader.read_dropped(files);
        }
    }
}

impl eframe::App for NodalApp {
    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        for ev in self.loader.poll() {
            self.on_event(ev, &ctx);
        }
        #[cfg(not(target_arch = "wasm32"))]
        crate::stitcher::poll(self, &ctx);
        self.handle_dropped_files(&ctx);
        self.process_cmds(&ctx, frame);
        // Auto-hide the viewer chrome after a few idle seconds.
        let now = ctx.input(|i| i.time);
        let active = ctx.input(|i| i.pointer.is_moving() || i.pointer.any_down() || !i.events.is_empty());
        if active {
            self.ui.last_activity = now;
        }
        let keep =
            self.editor.open || self.ui.info.is_some() || self.ui.show_projection || self.ui.show_help || self.screen != Screen::Viewer;
        let target = if keep || now - self.ui.last_activity < 3.5 { 1.0 } else { 0.0 };
        self.ui.ui_alpha = ctx.animate_value_with_time(egui::Id::unique("ui_alpha"), target, 0.35);
        if self.ui.ui_alpha > 0.0 && self.ui.ui_alpha < 1.0 {
            ctx.request_repaint();
        } else if target > 0.5 && self.screen == Screen::Viewer && !keep {
            ctx.request_repaint_after(std::time::Duration::from_secs_f64(3.6 - (now - self.ui.last_activity)));
        }

        match self.screen {
            Screen::Library => library::show(self, ui),
            Screen::Viewer => self.viewer(ui, frame),
            #[cfg(not(target_arch = "wasm32"))]
            Screen::Stitch => crate::stitcher::show(self, ui),
            #[cfg(target_arch = "wasm32")]
            Screen::Stitch => library::show(self, ui),
        }
        self.process_cmds(&ctx, frame);
        chrome::toasts(self, &ctx);
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        self.persisted.token = self.loader.token.clone();
        eframe::set_value(storage, "nodal", &self.persisted);
    }
}

/// Builds a hotspot at a direction (degrees).
pub fn new_point(id: String, yaw: f32, pitch: f32) -> nodal_core::Hotspot {
    nodal_core::Hotspot {
        id,
        kind: HotspotKind::Point { yaw, pitch, icon: Default::default() },
        label: String::new(),
        action: Action::None,
    }
}
