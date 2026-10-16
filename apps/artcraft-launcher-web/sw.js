// ArtCraft Launcher: the "web server" behind Open.
//
// The launcher page unpacks an app's web build into Cache Storage under
//   <this folder>/__app/<app id>/<version>/<file path>
// and this worker answers requests for those URLs from there, so each app runs from its own
// path with nothing but a static host behind it. It never touches any other request.
//
// A Service Worker has to be JavaScript; everything else in the launcher is Rust.

const CACHE = "craft-apps";
const PREFIX = "__app/";

self.addEventListener("install", () => self.skipWaiting());
self.addEventListener("activate", (event) => event.waitUntil(self.clients.claim()));

self.addEventListener("fetch", (event) => {
  const request = event.request;
  if (request.method !== "GET" && request.method !== "HEAD") return;
  const url = new URL(request.url);
  if (url.origin !== self.location.origin) return;
  const base = new URL("./", self.location.href);
  if (!url.pathname.startsWith(base.pathname + PREFIX)) return;
  event.respondWith(serve(request, url, base));
});

const MIME = {
  html: "text/html; charset=utf-8", htm: "text/html; charset=utf-8", js: "text/javascript", mjs: "text/javascript",
  css: "text/css", json: "application/json", wasm: "application/wasm", svg: "image/svg+xml", png: "image/png",
  jpg: "image/jpeg", jpeg: "image/jpeg", gif: "image/gif", webp: "image/webp", avif: "image/avif", ico: "image/x-icon",
  txt: "text/plain; charset=utf-8", map: "application/json", woff: "font/woff", woff2: "font/woff2", ttf: "font/ttf",
  otf: "font/otf", webmanifest: "application/manifest+json", mp3: "audio/mpeg", mp4: "video/mp4", webm: "video/webm",
  ogg: "audio/ogg", wav: "audio/wav", pdf: "application/pdf", xml: "application/xml",
};

async function serve(request, url, base) {
  const cache = await caches.open(CACHE);
  let path = url.pathname;
  if (path.endsWith("/")) path += "index.html";
  let hit = await cache.match(new URL(path, base.origin), { ignoreSearch: true });
  if (!hit && request.mode === "navigate" && !/\.[a-z0-9]+$/i.test(path)) {
    // A single-page app's own route: hand out the app's index.html.
    const rest = path.slice(base.pathname.length + PREFIX.length).split("/");
    if (rest.length >= 2) {
      hit = await cache.match(new URL(`${base.pathname}${PREFIX}${rest[0]}/${rest[1]}/index.html`, base.origin));
    }
  }
  if (!hit) return new Response("Not found", { status: 404, headers: { "content-type": "text/plain" } });

  const headers = new Headers(hit.headers);
  if (!headers.has("content-type")) {
    const ext = (path.split(".").pop() || "").toLowerCase();
    headers.set("content-type", MIME[ext] || "application/octet-stream");
  }
  headers.set("cache-control", "no-cache");
  headers.set("x-content-type-options", "nosniff");

  const range = /^bytes=(\d*)-(\d*)$/.exec(request.headers.get("range") || "");
  if (range && request.method === "GET") {
    const body = await hit.arrayBuffer();
    const size = body.byteLength;
    let start = range[1] === "" ? Math.max(0, size - Number(range[2] || 0)) : Number(range[1]);
    let end = range[1] === "" || range[2] === "" ? size - 1 : Math.min(Number(range[2]), size - 1);
    if (start > end || start >= size) {
      headers.set("content-range", `bytes */${size}`);
      return new Response(null, { status: 416, headers });
    }
    headers.set("content-range", `bytes ${start}-${end}/${size}`);
    headers.set("content-length", String(end - start + 1));
    return new Response(body.slice(start, end + 1), { status: 206, headers });
  }
  headers.set("accept-ranges", "bytes");
  return new Response(request.method === "HEAD" ? null : hit.body, { status: 200, headers });
}
