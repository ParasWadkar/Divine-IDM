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
//   node bench.mjs --tools divine-idm,curl
//   node bench.mjs --json results/x.json    # also write the numbers out
//
// # What a number here measures
//
// Every run reports a throughput, and a throughput is only interesting
// once you know which of two ceilings it is under. So the run begins by
// writing the same number of bytes straight to the same folder, with no
// network at all, and prints that as the disk ceiling. A tool near it is
// disk-bound and its scheduler is not being measured; a tool well under it
// is scheduler-bound, which is the thing this benchmark exists to compare.
// Every row is labelled with which it is.
//
// # Tools that are not here
//
// **Internet Download Manager** is compared on features only and is
// deliberately not installed. It is paid and closed, and its launcher hands
// the job to a background process and returns at once -- so timing it means
// watching a file grow, with a licence dialog as a failure mode. A number
// obtained that way would not be comparable with tools whose process exits
// when the file is done.
//
// **Ghost Downloader 3** is wired up below and is measured the moment its
// `ghd` command is on PATH. It is not installed here because `ghd` is not a
// download: it is installed from inside the application, through
// Settings -> Command Line Tool. That means installing a GUI program and
// clicking through it, which is a larger thing to do to somebody's machine
// than unzipping a portable binary, so it waits to be asked for.

import { createHash } from "node:crypto";
import { spawn, spawnSync } from "node:child_process";
import {
  closeSync,
  existsSync,
  mkdirSync,
  openSync,
  rmSync,
  writeFileSync,
  writeSync,
} from "node:fs";
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
const JSON_OUT = arg("json", "");

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

const TOOLS = [
  {
    name: "divine-idm",
    label: "DivinePulse (adaptive)",
    available: () => existsSync(OUR_EXE),
    run: (url, dir, name) =>
      timeSpawn(OUR_EXE, [url, "-o", dir, "--name", name, "--no-edge-pinning", "-q"]),
  },
  {
    name: "divine-idm-1",
    label: "DivinePulse (1 conn)",
    available: () => existsSync(OUR_EXE),
    run: (url, dir, name) =>
      timeSpawn(OUR_EXE, [url, "-o", dir, "--name", name, "-n", "1", "--no-edge-pinning", "-q"]),
  },
  {
    name: "divine-idm-16",
    label: "DivinePulse (16 conn)",
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
    name: "ghd",
    label: "Ghost Downloader 3",
    // On PATH only: `ghd` is installed from inside the application, so its
    // presence is a decision somebody made rather than something this
    // harness can arrange. Absent, the row says so instead of vanishing.
    available: () => onPath("ghd"),
    run: (url, dir, name) =>
      // `add` queues and returns, so the timing has to wait for the task.
      // `wait` is what the documented CLI offers for exactly that.
      timeSpawn("ghd", ["add", url, "--dir", dir, "--name", name, "--wait"]),
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
];

/** Whether a command can be found, without running anything of substance. */
function onPath(command) {
  const probe = spawnSync(command, ["--version"], { stdio: "ignore" });
  return !probe.error;
}

/**
 * How fast this machine takes the same number of bytes, with no network.
 *
 * The ceiling every row below sits under, and getting it wrong makes the
 * whole "bound by" column a lie -- which it was, twice, before this
 * settled down:
 *
 * **No `fsync`.** The first version flushed, and then curl "beat the
 * disk" by three times. Of course it did: none of these downloaders
 * fsync either, so they are racing the page cache and the ceiling was
 * racing the platter. The two have to be measured the same way, and the
 * way that matches the thing being measured is the unflushed one. So this
 * is really "how fast bytes can be handed to the file system", which is
 * the honest upper bound for a writer that does not flush.
 *
 * **A median of several.** One pass varied by six times run to run on a
 * nearly-full disk. The spread is returned as well as the figure, because
 * a ceiling that moves that much cannot label anything and the run should
 * say so rather than print a confident word.
 */
function diskCeiling(dir) {
  const path = join(dir, "ceiling.bin");
  const chunk = Buffer.allocUnsafe(1 << 20);
  for (let i = 0; i < chunk.length; i += 1) chunk[i] = byteAt(i);

  const passes = [];
  for (let pass = 0; pass < 3; pass += 1) {
    const started = process.hrtime.bigint();
    const handle = openSync(path, "w");
    try {
      for (let written = 0; written < SIZE; written += chunk.length) {
        writeSync(handle, chunk, 0, Math.min(chunk.length, SIZE - written));
      }
    } finally {
      closeSync(handle);
    }
    passes.push(Number(process.hrtime.bigint() - started) / 1e6);
    rmSync(path, { force: true });
  }

  const rates = passes.map((ms) => rate(SIZE, ms)).sort((a, b) => a - b);
  return { rate: rates[1], lowest: rates[0], highest: rates[2] };
}

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

  console.log(`\nDivinePulse benchmark`);
  console.log(`  file     ${(SIZE / 1048576).toFixed(0)} MiB`);
  console.log(`  shaping  ${shaping}`);
  console.log(`  latency  ${RTT} ms round trip`);
  console.log(`  runs     ${RUNS} per tool, median reported`);
  console.log(`  sha256   ${expected.slice(0, 16)}...`);

  // Measured before anything is downloaded, so the table below can say
  // which ceiling each row is under rather than leaving it to be guessed.
  mkdirSync(dir, { recursive: true });
  const measured = diskCeiling(dir);
  const ceiling = measured.rate;
  // More than a two-fold spread and the figure is not a ceiling, it is
  // a mood. Said out loud, because the column below leans on it.
  const steady = measured.highest < measured.lowest * 2;
  console.log(
    `  disk     ${human(ceiling)} handing the same bytes to the file system` +
      ` (${human(measured.lowest)}-${human(measured.highest)})`,
  );
  if (!steady) {
    console.log("           that spread is too wide to call any row disk-bound");
  }
  console.log();

  const results = [];
  for (const tool of TOOLS) {
    if (ONLY.length && !ONLY.includes(tool.name)) continue;
    if (!tool.available()) {
      results.push({ label: tool.label, skipped: "not installed" });
      continue;
    }

    // Per tool, not cumulative: how many requests a downloader made to
    // fetch one file is the number that separates discipline from
    // flailing, and it only means anything if it belongs to one tool.
    await fetch(`${base}/_reset`).catch(() => {});

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

    const used = await fetch(`${base}/_stats`)
      .then((reply) => reply.json())
      .catch(() => null);
    const asked = used ? Math.round(used.totalRequests / Math.max(times.length, 1)) : null;

    results.push(
      failure
        ? { label: tool.label, failed: failure }
        : {
            label: tool.label,
            ms: median(times),
            times,
            // Per run, since the counter spans every run of this tool.
            requestsPerRun: asked,
            peakConnections: used?.peakConnections ?? null,
          },
    );
  }

  rmSync(dir, { recursive: true, force: true });
  child.kill();

  const stats = await fetch(`${base}/_stats`).then((r) => r.json()).catch(() => null);

  console.log(
    `\n${"Tool".padEnd(28)}${"Median".padEnd(10)}${"Throughput".padEnd(14)}` +
      `${"vs curl".padEnd(9)}${"reqs".padEnd(7)}${"peak".padEnd(7)}bound by`,
  );
  console.log("-".repeat(100));
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
    const achieved = rate(SIZE, result.ms);
    // The whole point of measuring the disk first. Within a tenth of the
    // disk's own rate, this number is the disk and says nothing about the
    // downloader; well under it, the difference is the scheduler, which is
    // what was being compared.
    result.bound = !steady
      ? "unclear"
      : achieved >= ceiling * 0.9
        ? "the disk"
        : "the scheduler";
    result.bytesPerSec = achieved;
    console.log(
      result.label.padEnd(28) +
        `${(result.ms / 1000).toFixed(2)}s`.padEnd(10) +
        human(achieved).padEnd(14) +
        speedup.padEnd(9) +
        String(result.requestsPerRun ?? "-").padEnd(7) +
        String(result.peakConnections ?? "-").padEnd(7) +
        result.bound,
    );
  }
  console.log(
    `\n"the disk" means the row is within a tenth of ${human(ceiling)} and says nothing` +
      `\nabout the downloader. "the scheduler" means it is well under that, so the` +
      `\nnumber is how many connections the tool opened and how it spread the work.`,
  );

  if (stats) {
    console.log(`\nserver saw ${stats.totalRequests} requests, peak ${stats.peakConnections} concurrent`);
  }

  if (JSON_OUT) {
    // Written with everything needed to read the numbers back months
    // later: a throughput with no shaping, size or disk ceiling beside it
    // is a number nobody can check or repeat.
    const record = {
      measuredAt: new Date().toISOString(),
      sizeBytes: SIZE,
      shaping: { mode: MODE, bytesPerSec: BPS, rttMs: RTT },
      runs: RUNS,
      sha256: expected,
      diskCeiling: { ...measured, steady },
      server: stats,
      tools: results,
    };
    writeFileSync(JSON_OUT, `${JSON.stringify(record, null, 2)}\n`);
    console.log(`\nwritten to ${JSON_OUT}`);
  }
  console.log();
}

main().catch((error) => {
  console.error(error);
  process.exit(1);
});
