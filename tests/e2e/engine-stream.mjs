// Hands a stream manifest to the engine the way the browser extension does,
// and checks that a video comes out rather than a playlist.
//
// The CLI has its own path into the stream code; this exercises the one the
// window and the extension use. Without it a manifest handed over by a
// browser would be fetched as what it literally is — a few hundred bytes of
// text, saved as if it were the video.

import { spawn, spawnSync } from "node:child_process";
import { existsSync, mkdtempSync, readFileSync, readdirSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const HERE = dirname(fileURLToPath(import.meta.url));
const REPO = resolve(HERE, "..", "..");
const IDM = join(REPO, "target", "release", "idm.exe");
const HOST = join(REPO, "target", "release", "idm-host.exe");
const MOCK = join(REPO, "tests", "mock-server", "server.mjs");
const PORT = 8174;

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
let failures = 0;

function check(label, ok, detail = "") {
  console.log(`  ${ok ? "PASS" : "FAIL"}  ${label}${detail ? "  — " + detail : ""}`);
  if (!ok) failures += 1;
  return ok;
}

function byteAt(offset) {
  let x = (offset + 0x9e3779b9) | 0;
  x = Math.imul(x ^ (x >>> 16), 0x85ebca6b);
  x = Math.imul(x ^ (x >>> 13), 0xc2b2ae35);
  x = x ^ (x >>> 16);
  return x & 0xff;
}

/** One message to a freshly spawned host, as a browser does it. */
function askHost(message) {
  return new Promise((done) => {
    const child = spawn(HOST, ["chrome-extension://streamteststreamteststreamte/"], {
      stdio: ["pipe", "pipe", "ignore"],
    });
    const chunks = [];
    child.stdout.on("data", (chunk) => chunks.push(chunk));

    const body = Buffer.from(JSON.stringify(message), "utf8");
    const header = Buffer.alloc(4);
    header.writeUInt32LE(body.length);
    child.stdin.write(Buffer.concat([header, body]));

    setTimeout(() => {
      child.stdin.end();
      setTimeout(() => {
        const all = Buffer.concat(chunks);
        if (all.length < 4) return done({ type: "no_reply" });
        try {
          done(JSON.parse(all.subarray(4, 4 + all.readUInt32LE(0)).toString("utf8")));
        } catch (error) {
          done({ type: "unparseable", error: String(error) });
        }
      }, 400);
    }, 900);
  });
}

async function main() {
  for (const [label, path] of [["idm.exe", IDM], ["idm-host.exe", HOST]]) {
    if (!existsSync(path)) {
      console.error(`missing ${label}; run: cargo build --release --workspace`);
      process.exit(1);
    }
  }

  console.log("DivinePulse — a stream through the engine\n");

  const downloads = mkdtempSync(join(tmpdir(), "idm-engstream-"));
  const stateDir = mkdtempSync(join(tmpdir(), "idm-state-"));
  process.env.IDM_STATE_DIR = stateDir;

  const mock = spawn("node", [MOCK, "--port", String(PORT)], {
    stdio: ["ignore", "pipe", "ignore"],
  });
  await sleep(2000);

  const log = [];
  const engine = spawn(IDM, ["--serve", "-o", downloads], {
    stdio: ["ignore", "pipe", "pipe"],
  });
  engine.stderr.on("data", (d) => log.push(d.toString()));
  engine.stdout.on("data", (d) => log.push(d.toString()));
  await sleep(1500);

  try {
    const hello = await askHost({
      type: "hello",
      client_id: "stream-test-client",
      label: "Test browser",
    });
    if (hello.type === "pairing_pending") {
      spawnSync(IDM, ["--approve", hello.request_id], { encoding: "utf8" });
    }

    const segments = 6;
    const size = 32768;
    // The size hint is the playlist's own, which is tiny. The engine must
    // not measure a manifest against the takeover threshold: a few hundred
    // bytes of text stands for a video of gigabytes.
    const queued = await askHost({
      type: "download",
      client_id: "stream-test-client",
      url: `http://127.0.0.1:${PORT}/hls/master.m3u8?segments=${segments}&size=${size}`,
      filename: "lecture.m3u8",
      size_hint: 400,
      mime: "application/vnd.apple.mpegurl",
    });
    check(
      "a tiny manifest is taken over despite the size threshold",
      queued.type === "accepted",
      JSON.stringify(queued),
    );

    let file = null;
    for (let i = 0; i < 120 && !file; i += 1) {
      await sleep(250);
      const found = readdirSync(downloads).filter((name) => !name.startsWith("."));
      if (found.length === 1) {
        const path = join(downloads, found[0]);
        if (readFileSync(path).length === segments * size) file = path;
      }
    }
    check("the engine produces a video, not a playlist", Boolean(file), readdirSync(downloads).join(", "));

    if (file) {
      const body = readFileSync(file);
      check(
        "it is not the manifest text",
        !body.subarray(0, 16).toString("utf8").startsWith("#EXTM3U"),
        body.subarray(0, 7).toString("utf8"),
      );

      let bad = -1;
      for (let i = 0; i < body.length; i += 1) {
        if (body[i] !== byteAt(i)) {
          bad = i;
          break;
        }
      }
      check(
        "the segments are joined correctly, byte for byte",
        bad < 0,
        bad < 0 ? `${body.length} bytes` : `wrong at ${bad}`,
      );
    }
  } finally {
    engine.kill();
    mock.kill();
    await sleep(800);
    for (const dir of [downloads, stateDir]) {
      try {
        rmSync(dir, { recursive: true, force: true, maxRetries: 3 });
      } catch {
        // The engine may still hold the database open.
      }
    }
    if (failures) console.log("\nengine output:\n" + log.join("").trim());
  }

  console.log(`\n${failures ? failures + " check(s) failed" : "all checks passed"}`);
  process.exit(failures ? 1 : 0);
}

main().catch((error) => {
  console.error(error);
  process.exit(1);
});
