// Fails if the window's script would not load.
//
// This exists because of a bug that cost an hour and showed up as the app
// half-working. A second `function clockOf` was added at the top level of
// app.js — one formatting a timestamp, one formatting a time of day. In a
// classic script that is legal and the later one simply wins. In a module
// it is a *syntax* error, the whole file fails to evaluate, and nothing is
// wired up at all: tabs highlight, because the browser does that, and
// nothing else works.
//
// The trap is that `node --check app.js` passes. Node parses a `.js` file
// as CommonJS unless it has import/export, so it checks it under exactly
// the rules the browser does not use. `index.html` loads it with
// `type="module"`, so this re-checks it the way it is actually loaded.
//
//   node window-script.mjs

import { copyFileSync, mkdtempSync, readFileSync } from "node:fs";
import { execFileSync } from "node:child_process";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const REPO = resolve(dirname(fileURLToPath(import.meta.url)), "..", "..");
const UI = join(REPO, "app", "ui");
const PAGE = join(UI, "index.html");

let failures = 0;
function check(label, ok, detail = "") {
  console.log(`${ok ? "  ok  " : "  FAIL"}  ${label}${detail ? ` — ${detail}` : ""}`);
  if (!ok) failures += 1;
}

console.log("window-script");

const html = readFileSync(PAGE, "utf8");

// Every script the page loads, and how. A file loaded as a module has to
// be checked as one; a file loaded as a classic script must not be, or
// this would report errors the browser never sees.
const scripts = [...html.matchAll(/<script\b([^>]*)>/g)].map((match) => match[1]);
const modules = scripts
  .filter((attributes) => /\btype\s*=\s*["']module["']/.test(attributes))
  .map((attributes) => /\bsrc\s*=\s*["']([^"']+)["']/.exec(attributes)?.[1])
  .filter(Boolean);

check("the page loads at least one module", modules.length > 0, modules.join(", "));

const scratch = mkdtempSync(join(tmpdir(), "idm-window-script-"));
for (const source of modules) {
  const from = join(UI, source);
  // Copied to `.mjs` rather than checked in place: the extension is what
  // decides which rules Node parses under, and `.js` means CommonJS here.
  const to = join(scratch, `${source.replace(/[\\/]/g, "-")}.mjs`);
  copyFileSync(from, to);

  let error = "";
  try {
    execFileSync(process.execPath, ["--check", to], { stdio: ["ignore", "ignore", "pipe"] });
  } catch (failure) {
    // The first line of Node's own message, which names the identifier and
    // is more useful than anything this could say instead.
    error = String(failure.stderr ?? failure)
      .split("\n")
      .find((line) => line.includes("Error") || line.includes("error"))
      ?.trim() ?? "it would not parse";
  }
  check(`${source} loads as a module`, error === "", error);
}

console.log(`\nwindow-script: ${failures === 0 ? "PASS" : `FAIL (${failures})`}`);
process.exit(failures === 0 ? 0 : 1);
