// Checks the promises the extension makes about what it may do.
//
// The extension asks for very little at install time and asks for the rest
// at the moment it is needed. That is a property of two manifests and one
// content script, none of which anything else would notice changing: an
// added `content_scripts` entry, or a `fetch` in the overlay, would work
// perfectly and quietly turn a narrow extension into a broad one.
//
// So the invariants are written down here and fail a build rather than a
// review.
//
//   node extension-guard.mjs

import { execFileSync } from "node:child_process";
import { readFileSync, existsSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const REPO = resolve(dirname(fileURLToPath(import.meta.url)), "..", "..");
const EXT = join(REPO, "extension");

let failures = 0;
function check(ok, label, detail = "") {
  console.log(`  ${ok ? "PASS" : "FAIL"}  ${label}${detail ? `  — ${detail}` : ""}`);
  if (!ok) failures += 1;
}

console.log("Extension\n");

// ---------------------------------------------------------------------------
// The manifests, through the build's own validator
// ---------------------------------------------------------------------------

// Run rather than reimplemented: a second copy of the rules would be a
// second thing to keep in step, and the build's copy is the one that ships.
try {
  execFileSync(process.execPath, [join(EXT, "build.mjs"), "--check"], { stdio: "pipe" });
  check(true, "both manifests pass the build's own validation");
} catch (error) {
  check(false, "both manifests pass the build's own validation", String(error.stdout ?? error));
}

for (const target of ["chromium", "firefox"]) {
  const manifest = JSON.parse(readFileSync(join(EXT, target, "manifest.json"), "utf8"));

  // The one that matters for the overlay. A declared content script runs on
  // every matching page from the moment the extension is installed; the
  // overlay is registered at runtime, for one site, after the user asks.
  check(
    !manifest.content_scripts?.length,
    `${target}: no content script is declared`,
    "it would run on every matching page from install",
  );

  check(
    manifest.permissions?.includes("scripting"),
    `${target}: can register a content script at runtime`,
  );

  // activeTab is what lets the popup see which site the tab is on without
  // holding access to every site. Losing it would send the popup down the
  // path of asking for <all_urls> instead.
  check(manifest.permissions?.includes("activeTab"), `${target}: has activeTab`);

  check(
    !manifest.host_permissions?.length,
    `${target}: no host access at install time`,
  );
}

// ---------------------------------------------------------------------------
// The overlay itself
// ---------------------------------------------------------------------------

const overlayPath = join(EXT, "shared", "overlay.js");
check(existsSync(overlayPath), "shared/overlay.js is there");
const overlay = existsSync(overlayPath) ? readFileSync(overlayPath, "utf8") : "";

// A script running inside somebody else's page can read everything on it.
// This one is supposed to read video addresses and nothing else, so the
// things it would need to do more are named here.
const FORBIDDEN = [
  ["document.cookie", "a content script must not read cookies"],
  ["localStorage", "a content script must not read the page's storage"],
  ["sessionStorage", "a content script must not read the page's storage"],
  ["XMLHttpRequest", "the overlay must not fetch anything itself"],
  ["fetch(", "the overlay must not fetch anything itself"],
  ["innerHTML", "text goes in with textContent; the page chose these strings"],
];
for (const [needle, why] of FORBIDDEN) {
  check(!overlay.includes(needle), `the overlay has no ${needle}`, why);
}

// The one message it is allowed to send, and the one way it sends it.
const sends = [...overlay.matchAll(/type:\s*"([a-zA-Z]+)"/g)].map((match) => match[1]);
check(
  sends.length > 0 && sends.every((type) => type === "grabVideo"),
  "the overlay sends only grabVideo",
  sends.join(", ") || "it sends nothing at all",
);

// Isolation from the page: a closed shadow root is what keeps the page from
// restyling the button, and the button's own styles out of the page.
check(overlay.includes('attachShadow({ mode: "closed" })'), "the overlay is in a closed shadow root");

// ---------------------------------------------------------------------------
// The background's half of it
// ---------------------------------------------------------------------------

const background = readFileSync(join(EXT, "shared", "background.js"), "utf8");
for (const type of ["overlayHere", "overlayOff", "overlayState", "grabVideo"]) {
  check(background.includes(`case "${type}"`), `the background answers ${type}`);
}

// Per-site, not everywhere. `origins: ["<all_urls>"]` in the overlay's own
// request would make the one-site promise untrue while every test still
// passed.
const requestAt = background.indexOf('case "overlayHere"');
const requestEnd = background.indexOf('case "overlayOff"', requestAt);
const overlayCase = background.slice(requestAt, requestEnd);
check(
  requestAt > 0 && !overlayCase.includes("<all_urls>"),
  "switching the overlay on asks for one site, not every site",
);

console.log(`\nextension-guard: ${failures === 0 ? "PASS" : `FAIL (${failures})`}`);
process.exit(failures === 0 ? 0 : 1);
