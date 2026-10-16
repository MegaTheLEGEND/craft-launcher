//! The browser backend and entry point.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use artcraft_launcher_core::details::{AppDetails, MAX_SCREENSHOT_BYTES, looks_like_jpeg};
use artcraft_launcher_core::protocol::{Backend, Capabilities, CANCELLED, Command, Event, JobPhase, LatestRelease, Origin, PAUSED, PausedDownload, Source};
use artcraft_launcher_core::feed::{FEED_URL, Feed, FeedAnswer};
use artcraft_launcher_core::release::{self, Release};
use artcraft_launcher_core::{Arch, CraftApp, InstallSource, Installed, Os, Platform, SUITE, catalog, version};
use artcraft_launcher_ui_egui::{Host, LauncherUi, Settings, i18n};
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::JsFuture;

use crate::launch::{self, Ctl, Failure, Tick};
use crate::{plan, unzip};

const SETTINGS_KEY: &str = "settings";

/// Tell the UI the browser's languages (`?lang=ru` in the address first, as `ARTCRAFT_LAUNCHER_LOCALE`
/// does on the desktop) before the first frame, so the Automatic interface language follows the
/// browser; and show the loading text in that language.
fn detect_language() {
    let window = match web_sys::window() {
        Some(w) => w,
        None => return,
    };
    let document = match window.document() {
        Some(d) => d,
        None => return,
    };
    let mut tags = Vec::new();

    // ?lang=xx in the URL takes priority.
    if let Ok(href) = window.location().href()
        && let Some(idx) = href.find('?')
    {
        for pair in href[idx + 1..].split('&') {
            if let Some((k, v)) = pair.split_once('=')
                && k == "lang"
                && !v.is_empty()
            {
                tags.push(v.to_string());
            }
        }
    }

    // Then the browser's preferred languages.
    let navigator = window.navigator();
    let langs: js_sys::Array = navigator.languages();
    for i in 0..langs.length() {
        if let Some(v) = langs.get(i).as_string() {
            tags.push(v);
        }
    }
    if let Some(lang) = navigator.language() {
        tags.push(lang);
    }
    i18n::set_system_locales(&tags);

    // Screen readers and spell-check follow the page language: match <html lang>
    // to what was detected (`en` when the browser asks for nothing supported).
    if let Some(root) = document.document_element() {
        let _ = root.set_attribute("lang", i18n::system_lang().code());
    }

    // Update loading text in the detected language.
    if let Some(el) = document.get_element_by_id("launcher_loading") {
        el.set_text_content(Some(i18n::tr(i18n::system_lang(), "Loading ArtCraft Launcher…")));
    }
}
pub fn start() {
    eframe::WebLogger::init(log::LevelFilter::Warn).ok();
    // Detect language before starting the UI.
    detect_language();
    wasm_bindgen_futures::spawn_local(async {
        let Some(document) = web_sys::window().and_then(|w| w.document()) else { return };
        let Some(canvas) = document.get_element_by_id("launcher_canvas").and_then(|c| c.dyn_into::<web_sys::HtmlCanvasElement>().ok()) else {
            log::error!("no canvas");
            return;
        };
        let result = eframe::WebRunner::new()
            .start(
                canvas,
                eframe::WebOptions::default(),
                Box::new(|cc| {
                    let ctx = cc.egui_ctx.clone();
                    let settings: Settings = cc.storage.and_then(|s| eframe::get_value(s, SETTINGS_KEY)).unwrap_or_default();
                    let backend = WebBackend::new(ctx.clone());
                    Ok(Box::new(WebApp { ui: LauncherUi::new(&ctx, Box::new(backend), settings) }))
                }),
            )
            .await;
        if let Some(loading) = document.get_element_by_id("launcher_loading") {
            loading.remove();
        }
        if let Err(e) = result {
            log::error!("couldn't start: {e:?}");
        }
    });
}

/// The browser: nothing of the desktop's, but it knows whether the visitor asked for fewer animations.
struct WebHost;

impl Host for WebHost {
    fn reduce_motion(&self) -> bool {
        web_sys::window().and_then(|w| w.match_media("(prefers-reduced-motion: reduce)").ok().flatten()).is_some_and(|query| query.matches())
    }
}

struct WebApp {
    ui: LauncherUi,
}

impl eframe::App for WebApp {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.ui.logic(ctx, &mut WebHost);
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.ui.ui(ui, &mut WebHost);
    }

    fn raw_input_hook(&mut self, ctx: &egui::Context, raw_input: &mut egui::RawInput) {
        self.ui.raw_input(ctx, raw_input);
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        eframe::set_value(storage, SETTINGS_KEY, &self.ui.settings);
    }
}

/// Where backend work reports to: events for the UI, and a repaint so it notices.
#[derive(Clone)]
struct Out {
    events: Rc<RefCell<Vec<Event>>>,
    ctx: egui::Context,
}

impl Out {
    fn emit(&self, event: Event) {
        self.events.borrow_mut().push(event);
        self.ctx.request_repaint();
    }
}

struct WebBackend {
    out: Out,
    platform: Option<Platform>,
    checking: Rc<RefCell<bool>>,
    prereleases: bool,
    /// The GitHub token (empty: none). Nothing sets it right now: `BackendOptions` no longer carries one.
    token: Option<String>,
    /// Whether this page can run apps (service workers and browser storage are there).
    manage: bool,
    /// Running or paused launches and downloads, by app id.
    jobs: Rc<RefCell<HashMap<String, Rc<Cell<Ctl>>>>>,
    /// Apps whose details pictures are on their way (a second request meanwhile does nothing).
    pictures_busy: Rc<RefCell<HashSet<&'static str>>>,
}

impl WebBackend {
    fn new(ctx: egui::Context) -> Self {
        let manage = launch::supported();
        if manage {
            // Get the app server going early, so the first Open doesn't wait for it.
            wasm_bindgen_futures::spawn_local(async {
                if let Err(e) = launch::start_worker().await {
                    log::warn!("{e}");
                }
            });
        }
        let backend = Self {
            out: Out { events: Rc::default(), ctx },
            platform: visitor_platform(),
            checking: Rc::default(),
            prereleases: false,
            token: None,
            manage,
            jobs: Rc::default(),
            pictures_busy: Rc::default(),
        };
        // The UI waits for this answer before it shows the apps' buttons (it only re-asks every few minutes).
        if manage {
            backend.detect();
        }
        backend
    }

    /// Fetches the pictures of an app's details page, lead first. The browser keeps them in its own
    /// cache, so there is nothing to save here.
    fn fetch_screenshots(&self, app: &str) {
        let Some(details) = AppDetails::by_id(app) else { return };
        if !self.pictures_busy.borrow_mut().insert(details.id) {
            return;
        }
        let (out, busy) = (self.out.clone(), self.pictures_busy.clone());
        wasm_bindgen_futures::spawn_local(async move {
            // Once the network has failed the rest would fail the same way: say so at once.
            let mut offline: Option<String> = None;
            for shot in details.shots {
                let result = match (&offline, details.screenshot_url(shot.key)) {
                    (Some(why), _) => Err(why.clone()),
                    (None, Some(url)) => picture(&url).await.map_err(|failure| match failure {
                        PictureError::Network(why) => {
                            offline = Some(why.clone());
                            why
                        }
                        PictureError::Picture(why) => why,
                    }),
                    (None, None) => Err("this page has no such picture".to_string()),
                };
                out.emit(Event::Screenshot { app: details.id.to_string(), key: shot.key.to_string(), result });
            }
            busy.borrow_mut().remove(details.id);
        });
    }

    fn check(&self, force: bool) {
        if self.checking.replace(true) {
            return;
        }
        let out = self.out.clone();
        let checking = self.checking.clone();
        let pre = self.prereleases;
        let token = self.token.clone();
        wasm_bindgen_futures::spawn_local(async move {
            out.emit(Event::CheckStarted);
            let now = (js_sys::Date::now() / 1000.0) as i64;
            // One request to ArtCraft's release feed answers for every app. Pre-releases aren't in it,
            // so asking for them goes to GitHub; so does any app the feed can't vouch for.
            let feed = if pre { None } else { feed_lookup(force, now).await };
            let mut asked_github = false;
            let mut limited = None;
            for app in &SUITE {
                let from_feed = match feed.as_ref().map(|f| f.answer(app, now)) {
                    Some(FeedAnswer::Fresh(Some(r))) => Some(LatestRelease::Found(r)),
                    Some(FeedAnswer::Fresh(None)) => Some(LatestRelease::None),
                    Some(FeedAnswer::AskGitHub) | None => None,
                };
                if let Some(latest) = from_feed {
                    out.emit(Event::Latest { app: app.id.to_string(), latest, from: Origin::ArtCraftApi });
                    continue;
                }
                asked_github = true;
                let repo = feed.as_ref().map_or_else(|| app.repo.to_string(), |f| f.repo_for(app));
                let latest = latest(&repo, pre, token.as_deref(), force).await;
                if let Err(Some(reset)) = &latest {
                    limited = Some(*reset);
                }
                let latest = latest.unwrap_or_else(|e| {
                    LatestRelease::Failed(if e.is_some() { "GitHub's hourly limit was reached".into() } else { "couldn't reach GitHub".into() })
                });
                out.emit(Event::Latest { app: app.id.to_string(), latest, from: Origin::GitHub });
            }
            if let Ok(LatestRelease::Found(r)) = latest(catalog::LAUNCHER_REPO, false, token.as_deref(), force).await
                && let (Some(l), Some(c)) = (r.version.as_ref(), version::parse(env!("CARGO_PKG_VERSION")))
                && version::is_newer(l, &c)
            {
                out.emit(Event::LauncherUpdate(r));
            }
            let at = (js_sys::Date::now() / 1000.0) as i64;
            let source = if asked_github { Source::GitHubApi } else { Source::Feed };
            out.emit(Event::CheckFinished { at, source, rate_limited_until: limited });
            *checking.borrow_mut() = false;
        });
    }

    /// Every app is offered as "Open": the browser has nothing installed to look for, and Open
    /// fetches the newest build whenever it needs one. So no version is reported, which keeps
    /// the UI from ever offering a separate "update".
    fn detect(&self) {
        for app in &SUITE {
            let installed = Installed { version: None, source: InstallSource::Launcher, location: "Opens in a new browser tab".into(), package: None };
            self.out.emit(Event::Detected { app: app.id.to_string(), installed: Some(installed) });
        }
    }

    /// Starts a launch (`open_tab`) or a plain download of an app's newest web build.
    fn start(&self, id: &str, open_tab: bool) {
        let Some(app) = CraftApp::by_id(id) else { return };
        if self.jobs.borrow().contains_key(id) {
            return;
        }
        // The tab has to open now, inside the click, or pop-up blockers refuse it.
        let tab = if open_tab {
            let Some(tab) = launch::open_tab() else {
                let why = "The new tab was blocked. Allow pop-ups for this page, then try Open again.";
                self.out.emit(Event::JobFinished { app: id.to_string(), result: Err(why.into()) });
                return;
            };
            Some(tab)
        } else {
            None
        };
        let ctl = Rc::new(Cell::new(Ctl::Run));
        self.jobs.borrow_mut().insert(id.to_string(), ctl.clone());
        let out = self.out.clone();
        let jobs = self.jobs.clone();
        let pre = self.prereleases;
        let token = self.token.clone();
        wasm_bindgen_futures::spawn_local(async move {
            let result = run_job(app, tab.as_ref(), pre, token.as_deref(), &ctl, &out).await;
            jobs.borrow_mut().remove(app.id);
            if result.is_err()
                && let Some(tab) = &tab
            {
                let _ = tab.close();
            }
            if result.as_ref().is_err_and(|e| e == CANCELLED) {
                out.emit(Event::Paused { app: app.id.to_string(), download: None });
            }
            out.emit(Event::JobFinished { app: app.id.to_string(), result });
        });
    }

    fn remove(&self, id: &str) {
        let Some(app) = CraftApp::by_id(id) else { return };
        if self.jobs.borrow().contains_key(id) {
            return;
        }
        let out = self.out.clone();
        wasm_bindgen_futures::spawn_local(async move {
            out.emit(Event::Progress { app: app.id.to_string(), phase: JobPhase::Removing });
            let result = match launch::base_url() {
                Some(base) => launch::remove(&base, app.id).await.map(|()| format!("Removed the saved copy of {}", app.name)),
                None => Err("couldn't work out this page's address".into()),
            };
            out.emit(Event::JobFinished { app: app.id.to_string(), result });
        });
    }
}

/// One launch or download, start to finish: find the newest web build, fetch and check it, unpack
/// and store it, then (for a launch) show it in the new tab. Falls back to the saved copy when
/// GitHub can't be used. `Ok` is the message for the user.
async fn run_job(app: &'static CraftApp, tab: Option<&web_sys::Window>, prereleases: bool, token: Option<&str>, ctl: &Rc<Cell<Ctl>>, out: &Out) -> Result<String, String> {
    let id = app.id.to_string();
    let progress = |phase: JobPhase| out.emit(Event::Progress { app: id.clone(), phase });
    let tell = |text: &str| {
        if let Some(tab) = tab {
            launch::tab_message(tab, text);
        }
    };
    progress(JobPhase::Downloading { done: 0, total: 0 });
    tell(&format!("Opening {}…", app.name));
    let base = launch::base_url().ok_or("couldn't work out this page's address")?;
    launch::start_worker().await?;
    let saved = launch::saved_apps(&base).await.unwrap_or_default();

    let newest = match release_json(app.repo, prereleases, token, false).await {
        Lookup::Body(json) => plan::parse_plan(&json, app, prereleases),
        Lookup::Missing => Err("GitHub has no release of this app".to_string()),
        Lookup::Limited(_) => Err("GitHub's hourly limit was reached".to_string()),
        Lookup::Failed => Err("couldn't reach GitHub".to_string()),
    };
    let saved_version = saved.get(app.id).cloned();
    let (version, fallback) = match newest {
        Ok(p) if saved_version.as_deref() == Some(p.version.as_str()) => (p.version, None),
        Ok(p) => match install(app, &p, &base, token, ctl, out, &tell).await {
            Ok(()) => (p.version, None),
            Err(e) if e == CANCELLED || e == PAUSED => return Err(e),
            Err(e) => match saved_version {
                Some(v) => (v, Some(e)),
                None => return Err(e),
            },
        },
        Err(e) if e == CANCELLED => return Err(e),
        Err(e) => match saved_version {
            Some(v) => (v, Some(e)),
            None => return Err(e),
        },
    };

    if let Some(tab) = tab {
        launch::tab_show_app(tab, &plan::app_root_url(&base, app.id, &version), app.name)?;
    }
    Ok(match (tab.is_some(), fallback) {
        (true, None) => format!("Opened {} {version}", app.name),
        (true, Some(why)) => format!("Opened the saved copy of {} {version} ({why})", app.name),
        (false, _) => format!("{} {version} is saved in this browser", app.name),
    })
}

/// Downloads, verifies, unpacks and stores one release's web build.
async fn install(app: &'static CraftApp, p: &plan::Plan, base: &str, token: Option<&str>, ctl: &Rc<Cell<Ctl>>, out: &Out, tell: &dyn Fn(&str)) -> Result<(), String> {
    let id = app.id.to_string();
    let progress = |phase: JobPhase| out.emit(Event::Progress { app: id.clone(), phase });

    // The checksum comes first: nothing is worth downloading if it can't be verified.
    let expected = match (&p.zip.sha256, &p.sums) {
        (Some(hash), _) => hash.clone(),
        (None, Some(sums)) => {
            let bytes = launch::download(app.repo, &p.tag, sums, token, ctl, &mut |_tick: Tick| {}).await.map_err(failure_text)?;
            plan::sum_for(&String::from_utf8_lossy(&bytes), &p.zip.name)
                .ok_or_else(|| format!("{} {} isn't listed in its checksums, so it can't be verified", app.name, p.version))?
        }
        (None, None) => return Err(format!("{} {} publishes no checksums, so it can't be verified", app.name, p.version)),
    };

    tell(&format!("Downloading {} {}…", app.name, p.version));
    let mut zip = {
        let mut hook = |tick: Tick| match tick {
            Tick::Bytes(done, total) => progress(JobPhase::Downloading { done, total }),
            Tick::Paused(done, total) => {
                out.emit(Event::Paused { app: id.clone(), download: Some(PausedDownload { done, total }) });
                out.emit(Event::JobFinished { app: id.clone(), result: Err(PAUSED.to_string()) });
            }
            Tick::Resumed => {
                out.emit(Event::Paused { app: id.clone(), download: None });
                progress(JobPhase::Downloading { done: 0, total: 0 });
            }
        };
        launch::download(app.repo, &p.tag, &p.zip, token, ctl, &mut hook).await.map_err(failure_text)?
    };

    progress(JobPhase::Verifying);
    tell("Verifying the download…");
    let actual = launch::sha256_hex(&mut zip).await?;
    if actual != expected {
        return Err(format!("{} didn't match its published checksum, so it wasn't opened", p.zip.name));
    }

    progress(JobPhase::Installing);
    tell(&format!("Unpacking {}…", app.name));
    let files = unzip::unzip(&zip)?;
    drop(zip);
    launch::store(base, app.id, &p.version, files).await
}

fn failure_text(f: Failure) -> String {
    match f {
        Failure::Cancelled => CANCELLED.to_string(),
        Failure::RateLimited => "GitHub's hourly limit was reached".to_string(),
        Failure::Other(m) => m,
    }
}

/// What asking GitHub (or the saved answer) for an app's releases came to.
enum Lookup {
    /// The release JSON.
    Body(String),
    /// The repo has no release.
    Missing,
    /// Rate-limited; the reset time (unix seconds, 0 when GitHub didn't say).
    Limited(i64),
    Failed,
}

/// Why a picture couldn't be had.
enum PictureError {
    /// This picture only: it isn't published, or what came back isn't a picture.
    Picture(String),
    /// The network or the server: the other pictures would fail the same way.
    Network(String),
}

/// One picture of a details page. The size cap is checked on the announced length first and again
/// on what arrived; the bytes must start like a JPEG.
async fn picture(url: &str) -> Result<Vec<u8>, PictureError> {
    let offline = |what: &str| PictureError::Network(format!("couldn't load a picture ({what})"));
    let window = web_sys::window().ok_or_else(|| offline("no window"))?;
    let init = web_sys::RequestInit::new();
    init.set_method("GET");
    init.set_mode(web_sys::RequestMode::Cors);
    let request = web_sys::Request::new_with_str_and_init(url, &init).map_err(|_| offline("bad request"))?;
    let resp: web_sys::Response = JsFuture::from(window.fetch_with_request(&request))
        .await
        .map_err(|_| offline("no connection"))?
        .dyn_into()
        .map_err(|_| offline("no answer"))?;
    match resp.status() {
        200 => {}
        404 => return Err(PictureError::Picture("this picture isn't published".into())),
        status => return Err(PictureError::Network(format!("the server answered HTTP {status}"))),
    }
    let too_big = || PictureError::Picture(format!("a picture of more than {MAX_SCREENSHOT_BYTES} bytes"));
    let announced = resp.headers().get("content-length").ok().flatten().and_then(|n| n.parse::<usize>().ok());
    if announced.is_some_and(|n| n > MAX_SCREENSHOT_BYTES) {
        return Err(too_big());
    }
    let buffer = JsFuture::from(resp.array_buffer().map_err(|_| offline("unreadable"))?).await.map_err(|_| offline("unreadable"))?;
    let bytes = js_sys::Uint8Array::new(&buffer).to_vec();
    if bytes.len() > MAX_SCREENSHOT_BYTES {
        return Err(too_big());
    }
    if !looks_like_jpeg(&bytes) {
        return Err(PictureError::Picture("the answer isn't a JPEG picture".into()));
    }
    Ok(bytes)
}

async fn body_text(resp: &web_sys::Response) -> Option<String> {
    JsFuture::from(resp.text().ok()?).await.ok()?.as_string()
}

/// Where ArtCraft's release feed is kept in local storage.
const FEED_KEY: &str = "artcraft-launcher:feed";

/// ArtCraft's release feed (the same one the desktop launcher reads): the saved copy when under five
/// minutes old, else a fresh request, else whatever copy is saved (each app's entry still has to be
/// recent enough, [`Feed::answer`] checks). `None` when there's nothing usable, which sends the
/// check to GitHub. The server has to allow cross-origin reads from the page for the request to
/// succeed; when it doesn't, this is simply `None` every time.
async fn feed_lookup(force: bool, now: i64) -> Option<Feed> {
    let saved = launch::saved_get(FEED_KEY).and_then(|text| plan::SavedAnswer::parse(&text));
    if !force
        && let Some(s) = &saved
        && s.is_fresh(now)
        && let Some(feed) = Feed::parse(&s.body)
    {
        return Some(feed);
    }
    let etag = saved.as_ref().and_then(|s| s.etag.clone());
    let fetched = match launch::raw(FEED_URL, "application/json", None, etag.as_deref()).await {
        Ok(resp) => match resp.status() {
            200 => body_text(&resp).await.map(|body| plan::SavedAnswer { at: now, etag: resp.headers().get("etag").ok().flatten(), body }),
            304 => saved.clone().map(|s| plan::SavedAnswer { at: now, ..s }),
            _ => None,
        },
        Err(_) => None,
    };
    let answer = fetched.filter(|a| Feed::parse(&a.body).is_some()).or(saved)?;
    if answer.at == now
        && let Some(text) = answer.encode()
    {
        launch::saved_set(FEED_KEY, &text);
    }
    Feed::parse(&answer.body)
}

/// A repo's release JSON, kept in local storage between page loads: an answer under five minutes
/// old is used as it is; an older one is confirmed with a conditional request (`If-None-Match`; a
/// 304 costs no rate limit); and when GitHub can't be asked (offline, limited), the saved answer,
/// however old, beats nothing. `force` (the user pressed Refresh) skips the first step.
async fn release_json(repo: &str, prereleases: bool, token: Option<&str>, force: bool) -> Lookup {
    let key = plan::cache_key(repo, prereleases);
    let now = (js_sys::Date::now() / 1000.0) as i64;
    let saved = launch::saved_get(&key).and_then(|text| plan::SavedAnswer::parse(&text));
    if !force && let Some(s) = &saved && s.is_fresh(now) {
        return Lookup::Body(s.body.clone());
    }
    let offline = |saved: Option<plan::SavedAnswer>, otherwise: Lookup| saved.map_or(otherwise, |s| Lookup::Body(s.body));
    let url = plan::latest_url(repo, prereleases);
    let etag = saved.as_ref().and_then(|s| s.etag.clone());
    let Ok(resp) = launch::raw(&url, "application/vnd.github+json", token, etag.as_deref()).await else {
        return offline(saved, Lookup::Failed);
    };
    let keep = |answer: &plan::SavedAnswer| {
        if let Some(text) = answer.encode() {
            launch::saved_set(&key, &text);
        }
    };
    match resp.status() {
        200 => {
            let Some(body) = body_text(&resp).await else { return offline(saved, Lookup::Failed) };
            let etag = resp.headers().get("etag").ok().flatten();
            keep(&plan::SavedAnswer { at: now, etag, body: body.clone() });
            Lookup::Body(body)
        }
        304 => match saved {
            Some(mut s) => {
                s.at = now;
                keep(&s);
                Lookup::Body(s.body)
            }
            None => Lookup::Failed,
        },
        404 => Lookup::Missing,
        403 | 429 => {
            let reset = resp.headers().get("x-ratelimit-reset").ok().flatten().and_then(|r| r.parse().ok()).unwrap_or(0);
            offline(saved, Lookup::Limited(reset))
        }
        _ => offline(saved, Lookup::Failed),
    }
}

/// The latest release; `Err(Some(reset))` when rate-limited, `Err(None)` on other failures.
async fn latest(repo: &str, prereleases: bool, token: Option<&str>, force: bool) -> Result<LatestRelease, Option<i64>> {
    let text = match release_json(repo, prereleases, token, force).await {
        Lookup::Body(text) => text,
        Lookup::Missing => return Ok(LatestRelease::None),
        Lookup::Limited(reset) => return Err(Some(reset)),
        Lookup::Failed => return Err(None),
    };
    let found: Option<Release> = if prereleases {
        release::parse_github_release_list(&text, repo, true).map_err(|_| None)?
    } else {
        Some(release::parse_github_release(&text, repo).map_err(|_| None)?)
    };
    Ok(found.map_or(LatestRelease::None, LatestRelease::Found))
}

/// The visitor's OS and CPU, as far as the browser says.
fn visitor_platform() -> Option<Platform> {
    let nav = web_sys::window()?.navigator();
    let ua = nav.user_agent().ok()?.to_lowercase();
    let os = if ua.contains("windows") {
        Os::Windows
    } else if ua.contains("mac os") || ua.contains("macintosh") {
        Os::MacOs
    } else if ua.contains("freebsd") {
        Os::FreeBsd
    } else if ua.contains("linux") && !ua.contains("android") {
        Os::Linux
    } else {
        return None;
    };
    let arch = if ua.contains("aarch64") || ua.contains("arm64") || ua.contains("armv8") {
        Arch::Arm64
    } else if ua.contains("riscv") {
        Arch::Riscv64
    } else {
        Arch::X64
    };
    Some(Platform { os, arch })
}

impl Backend for WebBackend {
    fn send(&mut self, command: Command) {
        match command {
            Command::CheckForUpdates { force } => self.check(force),
            Command::FetchScreenshots { app } => self.fetch_screenshots(&app),
            Command::Configure(o) => {
                // The GitHub token setting is gone from `BackendOptions`, so `self.token` stays `None`.
                let changed = o.prereleases != self.prereleases;
                self.prereleases = o.prereleases;
                if changed {
                    self.check(false);
                }
            }
            _ if !self.manage => {}
            Command::Detect => self.detect(),
            Command::Launch { app } => self.start(&app, true),
            // Resume a paused download, or (Update/Retry) fetch the newest build without a tab.
            Command::Install { app } => {
                let paused = self.jobs.borrow().get(&app).filter(|c| c.get() == Ctl::Pause).cloned();
                match paused {
                    Some(ctl) => ctl.set(Ctl::Run),
                    None => self.start(&app, false),
                }
            }
            Command::Pause { app } => {
                if let Some(ctl) = self.jobs.borrow().get(&app).filter(|c| c.get() == Ctl::Run) {
                    ctl.set(Ctl::Pause);
                }
            }
            Command::Cancel { app } => {
                if let Some(ctl) = self.jobs.borrow().get(&app) {
                    ctl.set(Ctl::Cancel);
                }
            }
            Command::Uninstall { app } => self.remove(&app),
            Command::Reveal { app } => {
                if let Some(app) = CraftApp::by_id(&app) {
                    launch::open_page(&app.releases_url());
                }
            }
            Command::UpdateLauncher => {}
        }
    }

    fn poll(&mut self) -> Vec<Event> {
        std::mem::take(&mut *self.out.events.borrow_mut())
    }

    fn capabilities(&self) -> Capabilities {
        // "Reveal" has no meaning in a browser; the menu entry opens the app's releases instead.
        Capabilities { manage_apps: self.manage, reveal_label: if self.manage { "Show on GitHub" } else { "" }, self_update: false }
    }

    fn platform(&self) -> Option<Platform> {
        self.platform
    }

    fn now(&self) -> i64 {
        (js_sys::Date::now() / 1000.0) as i64
    }
}
