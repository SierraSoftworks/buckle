// A tiny static server for the built UI with the same SPA fallback the
// Buckle server has (crates/buckle-server/src/web/ui.rs): a file that exists
// is served, `/api/*` is a 404 (there is no server behind demo mode), and
// every other path gets index.html.
//
//   node e2e/serve.mjs [dist] [port]

import { createServer } from "node:http";
import { readFile, stat } from "node:fs/promises";
import { extname, join, normalize, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const TYPES = {
  ".html": "text/html; charset=utf-8",
  ".js": "text/javascript; charset=utf-8",
  ".css": "text/css; charset=utf-8",
  ".wasm": "application/wasm",
  ".json": "application/json",
  ".svg": "image/svg+xml",
  ".png": "image/png",
  ".ico": "image/x-icon",
};

export function serve(dist, port = 0) {
  const root = resolve(dist);
  const server = createServer(async (req, res) => {
    const path = decodeURIComponent(new URL(req.url, "http://x").pathname);
    if (path.startsWith("/api/")) {
      res.writeHead(404, { "content-type": "application/json" });
      res.end(JSON.stringify({ error: `No API route ${path} (static server).` }));
      return;
    }
    const file = normalize(join(root, path));
    let target = join(root, "index.html");
    if (file.startsWith(root) && path !== "/") {
      try {
        if ((await stat(file)).isFile()) target = file;
      } catch {}
    }
    try {
      const body = await readFile(target);
      res.writeHead(200, {
        "content-type": TYPES[extname(target)] ?? "application/octet-stream",
        "cache-control": target.endsWith("index.html") ? "no-cache" : "public, max-age=3600",
      });
      res.end(body);
    } catch (e) {
      res.writeHead(500);
      res.end(String(e));
    }
  });
  return new Promise((ok) => server.listen(port, "127.0.0.1", () => ok(server)));
}

if (process.argv[1] === fileURLToPath(import.meta.url)) {
  const dist = process.argv[2] ?? new URL("../dist", import.meta.url).pathname;
  const port = Number(process.argv[3] ?? 8082);
  const server = await serve(dist, port);
  console.log(`serving ${dist} on http://127.0.0.1:${server.address().port}`);
}
