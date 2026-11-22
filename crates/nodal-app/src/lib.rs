//! Nodal app: the native/web viewer and editor UI.

pub mod app;
mod camera;
mod chrome;
mod editor;
mod hotspots;
mod icons;
mod library;
pub mod loader;
mod net;
pub mod platform;
#[cfg(not(target_arch = "wasm32"))]
mod stitcher;
mod theme;

pub use app::NodalApp;

#[cfg(not(target_arch = "wasm32"))]
pub use library::open_paths;

impl NodalApp {
    /// Web startup: `?tour=url` opens a static tour; otherwise talk to the Nodal server.
    /// The hash may carry `id` (server tour), `scene`, `yaw`, `pitch` and `fov` deep-link parameters.
    pub fn start_web(&mut self) {
        let params = platform::url_params();
        let get = |k: &str| params.iter().find(|(key, _)| key == k).map(|(_, v)| v.clone());
        if let Some(scene) = get("scene") {
            let num = |k: &str, d: f32| get(k).and_then(|v| v.parse().ok()).unwrap_or(d);
            let projection = match get("proj").as_deref() {
                Some("planet") | Some("stereographic") => nodal_core::Projection::STEREOGRAPHIC,
                Some("fisheye") => nodal_core::Projection::FISHEYE,
                Some("pannini") => nodal_core::Projection::PANNINI,
                Some("flat") | Some("equirect") => nodal_core::Projection::EQUIRECT,
                _ => nodal_core::Projection::RECTILINEAR,
            };
            let v = nodal_core::View {
                yaw: num("yaw", 0.0),
                pitch: num("pitch", 0.0),
                fov: num("fov", 90.0),
                projection,
                ..Default::default()
            };
            self.initial_view = Some((scene, v));
        }
        if let Some(tour) = get("tour") {
            let url = if tour.contains("://") {
                tour
            } else {
                let base = format!("{}/", self.origin);
                nodal_core::tour::join_url(&base, tour.trim_start_matches("./"))
            };
            let url = if url.ends_with(".json") { url } else { format!("{}/tour.json", url.trim_end_matches('/')) };
            self.static_mode = true;
            self.loader.open_url(url.clone(), loader::Origin::Remote(url));
            return;
        }
        self.loader.server_config(&self.origin);
        self.loader.server_list(&self.origin);
        if let Some(id) = get("id") {
            self.loader.server_open(&self.origin, &id);
        }
    }
}

#[cfg(target_arch = "wasm32")]
#[wasm_bindgen::prelude::wasm_bindgen(start)]
pub fn start() {
    use wasm_bindgen::JsCast;
    eframe::WebLogger::init(log::LevelFilter::Info).ok();
    let mut web_options = eframe::WebOptions::default();
    // `?backend=webgl` forces WebGL 2 (useful if a browser's WebGPU implementation misbehaves).
    let force_gl = platform::url_params().iter().any(|(k, v)| k == "backend" && v == "webgl");
    if force_gl {
        if let eframe::egui_wgpu::WgpuSetup::CreateNew(setup) = &mut web_options.wgpu_options.wgpu_setup {
            setup.instance_descriptor.backends = eframe::wgpu::Backends::GL;
        }
    }
    wasm_bindgen_futures::spawn_local(async move {
        let document = web_sys::window().expect("window").document().expect("document");
        let canvas =
            document.get_element_by_id("nodal_canvas").expect("#nodal_canvas").dyn_into::<web_sys::HtmlCanvasElement>().expect("canvas");
        let result = eframe::WebRunner::new()
            .start(
                canvas,
                web_options,
                Box::new(|cc| {
                    let mut app = NodalApp::new(cc);
                    app.start_web();
                    Ok(Box::new(app))
                }),
            )
            .await;
        if let Some(el) = document.get_element_by_id("loading") {
            match result {
                Ok(()) => el.remove(),
                Err(e) => el.set_inner_html(&format!("<p>Nodal could not start: {e:?}</p><p>WebGPU or WebGL 2 is required.</p>")),
            }
        }
    });
}
