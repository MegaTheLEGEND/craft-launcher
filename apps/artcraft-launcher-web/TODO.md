# Launch-in-a-new-tab (web build)

Goal: host the files `trunk build` makes (`dist/web`) on any static server. Click **Open** on any app ->
the browser downloads + unzips the newest `<app>-web-<ver>.zip`, serves it from its own address, and opens it
in a new `about:blank` tab. All of it happens in the browser; no server process.

All changes are inside `apps/artcraft-launcher-web` (plus the hosting notes in `packaging/web/README.md`,
and the web package's entry in `Cargo.lock`). `ui-egui`, `core` and `native` are untouched.

## How it works
- `sw.js` – the "web server": a service worker answering `<launcher folder>/__app/<id>/<version>/<path>`
  from Cache Storage (index.html for folders, Range requests, SPA fallback). Copied to `dist/web` by Trunk.
- `src/web.rs` – the backend: Open -> `window.open("about:blank")` inside the click, then newest release ->
  download (progress / pause / cancel) -> SHA-256 -> unzip -> store -> frame in the tab. Falls back to the
  saved copy when GitHub is unreachable. Install (Update/Resume/Retry) does the same without a tab.
  Uninstall deletes the saved copy. Every app reports "installed" (no version), so Open is always offered.
- `src/launch.rs` – the browser calls (service worker, Cache Storage, streaming fetch, SubtleCrypto, tab).
- `src/plan.rs` – pure: pick the web zip + checksum from GitHub's JSON, storage paths, content types. Tested.
- `src/unzip.rs` – pure: in-memory zip reader (zip-slip, size caps, CRC). Tested.
- Without service workers (plain `http://`) the page behaves as before: downloads only.

## Status: WRITTEN BUT NOT COMPILED OR RUN HERE (no rustc / wasm target in the sandbox)
Only `sw.js` was run (Node, mocked caches). Everything Rust still needs its first build.

## TODO
1. [ ] `cargo test -p artcraft-launcher-web` (plan.rs + unzip.rs run natively), then
       `trunk build --release --cargo-profile wasm-release` in this folder; fix compile errors / clippy
       (`cargo xtask ci`). Likeliest trouble spots: web-sys method names/feature list in `Cargo.toml`,
       `Response::new_with_opt_u8_array_and_init`, `ReadableStream::get_reader`.
2. [ ] **Verify the download route (the main risk).** In a browser, from a served `dist/web`, Open PhotoCraft
       (v0.1.1, ~10 MB). Expected: `GET api.github.com/repos/storytold/photocraft/releases/assets/<id>` with
       `Accept: application/octet-stream` redirects to a signed URL whose response allows CORS. Checked
       from a shell: `release-assets.githubusercontent.com` (the file server) sends no `Access-Control-Allow-Origin`, so
       from a web page this is expected to fail. The apps' zips need to be served from somewhere that sends it (the
       ArtCraft API, or this site itself).
2b. [ ] Check every app publishes `<id>-web-<ver>.zip` (+ `SHA256SUMS.txt`, or an API `digest`).
3. [ ] Test in Chrome, Firefox, Safari: pop-up blocking (the tab opens from inside the egui frame after the
       click), writing to the `about:blank` document, iframe + service worker inside `about:blank`.
4. [ ] Serve `dist/web` with `python3 -m http.server` (localhost counts as secure) and from a real https host.
5. [ ] `packaging/web/package.sh` already copies everything in `dist/web` (so `sw.js` ships); confirm the zip has it.

## Known limitations / decisions to confirm
- Apps share the launcher's origin (a static host can't give each app its own), so they share storage.
- The UI is desktop-shaped: with `manage_apps` on it also shows "Installing on Linux" settings, "Quit",
  and the menu's "Reveal" entry (relabelled "Show on GitHub", opens the releases page). Fixing needs ui-egui.
- Button says "Open" (ui-egui wording).
- GitHub's API limit (60/h per IP) is shared by the version check (12 apps) and each Open (1-2 calls).
- Apps needing COOP/COEP (SharedArrayBuffer) aren't isolated inside an about:blank frame.
- An app that registers its own service worker is not supported (ours owns the scope).
- Memory: the zip and its unpacked files are held in memory (caps: 600 MiB download, 512 MiB unpacked).

## GitHub token (Settings > GitHub > Access token)
- [ ] Check in a browser: with a token the API calls carry `Authorization` (60 -> 5000 requests/hour), and the asset
      download still works after GitHub redirects to its file servers (the browser drops the header there). A refused
      token (401) is retried without it, so a bad token must not break anything.

## Release answers are cached (localStorage, `artcraft-launcher:release:*`)
- [ ] Reload a few times: no GitHub requests within 5 min; after that a conditional request answering 304 (check the
      browser allows `If-None-Match` cross-origin to api.github.com). Refresh bypasses the 5 min window.
- [ ] With pre-releases on, the 30-release lists are big (~100-300 KB each); a full storage just stops saving.

## ArtCraft release feed (api.storyteller.ai) for checks
- [ ] Check the browser console on load: a CORS error for `api.storyteller.ai/v1/craft_apps/release_info` means the server
      must send `Access-Control-Allow-Origin` for the host serving this page; until then checks fall back to GitHub.
- [ ] Open still reads GitHub (the feed has no asset ids, and the direct download links aren't CORS-readable).

## Site mirror of the apps' web zips (GitHub's file servers send no CORS headers)
- Downloads try, in order: `<launcher folder>/mirror/<owner>/<repo>/<tag>/<file>` (same origin), the API asset endpoint, the plain
  release link. Only the first can work from a web page.
- `mirror.sh <dist folder>` fills `mirror/` with each app's latest `*-web-*.zip` and `SHA256SUMS.txt` (needs `gh` and `GH_TOKEN`).
  Run it after `trunk build` and before the Pages upload. Example GitHub Actions step:
  ```yaml
  - run: bash apps/artcraft-launcher-web/mirror.sh dist/web
    env:
      GH_TOKEN: ${{ github.token }}
  ```
- [ ] Check on Pages: Open an app; the network tab shows `GET .../mirror/storytold/<app>/<tag>/<app>-web-<ver>.zip` answering 200.
- The mirror only has the version from the last deploy. A newer GitHub release makes the mirror 404 and falls through to the routes
  that fail in a browser; redeploy to refresh. Pre-releases aren't mirrored. Pages keeps the site under about 1 GB.
