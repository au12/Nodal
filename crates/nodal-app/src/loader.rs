//! Background operations (loading tours, talking to the server, tiling) reporting back
//! through a channel of [`Event`]s that the UI drains every frame.

use crate::net;
use nodal_core::tour::{join_url, parent_url, Tour};
use serde::Deserialize;
use std::sync::mpsc::{channel, Receiver, Sender};

#[derive(Clone, Debug, PartialEq)]
pub enum Origin {
    /// A tour folder on this machine (native).
    #[cfg(not(target_arch = "wasm32"))]
    Local(std::path::PathBuf),
    /// A tour managed by a Nodal server (id).
    Server(String),
    /// A read-only tour at some URL.
    Remote(String),
}

pub struct LoadedTour {
    pub tour: Tour,
    /// Directory/URL that tour paths are relative to.
    pub base_url: String,
    pub origin: Origin,
}

#[derive(Clone, Debug, Deserialize)]
pub struct TourSummary {
    pub id: String,
    pub title: String,
    pub scenes: usize,
    #[serde(default)]
    pub thumb: Option<String>,
    #[serde(default)]
    pub updated: u64,
}

#[derive(Clone, Debug, Deserialize, Default)]
pub struct ServerConfig {
    pub writable: bool,
    #[serde(default)]
    pub auth_required: bool,
    #[serde(default)]
    pub version: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct JobStatus {
    pub id: String,
    pub state: String,
    #[serde(default)]
    pub progress: f32,
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub tour: String,
}

pub enum Event {
    TourLoaded(LoadedTour),
    TourList(Vec<TourSummary>),
    Config(ServerConfig),
    /// Server-side tiling progress (`None` when all uploads finished).
    Uploading {
        done: usize,
        total: usize,
        progress: f32,
        stage: String,
    },
    UploadFinished {
        tour: String,
    },
    /// Native tiling progress.
    Tiling {
        progress: f32,
        stage: String,
    },
    #[cfg(not(target_arch = "wasm32"))]
    Tiled {
        dir: std::path::PathBuf,
        open: bool,
    },
    Saved,
    Deleted(String),
    /// Web: files dropped on the page, read into memory.
    #[cfg(target_arch = "wasm32")]
    Dropped(Vec<(String, Vec<u8>)>),
    Error(String),
}

pub struct Loader {
    tx: Sender<Event>,
    rx: Receiver<Event>,
    ctx: egui::Context,
    pub token: String,
}

impl Loader {
    pub fn new(ctx: egui::Context) -> Self {
        let (tx, rx) = channel();
        Self { tx, rx, ctx, token: String::new() }
    }

    pub fn poll(&self) -> Vec<Event> {
        self.rx.try_iter().collect()
    }

    fn sender(&self) -> (Sender<Event>, egui::Context) {
        (self.tx.clone(), self.ctx.clone())
    }

    fn auth(&self) -> Vec<(String, String)> {
        if self.token.is_empty() {
            vec![]
        } else {
            vec![("Authorization".into(), format!("Bearer {}", self.token))]
        }
    }

    pub fn report(&self, e: Event) {
        let _ = self.tx.send(e);
        self.ctx.request_repaint();
    }

    /// Loads a `tour.json` from a URL (http(s) or, natively, a file path).
    pub fn open_url(&self, url: String, origin: Origin) {
        let (tx, ctx) = self.sender();
        net::spawn(async move {
            let ev = match fetch_text(&url).await {
                Ok(text) => match Tour::from_json(&text) {
                    Ok(tour) => Event::TourLoaded(LoadedTour { tour, base_url: parent_url(&url), origin }),
                    Err(e) => Event::Error(format!("{url}: {e}")),
                },
                Err(e) => Event::Error(e),
            };
            let _ = tx.send(ev);
            ctx.request_repaint();
        });
    }

    pub fn server_config(&self, origin: &str) {
        let (tx, ctx) = self.sender();
        let url = format!("{origin}/api/config");
        net::spawn(async move {
            if let Ok(r) = net::request("GET", &url, None, vec![]).await {
                if r.ok() {
                    if let Ok(c) = serde_json::from_slice::<ServerConfig>(&r.body) {
                        let _ = tx.send(Event::Config(c));
                    }
                }
            }
            ctx.request_repaint();
        });
    }

    pub fn server_list(&self, origin: &str) {
        let (tx, ctx) = self.sender();
        let url = format!("{origin}/api/tours");
        net::spawn(async move {
            let ev = match net::request("GET", &url, None, vec![]).await {
                Ok(r) if r.ok() => match serde_json::from_slice::<Vec<TourSummary>>(&r.body) {
                    Ok(list) => Event::TourList(list),
                    Err(e) => Event::Error(e.to_string()),
                },
                Ok(r) => Event::Error(r.error_message()),
                Err(e) => Event::Error(e),
            };
            let _ = tx.send(ev);
            ctx.request_repaint();
        });
    }

    pub fn server_open(&self, origin: &str, id: &str) {
        self.open_url(format!("{origin}/tours/{id}/tour.json"), Origin::Server(id.to_string()));
    }

    /// Uploads images to the server: the first creates a tour (unless `tour` is given),
    /// the rest are added as scenes. Polls the tiling jobs until all are done.
    pub fn server_upload(&self, origin: &str, files: Vec<(String, Vec<u8>)>, tour: Option<String>, title: String) {
        let (tx, ctx) = self.sender();
        let origin = origin.to_string();
        let auth = self.auth();
        net::spawn(upload_task(tx, ctx, origin, auth, files, tour, title));
    }

    /// Web: opens the browser's file picker, then uploads the chosen panoramas.
    #[cfg(target_arch = "wasm32")]
    pub fn pick_and_upload(&self, origin: &str, tour: Option<String>) {
        let (tx, ctx) = self.sender();
        let origin = origin.to_string();
        let auth = self.auth();
        net::spawn(async move {
            let Some(handles) =
                rfd::AsyncFileDialog::new().add_filter("Panoramas", &["jpg", "jpeg", "png", "tif", "tiff", "webp"]).pick_files().await
            else {
                return;
            };
            let mut files = vec![];
            for h in handles {
                files.push((h.file_name(), h.read().await));
            }
            let title = files.first().map(|(n, _)| title_from_name(n)).unwrap_or_else(|| "New tour".into());
            upload_task(tx, ctx, origin, auth, files, tour, title).await;
        });
    }

    /// Web: reads dropped files asynchronously and reports them as [`Event::Dropped`].
    #[cfg(target_arch = "wasm32")]
    pub fn read_dropped(&self, files: Vec<std::sync::Arc<dyn egui::DroppedFile + Send + Sync>>) {
        let (tx, ctx) = self.sender();
        net::spawn(async move {
            let mut out = vec![];
            for f in &files {
                let name = f.path().file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "panorama.jpg".into());
                match f.bytes_async().await {
                    Ok(b) => out.push((name, b)),
                    Err(e) => {
                        let _ = tx.send(Event::Error(e));
                    }
                }
            }
            let _ = tx.send(Event::Dropped(out));
            ctx.request_repaint();
        });
    }

    pub fn server_save(&self, origin: &str, id: &str, tour: &Tour) {
        let (tx, ctx) = self.sender();
        let url = format!("{origin}/api/tours/{id}");
        let body = tour.to_json().into_bytes();
        let mut headers = self.auth();
        headers.push(("Content-Type".into(), "application/json".into()));
        net::spawn(async move {
            let ev = match net::request("PUT", &url, Some(body), headers).await {
                Ok(r) if r.ok() => Event::Saved,
                Ok(r) => Event::Error(r.error_message()),
                Err(e) => Event::Error(e),
            };
            let _ = tx.send(ev);
            ctx.request_repaint();
        });
    }

    pub fn server_delete(&self, origin: &str, id: &str) {
        let (tx, ctx) = self.sender();
        let url = format!("{origin}/api/tours/{id}");
        let id = id.to_string();
        let headers = self.auth();
        net::spawn(async move {
            let ev = match net::request("DELETE", &url, None, headers).await {
                Ok(r) if r.ok() => Event::Deleted(id),
                Ok(r) => Event::Error(r.error_message()),
                Err(e) => Event::Error(e),
            };
            let _ = tx.send(ev);
            ctx.request_repaint();
        });
    }

    /// Native: tiles images into a tour folder (new or existing) on a background thread.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn tile_local(&self, images: Vec<std::path::PathBuf>, dir: std::path::PathBuf, title: Option<String>, open: bool) {
        let (tx, ctx) = self.sender();
        std::thread::spawn(move || {
            let report = |f: f32, s: &str| {
                let _ = tx.send(Event::Tiling { progress: f, stage: s.to_string() });
                ctx.request_repaint();
            };
            let res = nodal_tiler::build_tour(&images, &dir, title.as_deref(), &Default::default(), &report);
            let _ = tx.send(match res {
                Ok(_) => Event::Tiled { dir, open },
                Err(e) => Event::Error(format!("{e:#}")),
            });
            ctx.request_repaint();
        });
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub fn save_local(&self, dir: &std::path::Path, tour: &Tour) {
        match nodal_tiler::write_tour(tour, dir) {
            Ok(()) => self.report(Event::Saved),
            Err(e) => self.report(Event::Error(format!("Saving failed: {e:#}"))),
        }
    }
}

async fn upload_task(
    tx: Sender<Event>,
    ctx: egui::Context,
    origin: String,
    auth: Vec<(String, String)>,
    files: Vec<(String, Vec<u8>)>,
    tour: Option<String>,
    title: String,
) {
    let total = files.len();
    let mut tour_id = tour;
    let fail = |e: String| {
        let _ = tx.send(Event::Error(e));
        ctx.request_repaint();
    };
    for (i, (name, bytes)) in files.into_iter().enumerate() {
        let _ = tx.send(Event::Uploading { done: i, total, progress: 0.0, stage: format!("Uploading {name}") });
        ctx.request_repaint();
        let mut headers = auth.clone();
        headers.push(("X-Filename".into(), sanitize_header(&name)));
        headers.push(("Content-Type".into(), "application/octet-stream".into()));
        let url = match &tour_id {
            Some(id) => format!("{origin}/api/tours/{id}/scenes"),
            None => {
                headers.push(("X-Title".into(), sanitize_header(&title)));
                format!("{origin}/api/tours")
            }
        };
        let job: JobStatus = match net::request("POST", &url, Some(bytes), headers).await {
            Ok(r) if r.ok() => match serde_json::from_slice(&r.body) {
                Ok(j) => j,
                Err(e) => return fail(e.to_string()),
            },
            Ok(r) => return fail(r.error_message()),
            Err(e) => return fail(e),
        };
        tour_id = Some(job.tour.clone());
        loop {
            sleep_ms(400).await;
            let st: JobStatus = match net::request("GET", &format!("{origin}/api/jobs/{}", job.id), None, vec![]).await {
                Ok(r) if r.ok() => match serde_json::from_slice(&r.body) {
                    Ok(s) => s,
                    Err(_) => continue,
                },
                _ => continue,
            };
            let _ = tx.send(Event::Uploading { done: i, total, progress: st.progress, stage: st.message.clone() });
            ctx.request_repaint();
            match st.state.as_str() {
                "done" => break,
                "failed" => return fail(format!("Tiling {name} failed: {}", st.message)),
                _ => {}
            }
        }
    }
    if let Some(id) = tour_id {
        let _ = tx.send(Event::UploadFinished { tour: id });
    }
    ctx.request_repaint();
}

/// `"venice_sunset.jpg"` → `"Venice Sunset"`.
pub fn title_from_name(name: &str) -> String {
    let stem = name.rsplit_once('.').map(|(s, _)| s).unwrap_or(name);
    stem.split(['_', '-', ' '])
        .filter(|w| !w.is_empty())
        .map(|w| {
            let mut c = w.chars();
            c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn sanitize_header(s: &str) -> String {
    s.chars().filter(|c| c.is_ascii_graphic() || *c == ' ').take(200).collect()
}

async fn fetch_text(url: &str) -> Result<String, String> {
    #[cfg(not(target_arch = "wasm32"))]
    if !url.starts_with("http://") && !url.starts_with("https://") {
        return std::fs::read_to_string(url.strip_prefix("file://").unwrap_or(url)).map_err(|e| format!("{url}: {e}"));
    }
    let r = net::request("GET", url, None, vec![]).await?;
    if !r.ok() {
        return Err(format!("{url}: {}", r.error_message()));
    }
    Ok(r.text())
}

async fn sleep_ms(ms: u32) {
    #[cfg(not(target_arch = "wasm32"))]
    std::thread::sleep(std::time::Duration::from_millis(ms as u64));
    #[cfg(target_arch = "wasm32")]
    {
        let p = js_sys::Promise::new(&mut |resolve, _| {
            let _ = web_sys::window().unwrap().set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, ms as i32);
        });
        let _ = wasm_bindgen_futures::JsFuture::from(p).await;
    }
}

/// Resolves a tour-relative path for egui's image loaders.
pub fn asset_uri(base_url: &str, rel: &str) -> String {
    let joined = join_url(base_url, rel);
    #[cfg(not(target_arch = "wasm32"))]
    if !joined.contains("://") {
        return crate::platform::file_uri(std::path::Path::new(&joined));
    }
    joined
}
