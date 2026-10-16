//! Everything the launch feature asks of the browser: the service worker that serves apps, Cache
//! Storage where their unpacked files live, streaming downloads, SHA-256, and the new tab.
//!
//! Only `web-sys`/`js-sys` calls, no hand-written JavaScript (the service worker itself, `sw.js`,
//! is the one JS file; a worker has to be).

use std::cell::Cell;
use std::collections::BTreeMap;

use js_sys::{Array, Promise, Reflect, Uint8Array};
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;

use crate::plan;
use crate::unzip::File;

/// The Cache Storage cache holding every unpacked app. Keep in sync with `CACHE` in `sw.js`.
const CACHE: &str = "craft-apps";
/// Largest download we'll hold in memory.
const MAX_DOWNLOAD: usize = 600 << 20;

/// What the user wants a running download to do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ctl {
    Run,
    Pause,
    Cancel,
}

/// Why a network step failed.
#[derive(Debug)]
pub enum Failure {
    Cancelled,
    RateLimited,
    Other(String),
}

impl Failure {
    fn other(text: impl Into<String>) -> Self {
        Self::Other(text.into())
    }
}

/// Progress of a download, for the UI.
#[derive(Clone, Copy, Debug)]
pub enum Tick {
    Bytes(u64, u64),
    Paused(u64, u64),
    Resumed,
}

/// A readable message for whatever the browser threw.
pub fn describe(e: &JsValue) -> String {
    if let Some(s) = e.as_string() {
        return s;
    }
    Reflect::get(e, &JsValue::from_str("message")).ok().and_then(|m| m.as_string()).unwrap_or_else(|| "unknown browser error".into())
}

/// Whether this page can run apps: it needs service workers and Cache Storage, which browsers
/// only offer on `https://` pages and `http://localhost`.
pub fn supported() -> bool {
    let Some(window) = web_sys::window() else { return false };
    let has = |target: &JsValue, key: &str| Reflect::get(target, &JsValue::from_str(key)).is_ok_and(|v| !v.is_undefined() && !v.is_null());
    has(&window.navigator(), "serviceWorker") && has(&window, "caches")
}

/// The folder the launcher is served from, with a trailing `/` (`https://example.com/launcher/`).
pub fn base_url() -> Option<String> {
    let href = web_sys::window()?.location().href().ok()?;
    let href = href.split(['?', '#']).next()?;
    let cut = href.rfind('/')?;
    href.get(..=cut).map(str::to_string)
}

pub async fn sleep(ms: i32) {
    let promise = Promise::new(&mut |resolve, _reject| {
        if let Some(window) = web_sys::window() {
            let _ = window.set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, ms);
        }
    });
    let _ = JsFuture::from(promise).await;
}

// ---- the app server (service worker) --------------------------------------------------------

/// Registers `sw.js` (next to the launcher's `index.html`) and waits until it is active.
pub async fn start_worker() -> Result<(), String> {
    let window = web_sys::window().ok_or("no browser window")?;
    let container = window.navigator().service_worker();
    JsFuture::from(container.register("./sw.js")).await.map_err(|e| format!("couldn't start the app server ({})", describe(&e)))?;
    let ready = container.ready().map_err(|e| describe(&e))?;
    JsFuture::from(ready).await.map_err(|e| format!("the app server didn't start ({})", describe(&e)))?;
    Ok(())
}

// ---- stored apps (Cache Storage) ------------------------------------------------------------

async fn open_cache() -> Result<web_sys::Cache, String> {
    let window = web_sys::window().ok_or("no browser window")?;
    let storage = window.caches().map_err(|e| format!("browser storage isn't available ({})", describe(&e)))?;
    let cache = JsFuture::from(storage.open(CACHE)).await.map_err(|e| format!("couldn't open browser storage ({})", describe(&e)))?;
    cache.dyn_into::<web_sys::Cache>().map_err(|_| "unexpected answer from browser storage".to_string())
}

async fn cached_urls(cache: &web_sys::Cache) -> Result<Vec<String>, String> {
    let keys = JsFuture::from(cache.keys()).await.map_err(|e| describe(&e))?;
    let list: Array = keys.dyn_into().map_err(|_| "unexpected answer from browser storage".to_string())?;
    Ok(list.iter().filter_map(|r| r.dyn_into::<web_sys::Request>().ok()).map(|r| r.url()).collect())
}

/// App id → saved version, for every app with a complete saved copy.
pub async fn saved_apps(base: &str) -> Result<BTreeMap<String, String>, String> {
    let cache = open_cache().await?;
    Ok(plan::saved_versions(base, &cached_urls(&cache).await?))
}

/// Stores an unpacked app, then drops its older versions. `index.html` goes in last, so an
/// interrupted store never looks like a complete copy.
pub async fn store(base: &str, id: &str, version: &str, files: Vec<File>) -> Result<(), String> {
    if !files.iter().any(|f| f.path == "index.html") {
        return Err("this app's web build has no index.html".into());
    }
    let cache = open_cache().await?;
    let (index, rest): (Vec<File>, Vec<File>) = files.into_iter().partition(|f| f.path == "index.html");
    for mut file in rest.into_iter().chain(index) {
        let url = plan::app_file_url(base, id, version, &file.path);
        let init = web_sys::ResponseInit::new();
        init.set_status(200);
        let headers = web_sys::Headers::new().map_err(|e| describe(&e))?;
        headers.set("content-type", plan::mime_for(&file.path)).map_err(|e| describe(&e))?;
        init.set_headers(&headers);
        let response = web_sys::Response::new_with_opt_u8_array_and_init(Some(file.data.as_mut_slice()), &init).map_err(|e| describe(&e))?;
        JsFuture::from(cache.put_with_str(&url, &response)).await.map_err(|e| format!("couldn't save {} ({}). Is the browser's storage full?", file.path, describe(&e)))?;
    }
    for url in cached_urls(&cache).await? {
        if plan::is_other_version(base, &url, id, version) {
            let _ = JsFuture::from(cache.delete_with_str(&url)).await;
        }
    }
    Ok(())
}

/// Deletes every saved version of an app.
pub async fn remove(base: &str, id: &str) -> Result<(), String> {
    let cache = open_cache().await?;
    for url in cached_urls(&cache).await? {
        // No version is kept, so every file of the app goes.
        if plan::is_other_version(base, &url, id, "") {
            let _ = JsFuture::from(cache.delete_with_str(&url)).await;
        }
    }
    Ok(())
}

// ---- saved answers ----------------------------------------------------------------------------

/// A value from the browser's local storage (`None` when absent or storage is unavailable).
pub fn saved_get(key: &str) -> Option<String> {
    web_sys::window()?.local_storage().ok()??.get_item(key).ok()?
}

/// Keeps a value in local storage. Best effort: a full or disabled storage just means no saving.
pub fn saved_set(key: &str, value: &str) {
    if let Some(Some(storage)) = web_sys::window().and_then(|w| w.local_storage().ok()) {
        let _ = storage.set_item(key, value);
    }
}

// ---- network ----------------------------------------------------------------------------------

/// The only host a GitHub token is ever sent to.
const API_HOST: &str = "https://api.github.com/";

async fn send(url: &str, accept: &str, token: Option<&str>, etag: Option<&str>) -> Result<web_sys::Response, Failure> {
    let window = web_sys::window().ok_or_else(|| Failure::other("no browser window"))?;
    let init = web_sys::RequestInit::new();
    init.set_method("GET");
    init.set_mode(web_sys::RequestMode::Cors);
    let request = web_sys::Request::new_with_str_and_init(url, &init).map_err(|e| Failure::other(describe(&e)))?;
    request.headers().set("Accept", accept).map_err(|e| Failure::other(describe(&e)))?;
    if let Some(etag) = etag {
        request.headers().set("If-None-Match", etag).map_err(|e| Failure::other(describe(&e)))?;
    }
    if let Some(token) = token
        && url.starts_with(API_HOST)
    {
        // Browsers drop this header when GitHub redirects a download to its file servers.
        request.headers().set("Authorization", &format!("Bearer {token}")).map_err(|e| Failure::other(describe(&e)))?;
    }
    let resp = JsFuture::from(window.fetch_with_request(&request)).await.map_err(|_| Failure::other("couldn't reach GitHub"))?;
    resp.dyn_into().map_err(|_| Failure::other("unexpected answer from the browser"))
}

/// A GET with the token (when given and the address is GitHub's API). A token GitHub refuses (401:
/// mistyped, expired or revoked) must not take the launcher down with it, so that answer is
/// retried once without it.
pub async fn raw(url: &str, accept: &str, token: Option<&str>, etag: Option<&str>) -> Result<web_sys::Response, Failure> {
    let resp = send(url, accept, token, etag).await?;
    if resp.status() == 401 && token.is_some() && url.starts_with(API_HOST) {
        return send(url, accept, None, etag).await;
    }
    Ok(resp)
}

async fn fetch(url: &str, accept: &str, token: Option<&str>) -> Result<web_sys::Response, Failure> {
    let resp = raw(url, accept, token, None).await?;
    match resp.status() {
        200 => Ok(resp),
        403 | 429 => Err(Failure::RateLimited),
        404 => Err(Failure::other("GitHub doesn't have that file")),
        status => Err(Failure::other(format!("GitHub answered {status}"))),
    }
}

/// Downloads `url` into memory, following `ctl` (pause keeps what's downloaded and the open
/// connection; resuming carries on). `expected` is the size the release lists, for progress when
/// the browser can't see the real one.
async fn download_one(url: &str, expected: u64, token: Option<&str>, ctl: &Cell<Ctl>, hook: &mut dyn FnMut(Tick)) -> Result<Vec<u8>, Failure> {
    let resp = fetch(url, "application/octet-stream", token).await?;
    let total = resp.headers().get("content-length").ok().flatten().and_then(|v| v.parse::<u64>().ok()).filter(|n| *n > 0).unwrap_or(expected);
    let stream = resp.body().ok_or_else(|| Failure::other("the download had no content"))?;
    let reader: web_sys::ReadableStreamDefaultReader = stream.get_reader().dyn_into().map_err(|_| Failure::other("couldn't read the download"))?;
    let mut out: Vec<u8> = Vec::with_capacity(usize::try_from(total).unwrap_or(0).min(64 << 20));
    loop {
        match ctl.get() {
            Ctl::Cancel => {
                let _ = reader.cancel();
                return Err(Failure::Cancelled);
            }
            Ctl::Pause => {
                hook(Tick::Paused(out.len() as u64, total));
                while ctl.get() == Ctl::Pause {
                    sleep(250).await;
                }
                if ctl.get() == Ctl::Run {
                    hook(Tick::Resumed);
                }
                continue;
            }
            Ctl::Run => {}
        }
        let chunk = JsFuture::from(reader.read()).await.map_err(|e| Failure::other(format!("the download was interrupted ({})", describe(&e))))?;
        if Reflect::get(&chunk, &JsValue::from_str("done")).ok().and_then(|d| d.as_bool()).unwrap_or(true) {
            break;
        }
        let value = Reflect::get(&chunk, &JsValue::from_str("value")).map_err(|e| Failure::other(describe(&e)))?;
        out.extend_from_slice(&Uint8Array::new(&value).to_vec());
        if out.len() > MAX_DOWNLOAD {
            let _ = reader.cancel();
            return Err(Failure::other("the download is bigger than expected, so it was stopped"));
        }
        hook(Tick::Bytes(out.len() as u64, total));
    }
    Ok(out)
}

/// Downloads a release file, trying the routes a browser may be allowed to use, in order: this
/// site's own copy (`mirror/`, same origin), the API's asset endpoint, then the plain release link.
pub async fn download(repo: &str, tag: &str, asset: &plan::AssetRef, token: Option<&str>, ctl: &Cell<Ctl>, hook: &mut dyn FnMut(Tick)) -> Result<Vec<u8>, Failure> {
    let mut routes = Vec::new();
    if let Some(base) = base_url() {
        routes.push(plan::mirror_asset_url(&base, repo, tag, &asset.name));
    }
    routes.push(plan::api_asset_url(repo, asset.id));
    routes.push(plan::direct_asset_url(repo, tag, &asset.name));
    let mut last = Failure::other(format!("couldn't download {}", asset.name));
    let mut limited = false;
    for url in routes {
        // The token is a last resort here: sent to the API's asset address it makes the browser
        // "preflight" GitHub's redirect to its file servers, and those answer 405, so the download
        // dies. Try without it first; use it only when GitHub's anonymous limit stopped the request.
        let mut result = download_one(&url, asset.size, None, ctl, hook).await;
        if matches!(result, Err(Failure::RateLimited)) {
            limited = true;
            if token.is_some() && url.starts_with(API_HOST) {
                result = download_one(&url, asset.size, token, ctl, hook).await;
            }
        }
        match result {
            Ok(bytes) => return Ok(bytes),
            Err(Failure::Cancelled) => return Err(Failure::Cancelled),
            Err(e) => last = e,
        }
    }
    // The other routes can't work from a browser (no CORS), so when GitHub's limit stopped the one
    // that can, that is the reason to give.
    if limited {
        return Err(Failure::RateLimited);
    }
    Err(match last {
        Failure::Other(m) => Failure::Other(format!("{m}. The browser may be blocking the download (GitHub's file servers don't always allow web pages to read them).")),
        other => other,
    })
}

/// Lowercase hex SHA-256 of `bytes`.
pub async fn sha256_hex(bytes: &mut [u8]) -> Result<String, String> {
    let window = web_sys::window().ok_or("no browser window")?;
    let subtle = window.crypto().map_err(|e| describe(&e))?.subtle();
    let promise = subtle.digest_with_str_and_u8_array("SHA-256", bytes).map_err(|e| describe(&e))?;
    let buffer = JsFuture::from(promise).await.map_err(|e| describe(&e))?;
    Ok(Uint8Array::new(&buffer).to_vec().iter().map(|b| format!("{b:02x}")).collect())
}

// ---- the new tab ------------------------------------------------------------------------------

const TAB_STYLE: &str = "margin:0;display:flex;align-items:center;justify-content:center;height:100vh;background:#121316;color:#f2f1ee;font:14px system-ui,sans-serif";

/// Opens an empty `about:blank` tab. Must run inside the click that asked for it, or pop-up
/// blockers refuse; `None` when the browser did refuse.
pub fn open_tab() -> Option<web_sys::Window> {
    let tab = web_sys::window()?.open_with_url_and_target("about:blank", "_blank").ok().flatten()?;
    tab_message(&tab, "Preparing…");
    Some(tab)
}

pub fn tab_message(tab: &web_sys::Window, text: &str) {
    let Some(body) = tab.document().and_then(|d| d.body()) else { return };
    let _ = body.set_attribute("style", TAB_STYLE);
    body.set_text_content(Some(text));
}

/// Fills the tab with the app: one full-page frame onto the app's own address on this server.
pub fn tab_show_app(tab: &web_sys::Window, url: &str, title: &str) -> Result<(), String> {
    let doc = tab.document().ok_or("the new tab was closed")?;
    let body = doc.body().ok_or("the new tab was closed")?;
    body.set_text_content(None);
    body.set_attribute("style", "margin:0;background:#121316;overflow:hidden").map_err(|e| describe(&e))?;
    let frame = doc.create_element("iframe").map_err(|e| describe(&e))?;
    for (name, value) in [
        ("src", url),
        ("title", title),
        ("allow", "fullscreen; clipboard-read; clipboard-write; autoplay"),
        ("style", "position:fixed;inset:0;width:100%;height:100%;border:0"),
    ] {
        frame.set_attribute(name, value).map_err(|e| describe(&e))?;
    }
    body.append_child(&frame).map_err(|e| describe(&e))?;
    doc.set_title(title);
    Ok(())
}

/// Opens a page in a new tab (for "show on GitHub").
pub fn open_page(url: &str) {
    if let Some(window) = web_sys::window() {
        let _ = window.open_with_url_and_target(url, "_blank");
    }
}
