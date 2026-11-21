//! Small platform shims (fullscreen, URL state, data directories).

#[cfg(target_arch = "wasm32")]
fn window() -> web_sys::Window {
    web_sys::window().expect("window")
}

/// `scheme://host:port` of the page on web, empty on native.
pub fn origin() -> String {
    #[cfg(target_arch = "wasm32")]
    {
        window().location().origin().unwrap_or_default()
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        String::new()
    }
}

/// Query string and hash parameters of the page (web). `?tour=x#scene=y&yaw=..` → pairs.
pub fn url_params() -> Vec<(String, String)> {
    #[cfg(target_arch = "wasm32")]
    {
        let loc = window().location();
        let mut out = vec![];
        for part in [loc.search().unwrap_or_default(), loc.hash().unwrap_or_default()] {
            let s = part.trim_start_matches(['?', '#']).to_string();
            if let Ok(params) = web_sys::UrlSearchParams::new_with_str(&s) {
                let keys = js_sys::try_iter(&params.keys()).ok().flatten();
                if let Some(keys) = keys {
                    for k in keys.flatten() {
                        if let Some(k) = k.as_string() {
                            if let Some(v) = params.get(&k) {
                                out.push((k, v));
                            }
                        }
                    }
                }
            }
        }
        out
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        vec![]
    }
}

/// Replaces the URL hash without adding a history entry (shareable deep links).
pub fn set_hash(hash: &str) {
    #[cfg(target_arch = "wasm32")]
    {
        if let Ok(h) = window().history() {
            let _ = h.replace_state_with_url(&wasm_bindgen::JsValue::NULL, "", Some(&format!("#{hash}")));
        }
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        let _ = hash;
    }
}

pub fn is_fullscreen(ctx: &egui::Context) -> bool {
    #[cfg(target_arch = "wasm32")]
    {
        let _ = ctx;
        window().document().and_then(|d| d.fullscreen_element()).is_some()
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        ctx.input(|i| i.viewport().fullscreen.unwrap_or(false))
    }
}

pub fn toggle_fullscreen(ctx: &egui::Context) {
    let full = is_fullscreen(ctx);
    #[cfg(target_arch = "wasm32")]
    {
        if let Some(doc) = window().document() {
            if full {
                doc.exit_fullscreen();
            } else if let Some(el) = doc.document_element() {
                let _ = el.request_fullscreen();
            }
        }
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(!full));
    }
}

/// Where the native app keeps tours it creates.
#[cfg(not(target_arch = "wasm32"))]
pub fn tours_dir() -> std::path::PathBuf {
    dirs::data_dir().unwrap_or_else(std::env::temp_dir).join("nodal").join("tours")
}

/// `file://` URI for a local path (for egui image loaders).
#[cfg(not(target_arch = "wasm32"))]
pub fn file_uri(path: &std::path::Path) -> String {
    let abs = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let s = abs.to_string_lossy().replace('\\', "/");
    if s.starts_with('/') {
        format!("file://{s}")
    } else {
        format!("file:///{s}")
    }
}
