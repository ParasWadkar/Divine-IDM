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

import { cpSync, mkdirSync, readFileSync, readdirSync, rmSync, existsSync } from "node:fs";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";

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
  rmSync(out, { recursive: true, force: true });
  mkdirSync(out, { recursive: true });
  for (const file of readdirSync(join(HERE, "shared"))) {
    cpSync(join(HERE, "shared", file), join(out, file));
  }
  cpSync(manifestPath, join(out, "manifest.json"));
  console.log(`${target}: built -> ${out}`);
}

if (failed) process.exit(1);

if (!CHECK_ONLY) {
  console.log("\nLoad unpacked:");
  console.log("  Chromium family: chrome://extensions -> Developer mode -> Load unpacked");
  console.log("                   -> extension/dist/chromium");
  console.log("  Firefox family:  about:debugging -> This Firefox -> Load Temporary Add-on");
  console.log("                   -> extension/dist/firefox/manifest.json");
  console.log("\nThen register the host with the extension ID the browser shows you:");
  console.log("  idm --install-host --extension-id <id>");
}
