//! Tile fetching and decoding, off the render thread.
//!
//! The renderer hands over the complete, priority-sorted list of tiles it is missing every
//! frame ([`TileFetcher::set_queue`]); anything not yet started is simply replaced, so stale
//! requests for tiles that scrolled out of view are dropped for free.
//!
//! * Native: a small worker pool reads files (or HTTP) and decodes to RGBA.
//! * Web: `fetch` + `createImageBitmap` (decodes off the main thread), pixels read back
//!   through a reused `OffscreenCanvas`.

use nodal_core::pyramid::TileId;
use std::sync::Arc;

/// A decoded tile, RGBA8 (sRGB).
pub enum TileImage {
    Rgba { width: u32, height: u32, data: Vec<u8> },
}

impl TileImage {
    pub fn size(&self) -> (u32, u32) {
        match self {
            TileImage::Rgba { width, height, .. } => (*width, *height),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TileKey {
    pub generation: u32,
    pub tile: TileId,
}

#[derive(Clone, Debug)]
pub struct TileRequest {
    pub key: TileKey,
    pub url: String,
    pub priority: f32,
}

/// Called from any thread when a tile finished loading (use it to request a repaint).
pub type Waker = Arc<dyn Fn() + Send + Sync>;

pub type FetchResult = (TileKey, Result<TileImage, String>);

pub trait TileFetcher {
    /// Replaces everything that has not started yet with `reqs` (any order).
    fn set_queue(&mut self, reqs: Vec<TileRequest>);
    /// Finished tiles, at most `max`.
    fn poll(&mut self, max: usize) -> Vec<FetchResult>;
    /// Requests that are queued or running.
    fn pending(&self) -> usize;
    fn is_active(&self, key: &TileKey) -> bool;
}

#[cfg(not(target_arch = "wasm32"))]
pub use native::ThreadFetcher;

#[cfg(target_arch = "wasm32")]
pub use web::WebFetcher;

#[cfg(not(target_arch = "wasm32"))]
mod native {
    use super::*;
    use std::collections::HashSet;
    use std::sync::mpsc::{channel, Receiver, Sender};
    use std::sync::{Condvar, Mutex};

    struct State {
        /// Sorted ascending by priority: `pop()` yields the most urgent.
        queue: Vec<TileRequest>,
        active: HashSet<TileKey>,
        quit: bool,
    }

    struct Shared {
        state: Mutex<State>,
        cv: Condvar,
    }

    pub struct ThreadFetcher {
        shared: Arc<Shared>,
        rx: Receiver<FetchResult>,
        workers: Vec<std::thread::JoinHandle<()>>,
    }

    impl ThreadFetcher {
        pub fn new(threads: usize, waker: Option<Waker>) -> Self {
            let shared =
                Arc::new(Shared { state: Mutex::new(State { queue: vec![], active: HashSet::new(), quit: false }), cv: Condvar::new() });
            let (tx, rx) = channel();
            let agent = ureq::Agent::new_with_defaults();
            let workers = (0..threads.max(1))
                .map(|i| {
                    let shared = shared.clone();
                    let tx: Sender<FetchResult> = tx.clone();
                    let waker = waker.clone();
                    let agent = agent.clone();
                    std::thread::Builder::new()
                        .name(format!("nodal-fetch-{i}"))
                        .spawn(move || worker(shared, tx, waker, agent))
                        .expect("spawn fetch worker")
                })
                .collect();
            Self { shared, rx, workers }
        }
    }

    fn worker(shared: Arc<Shared>, tx: Sender<FetchResult>, waker: Option<Waker>, agent: ureq::Agent) {
        loop {
            let req = {
                let mut st = shared.state.lock().unwrap();
                loop {
                    if st.quit {
                        return;
                    }
                    if let Some(r) = st.queue.pop() {
                        st.active.insert(r.key);
                        break r;
                    }
                    st = shared.cv.wait(st).unwrap();
                }
            };
            let result = load(&agent, &req.url);
            if tx.send((req.key, result)).is_err() {
                return;
            }
            shared.state.lock().unwrap().active.remove(&req.key);
            if let Some(w) = &waker {
                w();
            }
        }
    }

    pub fn read_bytes(agent: &ureq::Agent, url: &str) -> Result<Vec<u8>, String> {
        if url.starts_with("http://") || url.starts_with("https://") {
            let mut resp = agent.get(url).call().map_err(|e| format!("{url}: {e}"))?;
            resp.body_mut().with_config().limit(64 << 20).read_to_vec().map_err(|e| format!("{url}: {e}"))
        } else {
            let path = url.strip_prefix("file://").unwrap_or(url);
            std::fs::read(path).map_err(|e| format!("{path}: {e}"))
        }
    }

    fn load(agent: &ureq::Agent, url: &str) -> Result<TileImage, String> {
        let bytes = read_bytes(agent, url)?;
        let img = image::load_from_memory(&bytes).map_err(|e| format!("{url}: {e}"))?.into_rgba8();
        let (width, height) = img.dimensions();
        Ok(TileImage::Rgba { width, height, data: img.into_raw() })
    }

    impl TileFetcher for ThreadFetcher {
        fn set_queue(&mut self, mut reqs: Vec<TileRequest>) {
            let mut st = self.shared.state.lock().unwrap();
            reqs.retain(|r| !st.active.contains(&r.key));
            reqs.sort_by(|a, b| a.priority.total_cmp(&b.priority));
            let had_work = !reqs.is_empty();
            st.queue = reqs;
            drop(st);
            if had_work {
                self.shared.cv.notify_all();
            }
        }

        fn poll(&mut self, max: usize) -> Vec<FetchResult> {
            let mut out = vec![];
            while out.len() < max {
                match self.rx.try_recv() {
                    Ok(r) => out.push(r),
                    Err(_) => break,
                }
            }
            out
        }

        fn pending(&self) -> usize {
            let st = self.shared.state.lock().unwrap();
            st.queue.len() + st.active.len()
        }

        fn is_active(&self, key: &TileKey) -> bool {
            self.shared.state.lock().unwrap().active.contains(key)
        }
    }

    impl Drop for ThreadFetcher {
        fn drop(&mut self) {
            self.shared.state.lock().unwrap().quit = true;
            self.shared.cv.notify_all();
            for w in self.workers.drain(..) {
                let _ = w.join();
            }
        }
    }
}

#[cfg(target_arch = "wasm32")]
mod web {
    use super::*;
    use std::cell::RefCell;
    use std::collections::HashSet;
    use std::rc::Rc;
    use wasm_bindgen::JsCast;
    use wasm_bindgen_futures::JsFuture;

    pub struct WebFetcher {
        queue: Vec<TileRequest>,
        active: Rc<RefCell<HashSet<TileKey>>>,
        done: Rc<RefCell<Vec<FetchResult>>>,
        max_active: usize,
        waker: Option<Waker>,
    }

    impl WebFetcher {
        pub fn new(max_active: usize, waker: Option<Waker>) -> Self {
            Self { queue: vec![], active: Default::default(), done: Default::default(), max_active: max_active.max(1), waker }
        }

        fn pump(&mut self) {
            while self.active.borrow().len() < self.max_active {
                let Some(req) = self.queue.pop() else { break };
                self.active.borrow_mut().insert(req.key);
                let done = self.done.clone();
                let active = self.active.clone();
                let waker = self.waker.clone();
                wasm_bindgen_futures::spawn_local(async move {
                    let res = fetch_bitmap(&req.url).await;
                    active.borrow_mut().remove(&req.key);
                    done.borrow_mut().push((req.key, res));
                    if let Some(w) = waker {
                        w();
                    }
                });
            }
        }
    }

    fn js_err(e: wasm_bindgen::JsValue) -> String {
        e.as_string().unwrap_or_else(|| format!("{e:?}"))
    }

    pub async fn fetch_bitmap(url: &str) -> Result<TileImage, String> {
        let window = web_sys::window().ok_or("no window")?;
        let resp: web_sys::Response = JsFuture::from(window.fetch_with_str(url)).await.map_err(js_err)?.dyn_into().map_err(js_err)?;
        if !resp.ok() {
            return Err(format!("{url}: HTTP {}", resp.status()));
        }
        let blob: web_sys::Blob = JsFuture::from(resp.blob().map_err(js_err)?).await.map_err(js_err)?.dyn_into().map_err(js_err)?;
        let opts = web_sys::ImageBitmapOptions::new();
        opts.set_color_space_conversion(web_sys::ColorSpaceConversion::None);
        opts.set_premultiply_alpha(web_sys::PremultiplyAlpha::None);
        let promise = window.create_image_bitmap_with_blob_and_image_bitmap_options(&blob, &opts).map_err(js_err)?;
        let bitmap: web_sys::ImageBitmap = JsFuture::from(promise).await.map_err(js_err)?.dyn_into().map_err(js_err)?;
        // `copyExternalImageToTexture` is not reliable on every adapter (it throws on some
        // software/compat adapters, which wgpu turns into a panic), so read the decoded pixels
        // back through a reused OffscreenCanvas and upload them with `write_texture` instead.
        // Decoding itself still happens off the main thread inside `createImageBitmap`.
        let (w, h) = (bitmap.width(), bitmap.height());
        let data = READBACK.with(|cell| -> Result<Vec<u8>, String> {
            let mut slot = cell.borrow_mut();
            let needs_new = slot.as_ref().map(|(c, _)| c.width() != w || c.height() != h).unwrap_or(true);
            if needs_new {
                let canvas = web_sys::OffscreenCanvas::new(w, h).map_err(js_err)?;
                let attrs = js_sys::Object::new();
                let _ = js_sys::Reflect::set(&attrs, &"willReadFrequently".into(), &true.into());
                let ctx: web_sys::OffscreenCanvasRenderingContext2d = canvas
                    .get_context_with_context_options("2d", &attrs)
                    .map_err(js_err)?
                    .ok_or("no 2d context")?
                    .dyn_into()
                    .map_err(|o: js_sys::Object| js_err(o.into()))?;
                *slot = Some((canvas, ctx));
            }
            let (_, ctx) = slot.as_ref().unwrap();
            ctx.draw_image_with_image_bitmap(&bitmap, 0.0, 0.0).map_err(js_err)?;
            let img = ctx.get_image_data(0.0, 0.0, w as f64, h as f64).map_err(js_err)?;
            Ok(img.data().0)
        });
        bitmap.close();
        Ok(TileImage::Rgba { width: w, height: h, data: data? })
    }

    thread_local! {
        static READBACK: RefCell<Option<(web_sys::OffscreenCanvas, web_sys::OffscreenCanvasRenderingContext2d)>> = const { RefCell::new(None) };
    }

    impl TileFetcher for WebFetcher {
        fn set_queue(&mut self, mut reqs: Vec<TileRequest>) {
            {
                let active = self.active.borrow();
                reqs.retain(|r| !active.contains(&r.key));
            }
            reqs.sort_by(|a, b| a.priority.total_cmp(&b.priority));
            self.queue = reqs;
            self.pump();
        }

        fn poll(&mut self, max: usize) -> Vec<FetchResult> {
            self.pump();
            let mut done = self.done.borrow_mut();
            let n = done.len().min(max);
            done.drain(..n).collect()
        }

        fn pending(&self) -> usize {
            self.queue.len() + self.active.borrow().len()
        }

        fn is_active(&self, key: &TileKey) -> bool {
            self.active.borrow().contains(key)
        }
    }
}
