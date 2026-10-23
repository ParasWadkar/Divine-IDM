// Fails if the window listens for engine events without being allowed to.
//
// This exists because of a bug that went unnoticed for the whole life of the
// GUI. Tauri 2 denies every plugin command a window has not been granted, and
// the app shipped with no capabilities at all. Its own commands (add_download,
// history, reveal) are exempt, so pasting a URL, starting it and reading
// History all worked -- which is everything a quick manual check touches. But
// `listen("snapshot")` is a plugin command, and it failed with
//
//   Command plugin:event|listen not allowed by ACL
//
// as an unhandled promise rejection nothing displayed. The window therefore
// never received a single update: no progress bar, no speed graph, and a
// finished download left showing "Running 0 B/s" indefinitely.
//
// The engine was fine throughout; its history said "finished". Only watching
// the live window during a transfer, with a way to read the page's errors,
// shows it, so this guards the cause instead: any use of the event API in the
// page needs a grant for it.
//
//   node capability-guard.mjs

import { existsSync, readFileSync, readdirSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const REPO = resolve(dirname(fileURLToPath(import.meta.url)), "..", "..");
const UI = join(REPO, "app", "ui", "app.js");
const CAPS = join(REPO, "app", "src-tauri", "capabilities");

let failures = 0;
function check(label, ok, detail = "") {
  console.log(`${ok ? "  ok  " : "  FAIL"}  ${label}${detail ? ` — ${detail}` : ""}`);
  if (!ok) failures += 1;
}

console.log("capability-guard");

const source = readFileSync(UI, "utf8");
const usesEvents = /\blisten\s*\(|__TAURI__\.event|\bemit\s*\(/.test(source);
check("the page's use of the event API is detected", usesEvents, "app/ui/app.js calls listen()");

const grants = [];
if (existsSync(CAPS)) {
  for (const file of readdirSync(CAPS).filter((f) => f.endsWith(".json"))) {
    const capability = JSON.parse(readFileSync(join(CAPS, file), "utf8"));
    grants.push({ file, windows: capability.windows ?? [], permissions: capability.permissions ?? [] });
  }
}

check("a capability file exists", grants.length > 0, "app/src-tauri/capabilities/*.json");

// Anything that includes the listen permission, directly or via a default set.
const allowsListen = (permissions) =>
  permissions.some((p) =>
    typeof p === "string" &&
    ["core:event:allow-listen", "core:event:default", "core:default", "event:allow-listen", "event:default"].includes(p),
  );

if (usesEvents) {
  const granting = grants.filter((g) => allowsListen(g.permissions));
  check("the page may listen for events", granting.length > 0);
  check(
    "and the grant covers the window the page runs in",
    granting.some((g) => g.windows.includes("main") || g.windows.includes("*")),
    "window label `main`",
  );
}

// Least privilege: the window should not hold more than it uses. A broad
// default set would pass the checks above, so flag it rather than let the
// narrow grant quietly widen later.
const broad = grants.flatMap((g) => g.permissions.filter((p) => p === "core:default"));
check("the window is not given the full core default set", broad.length === 0, broad.join(", "));

console.log(failures === 0 ? "\ncapability-guard: PASS" : `\ncapability-guard: ${failures} FAILED`);
process.exit(failures === 0 ? 0 : 1);
