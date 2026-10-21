// A bandwidth-shaped HTTP server, so multi-connection downloading can be
// measured rather than asserted.
//
// Benchmarking against an unshaped localhost server measures the server, not
// the downloader: with no latency and effectively infinite bandwidth, one
// connection saturates and extra connections can only add overhead. Real
// origins are the opposite — they cap each connection and sit tens of
// milliseconds away — which is precisely the situation parallel downloading
// exists for.
//
// Two shaping modes, because they are the two that matter and they reward
// opposite strategies:
//
//   --bps-per-conn N   each response is capped independently, as a
//                      per-connection-shaped origin does. More connections
//                      genuinely multiply throughput here.
//   --bps-total N      all responses share one token bucket, as a per-IP
//                      shaped CDN does. More connections buy nothing, and a
//                      downloader that keeps adding them is wasting requests
//                      and inviting a 429.
//
//   node bench-server.mjs --port 0 --size 134217728 --bps-per-conn 8000000 --rtt 30

import { createServer } from "node:http";

function arg(name, fallback) {
  const index = process.argv.indexOf(`--${name}`);
  if (index === -1) return fallback;
  const value = Number(process.argv[index + 1]);
  return Number.isFinite(value) ? value : fallback;
}

const PORT = arg("port", 0);
const SIZE = arg("size", 128 * 1024 * 1024);
const BPS_PER_CONN = arg("bps-per-conn", 8_000_000);
const BPS_TOTAL = arg("bps-total", 0);
const RTT_MS = arg("rtt", 30);
/// Writes this often; smaller is smoother but costs more timer wakeups.
const TICK_MS = 20;

/** Deterministic byte for an absolute offset, matching the Rust verifier. */
function byteAt(offset) {
  let x = (offset + 0x9e3779b9) | 0;
  x = Math.imul(x ^ (x >>> 16), 0x85ebca6b);
  x = Math.imul(x ^ (x >>> 13), 0xc2b2ae35);
  x = x ^ (x >>> 16);
  return x & 0xff;
}

// The whole file is generated once at startup and served as slices.
//
// Generating bytes per tick in JavaScript made the *server* the bottleneck at
// roughly 55 MB/s, so a benchmark above that rate was measuring this process
// rather than the downloader under test -- and every tool converged on the
// same number for the wrong reason.
let CONTENT = null;
function buildContent(size) {
  const buffer = Buffer.allocUnsafe(size);
  for (let i = 0; i < size; i += 1) buffer[i] = byteAt(i);
  return buffer;
}

function bodyFor(start, end) {
  return CONTENT.subarray(start, Math.max(start, end));
}

/** Inclusive HTTP Range -> half-open [start, end). */
function parseRange(header, total) {
  if (!header) return null;
  const match = /^bytes=(\d*)-(\d*)$/.exec(header.trim());
  if (!match) return "unsatisfiable";
  const [, firstText, lastText] = match;
  if (firstText === "" && lastText === "") return "unsatisfiable";
  if (firstText === "") {
    const suffix = Number(lastText);
    if (suffix === 0) return "unsatisfiable";
    return { start: Math.max(0, total - suffix), end: total };
  }
  const start = Number(firstText);
  if (start >= total) return "unsatisfiable";
  const end = lastText === "" ? total : Math.min(total, Number(lastText) + 1);
  return end <= start ? "unsatisfiable" : { start, end };
}

// A shared token bucket, used only in --bps-total mode.
let sharedTokens = 0;
let lastRefill = Date.now();
function takeShared(wanted) {
  const now = Date.now();
  sharedTokens += ((now - lastRefill) / 1000) * BPS_TOTAL;
  sharedTokens = Math.min(sharedTokens, BPS_TOTAL); // at most one second of burst
  lastRefill = now;
  const given = Math.min(wanted, Math.floor(sharedTokens));
  sharedTokens -= given;
  return given;
}

let activeConnections = 0;
let peakConnections = 0;
let totalRequests = 0;
let bytesServed = 0;

const server = createServer((req, res) => {
  if (req.url.startsWith("/_stats")) {
    const payload = JSON.stringify({
      size: SIZE,
      bpsPerConn: BPS_PER_CONN,
      bpsTotal: BPS_TOTAL,
      rttMs: RTT_MS,
      totalRequests,
      peakConnections,
      bytesServed,
    });
    res.writeHead(200, {
      "Content-Type": "application/json",
      "Content-Length": String(Buffer.byteLength(payload)),
    });
    return res.end(payload);
  }
  if (req.url.startsWith("/_reset")) {
    totalRequests = 0;
    peakConnections = 0;
    bytesServed = 0;
    res.writeHead(204).end();
    return undefined;
  }

  totalRequests += 1;
  activeConnections += 1;
  peakConnections = Math.max(peakConnections, activeConnections);

  const range = parseRange(req.headers.range, SIZE);
  if (range === "unsatisfiable") {
    activeConnections -= 1;
    res.writeHead(416, { "Content-Range": `bytes */${SIZE}`, "Content-Length": "0" });
    return res.end();
  }
  const span = range ?? { start: 0, end: SIZE };

  // Half the round trip before the headers, half conceptually on the way back.
  setTimeout(() => {
    const headers = {
      "Accept-Ranges": "bytes",
      "Content-Type": "application/octet-stream",
      ETag: `"bench-${SIZE}"`,
      "Last-Modified": new Date(Date.UTC(2020, 0, 1)).toUTCString(),
      "Content-Length": String(span.end - span.start),
    };
    if (range) headers["Content-Range"] = `bytes ${span.start}-${span.end - 1}/${SIZE}`;
    res.writeHead(range ? 206 : 200, headers);

    let offset = span.start;
    const perTick = Math.max(1, Math.floor((BPS_PER_CONN * TICK_MS) / 1000));

    const tick = setInterval(() => {
      if (offset >= span.end || res.writableEnded || res.destroyed) {
        clearInterval(tick);
        if (!res.writableEnded) res.end();
        return;
      }
      let want = Math.min(perTick, span.end - offset);
      if (BPS_TOTAL > 0) {
        want = takeShared(want);
        if (want <= 0) return; // bucket empty this tick
      }
      const next = offset + want;
      res.write(bodyFor(offset, next));
      bytesServed += want;
      offset = next;
    }, TICK_MS);

    const done = () => {
      clearInterval(tick);
      activeConnections = Math.max(0, activeConnections - 1);
    };
    res.on("close", done);
    res.on("finish", done);
  }, RTT_MS / 2);
});

CONTENT = buildContent(SIZE);

server.listen(PORT, "127.0.0.1", () => {
  const mode = BPS_TOTAL > 0 ? `total ${BPS_TOTAL} B/s` : `per-conn ${BPS_PER_CONN} B/s`;
  console.log(
    `bench-server listening on http://127.0.0.1:${server.address().port} ` +
      `size=${SIZE} ${mode} rtt=${RTT_MS}ms`,
  );
});
