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

/**
 * Times our own CLI, and keeps what it says about itself.
 *
 * `--json` has existed since this benchmark was written -- its doc comment
 * in idm-cli even says it is for this -- and the harness was running `-q`
 * and throwing the information away. The settle reason and the steal count
 * are what tell a scheduler that stopped early apart from one that never
 * found the work, and no amount of staring at a throughput figure
 * distinguishes those.
 */
function timeOurs(args) {
  const started = process.hrtime.bigint();
  const result = spawnSync(OUR_EXE, args, { encoding: "utf8" });
  const elapsed = Number(process.hrtime.bigint() - started) / 1e6;

  let said = null;
  try {
    // One line of JSON on stdout; anything else means it failed before
    // printing, and the stderr is the interesting part then.
    said = JSON.parse((result.stdout ?? "").trim().split("\n").pop());
  } catch {
    said = null;
  }
  return {
    elapsed,
    status: result.status,
    stderr: result.stderr ?? "",
    // What the engine thought it was doing, for the table and the record.
    settled: said?.settled ?? null,
    connections: said?.connections ?? null,
    steals: said?.steals ?? null,
    hedges: said?.hedges ?? null,
    retries: said?.retries ?? null,
    writeMode: said?.write_mode ?? null,
  };
}

const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

const TOOLS = [
  {
    name: "divine-idm",
    label: "DivinePulse (adaptive)",
    available: () => existsSync(OUR_EXE),
    run: (url, dir, name) =>
      timeOurs([url, "-o", dir, "--name", name, "--no-edge-pinning", "--json"]),
  },
  {
    name: "divine-idm-1",
    label: "DivinePulse (1 conn)",
    available: () => existsSync(OUR_EXE),
    run: (url, dir, name) =>
      timeOurs([url, "-o", dir, "--name", name, "-n", "1", "--no-edge-pinning", "--json"]),
  },
  {
    name: "divine-idm-16",
    label: "DivinePulse (16 conn)",
    available: () => existsSync(OUR_EXE),
    run: (url, dir, name) =>
      timeOurs([url, "-o", dir, "--name", name, "-n", "16", "--no-edge-pinning", "--json"]),
  },
  {
    name: "curl",
    label: "curl (1 conn)",
    available: () => true,
    run: (url, dir, name) => timeSpawn("curl", ["-s", "-o", join(dir, name), url]),
  },
  {
    name: "ghd",
    label: "Ghost Downloader (ghd)",
    // `ghd` ships in the portable build's bin/ but is not put on PATH, so
    // GHD_EXE is how a machine says where its copy is. Absent, the row
    // says "not installed" rather than vanishing.
    available: () => Boolean(ghdExe()),
    run: timeGhd,
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

/** Where this machine's `ghd` is, or nothing. */
function ghdExe() {
  const named = process.env.GHD_EXE;
  if (named && existsSync(named)) return named;
  return onPath("ghd") ? "ghd" : null;
}

/** Task ids added during this run, so the app is left as it was found. */
const ghdTasks = [];
const NEWLINE = "\n";

/**
 * Times Ghost Downloader, which is two calls rather than one.
 *
 * `add` queues the task, starts it and returns at once with a task id;
 * `wait` blocks until it stops and exits non-zero unless it completed. So
 * the clock has to span both, and the pair has to be spawned back to back
 * -- anything between them is time the download is already spending.
 *
 * Both speak JSON on stdout, which is how the task id gets from one to the
 * other and how a failure explains itself.
 */
function timeGhd(url, dir, name) {
  const exe = ghdExe();
  const started = process.hrtime.bigint();

  const added = spawnSync(
    exe,
    // `--conflict overwrite` because the app remembers tasks between runs,
    // and "keep both" would quietly write payload (1).bin on run two and
    // leave the digest check reading a stale file.
    ["add", url, "-o", dir, "--name", name, "--conflict", "overwrite"],
    { encoding: "utf8" },
  );
  if (added.status !== 0) {
    return { elapsed: 0, status: added.status ?? 1, stderr: added.stdout || added.stderr || "" };
  }

  let taskId;
  try {
    taskId = JSON.parse(added.stdout).result[0].taskId;
  } catch {
    return { elapsed: 0, status: 1, stderr: `could not read a task id from ${added.stdout}` };
  }
  ghdTasks.push(taskId);

  const waited = spawnSync(exe, ["wait", taskId], { encoding: "utf8" });
  const elapsed = Number(process.hrtime.bigint() - started) / 1e6;
  return {
    elapsed,
    status: waited.status ?? 1,
    stderr: waited.status === 0 ? "" : waited.stdout || waited.stderr || "",
  };
}

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
    let said = null;
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
      // Only our own tool reports these; for the others they stay null.
      said = outcome.settled ? outcome : said;
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
            // What the engine said about its own run, where it says
            // anything: the settle reason is the difference between a
            // scheduler that stopped on purpose and one that ran out of
            // work to hand out.
            settled: said?.settled ?? null,
            connectionsSettledAt: said?.connections ?? null,
            steals: said?.steals ?? null,
            hedges: said?.hedges ?? null,
            retries: said?.retries ?? null,
            writeMode: said?.writeMode ?? null,
          },
    );
  }

  rmSync(dir, { recursive: true, force: true });
  // Ghost Downloader keeps its task list across runs, so a benchmark that
  // did not tidy up would leave dozens of finished rows in somebody's app.
  // After the directory is gone, so "unfinished files are always deleted"
  // has nothing left to delete.
  //
  // One id the app will not remove fails the whole batch, and once left
  // behind these are somebody's to clear by hand -- so they go one at a
  // time, and anything that survives is named rather than left as a
  // surprise in the app.
  const stubborn = [];
  for (const task of ghdTasks) {
    const gone = spawnSync(ghdExe(), ["remove", task], { stdio: "ignore" });
    if (gone.status !== 0) stubborn.push(task);
  }
  if (stubborn.length) {
    console.log(
      `${NEWLINE}could not remove ${stubborn.length} task(s) from Ghost Downloader:` +
        `${NEWLINE}  ghd remove ${stubborn.join(" ")}`,
    );
  }
  child.kill();

  const stats = await fetch(`${base}/_stats`).then((r) => r.json()).catch(() => null);

  console.log(
    `\n${"Tool".padEnd(28)}${"Median".padEnd(10)}${"Throughput".padEnd(14)}` +
      `${"vs curl".padEnd(9)}${"reqs".padEnd(7)}${"peak".padEnd(7)}${"steals".padEnd(7)}settled / bound by`,
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
        String(result.steals ?? "-").padEnd(7) +
        (result.settled ?? result.bound),
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
