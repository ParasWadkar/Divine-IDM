// Checks that a download whose URL carries no usable name is saved under the
// name the server gives it.
//
// This exists because of a bug found taking over a real Chrome download. A
// browser hands over the *redirected* address, and a CDN that redirects to a
// signed object ends in a GUID: `.../c6451f46-4a95-....`. The engine settled
// the destination from that, before talking to the server, so a 45 MB zip was
// saved as `c6451f46-4a95-4db3-970d-23373a471948` -- no extension, and so
// outside the Archives folder it belonged in -- while the server had said
// plainly, in Content-Disposition, that it was `Ghost-Downloader-....zip`.
//
// The unit tests cannot see this: it needs a server that answers with a
// header, and an engine that has to decide what to do about it.
//
//   node opaque-name.mjs

import { spawn, spawnSync } from "node:child_process";
import { existsSync, mkdtempSync, readdirSync, statSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const HERE = dirname(fileURLToPath(import.meta.url));
const REPO = resolve(HERE, "..", "..");
const IDM = join(REPO, "target", "release", "idm.exe");
const MOCK = join(REPO, "tests", "mock-server", "server.mjs");
const PORT = 8197;
const SIZE = 3 * 1024 * 1024;

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
let failures = 0;
function check(label, ok, detail = "") {
  console.log(`  ${ok ? "PASS" : "FAIL"}  ${label}${detail ? "  — " + detail : ""}`);
  if (!ok) failures += 1;
}

/** Every file under `dir`, as paths relative to it. */
function listing(dir) {
  const out = [];
  const walk = (d, prefix) => {
    for (const name of readdirSync(d)) {
      const full = join(d, name);
      if (statSync(full).isDirectory()) walk(full, `${prefix}${name}/`);
      else if (!/\.idm(part|jrn)$/.test(name)) out.push(`${prefix}${name}`);
    }
  };
  walk(dir, "");
  return out.sort();
}

async function waitFor(dir, wanted) {
  for (let i = 0; i < 60; i += 1) {
    await sleep(250);
    if (listing(dir).includes(wanted)) return true;
  }
  return false;
}

if (!existsSync(IDM)) {
  console.error("missing idm.exe; run: cargo build --release -p idm-cli");
  process.exit(1);
}

console.log("Divine IDM — a URL with no usable name\n");

const downloads = mkdtempSync(join(tmpdir(), "idm-name-dl-"));
const stateDir = mkdtempSync(join(tmpdir(), "idm-name-state-"));
const env = { ...process.env, IDM_STATE_DIR: stateDir };

const server = spawn("node", [MOCK, "--port", String(PORT)], { stdio: ["ignore", "ignore", "ignore"] });
await sleep(1500);
const engine = spawn(IDM, ["--serve", "-o", downloads], { stdio: "ignore", env });
await sleep(2000);

try {
  const send = (url) =>
    spawnSync(IDM, ["--send", url], { encoding: "utf8", env });

  // The URL ends in `named`: no extension, nothing to go on. Only the server
  // knows this is real-archive.zip.
  const named = send(`http://127.0.0.1:${PORT}/named?size=${SIZE}&name=real-archive.zip`);
  check("an opaque url is accepted", named.status === 0, (named.stderr || named.stdout).trim().split("\n")[0]);

  // Sorting applies by default, so a .zip belongs in Archives. It could only
  // land there if the name was known before the destination was chosen.
  const landed = await waitFor(downloads, "Archives/real-archive.zip");
  check(
    "it is saved under the server's name, in the folder that name belongs in",
    landed,
    `found: ${JSON.stringify(listing(downloads))}`,
  );
  check(
    "and not under the URL's last segment",
    !listing(downloads).some((f) => f === "named" || f.endsWith("/named")),
  );

  // Control: a server that gives no name at all. The behaviour must be what it
  // always was -- the URL's own segment -- so this cannot have made anything
  // worse.
  const plain = send(`http://127.0.0.1:${PORT}/honest?size=${SIZE}`);
  check("a url with no name anywhere is still accepted", plain.status === 0);
  check("and keeps the name from its url", await waitFor(downloads, "honest"), JSON.stringify(listing(downloads)));
} finally {
  engine.kill();
  server.kill();
}

console.log(failures === 0 ? "\nopaque-name: PASS" : `\nopaque-name: ${failures} FAILED`);
process.exit(failures === 0 ? 0 : 1);
