// Kills the engine in the middle of a download and checks that starting it
// again finishes the file rather than starting over.
//
// This is the reboot case, which is the one that actually happens: a machine
// restarts overnight with a large download part-done. Every piece of it has
// its own tests — the journal, the extent map, the store's paused rows — but
// nothing joined them up, and the joins are where this sort of thing breaks.
//
//   node resume-after-restart.mjs

import { spawn, spawnSync } from "node:child_process";
import { existsSync, mkdtempSync, readFileSync, readdirSync, rmSync, statSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const HERE = dirname(fileURLToPath(import.meta.url));
const REPO = resolve(HERE, "..", "..");
const IDM = join(REPO, "target", "release", "idm.exe");
const HOST = join(REPO, "target", "release", "idm-host.exe");
const BENCH = join(REPO, "tests", "bench", "bench-server.mjs");

const PORT = 8160;
const SIZE = 32 * 1024 * 1024;
// A cap on the whole server, not on each connection. Shaping per connection
// lets sixteen of them finish this in under a second, leaving no middle to
// interrupt — which is exactly what the first run of this test did.
const BYTES_TOTAL = 3_000_000;

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
let failures = 0;

function check(label, ok, detail = "") {
  console.log(`  ${ok ? "PASS" : "FAIL"}  ${label}${detail ? "  — " + detail : ""}`);
  if (!ok) failures += 1;
  return ok;
}

/** One message to a freshly spawned host, as a browser does it. */
function askHost(message) {
  return new Promise((done) => {
    const child = spawn(HOST, ["chrome-extension://resumetestresumetestresumetes/"], {
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

function startEngine(downloads, log) {
  const engine = spawn(IDM, ["--serve", "-o", downloads], { stdio: ["ignore", "pipe", "pipe"] });
  engine.stderr.on("data", (d) => log.push(d.toString()));
  engine.stdout.on("data", (d) => log.push(d.toString()));
  return engine;
}

/**
 * Whether a download is still in flight.
 *
 * Not by file size: the engine preallocates sparsely, so the part file
 * reports its full length from the first second and a size check says the
 * download finished instantly. The part file's *existence* is the honest
 * signal — it is renamed to the final name only once every byte is there.
 */
function inFlight(dir) {
  if (!existsSync(dir)) return false;
  return readdirSync(dir).some((name) => name.endsWith(".idmpart"));
}

function listing(dir) {
  return existsSync(dir) ? readdirSync(dir).join(", ") : "(no directory)";
}

async function main() {
  for (const [label, path] of [["idm.exe", IDM], ["idm-host.exe", HOST]]) {
    if (!existsSync(path)) {
      console.error(`missing ${label}; run: cargo build --release --workspace`);
      process.exit(1);
    }
  }

  console.log("DivinePulse — resume after a restart\n");

  const downloads = mkdtempSync(join(tmpdir(), "idm-resume-"));
  const stateDir = mkdtempSync(join(tmpdir(), "idm-state-"));
  process.env.IDM_STATE_DIR = stateDir;

  const bench = spawn(
    "node",
    [BENCH, "--port", String(PORT), "--size", String(SIZE),
     "--bps-total", String(BYTES_TOTAL), "--rtt", "20"],
    { stdio: ["ignore", "pipe", "ignore"] },
  );
  await sleep(2000);

  const log = [];
  let engine = startEngine(downloads, log);
  await sleep(1500);

  try {
    // Pair, so the download can be handed over the same way a browser does.
    const hello = await askHost({
      type: "hello",
      client_id: "resume-test-client",
      label: "Test browser",
    });
    if (hello.type === "pairing_pending") {
      spawnSync(IDM, ["--approve", hello.request_id], { encoding: "utf8" });
    }

    const queued = await askHost({
      type: "download",
      client_id: "resume-test-client",
      url: `http://127.0.0.1:${PORT}/big.bin`,
      filename: "big.bin",
      size_hint: SIZE,
    });
    check("the download starts", queued.type === "accepted", JSON.stringify(queued));

    // Let it get a few seconds in, then interrupt it while the part file
    // is still there. At the capped rate that is roughly a quarter of the
    // file, with plenty left to fetch afterwards.
    let started = false;
    for (let i = 0; i < 200 && !started; i += 1) {
      await sleep(50);
      started = inFlight(downloads);
    }
    check("the part file appears", started, listing(downloads));
    await sleep(4000);

    const interrupted = inFlight(downloads);
    check("it is still going when interrupted", interrupted, listing(downloads));

    // The reboot: no warning, no chance to flush anything.
    engine.kill("SIGKILL");
    await sleep(1500);

    check("the partial file survives", inFlight(downloads), listing(downloads));

    log.length = 0;
    engine = startEngine(downloads, log);

    let pickedUp = false;
    for (let i = 0; i < 40 && !pickedUp; i += 1) {
      await sleep(250);
      pickedUp = /picked up \d+ interrupted download/.test(log.join(""));
    }
    const lines = () => log.join("").split("\n");
    const pickedUpLine = lines().find((line) => line.includes("picked up"));
    check("the engine picks it up on startup", pickedUp, (pickedUpLine ?? "").trim());

    const target = join(downloads, "big.bin");
    let finished = false;
    for (let i = 0; i < 240 && !finished; i += 1) {
      await sleep(250);
      finished = existsSync(target) && statSync(target).size === SIZE;
    }
    check("it finishes without starting over", finished, finished ? "" : "never completed");

    // Finishing is not the same as resuming. A download that quietly started
    // over would also end up with a correct file, having thrown away
    // everything fetched before the interruption — which is exactly the
    // failure this whole feature exists to prevent.
    const resumedLine = lines().find((line) => line.startsWith("resumed "));
    const reused = Number(resumedLine?.match(/after (\d+) bytes/)?.[1] ?? 0);
    check(
      "it reuses what was already on disk rather than starting over",
      reused > 0 && reused < SIZE,
      resumedLine ? `${(reused / (1 << 20)).toFixed(1)} MiB reused` : "no resume reported",
    );

    if (finished) {
      const got = readFileSync(target);
      let bad = 0;
      for (let i = 0; i < got.length && bad === 0; i += 3581) {
        let x = (i + 0x9e3779b9) | 0;
        x = Math.imul(x ^ (x >>> 16), 0x85ebca6b);
        x = Math.imul(x ^ (x >>> 13), 0xc2b2ae35);
        x = x ^ (x >>> 16);
        if (got[i] !== (x & 0xff)) bad += 1;
      }
      // The point of the whole exercise: bytes from before the interruption
      // and bytes from after it have to form one correct file.
      check("the two halves form one correct file", bad === 0);
    }
  } finally {
    try {
      engine.kill();
    } catch {}
    bench.kill();
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
