//! Minimal async HTTP used by both targets: blocking `ureq` on a worker thread natively,
//! `fetch` in the browser.

use std::future::Future;

pub struct HttpResponse {
    pub status: u16,
    pub body: Vec<u8>,
}

impl HttpResponse {
    pub fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).to_string()
    }
    pub fn error_message(&self) -> String {
        let t = self.text();
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&t) {
            if let Some(e) = v.get("error").and_then(|e| e.as_str()) {
                return e.to_string();
            }
        }
        format!("HTTP {}: {}", self.status, t.chars().take(200).collect::<String>())
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub fn spawn(f: impl Future<Output = ()> + Send + 'static) {
    std::thread::spawn(move || pollster::block_on(f));
}

#[cfg(target_arch = "wasm32")]
pub fn spawn(f: impl Future<Output = ()> + 'static) {
    wasm_bindgen_futures::spawn_local(f);
}

#[cfg(not(target_arch = "wasm32"))]
pub async fn request(method: &str, url: &str, body: Option<Vec<u8>>, headers: Vec<(String, String)>) -> Result<HttpResponse, String> {
    let agent: ureq::Agent = ureq::Agent::config_builder().http_status_as_error(false).build().into();
    let result = match body {
        Some(b) => {
            let mut req = match method {
                "POST" => agent.post(url),
                "PUT" => agent.put(url),
                m => return Err(format!("unsupported method {m}")),
            };
            for (k, v) in &headers {
                req = req.header(k.as_str(), v.as_str());
            }
            req.send(&b[..])
        }
        None => {
            let mut req = match method {
                "GET" => agent.get(url),
                "DELETE" => agent.delete(url),
                m => return Err(format!("unsupported method {m}")),
            };
            for (k, v) in &headers {
                req = req.header(k.as_str(), v.as_str());
            }
            req.call()
        }
    };
    let mut resp = result.map_err(|e| format!("{url}: {e}"))?;
    let status = resp.status().as_u16();
    let body = resp.body_mut().with_config().limit(256 << 20).read_to_vec().map_err(|e| e.to_string())?;
    Ok(HttpResponse { status, body })
}

#[cfg(target_arch = "wasm32")]
pub async fn request(method: &str, url: &str, body: Option<Vec<u8>>, headers: Vec<(String, String)>) -> Result<HttpResponse, String> {
    use wasm_bindgen::JsCast;
    use wasm_bindgen_futures::JsFuture;
    let err = |e: wasm_bindgen::JsValue| e.as_string().unwrap_or_else(|| format!("{e:?}"));
    let window = web_sys::window().ok_or("no window")?;
    let opts = web_sys::RequestInit::new();
    opts.set_method(method);
    if let Some(b) = body {
        let arr = js_sys::Uint8Array::from(&b[..]);
        opts.set_body(&arr.into());
    }
    let h = web_sys::Headers::new().map_err(err)?;
    for (k, v) in &headers {
        h.set(k, v).map_err(err)?;
    }
    opts.set_headers(&h.into());
    let req = web_sys::Request::new_with_str_and_init(url, &opts).map_err(err)?;
    let resp: web_sys::Response = JsFuture::from(window.fetch_with_request(&req)).await.map_err(err)?.dyn_into().map_err(err)?;
    let buf = JsFuture::from(resp.array_buffer().map_err(err)?).await.map_err(err)?;
    Ok(HttpResponse { status: resp.status(), body: js_sys::Uint8Array::new(&buf).to_vec() })
}
