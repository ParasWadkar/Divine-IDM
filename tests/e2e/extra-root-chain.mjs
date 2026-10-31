// Checks that --extra-root reaches the registry.
//
// This exists because of a bug it would have caught. install() has taken an
// `extra_roots` parameter since it was written, and the module doc has
// promised a `--extra-root` flag for just as long — but no such flag existed
// and run_install passed an empty slice unconditionally. The documented
// escape hatch for a browser the table does not list was unreachable, and
// nothing said so: the code was there, it compiled, and the doc described
// it accurately enough to be believed.
//
// That is the fourth time on this project a feature has been written,
// plumbed through and never called. The unit tests cover the decision, but
// only something that runs the real binary can prove the flag is connected
// to it, because the gap was a single argument at one call site.
//
// Non-destructive: it snapshots every com.divine.pulse registration first and
// puts the machine back exactly as it found it, including removing any
// registration the run created.
//
//   node extra-root-chain.mjs

import { spawnSync } from "node:child_process";
import { existsSync, mkdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const HERE = dirname(fileURLToPath(import.meta.url));
const REPO = resolve(HERE, "..", "..");
const IDM = join(REPO, "target", "release", "idm.exe");
const HOST = join(REPO, "target", "release", "idm-host.exe");

// Roots no browser will ever own, so the test can assert on them without
// caring what is installed on the machine running it.
const CHROMIUM_ROOT = String.raw`DivinePulseTest\ExtraRootChromium`;
const GECKO_ROOT = String.raw`DivinePulseTest\ExtraRootGecko`;

// Every root the installer knows, so "put it back" means the whole surface
// rather than the ones this test happened to touch.
const KNOWN_ROOTS = [
  String.raw`Google\Chrome`, String.raw`Google\Chrome Beta`, String.raw`Google\Chrome Dev`,
  String.raw`Google\Chrome SxS`, "Chromium", String.raw`Microsoft\Edge`,
  String.raw`Microsoft\Edge Beta`, String.raw`Microsoft\Edge Dev`,
  String.raw`BraveSoftware\Brave-Browser`, String.raw`BraveSoftware\Brave-Browser-Beta`,
  "Vivaldi", String.raw`Opera Software\Opera Stable`, String.raw`Opera Software\Opera GX Stable`,
  String.raw`Yandex\YandexBrowser`, String.raw`TheBrowserCompany\Arc`, String.raw`CocCoc\Browser`,
  "Thorium", "Mozilla", "LibreWolf", "Waterfox", "Zen",
  CHROMIUM_ROOT, GECKO_ROOT,
];

let failures = 0;

function check(label, ok, detail = "") {
  console.log(`${ok ? "  ok  " : "  FAIL"}  ${label}${detail ? ` — ${detail}` : ""}`);
  if (!ok) failures += 1;
}

function keyPath(root) {
  return "HKCU\\Software\\" + root + "\\NativeMessagingHosts\\com.divine.pulse";
}

/** The manifest a root is registered to, or null when it is not registered. */
function registrationOf(root) {
  const out = spawnSync("reg", ["query", keyPath(root), "/ve"], { encoding: "utf8" });
  if (out.status !== 0) return null;
  const line = out.stdout.split(/\r?\n/).find((l) => l.includes("REG_SZ"));
  return line ? line.split("REG_SZ")[1].trim() : null;
}

function snapshot() {
  const state = new Map();
  for (const root of KNOWN_ROOTS) state.set(root, registrationOf(root));
  return state;
}

// The manifests have to be snapshotted alongside the registry, because
// --uninstall-host deletes them. Restoring only the keys leaves every
// browser pointing at a file that is no longer there, which is a worse
// state than the one the test started from and looks identical in the
// registry. Found the hard way.
const MANIFEST_DIR = join(
  process.env.LOCALAPPDATA ?? "",
  "DivineIDM",
  "hosts",
);
const MANIFEST_NAMES = ["com.divine.pulse.chromium.json", "com.divine.pulse.gecko.json"];

function snapshotManifests() {
  const state = new Map();
  for (const name of MANIFEST_NAMES) {
    const path = join(MANIFEST_DIR, name);
    state.set(path, existsSync(path) ? readFileSync(path, "utf8") : null);
  }
  return state;
}

function restoreManifests(before) {
  let changed = 0;
  for (const [path, was] of before) {
    const now = existsSync(path) ? readFileSync(path, "utf8") : null;
    if (now === was) continue;
    changed += 1;
    if (was === null) rmSync(path, { force: true });
    else {
      mkdirSync(dirname(path), { recursive: true });
      writeFileSync(path, was);
    }
  }
  return changed;
}

function removeRegistration(root) {
  spawnSync("reg", ["delete", keyPath(root), "/f"], { encoding: "utf8" });
}

function setRegistration(root, value) {
  spawnSync("reg", ["add", keyPath(root), "/ve", "/t", "REG_SZ", "/d", value, "/f"], {
    encoding: "utf8",
  });
}

/** Puts every root back to exactly what `before` recorded. */
function restore(before) {
  let changed = 0;
  for (const [root, was] of before) {
    const now = registrationOf(root);
    if (now === was) continue;
    changed += 1;
    if (was === null) removeRegistration(root);
    else setRegistration(root, was);
  }
  return changed;
}

function runInstall(extraArgs) {
  return spawnSync(
    IDM,
    ["--install-host", "--extension-id", extensionId(), ...extraArgs],
    { encoding: "utf8" },
  );
}

function extensionId() {
  const pub = join(REPO, "extension", "keys", "chromium.pub.txt");
  return readFileSync(pub, "utf8").split(/\r?\n/)[0].trim();
}

// ---------------------------------------------------------------------------

for (const [label, path] of [["idm.exe", IDM], ["idm-host.exe", HOST]]) {
  if (!existsSync(path)) {
    console.error(`${label} is missing; run: cargo build --release -p idm-cli -p idm-host`);
    process.exit(1);
  }
}

console.log("extra-root-chain");
const before = snapshot();
const manifestsBefore = snapshotManifests();
let restored = 0;

try {
  // --- the flag must exist at all -----------------------------------------
  const help = spawnSync(IDM, ["--help"], { encoding: "utf8" });
  check("--extra-root appears in --help", help.stdout.includes("--extra-root"));

  // --- a malformed value is refused before anything is written ------------
  const bad = runInstall(["--extra-root", "Thorium"]);
  check("a value without a family is refused", bad.status !== 0);
  check(
    "and the error shows the shape",
    /family:root|chromium:/i.test(bad.stderr),
    bad.stderr.trim().split("\n")[0],
  );

  // --- the control: without the flag, the root must stay unregistered -----
  // This is what fails if run_install goes back to passing an empty slice,
  // and what makes the next assertion mean something.
  removeRegistration(CHROMIUM_ROOT);
  runInstall([]);
  check(
    "an unnamed root is not registered",
    registrationOf(CHROMIUM_ROOT) === null,
    "control",
  );

  // --- the flag itself ----------------------------------------------------
  const run = runInstall([
    "--extra-root", `chromium:${CHROMIUM_ROOT}`,
    "--extra-root", `gecko:${GECKO_ROOT}`,
  ]);
  check("the install succeeds", run.status === 0, run.stderr.trim().split("\n").pop());

  const chromium = registrationOf(CHROMIUM_ROOT);
  const gecko = registrationOf(GECKO_ROOT);

  // Registered even though no browser wrote anything under either root:
  // naming it is the instruction, and without this the flag would do
  // nothing unless --all-browsers were passed too.
  check("a named chromium root is registered though absent", chromium !== null);
  check("a named gecko root is registered though absent", gecko !== null);

  // The family has to survive the trip, because it picks the manifest
  // dialect. A Gecko browser handed allowed_origins is registered, looks
  // right, and never starts the host.
  check(
    "the chromium root got the chromium manifest",
    chromium !== null && chromium.endsWith("com.divine.pulse.chromium.json"),
    chromium ?? "unregistered",
  );
  check(
    "the gecko root got the gecko manifest",
    gecko !== null && gecko.endsWith("com.divine.pulse.gecko.json"),
    gecko ?? "unregistered",
  );

  // And the manifest it points at has to exist and name this host.
  if (chromium && existsSync(chromium)) {
    const parsed = JSON.parse(readFileSync(chromium, "utf8"));
    check("the manifest points at a host that exists", existsSync(parsed.path), parsed.path);
    check(
      "and permits the pinned extension",
      JSON.stringify(parsed.allowed_origins ?? []).includes(extensionId()),
    );
  } else {
    check("the chromium manifest exists on disk", false, chromium ?? "unregistered");
  }

  // --- uninstall takes the named roots away too ---------------------------
  spawnSync(IDM, ["--uninstall-host"], { encoding: "utf8" });
  check("uninstall removes a named chromium root", registrationOf(CHROMIUM_ROOT) === null);
  check("uninstall removes a named gecko root", registrationOf(GECKO_ROOT) === null);
} finally {
  restored = restore(before) + restoreManifests(manifestsBefore);
}

console.log(`\nrestored ${restored} item(s) to their original state`);

const drift = [...before].filter(([root, was]) => registrationOf(root) !== was);
check("every registration is as it was found", drift.length === 0, drift.map(([r]) => r).join(", "));

const manifestDrift = [...manifestsBefore].filter(
  ([path, was]) => (existsSync(path) ? readFileSync(path, "utf8") : null) !== was,
);
check("every manifest is as it was found", manifestDrift.length === 0);

// A registration pointing at a manifest that is not there is exactly what
// a key-only restore leaves behind, and it is invisible in the registry
// alone: the key looks perfect and the browser silently cannot start the
// host. This run created that state once before the check existed.
const dangling = [...before]
  .filter(([, was]) => was !== null)
  .filter(([root]) => {
    const now = registrationOf(root);
    return now !== null && !existsSync(now);
  });
check(
  "no registration points at a missing manifest",
  dangling.length === 0,
  dangling.map(([r]) => r).join(", "),
);

console.log(failures === 0 ? "\nextra-root-chain: PASS" : `\nextra-root-chain: ${failures} FAILED`);
process.exit(failures === 0 ? 0 : 1);
