//! Nodal server: hosts tours and the web viewer, and tiles uploaded panoramas.
//!
//! Environment:
//! * `NODAL_DATA`        data directory (default `/data`); tours live in `$NODAL_DATA/tours/<id>/`
//! * `NODAL_PORT`        listen port (default `8080`)
//! * `NODAL_WEB`         web viewer directory (default `/app/web`, or `dist/web` when present)
//! * `NODAL_TOKEN`       if set, uploads/edits/deletes require `Authorization: Bearer <token>`
//! * `NODAL_READONLY`    `true` disables all changes
//! * `NODAL_MAX_UPLOAD_MB` upload size limit (default `2048`)
//! * `NODAL_QUALITY`     JPEG tile quality (default `86`)

use axum::body::Body;
use axum::extract::{DefaultBodyLimit, Path, State};
use axum::http::{header, HeaderMap, HeaderValue, Request, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use futures_util::StreamExt;
use nodal_core::tour::{slugify, Tour, TourSettings};
use serde::Serialize;
use std::collections::HashMap;
use std::path::{Path as FsPath, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;
use tower_http::compression::CompressionLayer;
use tower_http::services::{ServeDir, ServeFile};
use tower_http::trace::TraceLayer;

#[derive(Clone, Debug, Serialize)]
struct JobStatus {
    id: String,
    state: &'static str,
    progress: f32,
    message: String,
    tour: String,
}

struct Job {
    id: String,
    tour: String,
    upload: PathBuf,
    file_name: String,
    tour_title: Option<String>,
}

struct Inner {
    tours: PathBuf,
    uploads: PathBuf,
    token: Option<String>,
    readonly: bool,
    quality: u8,
    jobs: Mutex<HashMap<String, JobStatus>>,
    queue: mpsc::UnboundedSender<Job>,
    /// Serialises every read-modify-write of a `tour.json`.
    tour_lock: Mutex<()>,
}

type AppState = Arc<Inner>;

#[derive(Debug)]
struct ApiError(StatusCode, String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(serde_json::json!({ "error": self.1 }))).into_response()
    }
}

impl<E: std::fmt::Display> From<E> for ApiError {
    fn from(e: E) -> Self {
        ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
    }
}

fn bad(msg: impl Into<String>) -> ApiError {
    ApiError(StatusCode::BAD_REQUEST, msg.into())
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).ok().filter(|v| !v.is_empty()).unwrap_or_else(|| default.to_string())
}

fn now_ms() -> u128 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0)
}

/// Tour ids are slugs; anything else is rejected before touching the filesystem.
fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

fn check_write(state: &Inner, headers: &HeaderMap) -> Result<(), ApiError> {
    if state.readonly {
        return Err(ApiError(StatusCode::FORBIDDEN, "this server is read-only".into()));
    }
    if let Some(token) = &state.token {
        let given = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Bearer ")).unwrap_or("");
        if !constant_time_eq(given.as_bytes(), token.as_bytes()) {
            return Err(ApiError(StatusCode::UNAUTHORIZED, "missing or wrong access token".into()));
        }
    }
    Ok(())
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

async fn config(State(s): State<AppState>) -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "writable": !s.readonly,
        "auth_required": s.token.is_some(),
        "version": env!("CARGO_PKG_VERSION"),
    }))
}

#[derive(Serialize)]
struct TourSummary {
    id: String,
    title: String,
    scenes: usize,
    thumb: Option<String>,
    updated: u64,
}

async fn list_tours(State(s): State<AppState>) -> Result<Json<Vec<TourSummary>>, ApiError> {
    let dir = s.tours.clone();
    let list = tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<TourSummary>> {
        let mut out = vec![];
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let id = entry.file_name().to_string_lossy().to_string();
            let path = entry.path().join("tour.json");
            if !valid_id(&id) || !path.exists() {
                continue;
            }
            let Ok(tour) =
                std::fs::read_to_string(&path).map_err(anyhow::Error::from).and_then(|t| Tour::from_json(&t).map_err(Into::into))
            else {
                continue;
            };
            let updated = std::fs::metadata(&path)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let start = &tour.scenes[tour.start_index()];
            out.push(TourSummary { id, title: tour.title.clone(), scenes: tour.scenes.len(), thumb: start.thumb.clone(), updated });
        }
        out.sort_by_key(|t| std::cmp::Reverse(t.updated));
        Ok(out)
    })
    .await??;
    Ok(Json(list))
}

/// Streams the request body to `uploads/<job>` without holding it in memory.
async fn save_upload(s: &Inner, job: &str, body: Body) -> Result<PathBuf, ApiError> {
    let path = s.uploads.join(job);
    let mut file = tokio::fs::File::create(&path).await?;
    let mut stream = body.into_data_stream();
    let mut total = 0usize;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| bad(format!("upload interrupted: {e}")))?;
        total += chunk.len();
        file.write_all(&chunk).await?;
    }
    file.flush().await?;
    if total < 1024 {
        let _ = tokio::fs::remove_file(&path).await;
        return Err(bad("upload is empty or too small to be a panorama"));
    }
    Ok(path)
}

fn header_str(headers: &HeaderMap, key: &str) -> Option<String> {
    headers.get(key).and_then(|v| v.to_str().ok()).map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

fn enqueue(s: &Inner, job: Job) -> JobStatus {
    let status = JobStatus {
        id: job.id.clone(),
        state: "queued",
        progress: 0.0,
        message: format!("Queued {}", job.file_name),
        tour: job.tour.clone(),
    };
    s.jobs.lock().unwrap().insert(job.id.clone(), status.clone());
    let _ = s.queue.send(job);
    status
}

async fn create_tour(State(s): State<AppState>, headers: HeaderMap, body: Body) -> Result<Json<JobStatus>, ApiError> {
    check_write(&s, &headers)?;
    let file_name = header_str(&headers, "x-filename").unwrap_or_else(|| "panorama.jpg".into());
    let title = header_str(&headers, "x-title").unwrap_or_else(|| title_from_name(&file_name));
    // Reserve a unique tour directory.
    let id = {
        let _g = s.tour_lock.lock().unwrap();
        let base = slugify(&title);
        let mut id = base.clone();
        let mut n = 2;
        while s.tours.join(&id).exists() {
            id = format!("{base}-{n}");
            n += 1;
        }
        std::fs::create_dir_all(s.tours.join(&id))?;
        id
    };
    let job_id = format!("{:x}", now_ms());
    let upload = save_upload(&s, &job_id, body).await?;
    Ok(Json(enqueue(&s, Job { id: job_id, tour: id, upload, file_name, tour_title: Some(title) })))
}

async fn add_scene(State(s): State<AppState>, Path(id): Path<String>, headers: HeaderMap, body: Body) -> Result<Json<JobStatus>, ApiError> {
    check_write(&s, &headers)?;
    if !valid_id(&id) || !s.tours.join(&id).join("tour.json").exists() {
        return Err(ApiError(StatusCode::NOT_FOUND, format!("no tour '{id}'")));
    }
    let file_name = header_str(&headers, "x-filename").unwrap_or_else(|| "panorama.jpg".into());
    let job_id = format!("{:x}", now_ms());
    let upload = save_upload(&s, &job_id, body).await?;
    Ok(Json(enqueue(&s, Job { id: job_id, tour: id, upload, file_name, tour_title: None })))
}

async fn job_status(State(s): State<AppState>, Path(id): Path<String>) -> Result<Json<JobStatus>, ApiError> {
    s.jobs.lock().unwrap().get(&id).cloned().map(Json).ok_or_else(|| ApiError(StatusCode::NOT_FOUND, "unknown job".into()))
}

/// Saves editor changes. The client may change titles, views, hotspots, order and settings,
/// but scene pyramids always come from disk; scenes the client dropped are deleted.
async fn save_tour(State(s): State<AppState>, Path(id): Path<String>, headers: HeaderMap, body: String) -> Result<StatusCode, ApiError> {
    check_write(&s, &headers)?;
    if !valid_id(&id) {
        return Err(bad("invalid tour id"));
    }
    let dir = s.tours.join(&id);
    let mut incoming = Tour::from_json(&body).map_err(|e| bad(e.to_string()))?;
    let s2 = s.clone();
    tokio::task::spawn_blocking(move || -> Result<(), ApiError> {
        let _g = s2.tour_lock.lock().unwrap();
        let current = Tour::from_json(
            &std::fs::read_to_string(dir.join("tour.json")).map_err(|_| ApiError(StatusCode::NOT_FOUND, "no such tour".into()))?,
        )
        .map_err(|e| bad(e.to_string()))?;
        for scene in &mut incoming.scenes {
            let Some(disk) = current.scene(&scene.id) else {
                return Err(bad(format!("unknown scene '{}'", scene.id)));
            };
            scene.pyramid = disk.pyramid.clone();
            scene.source = disk.source.clone();
            scene.thumb = disk.thumb.clone();
        }
        incoming.validate().map_err(|e| bad(e.to_string()))?;
        nodal_tiler::write_tour(&incoming, &dir)?;
        for old in &current.scenes {
            if incoming.scene(&old.id).is_none() && valid_id(&old.id) {
                let _ = std::fs::remove_dir_all(dir.join("scenes").join(&old.id));
            }
        }
        Ok(())
    })
    .await??;
    Ok(StatusCode::NO_CONTENT)
}

async fn delete_tour(State(s): State<AppState>, Path(id): Path<String>, headers: HeaderMap) -> Result<StatusCode, ApiError> {
    check_write(&s, &headers)?;
    if !valid_id(&id) {
        return Err(bad("invalid tour id"));
    }
    let dir = s.tours.join(&id);
    if !dir.exists() {
        return Err(ApiError(StatusCode::NOT_FOUND, "no such tour".into()));
    }
    tokio::fs::remove_dir_all(dir).await?;
    Ok(StatusCode::NO_CONTENT)
}

fn title_from_name(name: &str) -> String {
    nodal_tiler::title_from_file(FsPath::new(name))
}

/// Tiles one upload into its tour (runs on a blocking thread).
fn process(s: &Inner, job: &Job) -> anyhow::Result<()> {
    let dir = s.tours.join(&job.tour);
    let set = |p: f32, m: &str| {
        if let Some(st) = s.jobs.lock().unwrap().get_mut(&job.id) {
            st.progress = p;
            st.message = m.to_string();
        }
    };
    set(0.02, &format!("Decoding {}", job.file_name));
    let img = nodal_tiler::load_image(&job.upload)?;
    let title = title_from_name(&job.file_name);
    // Reserve a scene id (the directory is the reservation).
    let scene_id = {
        let _g = s.tour_lock.lock().unwrap();
        let tour_path = dir.join("tour.json");
        let existing = if tour_path.exists() { Some(Tour::from_json(&std::fs::read_to_string(&tour_path)?)?) } else { None };
        let base = slugify(&title);
        let taken = |c: &str| existing.as_ref().map(|t| t.scene(c).is_some()).unwrap_or(false) || dir.join("scenes").join(c).exists();
        let mut id = base.clone();
        let mut n = 2;
        while taken(&id) {
            id = format!("{base}-{n}");
            n += 1;
        }
        std::fs::create_dir_all(dir.join("scenes").join(&id))?;
        id
    };
    let opts = nodal_tiler::TileOptions { quality: s.quality, ..Default::default() };
    let report = |f: f32, stage: &str| set(0.05 + 0.93 * f, &format!("{title}: {stage}"));
    let scene = nodal_tiler::tile_scene(img, &dir, &scene_id, &title, &job.file_name, &opts, &report)?;
    let _g = s.tour_lock.lock().unwrap();
    let tour_path = dir.join("tour.json");
    let mut tour = if tour_path.exists() {
        Tour::from_json(&std::fs::read_to_string(&tour_path)?)?
    } else {
        Tour {
            version: nodal_core::tour::FORMAT_VERSION,
            title: job.tour_title.clone().unwrap_or_else(|| title.clone()),
            description: String::new(),
            start_scene: None,
            settings: TourSettings::default(),
            scenes: vec![],
        }
    };
    tour.scenes.push(scene);
    if tour.start_scene.is_none() {
        tour.start_scene = Some(scene_id);
    }
    nodal_tiler::write_tour(&tour, &dir)?;
    Ok(())
}

async fn worker(s: AppState, mut rx: mpsc::UnboundedReceiver<Job>) {
    while let Some(job) = rx.recv().await {
        if let Some(st) = s.jobs.lock().unwrap().get_mut(&job.id) {
            st.state = "running";
        }
        let s2 = s.clone();
        let result = tokio::task::spawn_blocking(move || {
            let r = process(&s2, &job);
            let _ = std::fs::remove_file(&job.upload);
            (job, r)
        })
        .await;
        match result {
            Ok((job, Ok(()))) => {
                tracing::info!("job {} done (tour {})", job.id, job.tour);
                if let Some(st) = s.jobs.lock().unwrap().get_mut(&job.id) {
                    st.state = "done";
                    st.progress = 1.0;
                    st.message = "Done".into();
                }
            }
            Ok((job, Err(e))) => {
                tracing::error!("job {} failed: {e:#}", job.id);
                // A brand-new tour whose first scene failed leaves nothing worth keeping.
                let dir = s.tours.join(&job.tour);
                if !dir.join("tour.json").exists() {
                    let _ = std::fs::remove_dir_all(&dir);
                }
                if let Some(st) = s.jobs.lock().unwrap().get_mut(&job.id) {
                    st.state = "failed";
                    st.message = format!("{e:#}");
                }
            }
            Err(e) => tracing::error!("tiling task panicked: {e}"),
        }
    }
}

/// Long-lived caching for tiles, revalidation for everything that can change in place.
async fn cache_headers(req: Request<Body>, next: Next) -> Response {
    let path = req.uri().path().to_string();
    let mut resp = next.run(req).await;
    let value =
        if path.ends_with(".json") || path.ends_with(".html") || path.ends_with('/') || path.ends_with(".js") || path.ends_with(".wasm") {
            "no-cache"
        } else if path.contains("/tiles/") {
            "public, max-age=604800"
        } else {
            "public, max-age=3600"
        };
    resp.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static(value));
    resp
}

fn router(state: AppState, web: PathBuf, max_upload: usize) -> Router {
    let tours = ServeDir::new(&state.tours).precompressed_gzip();
    let web_index = web.join("index.html");
    let site = ServeDir::new(&web).precompressed_br().precompressed_gzip().fallback(ServeFile::new(web_index));
    Router::new()
        .route("/api/config", get(config))
        .route("/api/tours", get(list_tours).post(create_tour))
        .route("/api/tours/{id}", put(save_tour).delete(delete_tour))
        .route("/api/tours/{id}/scenes", post(add_scene))
        .route("/api/jobs/{id}", get(job_status))
        .route("/healthz", get(|| async { "ok" }))
        .layer(DefaultBodyLimit::max(max_upload))
        .with_state(state)
        .nest_service("/tours", tours)
        .fallback_service(site)
        .layer(middleware::from_fn(cache_headers))
        .layer(CompressionLayer::new())
        .layer(TraceLayer::new_for_http())
}

/// `nodal-server healthcheck`: exit 0 if the local server answers `/healthz` (for Docker HEALTHCHECK,
/// since the distroless runtime image has no shell or curl).
fn healthcheck() -> std::process::ExitCode {
    use std::io::{Read, Write};
    let port = env_or("NODAL_PORT", "8080");
    let ok = (|| -> std::io::Result<bool> {
        let mut s = std::net::TcpStream::connect(format!("127.0.0.1:{port}"))?;
        s.set_read_timeout(Some(std::time::Duration::from_secs(3)))?;
        s.write_all(b"GET /healthz HTTP/1.0\r\nHost: localhost\r\n\r\n")?;
        let mut buf = String::new();
        s.read_to_string(&mut buf)?;
        Ok(buf.starts_with("HTTP/1.1 200") || buf.starts_with("HTTP/1.0 200"))
    })()
    .unwrap_or(false);
    if ok {
        std::process::ExitCode::SUCCESS
    } else {
        std::process::ExitCode::FAILURE
    }
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        if let Ok(mut s) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            s.recv().await;
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {},
        _ = term => {},
    }
    tracing::info!("shutting down");
}

fn main() -> anyhow::Result<std::process::ExitCode> {
    if std::env::args().nth(1).as_deref() == Some("healthcheck") {
        return Ok(healthcheck());
    }
    tokio::runtime::Builder::new_multi_thread().enable_all().build()?.block_on(serve())?;
    Ok(std::process::ExitCode::SUCCESS)
}

async fn serve() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info,tower_http=warn".into()))
        .init();
    let data = PathBuf::from(env_or("NODAL_DATA", "/data"));
    let tours = data.join("tours");
    let uploads = data.join("uploads");
    std::fs::create_dir_all(&tours)?;
    std::fs::create_dir_all(&uploads)?;
    // Leftovers from an interrupted run.
    for e in std::fs::read_dir(&uploads)?.flatten() {
        let _ = std::fs::remove_file(e.path());
    }
    let web = std::env::var("NODAL_WEB").map(PathBuf::from).unwrap_or_else(|_| {
        if FsPath::new("dist/web/index.html").exists() {
            PathBuf::from("dist/web")
        } else {
            PathBuf::from("/app/web")
        }
    });
    let port: u16 = env_or("NODAL_PORT", "8080").parse()?;
    let max_upload = env_or("NODAL_MAX_UPLOAD_MB", "2048").parse::<usize>()? << 20;
    let (tx, rx) = mpsc::unbounded_channel();
    let state = Arc::new(Inner {
        tours,
        uploads,
        token: std::env::var("NODAL_TOKEN").ok().filter(|t| !t.is_empty()),
        readonly: matches!(env_or("NODAL_READONLY", "false").to_ascii_lowercase().as_str(), "1" | "true" | "yes"),
        quality: env_or("NODAL_QUALITY", "86").parse().unwrap_or(86),
        jobs: Mutex::new(HashMap::new()),
        queue: tx,
        tour_lock: Mutex::new(()),
    });
    tokio::spawn(worker(state.clone(), rx));
    if state.token.is_none() && !state.readonly {
        tracing::warn!("NODAL_TOKEN is not set: anyone who can reach this server can upload, edit and delete tours");
    }
    let app = router(state, web.clone(), max_upload);
    let listener = tokio::net::TcpListener::bind(("0.0.0.0", port)).await?;
    tracing::info!("Nodal {} listening on :{port} (data {}, web {})", env!("CARGO_PKG_VERSION"), data.display(), web.display());
    axum::serve(listener, app).with_graceful_shutdown(shutdown_signal()).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_cannot_escape_the_data_dir() {
        assert!(valid_id("venice-sunset-2"));
        for bad in ["", "..", "../etc", "a/b", "A", "x".repeat(65).as_str(), "tour.json", "a b"] {
            assert!(!valid_id(bad), "{bad:?}");
        }
    }

    #[test]
    fn token_comparison() {
        assert!(constant_time_eq(b"s3cret", b"s3cret"));
        assert!(!constant_time_eq(b"s3cret", b"s3cre"));
        assert!(!constant_time_eq(b"s3cret", b"s3creT"));
    }

    #[test]
    fn titles_from_uploads() {
        assert_eq!(title_from_name("piazza_bologni.jpg"), "Piazza Bologni");
    }
}
