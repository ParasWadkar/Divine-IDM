// Checks that losing the network mid-download is a pause, not a failure, and
// that the file is right afterwards.
//
// This is the behaviour people actually judge a download manager by. The
// engine had the hard half already -- a journal and per-block CRCs, so
// resuming appends to verified bytes -- but nothing joined it up: any error
// ended the download as Failed, so a dropped Wi-Fi meant starting a 4 GB file
// again. Worse, the error was flattened to a string on the way out, so a 404
// and a dropped connection were indistinguishable by the time anything could
// act on them.
//
// The server is killed mid-transfer and brought back on the same port, which
// is what a connection dropping looks like from the engine's side.
//
//   node network-drop.mjs

import { spawn, spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import { existsSync, mkdtempSync, readFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const HERE = dirname(fileURLToPath(import.meta.url));
const REPO = resolve(HERE, "..", "..");
const IDM = join(REPO, "target", "release", "idm.exe");
const MOCK = join(REPO, "tests", "mock-server", "server.mjs");
const PORT = 8199;
const SIZE = 12 * 1024 * 1024;
// Per *connection*, and the engine opens about sixteen, so the whole
// transfer runs at roughly sixteen times this. Sized so the file takes
// something like fifteen seconds and there is a real window in which to
// pull the plug partway through.
const BPS = 50_000;

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
let failures = 0;
function check(label, ok, detail = "") {
  console.log(`  ${ok ? "PASS" : "FAIL"}  ${label}${detail ? "  — " + detail : ""}`);
  if (!ok) failures += 1;
}

const env = { ...process.env, IDM_STATE_DIR: mkdtempSync(join(tmpdir(), "idm-drop-state-")) };
const downloads = mkdtempSync(join(tmpdir(), "idm-drop-dl-"));

/** What the mock server's generator produces, computed independently. */
function expected(length) {
  const b = Buffer.allocUnsafe(length);
  for (let i = 0; i < length; i += 1) {
    let x = (i + 0x9e3779b9) | 0;
    x = Math.imul(x ^ (x >>> 16), 0x85ebca6b);
    x = Math.imul(x ^ (x >>> 13), 0xc2b2ae35);
    x = x ^ (x >>> 16);
    b[i] = x & 0xff;
  }
  return createHash("sha256").update(b).digest("hex");
}

const startServer = () =>
  spawn("node", [MOCK, "--port", String(PORT)], { stdio: ["ignore", "ignore", "ignore"] });

function jobs() {
  const out = spawnSync(IDM, ["--jobs"], { encoding: "utf8", env });
  try {
    return JSON.parse(out.stdout);
  } catch {
    return [];
  }
}

/** Waits for any job to reach one of `states`, returning the job or null. */
async function waitForState(states, seconds) {
  for (let i = 0; i < seconds * 4; i += 1) {
    await sleep(250);
    const job = jobs().find((j) => states.includes(j.state));
    if (job) return job;
  }
  return null;
}

if (!existsSync(IDM)) {
  console.error("missing idm.exe; run: cargo build --release -p idm-cli");
  process.exit(1);
}

console.log("Divine IDM — the network going out mid-download\n");

let server = startServer();
await sleep(1500);
const engine = spawn(IDM, ["--serve", "-o", downloads], { stdio: "ignore", env });
await sleep(2000);

try {
  const url = `http://127.0.0.1:${PORT}/slow?size=${SIZE}&bps=${BPS}`;
  const sent = spawnSync(IDM, ["--send", url], { encoding: "utf8", env });
  check("the download starts", sent.status === 0, (sent.stderr || sent.stdout).trim());

  // Let it get somewhere, so this is a resume rather than a fresh start.
  const running = await waitForState(["running"], 15);
  check("it is running", running !== null);
  await sleep(2500);
  const before = jobs()[0]?.downloaded ?? 0;
  check(
    "it is partway through, so this will be a resume and not a fresh start",
    before > 0 && before < SIZE,
    `${before} of ${SIZE} bytes`,
  );

  // The plug.
  server.kill();
  server = null;
  console.log("  ... server killed");

  // Longer than the watchdog's patience: a dropped connection does not
  // raise an error inside the scheduler, so it is noticed by bytes failing
  // to move rather than by anything failing outright.
  const waiting = await waitForState(["reconnecting"], 60);
  check(
    "losing the connection leaves it reconnecting, not failed",
    waiting !== null,
    waiting ? `state=${waiting.state}` : `states seen: ${JSON.stringify(jobs().map((j) => j.state))}`,
  );
  check(
    "and it is not reported as a failure",
    !jobs().some((j) => j.state === "failed"),
    JSON.stringify(jobs().map((j) => j.state)),
  );

  // The network comes back.
  server = startServer();
  await sleep(1500);
  console.log("  ... server back");

  let done = false;
  for (let i = 0; i < 240 && !done; i += 1) {
    await sleep(500);
    done = jobs().some((j) => j.state === "completed");
  }
  check("it finishes by itself once the connection returns", done, JSON.stringify(jobs().map((j) => j.state)));

  const path = join(downloads, "slow");
  if (existsSync(path)) {
    const data = readFileSync(path);
    check("the file is the right size", data.length === SIZE, `${data.length} of ${SIZE}`);
    check(
      "and every byte is correct across the break",
      createHash("sha256").update(data).digest("hex") === expected(SIZE),
    );
  } else {
    check("the file is on disk", false, `nothing at ${path}`);
  }
} finally {
  engine.kill();
  if (server) server.kill();
}

console.log(failures === 0 ? "\nnetwork-drop: PASS" : `\nnetwork-drop: ${failures} FAILED`);
process.exit(failures === 0 ? 0 : 1);
