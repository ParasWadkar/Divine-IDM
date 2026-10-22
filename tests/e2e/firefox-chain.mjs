// Drives the Gecko side of the chain in a real Firefox: loads the add-on,
// pairs it, and watches a download get taken over.
//
// Firefox release builds refuse unsigned add-ons permanently, and a temporary
// add-on normally needs a human at about:debugging. Marionette -- Firefox's
// own automation server, enabled by the --marionette flag -- can install one
// with no clicking, so this runs unattended like the Chromium chain does.
//
// Worth having rather than assuming Gecko behaves like Chromium: the two
// differ in the host manifest dialect (`allowed_extensions` with a literal id
// versus `allowed_origins` with a path hash), in the background context
// (event page versus service worker), and in the registry root. None of that
// is exercised anywhere else.
//
//   node firefox-chain.mjs

import { spawn, spawnSync } from "node:child_process";
import { connect } from "node:net";
import {
  existsSync,
  mkdtempSync,
  readFileSync,
  readdirSync,
  rmSync,
  statSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const HERE = dirname(fileURLToPath(import.meta.url));
const REPO = resolve(HERE, "..", "..");
const IDM = join(REPO, "target", "release", "idm.exe");
const MOCK = join(REPO, "tests", "mock-server", "server.mjs");
const ADDON = join(REPO, "extension", "dist", "firefox");

const FIREFOX = [
  "C:/Program Files/Mozilla Firefox/firefox.exe",
  "C:/Program Files (x86)/Mozilla Firefox/firefox.exe",
].find((path) => existsSync(path));

const PORT = 8125;
const MARIONETTE_PORT = 2829;
const SIZE = 6 * 1024 * 1024;

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
let failures = 0;

function check(label, ok, detail = "") {
  console.log(`  ${ok ? "PASS" : "FAIL"}  ${label}${detail ? "  — " + detail : ""}`);
  if (!ok) failures += 1;
  return ok;
}

// ---------------------------------------------------------------------------
// Marionette: length-prefixed JSON, "<byteLength>:<payload>"
// ---------------------------------------------------------------------------

class Marionette {
  constructor(socket) {
    this.socket = socket;
    this.buffer = Buffer.alloc(0);
    this.nextId = 1;
    this.waiting = new Map();
    this.handshake = null;

    socket.on("data", (chunk) => {
      this.buffer = Buffer.concat([this.buffer, chunk]);
      this.drain();
    });
  }

  /** Pulls every complete frame out of the buffer. */
  drain() {
    for (;;) {
      const colon = this.buffer.indexOf(0x3a); // ':'
      if (colon < 0) return;
      const length = Number(this.buffer.subarray(0, colon).toString("ascii"));
      if (!Number.isFinite(length)) return;
      const start = colon + 1;
      if (this.buffer.length < start + length) return;

      const body = this.buffer.subarray(start, start + length).toString("utf8");
      this.buffer = this.buffer.subarray(start + length);

      let message;
      try {
        message = JSON.parse(body);
      } catch {
        continue;
      }

      // The first frame is the server's hello, not a command reply.
      if (!Array.isArray(message)) {
        this.handshake = message;
        continue;
      }
      const [, id, error, result] = message;
      const pending = this.waiting.get(id);
      if (!pending) continue;
      this.waiting.delete(id);
      if (error) pending.reject(new Error(JSON.stringify(error)));
      else pending.resolve(result);
    }
  }

  send(command, parameters = {}) {
    const id = this.nextId++;
    const payload = JSON.stringify([0, id, command, parameters]);
    const body = Buffer.from(payload, "utf8");
    this.socket.write(`${body.length}:`);
    this.socket.write(body);
    return new Promise((resolve_, reject) => {
      this.waiting.set(id, { resolve: resolve_, reject });
      setTimeout(() => {
        if (this.waiting.delete(id)) reject(new Error(`${command} timed out`));
      }, 60_000);
    });
  }
}

function dial(port, attemptsLeft = 40) {
  return new Promise((resolve_, reject) => {
    const socket = connect(port, "127.0.0.1");
    socket.once("connect", () => resolve_(socket));
    socket.once("error", async (error) => {
      socket.destroy();
      if (attemptsLeft <= 0) return reject(error);
      await sleep(500);
      dial(port, attemptsLeft - 1).then(resolve_, reject);
    });
  });
}

// ---------------------------------------------------------------------------

/** A profile that downloads without asking, and talks Marionette. */
function makeProfile(downloadDir) {
  const dir = mkdtempSync(join(tmpdir(), "idm-ff-"));
  const prefs = [
    // Save to our directory without a dialog.
    'user_pref("browser.download.folderList", 2);',
    `user_pref("browser.download.dir", ${JSON.stringify(downloadDir)});`,
    'user_pref("browser.download.useDownloadDir", true);',
    'user_pref("browser.download.always_ask_before_handling_new_types", false);',
    'user_pref("browser.helperApps.neverAsk.saveToDisk", "application/octet-stream");',
    // Keep the run quiet and offline-ish.
    'user_pref("browser.shell.checkDefaultBrowser", false);',
    'user_pref("datareporting.policy.dataSubmissionEnabled", false);',
    'user_pref("app.update.auto", false);',
    'user_pref("browser.aboutwelcome.enabled", false);',
    `user_pref("marionette.port", ${MARIONETTE_PORT});`,
    // Sends extension console output to Firefox's stdout, so a failure
    // inside the add-on shows up here instead of only in the Browser Console.
    'user_pref("devtools.console.stdout.chrome", true);',
  ];
  writeFileSync(join(dir, "user.js"), prefs.join("\n") + "\n");
  return dir;
}

function startMock(port) {
  const child = spawn("node", [MOCK, "--port", String(port)], {
    stdio: ["ignore", "pipe", "ignore"],
  });
  return new Promise((resolve_) => {
    child.stdout.on("data", (d) => {
      if (d.toString().includes("listening")) resolve_(child);
    });
    setTimeout(() => resolve_(child), 3000);
  });
}

/**
 * Starts a download without waiting for a page load.
 *
 * WebDriver:Navigate is wrong here: it waits for a load event, and a URL
 * that starts a download never produces one, so the call hangs until it
 * times out. Deferring `location.href` with setTimeout is wrong too, and
 * more insidiously -- the callback is pending when the script returns, and
 * Marionette may tear the sandbox down before it runs, so the navigation
 * silently never happens. That showed up as an intermittently empty
 * download directory. Clicking a link is synchronous: by the time the
 * script returns, the download has already been asked for.
 */
function clickLink(session, url) {
  return session.send("WebDriver:ExecuteScript", {
    script: [
      "var a = document.createElement('a');",
      "a.href = arguments[0];",
      "document.body.appendChild(a);",
      "a.click();",
    ].join(""),
    args: [url],
  });
}

/**
 * Starts a download and waits for `settled` to report it, clicking again if
 * nothing happens.
 *
 * The click is the only step in this harness with no acknowledgement: the
 * script returns as soon as it has dispatched the event, whether or not
 * Firefox went on to start a download. Returns the number of attempts, so a
 * retry that was actually needed shows up in the output rather than hiding
 * a flaky trigger behind a pass.
 */
async function startDownload(session, url, settled, attempts = 3) {
  for (let attempt = 1; attempt <= attempts; attempt += 1) {
    await clickLink(session, url);
    for (let i = 0; i < 24; i += 1) {
      await sleep(500);
      if (await settled()) return attempt;
    }
  }
  return attempts;
}

async function main() {
  if (!FIREFOX) {
    console.error("Firefox not found; skipping.");
    process.exit(0);
  }
  for (const [label, path] of [
    ["idm.exe", IDM],
    ["the built add-on", ADDON],
  ]) {
    if (!existsSync(path)) {
      console.error(`missing ${label} at ${path}`);
      process.exit(1);
    }
  }

  console.log("Divine IDM — Firefox chain\n");
  const startedAt = Date.now();

  const downloads = mkdtempSync(join(tmpdir(), "idm-ffdl-"));
  // Its own state directory. Sharing the real one means a run that dies
  // partway can leave settings behind that break every later run, and can
  // damage a real install's pairings and history.
  const stateDir = mkdtempSync(join(tmpdir(), "idm-state-"));
  process.env.IDM_STATE_DIR = stateDir;
  const browserDownloads = mkdtempSync(join(tmpdir(), "idm-ffbrowser-"));
  const profile = makeProfile(browserDownloads);

  const mock = await startMock(PORT);
  const engine = spawn(IDM, ["--serve", "-o", downloads], {
    stdio: ["ignore", "pipe", "pipe"],
  });
  const engineLog = [];
  engine.stderr.on("data", (d) => engineLog.push(d.toString()));
  await sleep(1500);

  // browser_integration persists across engine restarts, so a previous run
  // that died after disabling it would leave every later run unable to take
  // a download over, with no sign of why.
  spawnSync(IDM, ["--integration", "on"], { encoding: "utf8" });

  const firefox = spawn(
    FIREFOX,
    ["-profile", profile, "-marionette", "-no-remote", "-url", "about:blank"],
    { stdio: ["ignore", "pipe", "pipe"] },
  );
  const browserLog = [];
  firefox.stdout.on("data", (d) => browserLog.push(d.toString()));
  firefox.stderr.on("data", (d) => browserLog.push(d.toString()));

  try {
    const socket = await dial(MARIONETTE_PORT);
    const session = new Marionette(socket);
    await sleep(500);
    check("Marionette is reachable", session.handshake !== null, JSON.stringify(session.handshake));

    await session.send("WebDriver:NewSession", {});

    // The one step a human would otherwise do at about:debugging.
    const installed = await session.send("Addon:Install", { path: ADDON, temporary: true });
    check("the add-on installs", Boolean(installed?.value ?? installed), JSON.stringify(installed));

    // The event page greets on load; give it a moment to reach the engine.
    let requestId = null;
    for (let i = 0; i < 40 && !requestId; i += 1) {
      await sleep(500);
      const match = engineLog.join("").match(/idm --approve ([0-9a-f]+)/);
      if (match) requestId = match[1];
    }
    check("Firefox asks to be paired", Boolean(requestId), requestId ?? "no request appeared");

    const label = (engineLog.join("").match(/Pairing requested by "([^"]+)"/) ?? [])[1];
    check("it names itself Firefox", /firefox/i.test(label ?? ""), label ?? "no label");

    if (requestId) {
      const approved = spawnSync(IDM, ["--approve", requestId], { encoding: "utf8" });
      check(
        "the CLI approves it",
        /approved/i.test(approved.stdout + approved.stderr),
        (approved.stderr || approved.stdout).trim().split("\n")[0],
      );
    }

    const target = join(downloads, "honest");
    const landedFully = () => existsSync(target) && readFileSync(target).length === SIZE;
    const tries = await startDownload(
      session,
      `http://127.0.0.1:${PORT}/honest?size=${SIZE}`,
      async () => landedFully(),
    );
    const landed = landedFully();
    check(
      "the engine takes the download",
      landed,
      landed ? (tries > 1 ? `after ${tries} attempts` : "") : "never appeared",
    );

    if (landed) {
      const got = readFileSync(target);
      let ok = true;
      for (let i = 0; i < got.length && ok; i += 4093) {
        let x = (i + 0x9e3779b9) | 0;
        x = Math.imul(x ^ (x >>> 16), 0x85ebca6b);
        x = Math.imul(x ^ (x >>> 13), 0xc2b2ae35);
        x = x ^ (x >>> 16);
        if (got[i] !== (x & 0xff)) ok = false;
      }
      check("the bytes verify against the generator", ok);
    }

    const leftBehind = readdirSync(browserDownloads);
    check(
      "Firefox kept no copy of its own",
      leftBehind.length === 0,
      leftBehind.join(", ") || "(empty)",
    );

    // Declining is the branch that can strand a download: it is paused
    // before the engine is asked, so a refusal has to resume it. If that
    // ever stops working the download just stops, with nothing on screen
    // to say why.
    const off = spawnSync(IDM, ["--integration", "off"], { encoding: "utf8" });
    console.log(`        [integration off: ${(off.stderr || off.stdout).trim()}]`);
    const second = 3 * 1024 * 1024;
    const findKept = () => {
      const match = readdirSync(browserDownloads)
        .map((name) => {
          // Firefox renames the partial as it finishes, so a name from
          // readdir can be gone by the time it is stat'd.
          try {
            return { name, size: statSync(join(browserDownloads, name)).size };
          } catch {
            return null;
          }
        })
        .find((file) => file && file.size === second);
      return match ?? null;
    };

    const keptTries = await startDownload(
      session,
      `http://127.0.0.1:${PORT}/honest?size=${second}`,
      async () => findKept() !== null,
    );
    const kept = findKept();
    check(
      `a declined download is resumed and finishes in Firefox${
        kept && keptTries > 1 ? ` (after ${keptTries} attempts)` : ""
      }`,
      Boolean(kept),
      kept ? `${kept.name} (${kept.size})` : readdirSync(browserDownloads).join(", ") || "(nothing)",
    );
    if (!kept) {
      console.log(`        [engine dir now: ${readdirSync(downloads).join(", ") || "(empty)"}]`);
      // Firefox can stage a partial download in the system temp directory and
      // only move it on completion, so an empty target directory does not by
      // itself mean the download never started. These two cases need very
      // different fixes, so look.
      const since = startedAt;
      const strays = readdirSync(tmpdir())
        .map((name) => {
          try {
            const s = statSync(join(tmpdir(), name));
            return { name, size: s.size, at: s.mtimeMs };
          } catch {
            return null;
          }
        })
        .filter((f) => f && f.at > since && (f.size > 100_000 || /\.part$/.test(f.name)))
        .map((f) => `${f.name} (${f.size})`);
      console.log(`        [recent files in temp: ${strays.join(", ") || "(none)"}]`);
    }

  } finally {
    // In the finally, not after the check: an exception in between would
    // otherwise leave integration off for whatever runs next.
    spawnSync(IDM, ["--integration", "on"], { encoding: "utf8" });
    try {
      firefox.kill();
    } catch {}
    engine.kill();
    mock.kill();
    await sleep(1000);
    for (const dir of [downloads, browserDownloads, profile, stateDir]) {
      try {
        rmSync(dir, { recursive: true, force: true, maxRetries: 3 });
      } catch {
        // The engine may still hold the database open. A temporary
        // directory left behind is not worth failing a run over.
      }
    }
    const addonLines = browserLog
      .join("")
      .split("\n")
      .map((line) => line.trimEnd())
      .filter((line) => /Divine IDM|idm@divine/i.test(line));
    if ((failures || process.env.IDM_VERBOSE) && addonLines.length) {
      console.log("\nadd-on output:\n" + addonLines.join("\n"));
    }
    if (failures && engineLog.length) {
      console.log("\nengine output:\n" + engineLog.join("").trim());
    }
  }

  console.log(`\n${failures ? failures + " check(s) failed" : "all checks passed"}`);
  process.exit(failures ? 1 : 0);
}

main().catch((error) => {
  console.error(error);
  process.exit(1);
});
