// Assembles a loadable extension for each browser family.
//
// There is exactly one copy of the logic, in shared/. A browser directory
// holds only its manifest, because that is genuinely all that differs: the
// Chromium and Gecko extension APIs this uses are the same, and both families
// read the same background script. Keeping two edited copies in sync by hand
// is how the two quietly drift apart.
//
//   node build.mjs          # build both
//   node build.mjs --check  # validate without writing

import {
  cpSync,
  mkdirSync,
  readFileSync,
  readdirSync,
  rmSync,
  existsSync,
  writeFileSync,
} from "node:fs";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";

import { readExisting } from "./keygen.mjs";

const HERE = dirname(fileURLToPath(import.meta.url));
const TARGETS = ["chromium", "firefox"];
const CHECK_ONLY = process.argv.includes("--check");

/** Fields a manifest must have for the extension to work at all. */
function validate(target, manifest) {
  const problems = [];
  if (manifest.manifest_version !== 3) {
    problems.push(`manifest_version should be 3, got ${manifest.manifest_version}`);
  }
  for (const needed of ["downloads", "nativeMessaging", "storage", "scripting"]) {
    if (!manifest.permissions?.includes(needed)) {
      problems.push(`missing required permission: ${needed}`);
    }
  }

  // Cookie access is the most invasive thing this extension can do, so it
  // must be optional and requested at the moment of use. A build that
  // promoted it to a required permission would change the install prompt
  // without anyone noticing, so it fails here instead.
  if (manifest.permissions?.includes("cookies")) {
    problems.push("cookies must be optional, not requested at install time");
  }
  if (manifest.host_permissions?.length) {
    problems.push("host permissions must be optional, not requested at install time");
  }

  // The on-page overlay is registered at runtime, per site, after the user
  // asks for it (see shared/overlay.js). Declaring it here would run it on
  // every matching page from the moment the extension is installed -- which
  // is a different feature with the same name, and one nobody agreed to. So
  // a manifest that declares any content script fails the build.
  if (manifest.content_scripts?.length) {
    problems.push(
      "content scripts must be registered at runtime, not declared: see shared/overlay.js",
    );
  }

  // The two families declare their background script differently; getting
  // this wrong means the extension loads and silently does nothing.
  if (target === "chromium" && !manifest.background?.service_worker) {
    problems.push("Chromium needs background.service_worker");
  }
  if (target === "firefox") {
    if (!manifest.background?.scripts?.length) {
      problems.push("Gecko needs background.scripts");
    }
    if (!manifest.browser_specific_settings?.gecko?.id) {
      problems.push("Gecko needs browser_specific_settings.gecko.id, which the host manifest pins");
    }
  }
  return problems;
}

// The files shared/ must hold for the extension to do what its manifest
// says. A missing one is a silent half-feature: the background registers a
// content script that is not there, the browser refuses it, and nothing in
// the popup says why.
const NEEDED_FILES = ["background.js", "popup.html", "popup.js", "overlay.js"];
for (const file of NEEDED_FILES) {
  if (!existsSync(join(HERE, "shared", file))) {
    console.error(`shared/${file} is missing`);
    process.exit(1);
  }
}

const chromiumKey = CHECK_ONLY ? null : readExisting();
if (!CHECK_ONLY && !chromiumKey) {
  // Built without the pinned key rather than minting one. The result is a
  // perfectly good extension for loading unpacked, but Chromium will derive
  // its id from the folder path, so it is not the one the installer
  // registers -- say so, because a release built this way would pair with
  // nothing.
  console.log("no signing key in extension/keys; building without a pinned id.");
  console.log("  run `node extension/keygen.mjs` first if this build is for release.");
}
let builtId = null;

let failed = false;
for (const target of TARGETS) {
  const manifestPath = join(HERE, target, "manifest.json");
  if (!existsSync(manifestPath)) {
    console.error(`${target}: no manifest.json`);
    failed = true;
    continue;
  }

  let manifest;
  try {
    manifest = JSON.parse(readFileSync(manifestPath, "utf8"));
  } catch (error) {
    console.error(`${target}: manifest is not valid JSON — ${error.message}`);
    failed = true;
    continue;
  }

  const problems = validate(target, manifest);
  if (problems.length) {
    failed = true;
    console.error(`${target}: ${problems.length} problem(s)`);
    for (const problem of problems) console.error(`  - ${problem}`);
    continue;
  }

  if (CHECK_ONLY) {
    console.log(`${target}: manifest ok`);
    continue;
  }

  const out = join(HERE, "dist", target);
  // Updated in place, never deleted and recreated.
  //
  // Chrome loads this directory as an unpacked extension and keeps using
  // that path. Removing it mid-session pulls the folder out from under a
  // running browser: the extension stays listed and enabled, its service
  // worker registration survives, and it quietly stops working until
  // someone presses Reload -- with nothing in the app, the engine or the
  // extension saying why, because the extension is no longer running to
  // say anything. Rebuilding twice during an installer build is enough to
  // do it.
  //
  // So the directory keeps its identity and only its contents change.
  // Files the build no longer produces are removed individually, which is
  // what the wholesale delete was for.
  mkdirSync(out, { recursive: true });
  const written = new Set(["manifest.json"]);
  for (const file of readdirSync(join(HERE, "shared"))) {
    cpSync(join(HERE, "shared", file), join(out, file));
    written.add(file);
  }
  for (const stale of readdirSync(out)) {
    if (!written.has(stale)) rmSync(join(out, stale), { recursive: true, force: true });
  }
  if (target === "chromium" && chromiumKey) {
    // Pinning the public key fixes the extension ID. Without it Chromium
    // derives the ID from the folder path, so the native messaging manifest
    // -- which names the ID in allowed_origins -- would stop matching the
    // moment the folder moved, and would differ on every machine. Omitted
    // rather than invented when there is no key: a path-derived id is the
    // honest outcome of building without one, and minting a key here would
    // change the id this project ships under.
    manifest.key = chromiumKey.key;
    builtId = chromiumKey.id;
  }
  writeFileSync(join(out, "manifest.json"), JSON.stringify(manifest, null, 2) + "\n");
  console.log(`${target}: built -> ${out}`);
}

if (failed) process.exit(1);

if (!CHECK_ONLY) {
  console.log("\nLoad unpacked:");
  console.log("  Chromium family: chrome://extensions -> Developer mode -> Load unpacked");
  console.log("                   -> extension/dist/chromium");
  console.log("  Firefox family:  about:debugging -> This Firefox -> Load Temporary Add-on");
  console.log("                   -> extension/dist/firefox/manifest.json");
  console.log("\nThen register the native messaging host:");
  if (builtId) {
    console.log(`  idm --install-host --extension-id ${builtId}`);
    console.log("\nThat id is pinned by the key in extension/keys/, so it is the");
    console.log("same wherever the folder sits and on every machine.");
  } else {
    // No pinned id to print, and guessing one would be worse than
    // saying so: Chromium derives it from wherever this folder sits.
    console.log("  idm --install-host --extension-id <the id Chromium shows>");
    console.log("\nThis build has no pinned key, so that id comes from the folder");
    console.log("path and changes if it moves. Run `node extension/keygen.mjs`");
    console.log("for a stable one.");
  }
}
