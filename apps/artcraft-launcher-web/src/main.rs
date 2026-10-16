//! ArtCraft Launcher in the browser.
//!
//! A static site: the page lists the Craft App Suite with each app's latest release (read straight
//! from the GitHub API, which allows cross-origin requests). Where the browser allows service
//! workers (`https://` or `http://localhost`), **Open** also runs the app itself: it downloads the
//! app's newest `<id>-web-<version>.zip`, checks it against the published SHA-256, unpacks it into
//! browser storage, and a service worker (`sw.js`) serves it from this site's own `__app/` folder
//! into a new `about:blank` tab. No server code and nothing outside the browser. See HOSTING.md.
//! Elsewhere the page offers the usual downloads for the visitor's system.

#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

// Pure logic, unit-tested natively: choosing a release, paths in storage, unpacking a zip.
#[cfg(any(target_arch = "wasm32", test))]
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
mod plan;
#[cfg(any(target_arch = "wasm32", test))]
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
mod unzip;

#[cfg(target_arch = "wasm32")]
mod launch;
#[cfg(target_arch = "wasm32")]
mod web;

#[cfg(target_arch = "wasm32")]
fn main() {
    web::start();
}

/// The web build only runs in a browser: build it with `trunk build` (see HOSTING.md).
#[cfg(not(target_arch = "wasm32"))]
fn main() {
    println!("artcraft-launcher-web is the browser version of ArtCraft Launcher. Build it with `trunk build --release --cargo-profile wasm-release` in apps/artcraft-launcher-web.");
}
