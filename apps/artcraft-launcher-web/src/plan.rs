//! The pure part of launching: choosing which release and file to fetch from GitHub's answer,
//! where an unpacked app lives in Cache Storage, and what content type a file gets. No browser
//! calls here, so it is all unit-tested natively.

use std::collections::BTreeMap;

use artcraft_launcher_core::CraftApp;
use artcraft_launcher_core::release::{parse_sha256sums, valid_file_name};

/// Folder (below the launcher's own URL) the service worker serves apps from. Keep in sync with
/// `PREFIX` in `sw.js`.
pub const APP_PREFIX: &str = "__app/";

/// A file of a release, as GitHub's API describes it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AssetRef {
    pub id: u64,
    pub name: String,
    pub size: u64,
    /// Lowercase hex SHA-256, when the API reports a digest.
    pub sha256: Option<String>,
}

/// What to download for one app.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Plan {
    pub tag: String,
    /// `0.1.1` for tag `v0.1.1`; safe to use as a path segment.
    pub version: String,
    pub zip: AssetRef,
    pub sums: Option<AssetRef>,
}

/// `v0.1.1` → `0.1.1`.
pub fn version_of(tag: &str) -> &str {
    tag.strip_prefix(['v', 'V']).unwrap_or(tag)
}

/// The names an app's web zip may have: `<id>-web-<version>.zip`, then under its former ids.
pub fn asset_names(app: &CraftApp, version: &str) -> Vec<String> {
    app.stems().map(|stem| format!("{stem}-web-{version}.zip")).collect()
}

/// Tags and versions become URL path segments, so only plain characters pass.
fn safe_segment(s: &str) -> bool {
    !s.is_empty() && s.len() <= 64 && !s.starts_with('.') && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-+".contains(&b))
}

fn is_hex64(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

fn asset_ref(v: &serde_json::Value) -> Option<AssetRef> {
    let name = v["name"].as_str().filter(|n| valid_file_name(n))?.to_string();
    let id = v["id"].as_u64()?;
    let sha256 = v["digest"].as_str().and_then(|d| d.strip_prefix("sha256:")).filter(|h| is_hex64(h)).map(str::to_ascii_lowercase);
    Some(AssetRef { id, name, size: v["size"].as_u64().unwrap_or(0), sha256 })
}

fn plan_from_release(v: &serde_json::Value, app: &CraftApp) -> Result<Plan, String> {
    let tag = v["tag_name"].as_str().filter(|t| safe_segment(t)).ok_or("GitHub sent a release without a usable tag")?.to_string();
    let version = version_of(&tag).to_string();
    if !safe_segment(&version) {
        return Err("GitHub sent a release without a usable version".into());
    }
    let assets: Vec<&serde_json::Value> = v["assets"].as_array().map(|a| a.iter().collect()).unwrap_or_default();
    let names = asset_names(app, &version);
    let zip = names
        .iter()
        .find_map(|n| assets.iter().find(|a| a["name"].as_str() == Some(n.as_str())).and_then(|a| asset_ref(a)))
        .ok_or_else(|| format!("{} {version} has no web version to open", app.name))?;
    let sums = assets.iter().find(|a| a["name"].as_str() == Some("SHA256SUMS.txt")).and_then(|a| asset_ref(a));
    Ok(Plan { tag, version, zip, sums })
}

/// Reads GitHub's answer: one release (`/releases/latest`) or a list (`/releases`). A list yields
/// the first published release (pre-releases only when `prereleases`) that has a web build.
pub fn parse_plan(json: &str, app: &CraftApp, prereleases: bool) -> Result<Plan, String> {
    let v: serde_json::Value = serde_json::from_str(json).map_err(|e| format!("unreadable answer from GitHub ({e})"))?;
    if let Some(list) = v.as_array() {
        let mut last_err = None;
        for item in list {
            if item["draft"].as_bool().unwrap_or(false) || (item["prerelease"].as_bool().unwrap_or(false) && !prereleases) {
                continue;
            }
            match plan_from_release(item, app) {
                Ok(p) => return Ok(p),
                Err(e) => last_err = Some(e),
            }
        }
        return Err(last_err.unwrap_or_else(|| format!("{} has no published release yet", app.name)));
    }
    if let Some(msg) = v["message"].as_str() {
        return Err(if msg == "Not Found" { format!("{} has no published release yet", app.name) } else { format!("GitHub says: {msg}") });
    }
    plan_from_release(&v, app)
}

/// The checksum `SHA256SUMS.txt` lists for `asset`.
pub fn sum_for(sums_text: &str, asset: &str) -> Option<String> {
    parse_sha256sums(sums_text).into_iter().find(|(_, name)| name == asset).map(|(hash, _)| hash)
}

pub fn latest_url(repo: &str, prereleases: bool) -> String {
    if prereleases { format!("https://api.github.com/repos/{repo}/releases?per_page=30") } else { format!("https://api.github.com/repos/{repo}/releases/latest") }
}

/// How long a saved GitHub answer is used without asking again.
pub const RELEASE_TTL_SECS: i64 = 5 * 60;

/// Where a repo's release answer is kept in the browser's local storage.
pub fn cache_key(repo: &str, prereleases: bool) -> String {
    format!("artcraft-launcher:release:{repo}:{}", if prereleases { "all" } else { "latest" })
}

/// A GitHub release answer kept between page loads, with the `ETag` to ask "still the same?" with.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SavedAnswer {
    /// Unix seconds when GitHub last confirmed it.
    pub at: i64,
    pub etag: Option<String>,
    pub body: String,
}

impl SavedAnswer {
    pub fn parse(text: &str) -> Option<Self> {
        serde_json::from_str(text).ok()
    }

    pub fn encode(&self) -> Option<String> {
        serde_json::to_string(self).ok()
    }

    /// Recent enough to use without asking. A clock that went backwards counts as not fresh.
    pub fn is_fresh(&self, now: i64) -> bool {
        (0..RELEASE_TTL_SECS).contains(&(now - self.at))
    }
}

/// The API's download endpoint for an asset (with `Accept: application/octet-stream` it redirects to
/// the file, and, unlike `github.com/.../releases/download`, answers cross-origin requests).
pub fn api_asset_url(repo: &str, id: u64) -> String {
    format!("https://api.github.com/repos/{repo}/releases/assets/{id}")
}

/// The ordinary download link, tried when the API route fails.
pub fn direct_asset_url(repo: &str, tag: &str, name: &str) -> String {
    format!("https://github.com/{repo}/releases/download/{tag}/{name}")
}

/// Where the site's own copy of a release file is kept (`mirror.sh` fills it when the site is
/// deployed): `<launcher folder>/mirror/<owner>/<repo>/<tag>/<file>`. Same origin, so no CORS.
pub fn mirror_asset_url(base: &str, repo: &str, tag: &str, name: &str) -> String {
    format!("{base}mirror/{repo}/{tag}/{name}")
}

/// Where a file of an app lives: `<base>__app/<id>/<version>/<path>`; `base` ends in `/`.
pub fn app_file_url(base: &str, id: &str, version: &str, path: &str) -> String {
    format!("{base}{APP_PREFIX}{id}/{version}/{path}")
}

/// The address the new tab shows (the service worker answers a folder with its `index.html`).
pub fn app_root_url(base: &str, id: &str, version: &str) -> String {
    app_file_url(base, id, version, "")
}

/// Which app versions are fully stored, from the URLs of everything in the cache: a copy counts
/// only once its `index.html` is there (it is stored last).
pub fn saved_versions(base: &str, urls: &[String]) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for url in urls {
        let Some(rest) = url.strip_prefix(base).and_then(|r| r.strip_prefix(APP_PREFIX)) else { continue };
        let mut parts = rest.split('/');
        if let (Some(id), Some(version), Some("index.html"), None) = (parts.next(), parts.next(), parts.next(), parts.next()) {
            out.insert(id.to_string(), version.to_string());
        }
    }
    out
}

/// `true` for a cached URL that belongs to `id` but not to `keep_version`.
pub fn is_other_version(base: &str, url: &str, id: &str, keep_version: &str) -> bool {
    let own = format!("{base}{APP_PREFIX}{id}/");
    url.strip_prefix(&own).is_some_and(|rest| !rest.starts_with(&format!("{keep_version}/")))
}

/// Content type for a file name (the service worker has the same table as a fallback).
pub fn mime_for(path: &str) -> &'static str {
    let ext = path.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default();
    match ext.as_str() {
        "html" | "htm" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript",
        "css" => "text/css",
        "json" | "map" => "application/json",
        "wasm" => "application/wasm",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "ico" => "image/x-icon",
        "txt" => "text/plain; charset=utf-8",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        "webmanifest" => "application/manifest+json",
        "mp3" => "audio/mpeg",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "ogg" => "audio/ogg",
        "wav" => "audio/wav",
        "pdf" => "application/pdf",
        "xml" => "application/xml",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saved_answers_expire_and_round_trip() {
        let saved = SavedAnswer { at: 1_000, etag: Some("W/\"abc\"".into()), body: "{}".into() };
        assert!(saved.is_fresh(1_000) && saved.is_fresh(1_000 + RELEASE_TTL_SECS - 1));
        assert!(!saved.is_fresh(1_000 + RELEASE_TTL_SECS) && !saved.is_fresh(999));
        assert_eq!(SavedAnswer::parse(&saved.encode().unwrap()), Some(saved));
        assert_eq!(SavedAnswer::parse("not json"), None);
        assert_ne!(cache_key("a/b", true), cache_key("a/b", false));
    }

    const HASH: &str = "8a5edab282632443219e051e4ade2d1d5bbc671c781051bf1437897cbdfea0f1";

    fn photo() -> &'static CraftApp {
        CraftApp::by_id("photocraft").unwrap()
    }

    fn release(tag: &str, prerelease: bool, with_zip: bool) -> String {
        let zip = if with_zip {
            format!(r#"{{"id":11,"name":"photocraft-web-{}.zip","size":1000,"digest":"sha256:{HASH}"}},"#, version_of(tag))
        } else {
            String::new()
        };
        format!(r#"{{"tag_name":"{tag}","prerelease":{prerelease},"draft":false,"assets":[{zip}{{"id":12,"name":"SHA256SUMS.txt","size":90}},{{"id":13,"name":"photocraft-0.1.1.msi","size":5}}]}}"#)
    }

    #[test]
    fn picks_the_web_zip_and_checksums_of_the_latest_release() {
        let p = parse_plan(&release("v0.1.1", false, true), photo(), false).unwrap();
        assert_eq!((p.tag.as_str(), p.version.as_str()), ("v0.1.1", "0.1.1"));
        assert_eq!(p.zip, AssetRef { id: 11, name: "photocraft-web-0.1.1.zip".into(), size: 1000, sha256: Some(HASH.into()) });
        assert_eq!(p.sums.map(|s| s.id), Some(12));
    }

    #[test]
    fn a_release_without_a_web_build_is_reported() {
        let e = parse_plan(&release("v0.1.1", false, false), photo(), false).unwrap_err();
        assert!(e.contains("no web version"), "{e}");
    }

    #[test]
    fn lists_skip_drafts_prereleases_and_builds_without_a_web_zip() {
        let list = format!("[{},{},{}]", release("v0.3.0-rc.1", true, true), release("v0.2.0", false, false), release("v0.1.1", false, true));
        assert_eq!(parse_plan(&list, photo(), false).unwrap().version, "0.1.1");
        assert_eq!(parse_plan(&list, photo(), true).unwrap().version, "0.3.0-rc.1");
        assert!(parse_plan("[]", photo(), false).unwrap_err().contains("no published release"));
    }

    #[test]
    fn github_errors_and_hostile_tags_are_refused() {
        assert!(parse_plan(r#"{"message":"Not Found"}"#, photo(), false).unwrap_err().contains("no published release"));
        assert!(parse_plan(r#"{"message":"API rate limit exceeded"}"#, photo(), false).unwrap_err().contains("rate limit"));
        for tag in ["../x", "v1/../../x", "a b", ".hidden", ""] {
            assert!(parse_plan(&release(tag, false, true), photo(), false).is_err(), "{tag:?}");
        }
        assert!(parse_plan("not json", photo(), false).is_err());
    }

    #[test]
    fn former_names_are_found_too() {
        let pdf = CraftApp::by_id("pdfcraft").unwrap();
        assert_eq!(asset_names(pdf, "0.2.0"), ["pdfcraft-web-0.2.0.zip", "printcraft-web-0.2.0.zip"]);
        let json = r#"{"tag_name":"v0.2.0","assets":[{"id":5,"name":"printcraft-web-0.2.0.zip","size":1}]}"#;
        assert_eq!(parse_plan(json, pdf, false).unwrap().zip.id, 5);
    }

    #[test]
    fn checksums_are_looked_up_by_file_name() {
        let text = format!("{HASH}  photocraft-web-0.1.1.zip\n{HASH}  other.zip\n");
        assert_eq!(sum_for(&text, "photocraft-web-0.1.1.zip").as_deref(), Some(HASH));
        assert_eq!(sum_for(&text, "missing.zip"), None);
    }

    #[test]
    fn urls_and_saved_copies() {
        let base = "https://h.test/launcher/";
        assert_eq!(app_file_url(base, "photocraft", "0.1.1", "js/a.js"), "https://h.test/launcher/__app/photocraft/0.1.1/js/a.js");
        assert_eq!(app_root_url(base, "photocraft", "0.1.1"), "https://h.test/launcher/__app/photocraft/0.1.1/");
        let urls: Vec<String> = [
            "https://h.test/launcher/__app/photocraft/0.1.1/index.html",
            "https://h.test/launcher/__app/photocraft/0.1.1/js/index.html",
            "https://h.test/launcher/__app/wordcraft/0.2.0/app.js", // no index.html yet: not complete
            "https://other.test/__app/vectorcraft/1/index.html",
        ]
        .map(String::from)
        .into();
        let saved = saved_versions(base, &urls);
        assert_eq!(saved.len(), 1);
        assert_eq!(saved.get("photocraft").map(String::as_str), Some("0.1.1"));
        assert!(is_other_version(base, "https://h.test/launcher/__app/photocraft/0.1.0/index.html", "photocraft", "0.1.1"));
        assert!(!is_other_version(base, "https://h.test/launcher/__app/photocraft/0.1.1/index.html", "photocraft", "0.1.1"));
        assert!(!is_other_version(base, "https://h.test/launcher/__app/wordcraft/0.1.0/index.html", "photocraft", "0.1.1"));
        // "0.1.1" must not keep "0.1.10" alive by prefix accident, nor the reverse.
        assert!(is_other_version(base, "https://h.test/launcher/__app/photocraft/0.1.10/index.html", "photocraft", "0.1.1"));
    }

    #[test]
    fn content_types() {
        assert_eq!(mime_for("a/app_bg.wasm"), "application/wasm");
        assert_eq!(mime_for("index.HTML"), "text/html; charset=utf-8");
        assert_eq!(mime_for("noext"), "application/octet-stream");
    }

    #[test]
    fn the_site_copy_of_a_release_file_lives_under_mirror() {
        assert_eq!(
            mirror_asset_url("https://me.github.io/launcher/", "storytold/photocraft", "v0.1.1", "photocraft-web-0.1.1.zip"),
            "https://me.github.io/launcher/mirror/storytold/photocraft/v0.1.1/photocraft-web-0.1.1.zip"
        );
    }
}
