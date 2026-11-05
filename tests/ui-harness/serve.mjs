// Serves app/ui in an ordinary browser, with a stub in place of Tauri.
//
//   node tests/ui-harness/serve.mjs          # then open http://127.0.0.1:8111
//
// The window is normally only reachable by building and running the app,
// which means every change to a dialog costs a link and a launch, and
// checking a layout at several widths means resizing a real window by hand.
// This serves the same HTML, CSS and script to any browser, with
// `window.__TAURI__` replaced by a stub that answers every command with a
// plausible shape.
//
// WHAT IT PROVES
//
// That the page loads as a module, that handlers are wired to the elements
// they name, that rendering produces the rows and labels it should, and that
// a dialog is the size it was meant to be. The fault that prompted this --
// `.modal-wide` setting a width while `.modal` capped max-width at 360px, so
// two dialogs rendered at the wrong size with their buttons wrapped -- is
// invisible in the source and obvious here.
//
// WHAT IT DOES NOT PROVE
//
// Anything about the engine, because there isn't one: every reply is canned.
// Nor anything about WebView2, which is not this browser. It is a way to see
// the page, not a substitute for running the app -- and the argument names
// the page sends are checked separately, by tests/e2e/invoke-wiring.mjs,
// because a stub that accepts anything would never catch a wrong one.
//
// A test can drive it by setting a reply before acting:
//
//   window.__REPLIES__.grab_links = { links: [...], pages: 1, note: null };
//
// and read back what the page sent in `window.__TAURI_CALLS__`.

import { createServer } from "node:http";
import { existsSync, readFileSync } from "node:fs";
import { dirname, extname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const HERE = dirname(fileURLToPath(import.meta.url));
const UI = resolve(HERE, "..", "..", "app", "ui");
const PORT = Number(process.argv[2] ?? 8111);

const TYPES = {
  ".html": "text/html; charset=utf-8",
  ".js": "text/javascript; charset=utf-8",
  ".css": "text/css; charset=utf-8",
  ".svg": "image/svg+xml",
  ".png": "image/png",
  ".ico": "image/x-icon",
  ".woff2": "font/woff2",
};

// The stub, injected ahead of the module script so it exists before app.js
// destructures `window.__TAURI__.core`.
//
// Shapes match what the Tauri commands really return, because a reply of the
// wrong shape produces an error in the page that looks like a bug in the page.
const STUB = `
window.__TAURI_CALLS__ = [];
window.__REPLIES__ = {
  snapshot: {
    jobs: [],
    settings: {
      browser_integration: true,
      post_action: "nothing",
      post_action_countdown_secs: 60,
      min_takeover_bytes: 2097152,
      resume_on_start: true,
      start_with_windows: false,
      max_concurrent_downloads: 4,
      sort_into_folders: true,
      speed_limit_bytes_per_sec: 0,
      download_dir: "D:/Downloads",
      hidden_browsers: [],
      proxy: "system",
      proxy_address: "",
    },
    paired: [],
    pending: [],
  },
  opened_files: [],
  history: [],
  queues: [],
  browsers: [],
  sidecars: [],
  credentials: [],
  bandwidth_rules: { rules: [], now: { weekday: 0, minute_of_day: 600, action: null, until: null } },
  browser_setup: null,
};

window.__TAURI__ = {
  core: {
    invoke: async (name, args) => {
      window.__TAURI_CALLS__.push({ name, args });
      // Unanswered commands come back as null rather than throwing, so that
      // one the harness has not been taught about does not stop the page.
      return name in window.__REPLIES__ ? window.__REPLIES__[name] : null;
    },
  },
  event: { listen: async () => () => {} },
};
`;

createServer((request, response) => {
  const path = (request.url ?? "/").split("?")[0];
  const name = path === "/" ? "/index.html" : path;
  // Only inside app/ui: a harness is not a file server.
  const file = resolve(join(UI, name));
  if (!file.startsWith(UI) || !existsSync(file)) {
    response.writeHead(404, { "Content-Length": "0" }).end();
    return;
  }

  if (name === "/index.html") {
    const html = readFileSync(file, "utf8").replace(
      '<script type="module" src="app.js"></script>',
      `<script>${STUB}</script>\n    <script type="module" src="app.js"></script>`,
    );
    response.writeHead(200, { "Content-Type": TYPES[".html"] });
    response.end(html);
    return;
  }

  response.writeHead(200, {
    "Content-Type": TYPES[extname(file)] ?? "application/octet-stream",
  });
  response.end(readFileSync(file));
}).listen(PORT, "127.0.0.1", () => {
  console.log(`window harness on http://127.0.0.1:${PORT}`);
});
