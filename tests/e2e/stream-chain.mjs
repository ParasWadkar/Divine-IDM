// Downloads an HLS stream end to end and checks the result byte for byte.
//
// The mock server's segments are slices of the same generated sequence every
// other route serves, so a correctly joined stream is bit-identical to an
// ordinary download of the same length. That turns "did the segments go back
// together in the right order" into an exact question rather than a visual
// one — which matters, because segments joined out of order produce a file
// that plays and is wrong.
//
//   node stream-chain.mjs

import { spawn, spawnSync } from "node:child_process";
import { existsSync, mkdtempSync, readFileSync, readdirSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const HERE = dirname(fileURLToPath(import.meta.url));
const REPO = resolve(HERE, "..", "..");
const IDM = join(REPO, "target", "release", "idm.exe");
const MOCK = join(REPO, "tests", "mock-server", "server.mjs");
const PORT = 8171;

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
let failures = 0;

function check(label, ok, detail = "") {
  console.log(`  ${ok ? "PASS" : "FAIL"}  ${label}${detail ? "  — " + detail : ""}`);
  if (!ok) failures += 1;
  return ok;
}

/** The byte the mock server serves at a given offset. */
function byteAt(offset) {
  let x = (offset + 0x9e3779b9) | 0;
  x = Math.imul(x ^ (x >>> 16), 0x85ebca6b);
  x = Math.imul(x ^ (x >>> 13), 0xc2b2ae35);
  x = x ^ (x >>> 16);
  return x & 0xff;
}

function run(url, out, extra = []) {
  return spawnSync(IDM, [url, "-o", out, ...extra], { encoding: "utf8" });
}

function onlyFile(dir) {
  const files = readdirSync(dir);
  return files.length === 1 ? join(dir, files[0]) : null;
}

async function main() {
  if (!existsSync(IDM)) {
    console.error("missing idm.exe; run: cargo build --release --workspace");
    process.exit(1);
  }

  console.log("Divine IDM — HLS stream\n");
  const mock = spawn("node", [MOCK, "--port", String(PORT)], {
    stdio: ["ignore", "pipe", "ignore"],
  });
  await sleep(2000);

  const out = mkdtempSync(join(tmpdir(), "idm-stream-"));

  try {
    // 1. A master playlist: pick a rendition, fetch it, join it.
    const result = run(`http://127.0.0.1:${PORT}/hls/master.m3u8?segments=8&size=65536`, out);
    const said = (result.stderr || result.stdout).trim();
    check("a master playlist downloads", result.status === 0, said.split("\n")[0]);
    check(
      "the largest rendition is taken by default",
      /1080p/.test(said),
      said.split("\n")[0],
    );

    const file = onlyFile(out);
    check("one file comes out", Boolean(file), readdirSync(out).join(", "));

    if (file) {
      const got = readFileSync(file);
      check("it is the right length", got.length === 8 * 65536, `${got.length} bytes`);

      let bad = 0;
      for (let i = 0; i < got.length && bad === 0; i += 1) {
        if (got[i] !== byteAt(i)) bad = i + 1;
      }
      // Segments joined in the wrong order still produce a plausible file.
      check(
        "the segments are joined in the right order, byte for byte",
        bad === 0,
        bad ? `first wrong byte at ${bad - 1}` : "",
      );
    }

    // 2. A height limit picks a smaller rendition.
    const smaller = mkdtempSync(join(tmpdir(), "idm-stream-"));
    const limited = run(
      `http://127.0.0.1:${PORT}/hls/master.m3u8?segments=4&size=32768`,
      smaller,
      ["--max-height", "720"],
    );
    const limitedSaid = (limited.stderr || limited.stdout).trim();
    check("a height limit is honoured", /720p/.test(limitedSaid), limitedSaid.split("\n")[0]);
    rmSync(smaller, { recursive: true, force: true });

    // 3. A media playlist on its own, with no master above it.
    const direct = mkdtempSync(join(tmpdir(), "idm-stream-"));
    const alone = run(
      `http://127.0.0.1:${PORT}/hls/720/index.m3u8?segments=3&size=16384`,
      direct,
    );
    const file3 = onlyFile(direct);
    check(
      "a media playlist works without a master",
      alone.status === 0 && Boolean(file3) && readFileSync(file3).length === 3 * 16384,
      (alone.stderr || alone.stdout).trim().split("\n")[0],
    );
    rmSync(direct, { recursive: true, force: true });

    // 4. Without ffmpeg the download is still delivered, and says so.
    //    Discarding work already done because a muxer is missing would be
    //    the wrong trade.
    check(
      "a missing ffmpeg is reported rather than fatal",
      result.status === 0 && /ffmpeg was not found/.test(said),
      /ffmpeg/.test(said) ? "noted" : "no note (ffmpeg may be installed)",
    );
    // 5. AES-128 encrypted streams. Each segment is encrypted with its own
    //    initialisation vector, derived from its media sequence number, so a
    //    stream decrypted with one vector throughout gives a correct first
    //    segment and ruins every one after — which reads as success unless
    //    the bytes are checked.
    for (const [label, query, segments, size] of [
      ["an encrypted stream decrypts", "", 6, 32768],
      ["a stated initialisation vector is used", "&iv=stated", 4, 16384],
      ["a media sequence offset is carried into the vector", "&first=9000", 4, 16384],
    ]) {
      const dir = mkdtempSync(join(tmpdir(), "idm-stream-"));
      const got = run(
        `http://127.0.0.1:${PORT}/hls-aes/index.m3u8?segments=${segments}&size=${size}${query}`,
        dir,
      );
      const file = onlyFile(dir);
      let verdict = "no file";
      let ok = false;

      if (got.status === 0 && file) {
        const body = readFileSync(file);
        let bad = -1;
        for (let i = 0; i < body.length; i += 1) {
          if (body[i] !== byteAt(i)) {
            bad = i;
            break;
          }
        }
        ok = bad < 0 && body.length === segments * size;
        verdict = bad < 0 ? `${body.length} bytes, all correct` : `wrong at byte ${bad}`;
      } else {
        verdict = (got.stderr || got.stdout).trim().split("\n").pop() ?? "failed";
      }

      check(label, ok, verdict);
      rmSync(dir, { recursive: true, force: true });
    }

  } finally {
    mock.kill();
    await sleep(500);
    rmSync(out, { recursive: true, force: true, maxRetries: 3 });
  }

  console.log(`\n${failures ? failures + " check(s) failed" : "all checks passed"}`);
  process.exit(failures ? 1 : 0);
}

main().catch((error) => {
  console.error(error);
  process.exit(1);
});
