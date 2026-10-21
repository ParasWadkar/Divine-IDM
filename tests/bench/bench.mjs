// Benchmark harness: times this engine against other downloaders on a shaped
// local server, and verifies that each one produced the right bytes.
//
// A faster download of the wrong bytes is worthless, so every run is checked
// against a SHA-256 computed from the same deterministic generator the server
// uses. A tool whose digest does not match is reported as WRONG, not as fast.
//
//   node bench.mjs                          # per-connection shaping, the default
//   node bench.mjs --mode total             # per-IP shaping: more connections buy nothing
//   node bench.mjs --size 268435456 --runs 5
//   node bench.mjs --tools idm,curl

import { createHash } from "node:crypto";
import { spawn, spawnSync } from "node:child_process";
import { existsSync, mkdirSync, rmSync, statSync } from "node:fs";
import { readFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

const HERE = fileURLToPath(new URL(".", import.meta.url));
const REPO = join(HERE, "..", "..");

function arg(name, fallback) {
  const index = process.argv.indexOf(`--${name}`);
  if (index === -1) return fallback;
  const raw = process.argv[index + 1];
  const value = Number(raw);
  return Number.isFinite(value) ? value : raw;
}

const SIZE = Number(arg("size", 128 * 1024 * 1024));
const RTT = Number(arg("rtt", 30));
const BPS = Number(arg("bps", 8_000_000));
const MODE = String(arg("mode", "per-conn"));
const RUNS = Number(arg("runs", 3));
const ONLY = String(arg("tools", "")).split(",").filter(Boolean);

const IDM_EXE = "C:\\Program Files (x86)\\Internet Download Manager\\IDMan.exe";
const ARIA2 = join(
  process.env.LOCALAPPDATA ?? "",
  "Microsoft\\WinGet\\Packages\\aria2.aria2_Microsoft.Winget.Source_8wekyb3d8bbwe",
  "aria2-1.37.0-win-64bit-build1\\aria2c.exe",
);
const OUR_EXE = join(REPO, "target", "release", "idm.exe");

function byteAt(offset) {
  let x = (offset + 0x9e3779b9) | 0;
  x = Math.imul(x ^ (x >>> 16), 0x85ebca6b);
  x = Math.imul(x ^ (x >>> 13), 0xc2b2ae35);
  x = x ^ (x >>> 16);
  return x & 0xff;
}

/** The digest the file must have, computed independently of any downloader. */
function expectedDigest(size) {
  const hash = createHash("sha256");
  const chunk = 1 << 20;
  const buffer = Buffer.allocUnsafe(chunk);
  for (let base = 0; base < size; base += chunk) {
    const len = Math.min(chunk, size - base);
    for (let i = 0; i < len; i += 1) buffer[i] = byteAt(base + i);
    hash.update(buffer.subarray(0, len));
  }
  return hash.digest("hex");
}

async function digestOf(path) {
  const hash = createHash("sha256");
  hash.update(await readFile(path));
  return hash.digest("hex");
}

function startServer() {
  return new Promise((resolve, reject) => {
    const args = [
      join(HERE, "bench-server.mjs"),
      "--port", "0",
      "--size", String(SIZE),
      "--rtt", String(RTT),
    ];
    if (MODE === "total") args.push("--bps-total", String(BPS), "--bps-per-conn", String(BPS));
    else args.push("--bps-per-conn", String(BPS));

    const child = spawn("node", args, { stdio: ["ignore", "pipe", "inherit"] });
    let buffered = "";
    child.stdout.on("data", (data) => {
      buffered += data.toString();
      const match = /http:\/\/127\.0\.0\.1:(\d+)/.exec(buffered);
      if (match) resolve({ child, base: match[0] });
    });
    child.on("exit", (code) => reject(new Error(`bench server exited with ${code}`)));
    setTimeout(() => reject(new Error("bench server did not start")), 10_000);
  });
}

/** Runs a command to completion and returns elapsed milliseconds. */
function timeSpawn(command, args) {
  const started = process.hrtime.bigint();
  const result = spawnSync(command, args, { stdio: ["ignore", "ignore", "pipe"] });
  const elapsed = Number(process.hrtime.bigint() - started) / 1e6;
  return { elapsed, status: result.status, stderr: result.stderr?.toString() ?? "" };
}

const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

/**
 * IDM hands the job to its own background process and IDMan.exe returns
 * immediately, so timing the launcher measures nothing. Wall clock is taken
 * from launch until the output file reaches its expected size.
 */
async function timeIdm(url, dir, name) {
  const target = join(dir, name);
  const started = process.hrtime.bigint();
  const launch = spawnSync(IDM_EXE, ["/d", url, "/p", dir, "/f", name, "/n", "/q"], {
    stdio: ["ignore", "ignore", "pipe"],
  });
  if (launch.error) return { elapsed: 0, status: 1, stderr: String(launch.error) };

  const deadline = Date.now() + 600_000;
  for (;;) {
    if (existsSync(target)) {
      try {
        if (statSync(target).size === SIZE) {
          await sleep(100); // let the final flush and rename settle
          return {
            elapsed: Number(process.hrtime.bigint() - started) / 1e6,
            status: 0,
            stderr: "",
          };
        }
      } catch {
        /* mid-rename; try again */
      }
    }
    if (Date.now() > deadline) {
      return { elapsed: 0, status: 1, stderr: "timed out waiting for IDM" };
    }
    await sleep(50);
  }
}

const TOOLS = [
  {
    name: "divine-idm",
    label: "Divine IDM (adaptive)",
    available: () => existsSync(OUR_EXE),
    run: (url, dir, name) =>
      timeSpawn(OUR_EXE, [url, "-o", dir, "--name", name, "--no-edge-pinning", "-q"]),
  },
  {
    name: "divine-idm-1",
    label: "Divine IDM (1 conn)",
    available: () => existsSync(OUR_EXE),
    run: (url, dir, name) =>
      timeSpawn(OUR_EXE, [url, "-o", dir, "--name", name, "-n", "1", "--no-edge-pinning", "-q"]),
  },
  {
    name: "divine-idm-16",
    label: "Divine IDM (16 conn)",
    available: () => existsSync(OUR_EXE),
    run: (url, dir, name) =>
      timeSpawn(OUR_EXE, [url, "-o", dir, "--name", name, "-n", "16", "--no-edge-pinning", "-q"]),
  },
  {
    name: "curl",
    label: "curl (1 conn)",
    available: () => true,
    run: (url, dir, name) => timeSpawn("curl", ["-s", "-o", join(dir, name), url]),
  },
  {
    name: "aria2c",
    label: "aria2c (-x16)",
    available: () => existsSync(ARIA2),
    run: (url, dir, name) =>
      timeSpawn(ARIA2, [
        "-x", "16", "-s", "16", "-k", "1M",
        "--console-log-level=error", "--summary-interval=0",
        "-d", dir, "-o", name, url,
      ]),
  },
  {
    name: "idm",
    label: "IDM 6.43 (its own default)",
    available: () => existsSync(IDM_EXE),
    run: timeIdm,
  },
];

function median(values) {
  const sorted = [...values].sort((a, b) => a - b);
  return sorted[Math.floor(sorted.length / 2)];
}

function rate(bytes, ms) {
  return bytes / (ms / 1000);
}

function human(bytesPerSec) {
  return `${(bytesPerSec / 1e6).toFixed(1)} MB/s`;
}

async function main() {
  const expected = expectedDigest(SIZE);
  const { child, base } = await startServer();
  const url = `${base}/payload.bin`;
  const dir = join(tmpdir(), `idm-bench-${process.pid}`);

  const shaping =
    MODE === "total"
      ? `one shared ${(BPS / 1e6).toFixed(0)} MB/s bucket for all connections (per-IP shaped)`
      : `${(BPS / 1e6).toFixed(0)} MB/s per connection (per-connection shaped)`;

  console.log(`\nDivine IDM benchmark`);
  console.log(`  file     ${(SIZE / 1048576).toFixed(0)} MiB`);
  console.log(`  shaping  ${shaping}`);
  console.log(`  latency  ${RTT} ms round trip`);
  console.log(`  runs     ${RUNS} per tool, median reported`);
  console.log(`  sha256   ${expected.slice(0, 16)}...\n`);

  const results = [];
  for (const tool of TOOLS) {
    if (ONLY.length && !ONLY.includes(tool.name)) continue;
    if (!tool.available()) {
      results.push({ label: tool.label, skipped: "not installed" });
      continue;
    }

    const times = [];
    let failure = null;
    for (let run = 0; run < RUNS; run += 1) {
      rmSync(dir, { recursive: true, force: true });
      mkdirSync(dir, { recursive: true });
      process.stdout.write(`  ${tool.label} run ${run + 1}/${RUNS} ... `);

      const outcome = await tool.run(url, dir, "payload.bin");
      const path = join(dir, "payload.bin");

      if (outcome.status !== 0 || !existsSync(path)) {
        failure = outcome.stderr.split("\n")[0] || `exit ${outcome.status}`;
        console.log(`FAILED (${failure})`);
        break;
      }
      const actual = await digestOf(path);
      if (actual !== expected) {
        failure = "WRONG BYTES";
        console.log(`WRONG BYTES — produced a corrupt file`);
        break;
      }
      times.push(outcome.elapsed);
      console.log(`${(outcome.elapsed / 1000).toFixed(2)}s  ${human(rate(SIZE, outcome.elapsed))}`);
    }

    results.push(
      failure
        ? { label: tool.label, failed: failure }
        : { label: tool.label, ms: median(times), times },
    );
  }

  rmSync(dir, { recursive: true, force: true });
  child.kill();

  const stats = await fetch(`${base}/_stats`).then((r) => r.json()).catch(() => null);

  console.log(`\n${"Tool".padEnd(28)}${"Median".padEnd(10)}${"Throughput".padEnd(14)}vs curl`);
  console.log("-".repeat(66));
  const baseline = results.find((r) => r.label.startsWith("curl"))?.ms;
  for (const result of results) {
    if (result.skipped) {
      console.log(`${result.label.padEnd(28)}${"-".padEnd(10)}${result.skipped}`);
      continue;
    }
    if (result.failed) {
      console.log(`${result.label.padEnd(28)}${"-".padEnd(10)}${result.failed}`);
      continue;
    }
    const speedup = baseline ? `${(baseline / result.ms).toFixed(2)}x` : "-";
    console.log(
      result.label.padEnd(28) +
        `${(result.ms / 1000).toFixed(2)}s`.padEnd(10) +
        human(rate(SIZE, result.ms)).padEnd(14) +
        speedup,
    );
  }
  if (stats) {
    console.log(`\nserver saw ${stats.totalRequests} requests, peak ${stats.peakConnections} concurrent`);
  }
  console.log();
}

main().catch((error) => {
  console.error(error);
  process.exit(1);
});
