// Putting the native messaging host back exactly as it was found.
//
// Any guard that drives a real browser has to register the host first, and
// registering it writes to the user's own registry and to the manifest
// folder under LOCALAPPDATA. That is somebody's machine, so a guard that
// touches it owes them the state it started with -- including when it
// fails, and including the roots it never meant to touch, since
// `--install-host` writes to every browser it can find rather than only
// the one under test.
//
// The manifests have to be snapshotted alongside the registry, because
// `--uninstall-host` deletes them. Restoring only the keys leaves every
// browser pointing at a file that is no longer there, which is a worse
// state than the one the guard started from and looks identical in the
// registry. Found the hard way, in extra-root-chain, and shared from here
// so the next guard does not have to find it again.

import { spawnSync } from "node:child_process";
import { existsSync, mkdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

/** Every root the installer knows, so "put it back" means the whole surface. */
export const KNOWN_ROOTS = [
  String.raw`Google\Chrome`, String.raw`Google\Chrome Beta`, String.raw`Google\Chrome Dev`,
  String.raw`Google\Chrome SxS`, "Chromium", String.raw`Microsoft\Edge`,
  String.raw`Microsoft\Edge Beta`, String.raw`Microsoft\Edge Dev`,
  String.raw`BraveSoftware\Brave-Browser`, String.raw`BraveSoftware\Brave-Browser-Beta`,
  "Vivaldi", String.raw`Opera Software\Opera Stable`, String.raw`Opera Software\Opera GX Stable`,
  String.raw`Yandex\YandexBrowser`, String.raw`TheBrowserCompany\Arc`, String.raw`CocCoc\Browser`,
  "Thorium", "Mozilla", "LibreWolf", "Waterfox", "Zen",
];

const HOST_NAME = "com.divine.pulse";

const REPO = resolve(dirname(fileURLToPath(import.meta.url)), "..", "..");

/**
 * The Chromium extension's id, derived from its published key.
 *
 * Needed even by a guard that only cares about Firefox: `--install-host`
 * registers with every browser it can find rather than one, and refuses to
 * run at all without an id for the Chromium side.
 */
export function chromiumExtensionId() {
  const pub = join(REPO, "extension", "keys", "chromium.pub.txt");
  return readFileSync(pub, "utf8").split(/\r?\n/)[0].trim();
}

const MANIFEST_DIR = join(process.env.LOCALAPPDATA ?? "", "DivineIDM", "hosts");
const MANIFEST_NAMES = [`${HOST_NAME}.chromium.json`, `${HOST_NAME}.gecko.json`];

export function keyPath(root) {
  return `HKCU\\Software\\${root}\\NativeMessagingHosts\\${HOST_NAME}`;
}

/** The manifest a root is registered to, or null when it is not registered. */
export function registrationOf(root) {
  const out = spawnSync("reg", ["query", keyPath(root), "/ve"], { encoding: "utf8" });
  if (out.status !== 0) return null;
  const line = out.stdout.split(/\r?\n/).find((l) => l.includes("REG_SZ"));
  return line ? line.split("REG_SZ")[1].trim() : null;
}

export function snapshot(roots = KNOWN_ROOTS) {
  const state = new Map();
  for (const root of roots) state.set(root, registrationOf(root));
  return state;
}

export function snapshotManifests() {
  const state = new Map();
  for (const name of MANIFEST_NAMES) {
    const path = join(MANIFEST_DIR, name);
    state.set(path, existsSync(path) ? readFileSync(path, "utf8") : null);
  }
  return state;
}

export function restoreManifests(before) {
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

export function removeRegistration(root) {
  spawnSync("reg", ["delete", keyPath(root), "/f"], { encoding: "utf8" });
}

export function setRegistration(root, value) {
  spawnSync("reg", ["add", keyPath(root), "/ve", "/t", "REG_SZ", "/d", value, "/f"], {
    encoding: "utf8",
  });
}

/** Puts every root back to exactly what `before` recorded. */
export function restore(before) {
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
