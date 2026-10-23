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

import { loadOrCreate } from "./keygen.mjs";

const HERE = dirname(fileURLToPath(import.meta.url));
const TARGETS = ["chromium", "firefox"];
const CHECK_ONLY = process.argv.includes("--check");

/** Fields a manifest must have for the extension to work at all. */
function validate(target, manifest) {
  const problems = [];
  if (manifest.manifest_version !== 3) {
    problems.push(`manifest_version should be 3, got ${manifest.manifest_version}`);
  }
  for (const needed of ["downloads", "nativeMessaging", "storage"]) {
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

const chromiumKey = CHECK_ONLY ? null : loadOrCreate();
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
  if (target === "chromium") {
    // Pinning the public key fixes the extension ID. Without it Chromium
    // derives the ID from the folder path, so the native messaging manifest
    // -- which names the ID in allowed_origins -- would stop matching the
    // moment the folder moved, and would differ on every machine.
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
  console.log(`  idm --install-host --extension-id ${builtId}`);
  console.log("\nThat id is pinned by the key in extension/keys/, so it is the");
  console.log("same wherever the folder sits and on every machine.");
}
