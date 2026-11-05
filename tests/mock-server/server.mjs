// A deliberately misbehaving HTTP server, one route per pathology from the
// engine's "servers that lie" table.
//
// Deliberately written in Node rather than Rust: the thing under test is the
// Rust engine, and a test oracle that shares no code with it is worth far more
// than one that reuses the same range-parsing helpers.
//
// Body bytes are a pure function of their absolute offset (see byteAt), so a
// client can verify any range it receives without the server storing a file,
// and a misordered or misplaced write shows up immediately as a content
// mismatch rather than merely a wrong length.
//
//   node server.mjs [--port 8080]
//
// GET /_routes for the machine-readable route list.

import { createServer } from "node:http";
import { createCipheriv, createHash, randomBytes } from "node:crypto";
import { gzipSync } from "node:zlib";

const DEFAULT_PORT = 8080;
const MIB = 1024 * 1024;

/** Deterministic byte for an absolute file offset (murmur-style finalizer). */
function byteAt(offset) {
  let x = (offset + 0x9e3779b9) | 0;
  x = Math.imul(x ^ (x >>> 16), 0x85ebca6b);
  x = Math.imul(x ^ (x >>> 13), 0xc2b2ae35);
  x = x ^ (x >>> 16);
  return x & 0xff;
}

/**
 * What `/robots.txt` says, or `null` for a 404.
 *
 * Set by `/_robots`. State in a mock server is usually a mistake, but each
 * test spawns its own, so there is nobody to race with.
 */
let robotsText = null;

/** The key the encrypted HLS routes use. Fixed, so tests are repeatable. */
const HLS_KEY = Buffer.from("0123456789abcdef0123456789abcdef", "hex");

/** An explicit IV, for the playlist variant that states one. */
const STATED_IV = Buffer.from("00112233445566778899aabbccddeeff", "hex");

/**
 * The IV HLS derives from a media sequence number: the number as a
 * 16-octet big-endian value, per RFC 8216.
 */
function ivForSequence(sequence) {
  const iv = Buffer.alloc(16);
  iv.writeBigUInt64BE(BigInt(sequence), 8);
  return iv;
}

/** Bytes for the half-open range [start, end). */
function bodyFor(start, end) {
  const buffer = Buffer.allocUnsafe(Math.max(0, end - start));
  for (let i = 0; i < buffer.length; i += 1) {
    buffer[i] = byteAt(start + i);
  }
  return buffer;
}

/**
 * Parses a single-range inclusive HTTP Range header into a half-open range.
 * Returns null when absent, and "unsatisfiable" when it cannot be met.
 */
function parseRange(header, total) {
  if (!header) return null;
  const match = /^bytes=(\d*)-(\d*)$/.exec(header.trim());
  if (!match) return "unsatisfiable";

  const [, firstText, lastText] = match;
  if (firstText === "" && lastText === "") return "unsatisfiable";

  // Suffix form: "bytes=-N" means the final N bytes.
  if (firstText === "") {
    const suffixLength = Number(lastText);
    if (suffixLength === 0) return "unsatisfiable";
    const start = Math.max(0, total - suffixLength);
    return { start, end: total };
  }

  const start = Number(firstText);
  if (start >= total) return "unsatisfiable";
  // `last` is inclusive on the wire; +1 converts to half-open.
  const end = lastText === "" ? total : Math.min(total, Number(lastText) + 1);
  if (end <= start) return "unsatisfiable";
  return { start, end };
}

function sizeFromQuery(url, fallback = 8 * MIB) {
  const raw = url.searchParams.get("size");
  const parsed = raw === null ? Number.NaN : Number(raw);
  return Number.isFinite(parsed) && parsed > 0 ? Math.floor(parsed) : fallback;
}

/** Per-route, per-client counters so a route can misbehave only after N hits. */
const hitCounts = new Map();

// ---------------------------------------------------------------------------
// A real zip, built here
// ---------------------------------------------------------------------------
//
// Built rather than kept as a fixture so the shape can be varied per
// request -- how many files, how large, how long a comment -- and so the
// bytes are visible in this file rather than opaque in the repository.
//
// Everything is STORED rather than deflated. The engine only reads the
// index, so compression would add a dependency and prove nothing; and a
// stored archive is larger than its contents, which is what a test about
// reading only the tail needs.

const CRC_TABLE = (() => {
  const table = new Int32Array(256);
  for (let n = 0; n < 256; n += 1) {
    let c = n;
    for (let k = 0; k < 8; k += 1) c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
    table[n] = c;
  }
  return table;
})();

function crc32(buffer) {
  let c = 0 ^ -1;
  for (let i = 0; i < buffer.length; i += 1) {
    c = (c >>> 8) ^ CRC_TABLE[(c ^ buffer[i]) & 0xff];
  }
  return (c ^ -1) >>> 0;
}

/**
 * A valid STORED zip holding `files`, which are `[name, size]` pairs.
 *
 * Real enough that any zip tool opens it: correct CRCs, correct offsets,
 * a proper central directory and end record.
 */
function buildZip(files, comment = "") {
  const locals = [];
  const central = [];
  let at = 0;

  for (const [name, size] of files) {
    const nameBytes = Buffer.from(name, "utf8");
    const data = Buffer.alloc(size, "a");
    const crc = crc32(data);

    const local = Buffer.alloc(30);
    local.writeUInt32LE(0x04034b50, 0);
    local.writeUInt16LE(20, 4); // version needed
    local.writeUInt16LE(0x0800, 6); // UTF-8 names
    local.writeUInt16LE(0, 8); // stored
    local.writeUInt16LE(0, 10); // time
    local.writeUInt16LE(0, 12); // date
    local.writeUInt32LE(crc, 14);
    local.writeUInt32LE(size, 18);
    local.writeUInt32LE(size, 22);
    local.writeUInt16LE(nameBytes.length, 26);
    local.writeUInt16LE(0, 28);
    locals.push(local, nameBytes, data);

    const entry = Buffer.alloc(46);
    entry.writeUInt32LE(0x02014b50, 0);
    entry.writeUInt16LE(20, 4); // made by
    entry.writeUInt16LE(20, 6); // needed
    entry.writeUInt16LE(0x0800, 8); // UTF-8 names
    entry.writeUInt16LE(0, 10); // stored
    entry.writeUInt16LE(0, 12);
    entry.writeUInt16LE(0, 14);
    entry.writeUInt32LE(crc, 16);
    entry.writeUInt32LE(size, 20);
    entry.writeUInt32LE(size, 24);
    entry.writeUInt16LE(nameBytes.length, 28);
    entry.writeUInt16LE(0, 30); // extra
    entry.writeUInt16LE(0, 32); // comment
    entry.writeUInt16LE(0, 34); // disk
    entry.writeUInt16LE(0, 36); // internal
    entry.writeUInt32LE(0, 38); // external
    entry.writeUInt32LE(at, 42); // where its local header is
    central.push(entry, nameBytes);

    at += local.length + nameBytes.length + data.length;
  }

  const body = Buffer.concat(locals);
  const directory = Buffer.concat(central);
  const commentBytes = Buffer.from(comment, "utf8");

  const end = Buffer.alloc(22);
  end.writeUInt32LE(0x06054b50, 0);
  end.writeUInt16LE(0, 4);
  end.writeUInt16LE(0, 6);
  end.writeUInt16LE(files.length, 8);
  end.writeUInt16LE(files.length, 10);
  end.writeUInt32LE(directory.length, 12);
  end.writeUInt32LE(body.length, 16);
  end.writeUInt16LE(commentBytes.length, 20);

  return Buffer.concat([body, directory, end, commentBytes]);
}



// ---------------------------------------------------------------------------
// Signing in
// ---------------------------------------------------------------------------
//
// The credentials every auth route expects. Fixed rather than configurable
// because the tests assert on an exact Digest response, which is a hash of
// these; a knob here would only move that constant somewhere else.
const AUTH_USER = "mufasa";
const AUTH_PASS = "circle-of-life";
const AUTH_REALM = "downloads@example.org";

// Nonces this server has handed out and still accepts. A route can issue one
// already expired to exercise the `stale=true` path, which is what decides
// whether a download longer-lived than a nonce survives.
const liveNonces = new Map();

function mintNonce(stale = false) {
  const nonce = randomBytes(16).toString("hex");
  liveNonces.set(nonce, { stale, used: new Set() });
  return nonce;
}

function authHash(algorithm, text) {
  return createHash(algorithm === "SHA-256" ? "sha256" : "md5").update(text).digest("hex");
}

/** Parses an `Authorization` header's comma-separated parameters. */
function authParams(header) {
  const out = {};
  // Quoted values may contain commas, so the match respects quoting rather
  // than splitting on the comma.
  const pattern = /(\w+)=(?:"((?:[^"\\]|\\.)*)"|([^,\s]+))/g;
  let match;
  while ((match = pattern.exec(header)) !== null) {
    out[match[1].toLowerCase()] = (match[2] ?? match[3]).replace(/\\(.)/g, "$1");
  }
  return out;
}

/**
 * Checks a Digest `Authorization` against what this server would compute.
 *
 * Returns "ok", "stale" when the nonce has expired though the password was
 * right, or "no" otherwise. Keeping those apart is the whole point: a stale
 * nonce must not look like a wrong password.
 */
function checkDigest(req, algorithm, requireQop) {
  const header = req.headers.authorization;
  if (!header || !/^Digest /i.test(header)) return "no";
  const p = authParams(header);
  if (p.username !== AUTH_USER) return "no";
  if (!liveNonces.has(p.nonce)) return "no";

  const ha1 = authHash(algorithm, AUTH_USER + ":" + AUTH_REALM + ":" + AUTH_PASS);
  const ha2 = authHash(algorithm, req.method + ":" + p.uri);
  const expected = requireQop
    ? authHash(algorithm, [ha1, p.nonce, p.nc, p.cnonce, "auth", ha2].join(":"))
    : authHash(algorithm, [ha1, p.nonce, ha2].join(":"));
  if (p.response !== expected) return "no";

  if (requireQop) {
    // The nonce count must be unique per nonce. Repeats are exactly what a
    // per-lane counter would produce, so this is the check that catches it.
    const record = liveNonces.get(p.nonce);
    if (record.used.has(p.nc)) return "no";
    record.used.add(p.nc);
    if (record.stale) return "stale";
  }
  return "ok";
}

function sendChallenge(res, value) {
  res.writeHead(401, { "WWW-Authenticate": value, "Content-Length": "0" });
  res.end();
}

function basicExpected() {
  return "Basic " + Buffer.from(AUTH_USER + ":" + AUTH_PASS).toString("base64");
}


function bumpHits(key) {
  const next = (hitCounts.get(key) ?? 0) + 1;
  hitCounts.set(key, next);
  return next;
}

/**
 * Drops undefined-valued headers. Node throws ERR_HTTP_INVALID_HEADER_VALUE on
 * them, and routes rely on `Header: undefined` to *suppress* a header that
 * serveHonestly would otherwise set.
 */
function dropUndefined(headers) {
  return Object.fromEntries(
    Object.entries(headers).filter(([, value]) => value !== undefined),
  );
}

function sendUnsatisfiable(res, total) {
  res.writeHead(416, {
    "Content-Range": `bytes */${total}`,
    "Content-Length": "0",
  });
  res.end();
}

/**
 * The well-behaved baseline: strong ETag, honours Range, accurate lengths.
 *
 * `body` lets a route serve a different sequence at the same length, which is
 * what a mirror holding a different build looks like from outside.
 */
function serveHonestly(req, res, total, extraHeaders = {}, body = bodyFor) {
  const range = parseRange(req.headers.range, total);
  if (range === "unsatisfiable") return sendUnsatisfiable(res, total);

  const headers = dropUndefined({
    "Accept-Ranges": "bytes",
    "Content-Type": "application/octet-stream",
    ETag: `"honest-${total}"`,
    "Last-Modified": new Date(Date.UTC(2020, 0, 1)).toUTCString(),
    Date: new Date().toUTCString(),
    ...extraHeaders,
  });

  if (!range) {
    res.writeHead(200, { ...headers, "Content-Length": String(total) });
    if (req.method === "HEAD") return res.end();
    return res.end(body(0, total));
  }

  res.writeHead(206, {
    ...headers,
    "Content-Range": `bytes ${range.start}-${range.end - 1}/${total}`,
    "Content-Length": String(range.end - range.start),
  });
  if (req.method === "HEAD") return res.end();
  return res.end(body(range.start, range.end));
}

const routes = {
  // --- streaming manifests -------------------------------------------
  //
  // A small but complete HLS stream, so segment fetching and joining can be
  // exercised without reaching the internet. The segments are slices of the
  // same generated sequence every other route uses, which means the joined
  // file can be verified byte for byte against it.

  "/hls/key.bin": {
    description: "The AES-128 key for the encrypted HLS stream. Sixteen bytes.",
    handler: (req, res) => {
      res.writeHead(200, {
        "Content-Type": "application/octet-stream",
        "Content-Length": HLS_KEY.length,
      });
      res.end(HLS_KEY);
    },
  },

  "/hls-aes/index.m3u8": {
    description:
      "AES-128 encrypted HLS media playlist. ?iv=stated uses an explicit IV; " +
      "otherwise the media sequence number is the IV, which is the usual case.",
    handler: (req, res, url) => {
      const origin = `http://${req.headers.host}`;
      const count = Number(url.searchParams.get("segments") ?? 6);
      const size = Number(url.searchParams.get("size") ?? 32768);
      const first = Number(url.searchParams.get("first") ?? 0);
      const stated = url.searchParams.get("iv") === "stated";

      const lines = [
        "#EXTM3U",
        "#EXT-X-VERSION:3",
        "#EXT-X-TARGETDURATION:2",
        `#EXT-X-MEDIA-SEQUENCE:${first}`,
      ];
      const keyTag = stated
        ? `#EXT-X-KEY:METHOD=AES-128,URI="${origin}/hls/key.bin",IV=0x${STATED_IV.toString("hex")}`
        : `#EXT-X-KEY:METHOD=AES-128,URI="${origin}/hls/key.bin"`;
      lines.push(keyTag);

      for (let i = 0; i < count; i += 1) {
        lines.push("#EXTINF:2.0,");
        lines.push(
          `seg${i}.ts?size=${size}&index=${i}&seq=${first + i}` +
            (stated ? "&iv=stated" : ""),
        );
      }
      lines.push("#EXT-X-ENDLIST", "");
      const body = lines.join("\n");
      res.writeHead(200, {
        "Content-Type": "application/vnd.apple.mpegurl",
        "Content-Length": Buffer.byteLength(body),
      });
      res.end(body);
    },
  },

  "/hls-aes/segment": {
    description: "One AES-128 encrypted segment of the generated sequence.",
    handler: (req, res, url) => {
      const size = Number(url.searchParams.get("size") ?? 32768);
      const index = Number(url.searchParams.get("index") ?? 0);
      const sequence = Number(url.searchParams.get("seq") ?? index);
      const stated = url.searchParams.get("iv") === "stated";

      const plaintext = bodyFor(index * size, (index + 1) * size);
      const iv = stated ? STATED_IV : ivForSequence(sequence);
      const cipher = createCipheriv("aes-128-cbc", HLS_KEY, iv);
      const body = Buffer.concat([cipher.update(plaintext), cipher.final()]);

      res.writeHead(200, {
        "Content-Type": "video/mp2t",
        "Content-Length": body.length,
      });
      res.end(body);
    },
  },

  "/hls/master.m3u8": {
    description: "HLS master playlist offering three renditions.",
    handler: (req, res, url) => {
      const origin = `http://${req.headers.host}`;
      const query = url.search ?? "";
      const body = [
        "#EXTM3U",
        "#EXT-X-STREAM-INF:BANDWIDTH=400000,RESOLUTION=640x360",
        `${origin}/hls/360/index.m3u8${query}`,
        "#EXT-X-STREAM-INF:BANDWIDTH=1200000,RESOLUTION=1280x720",
        `${origin}/hls/720/index.m3u8${query}`,
        "#EXT-X-STREAM-INF:BANDWIDTH=3000000,RESOLUTION=1920x1080",
        `${origin}/hls/1080/index.m3u8${query}`,
        "",
      ].join("\n");
      res.writeHead(200, {
        // Deliberately not an HLS content type: plenty of real servers get
        // this wrong, so the body is what should decide.
        "Content-Type": "text/plain",
        "Content-Length": Buffer.byteLength(body),
      });
      res.end(body);
    },
  },

  "/hls/index.m3u8": {
    description: "HLS media playlist; ?segments=N&size=B sets its shape.",
    handler: (req, res, url) => {
      const count = Number(url.searchParams.get("segments") ?? 10);
      const size = Number(url.searchParams.get("size") ?? 65536);
      const lines = [
        "#EXTM3U",
        "#EXT-X-VERSION:3",
        "#EXT-X-TARGETDURATION:2",
        "#EXT-X-MEDIA-SEQUENCE:0",
      ];
      for (let i = 0; i < count; i += 1) {
        lines.push("#EXTINF:2.0,");
        // Relative, so address resolution is exercised too.
        lines.push(`seg${i}.ts?size=${size}&index=${i}`);
      }
      lines.push("#EXT-X-ENDLIST", "");
      const body = lines.join("\n");
      res.writeHead(200, {
        "Content-Type": "application/vnd.apple.mpegurl",
        "Content-Length": Buffer.byteLength(body),
      });
      res.end(body);
    },
  },

  "/hls/segment": {
    description: "One HLS segment: a slice of the generated sequence.",
    handler: (req, res, url) => {
      const size = Number(url.searchParams.get("size") ?? 65536);
      const index = Number(url.searchParams.get("index") ?? 0);
      const body = bodyFor(index * size, (index + 1) * size);
      res.writeHead(200, {
        "Content-Type": "video/mp2t",
        "Content-Length": body.length,
      });
      res.end(body);
    },
  },

  "/honest": {
    description: "Well-behaved baseline: honours Range, strong ETag, truthful lengths.",
    handler: (req, res, url) => serveHonestly(req, res, sizeFromQuery(url)),
  },

  // --- mirrors ---------------------------------------------------------
  //
  // For vetting a second address before it is allowed to contribute bytes.
  // All three are honest servers: the question is not whether they behave,
  // it is whether they are serving the same file.

  "/mirror-agrees": {
    description:
      "Byte-identical to /honest, but with its own ETag -- which is what a real " +
      "second mirror looks like. An ETag is opaque and per-origin, so two mirrors " +
      "serving the same bytes disagree about it, and that is evidence of nothing.",
    handler: (req, res, url) =>
      serveHonestly(req, res, sizeFromQuery(url), { ETag: '"other-origin"' }),
  },

  "/mirror-tail-differs": {
    description:
      "Same length as /honest and the same first megabyte, but the last megabyte " +
      "differs: two builds of one release sharing a header. A check that samples " +
      "only the head accepts this and then silently blends the two.",
    handler: (req, res, url) => {
      const total = sizeFromQuery(url);
      const from = Math.max(0, total - MIB);
      serveHonestly(req, res, total, { ETag: '"other-origin"' }, (start, end) => {
        const buffer = bodyFor(start, end);
        for (let i = 0; i < buffer.length; i += 1) {
          if (start + i >= from) buffer[i] ^= 0xff;
        }
        return buffer;
      });
    },
  },

  "/mirror-shorter": {
    description:
      "Honest, but one byte shorter than /honest at the same `?size=`. The cheap " +
      "half of the check: different lengths cannot be the same file.",
    handler: (req, res, url) =>
      serveHonestly(req, res, Math.max(1, sizeFromQuery(url) - 1), {
        ETag: '"other-origin"',
      }),
  },

  "/named": {
    description:
      "Honest, but the URL's last segment is opaque and the real name is only in " +
      "Content-Disposition, as on a CDN that redirects to a signed GUID-named object. " +
      "`?name=` sets it.",
    handler: (req, res, url) =>
      serveHonestly(req, res, sizeFromQuery(url), {
        "Content-Disposition": `attachment; filename=${url.searchParams.get("name") ?? "named.bin"}`,
      }),
  },

  "/no-accept-ranges": {
    description: "Declares `Accept-Ranges: none` and only ever serves 200. Not resumable.",
    handler: (req, res, url) => {
      const total = sizeFromQuery(url);
      res.writeHead(200, {
        "Accept-Ranges": "none",
        "Content-Length": String(total),
        "Content-Type": "application/octet-stream",
      });
      if (req.method === "HEAD") return res.end();
      return res.end(bodyFor(0, total));
    },
  },

  "/ignores-range": {
    description:
      "Advertises `Accept-Ranges: bytes` but ignores Range and returns the whole body with 200. " +
      "The engine must keep the body and stream from offset 0 rather than discard it.",
    handler: (req, res, url) => {
      const total = sizeFromQuery(url);
      res.writeHead(200, {
        // The lie: it claims range support it does not honour.
        "Accept-Ranges": "bytes",
        "Content-Length": String(total),
        "Content-Type": "application/octet-stream",
      });
      if (req.method === "HEAD") return res.end();
      return res.end(bodyFor(0, total));
    },
  },

  "/omits-accept-ranges": {
    description:
      "Honours Range perfectly but never sends `Accept-Ranges`. Proves the header's absence " +
      "must not be read as a lack of range support.",
    handler: (req, res, url) => {
      const total = sizeFromQuery(url);
      const range = parseRange(req.headers.range, total);
      if (range === "unsatisfiable") return sendUnsatisfiable(res, total);
      if (!range) {
        res.writeHead(200, { "Content-Length": String(total) });
        return res.end(bodyFor(0, total));
      }
      res.writeHead(206, {
        "Content-Range": `bytes ${range.start}-${range.end - 1}/${total}`,
        "Content-Length": String(range.end - range.start),
      });
      return res.end(bodyFor(range.start, range.end));
    },
  },

  "/bad-content-range": {
    description:
      "A broken range-caching proxy: answers 206 but the Content-Range start disagrees with " +
      "what was asked for, while the body is for the offset it names.",
    handler: (req, res, url) => {
      const total = sizeFromQuery(url);
      const range = parseRange(req.headers.range, total);
      if (!range || range === "unsatisfiable") return serveHonestly(req, res, total);

      const skew = 4096;
      const start = Math.max(0, range.start - skew);
      const end = Math.min(total, start + (range.end - range.start));
      res.writeHead(206, {
        "Accept-Ranges": "bytes",
        "Content-Range": `bytes ${start}-${end - 1}/${total}`,
        "Content-Length": String(end - start),
      });
      return res.end(bodyFor(start, end));
    },
  },

  "/short-range": {
    description:
      "Returns a legal but shorter range than requested (half of it). The engine must take " +
      "end from Content-Range and requeue the residue.",
    handler: (req, res, url) => {
      const total = sizeFromQuery(url);
      const range = parseRange(req.headers.range, total);
      if (!range || range === "unsatisfiable") return serveHonestly(req, res, total);

      const requested = range.end - range.start;
      const end = range.start + Math.max(1, Math.floor(requested / 2));
      res.writeHead(206, {
        "Accept-Ranges": "bytes",
        "Content-Range": `bytes ${range.start}-${end - 1}/${total}`,
        "Content-Length": String(end - range.start),
      });
      return res.end(bodyFor(range.start, end));
    },
  },

  "/lies-about-length": {
    description:
      "Content-Length overstates the body by 1024 bytes, so the connection ends early. " +
      "The engine must trust the bytes it actually received.",
    handler: (req, res, url) => {
      const total = sizeFromQuery(url);
      const body = bodyFor(0, total);
      res.writeHead(200, {
        "Accept-Ranges": "bytes",
        "Content-Length": String(total + 1024),
        "Content-Type": "application/octet-stream",
      });
      return res.end(body);
    },
  },

  "/etag-flips": {
    description:
      "Serves a different strong ETag (and different content) from the 3rd request onward: " +
      "the representation changed mid-download. Blending the two would pass every length " +
      "check and produce a silently corrupt file, so the engine must fail fast.",
    handler: (req, res, url) => {
      const total = sizeFromQuery(url);
      const hits = bumpHits("etag-flips");
      const generation = hits >= 3 ? 2 : 1;
      const range = parseRange(req.headers.range, total);
      if (range === "unsatisfiable") return sendUnsatisfiable(res, total);

      const headers = {
        "Accept-Ranges": "bytes",
        ETag: `"gen-${generation}"`,
        "X-Mock-Generation": String(generation),
      };
      // Generation 2 has different bytes at every offset.
      const shift = generation === 2 ? 0x5a : 0x00;
      const span = range ?? { start: 0, end: total };
      const body = bodyFor(span.start, span.end);
      if (shift) {
        for (let i = 0; i < body.length; i += 1) body[i] ^= shift;
      }

      if (!range) {
        res.writeHead(200, { ...headers, "Content-Length": String(total) });
        return res.end(body);
      }
      res.writeHead(206, {
        ...headers,
        "Content-Range": `bytes ${span.start}-${span.end - 1}/${total}`,
        "Content-Length": String(body.length),
      });
      return res.end(body);
    },
  },

  "/weak-etag": {
    description:
      "Only a weak ETag (W/\"...\"), which is invalid for If-Range. The engine must not use " +
      "it to validate a resume.",
    handler: (req, res, url) =>
      serveHonestly(req, res, sizeFromQuery(url), { ETag: 'W/"weak-1"' }),
  },

  "/same-second-mtime": {
    description:
      "Last-Modified equals the response Date, so a same-second modification would be " +
      "undetectable. Must not be trusted as a validator.",
    handler: (req, res, url) => {
      const now = new Date().toUTCString();
      return serveHonestly(req, res, sizeFromQuery(url), {
        "Last-Modified": now,
        Date: now,
        // No strong validator at all, so Last-Modified is all there is --
        // and at one-second granularity it cannot be trusted.
        ETag: undefined,
      });
    },
  },

  "/content-encoding": {
    description:
      "Applies gzip to a *ranged* response, which makes byte offsets meaningless because " +
      "they then address the encoded entity. The engine must detect this and fall back to a " +
      "single stream.",
    handler: (req, res, url) => {
      const total = sizeFromQuery(url);
      const range = parseRange(req.headers.range, total);
      const span = range && range !== "unsatisfiable" ? range : { start: 0, end: total };
      const encoded = gzipSync(bodyFor(span.start, span.end));
      res.writeHead(range && range !== "unsatisfiable" ? 206 : 200, {
        "Accept-Ranges": "bytes",
        "Content-Encoding": "gzip",
        // Note the trap: Content-Range is in *decoded* offsets while
        // Content-Length is the *encoded* size.
        ...(range && range !== "unsatisfiable"
          ? { "Content-Range": `bytes ${span.start}-${span.end - 1}/${total}` }
          : {}),
        "Content-Length": String(encoded.length),
      });
      return res.end(encoded);
    },
  },

  "/chunked": {
    description: "Chunked transfer with no Content-Length: unknown total, no preallocation.",
    handler: (req, res, url) => {
      const total = sizeFromQuery(url);
      res.writeHead(200, { "Content-Type": "application/octet-stream" });
      for (let offset = 0; offset < total; offset += 64 * 1024) {
        res.write(bodyFor(offset, Math.min(total, offset + 64 * 1024)));
      }
      return res.end();
    },
  },

  "/throttle-429": {
    description:
      "Returns 429 with Retry-After on every 2nd request. Retrying on a *fresh* connection " +
      "instead of backing off is what escalates this to an IP ban.",
    handler: (req, res, url) => {
      const total = sizeFromQuery(url);
      if (bumpHits("throttle-429") % 2 === 0) {
        res.writeHead(429, { "Retry-After": "2", "Content-Length": "0" });
        return res.end();
      }
      return serveHonestly(req, res, total);
    },
  },

  "/expiring": {
    description:
      "A signed URL: 403 unless `?sig=` is present and fresh. Models the expiring-CDN-link " +
      "case, where the engine must re-resolve the original URL and carry on.",
    handler: (req, res, url) => {
      const total = sizeFromQuery(url);
      const signature = url.searchParams.get("sig");
      if (!signature) {
        res.writeHead(403, { "Content-Length": "0" });
        return res.end();
      }
      return serveHonestly(req, res, total);
    },
  },

  "/drops": {
    description:
      "Destroys the socket partway through the body without a trailing chunk, so the client " +
      "sees a truncated response rather than a clean EOF.",
    handler: (req, res, url) => {
      const total = sizeFromQuery(url);
      const range = parseRange(req.headers.range, total);
      const span = range && range !== "unsatisfiable" ? range : { start: 0, end: total };
      const length = span.end - span.start;
      res.writeHead(range && range !== "unsatisfiable" ? 206 : 200, {
        "Accept-Ranges": "bytes",
        ...(range && range !== "unsatisfiable"
          ? { "Content-Range": `bytes ${span.start}-${span.end - 1}/${total}` }
          : {}),
        "Content-Length": String(length),
      });
      const cut = Math.max(1, Math.floor(length / 3));
      // Destroy only once the partial body has actually flushed to the kernel.
      // Destroying immediately after write() discards it, and the client sees
      // an empty reply instead of the truncated one we want to exercise.
      return res.write(bodyFor(span.start, span.start + cut), () => {
        res.socket?.destroy();
      });
    },
  },

  "/slow": {
    description:
      "Trickles the body at a configurable `?bps=` so one connection becomes the straggler " +
      "that work stealing and hedged tail requests exist to rescue.",
    handler: (req, res, url) => {
      const total = sizeFromQuery(url);
      const bytesPerSecond = Number(url.searchParams.get("bps") ?? 64 * 1024);
      const range = parseRange(req.headers.range, total);
      if (range === "unsatisfiable") return sendUnsatisfiable(res, total);
      const span = range ?? { start: 0, end: total };

      res.writeHead(range ? 206 : 200, {
        "Accept-Ranges": "bytes",
        ETag: `"honest-${total}"`,
        ...(range
          ? { "Content-Range": `bytes ${span.start}-${span.end - 1}/${total}` }
          : {}),
        "Content-Length": String(span.end - span.start),
      });

      const chunk = Math.max(1, Math.floor(bytesPerSecond / 10));
      let offset = span.start;
      const tick = setInterval(() => {
        if (offset >= span.end || res.writableEnded) {
          clearInterval(tick);
          return res.end();
        }
        const next = Math.min(span.end, offset + chunk);
        res.write(bodyFor(offset, next));
        offset = next;
      }, 100);
      req.on("close", () => clearInterval(tick));
      return undefined;
    },
  },


  "/auth-basic": {
    description:
      "401 with a Basic challenge until the right password arrives, then serves the file " +
      "normally. The plainest case, and what every other auth route is compared against.",
    handler: (req, res, url) => {
      if (req.headers.authorization !== basicExpected()) {
        return sendChallenge(res, 'Basic realm="' + AUTH_REALM + '"');
      }
      return serveHonestly(req, res, sizeFromQuery(url));
    },
  },

  "/auth-digest": {
    description:
      "401 with a Digest challenge using MD5 and qop=auth, then serves the file. Rejects a " +
      "repeated nonce count, which is what a per-lane counter would produce.",
    handler: (req, res, url) => {
      if (checkDigest(req, "MD5", true) !== "ok") {
        return sendChallenge(
          res,
          'Digest realm="' + AUTH_REALM + '", qop="auth", algorithm=MD5, nonce="' +
            mintNonce() + '"',
        );
      }
      return serveHonestly(req, res, sizeFromQuery(url));
    },
  },

  "/auth-digest-sha256": {
    description:
      "As /auth-digest but algorithm=SHA-256, which RFC 7616 added and which the engine " +
      "must prefer when a server offers both.",
    handler: (req, res, url) => {
      if (checkDigest(req, "SHA-256", true) !== "ok") {
        return sendChallenge(
          res,
          'Digest realm="' + AUTH_REALM + '", qop="auth", algorithm=SHA-256, nonce="' +
            mintNonce() + '"',
        );
      }
      return serveHonestly(req, res, sizeFromQuery(url));
    },
  },

  "/auth-digest-no-qop": {
    description:
      "The RFC 2617 form with no qop, so no nc and no cnonce, and a differently hashed " +
      "response. Still what a great many devices serve.",
    handler: (req, res, url) => {
      if (checkDigest(req, "MD5", false) !== "ok") {
        return sendChallenge(
          res,
          'Digest realm="' + AUTH_REALM + '", algorithm=MD5, nonce="' + mintNonce() + '"',
        );
      }
      return serveHonestly(req, res, sizeFromQuery(url));
    },
  },

  "/auth-stale": {
    description:
      "Issues a nonce that is already expired, so the first correct answer comes back 401 " +
      "with stale=true. Proves a nonce outliving its download is retried rather than " +
      "mistaken for a wrong password.",
    handler: (req, res, url) => {
      const verdict = checkDigest(req, "MD5", true);
      if (verdict === "ok") return serveHonestly(req, res, sizeFromQuery(url));
      if (verdict === "stale") {
        return sendChallenge(
          res,
          'Digest realm="' + AUTH_REALM + '", qop="auth", algorithm=MD5, nonce="' +
            mintNonce() + '", stale=true',
        );
      }
      return sendChallenge(
        res,
        'Digest realm="' + AUTH_REALM + '", qop="auth", algorithm=MD5, nonce="' +
          mintNonce(true) + '"',
      );
    },
  },

  "/auth-ntlm-only": {
    description:
      "Offers nothing but Windows sign-in, so the refusal can be asserted as prose rather " +
      "than as a bare 401.",
    handler: (_req, res) => sendChallenge(res, "Negotiate, NTLM"),
  },

  "/auth-both": {
    description:
      "Offers Negotiate, NTLM and Basic together, which is what a Windows server commonly " +
      "does. The engine must use the Basic rather than take the refusal.",
    handler: (req, res, url) => {
      if (req.headers.authorization !== basicExpected()) {
        return sendChallenge(res, 'Negotiate, NTLM, Basic realm="' + AUTH_REALM + '"');
      }
      return serveHonestly(req, res, sizeFromQuery(url));
    },
  },

  "/auth-wrong-password": {
    description:
      "Never accepts anything, so a wrong password is refused once rather than retried " +
      "until the server locks the account.",
    handler: (_req, res) => sendChallenge(res, 'Basic realm="' + AUTH_REALM + '"'),
  },


  "/proxy.pac": {
    description:
      "A proxy auto-config script, served so Windows' own PAC evaluator can be driven for " +
      "real. Both branches matter: a host that should be proxied and one that should not, " +
      "decided by shExpMatch so a genuine PAC host function is exercised rather than a " +
      "constant being returned.",
    handler: (_req, res, url) => {
      // `?via=host:port` names a proxy that is actually listening, so the
      // script can be pointed at the forward proxy in proxy.mjs and the
      // whole chain driven for real. The default names a host that does
      // not exist, which is all the unit-level test needs.
      const via = url.searchParams.get("via") ?? "pac-proxy.example.test:8888";
      const script = [
        "function FindProxyForURL(url, host) {",
        '  if (shExpMatch(host, "*.proxied.test")) {',
        `    return "PROXY ${via}";`,
        "  }",
        '  return "DIRECT";',
        "}",
        "",
      ].join("\n");
      res.writeHead(200, {
        // The type WinHTTP expects. Served as text/plain it is fetched and
        // then ignored, which would make this test pass for the wrong
        // reason -- every lookup would come back DIRECT.
        "Content-Type": "application/x-ns-proxy-autoconfig",
        "Content-Length": String(Buffer.byteLength(script)),
      });
      return res.end(script);
    },
  },


  "/suffix-honest": {
    description:
      "Honours `bytes=-N`, answering 206 with the last N bytes and a Content-Range naming " +
      "where they start. The shape a zip's index is read with.",
    handler: (req, res, url) => {
      const total = sizeFromQuery(url);
      const match = /^bytes=-(\d+)$/.exec(req.headers.range ?? "");
      if (!match) return serveHonestly(req, res, total);

      const want = Math.min(Number(match[1]), total);
      const start = total - want;
      res.writeHead(206, {
        "Accept-Ranges": "bytes",
        "Content-Range": `bytes ${start}-${total - 1}/${total}`,
        "Content-Length": String(want),
        "Content-Type": "application/octet-stream",
      });
      return res.end(bodyFor(start, total));
    },
  },

  "/suffix-ignored": {
    description:
      "Ignores a suffix range and answers 200 with the whole file. Legal, and the reason a " +
      "tail read has to say whether it got the tail: the opening bytes must never be read " +
      "as the closing ones.",
    handler: (req, res, url) => {
      const total = sizeFromQuery(url);
      res.writeHead(200, {
        "Content-Length": String(total),
        "Content-Type": "application/octet-stream",
      });
      return res.end(bodyFor(0, total));
    },
  },

  "/suffix-lies": {
    description:
      "Answers 206 to a suffix range but sends far more than was asked for. The case a cap " +
      "exists for: without one this is a server deciding how much of this process's memory " +
      "to use.",
    handler: (req, res, url) => {
      const total = sizeFromQuery(url);
      res.writeHead(206, {
        "Accept-Ranges": "bytes",
        "Content-Range": `bytes 0-${total - 1}/${total}`,
        "Content-Length": String(total),
        "Content-Type": "application/octet-stream",
      });
      return res.end(bodyFor(0, total));
    },
  },

  "/range-no-content-range": {
    description:
      "Answers 206 without a Content-Range, so there is no way to know which bytes arrived. " +
      "Refused rather than guessed at: guessing is how the wrong part of a file gets read " +
      "as the right one.",
    handler: (req, res, url) => {
      const total = sizeFromQuery(url);
      const want = Math.min(64, total);
      res.writeHead(206, {
        "Content-Length": String(want),
        "Content-Type": "application/octet-stream",
      });
      return res.end(bodyFor(0, want));
    },
  },


  // --- pages with links on them --------------------------------------
  //
  // For the link grabber. Deliberately messy: an off-origin link, a
  // `mailto:`, an image `src`, a repeat, and -- on the followed pages -- a
  // link one level further out, which must not be followed.

  "/robots.txt": {
    description:
      "Whatever a test last set with /_robots?text=..., or a 404 when it set nothing. " +
      "A site with no robots.txt is saying nothing, which is not the same as saying no.",
    handler: (req, res) => {
      if (robotsText === null) {
        res.writeHead(404, { "Content-Type": "text/plain" });
        return res.end("no robots.txt here\n");
      }
      res.writeHead(200, { "Content-Type": "text/plain" });
      res.end(robotsText);
    },
  },

  "/_robots": {
    description: "Sets what /robots.txt will say. ?text= the body, or omitted to go back to a 404.",
    handler: (req, res, url) => {
      robotsText = url.searchParams.has("text") ? url.searchParams.get("text") : null;
      res.writeHead(200, { "Content-Type": "text/plain" });
      res.end("ok\n");
    },
  },

  "/site/index.html": {
    description:
      "A page of links. ?files= how many file links, ?pages= how many page links worth " +
      "following. Always includes one off-origin link, one mailto, one image src and one " +
      "repeat of the first file, so the filtering and the de-duplication are exercised.",
    handler: (req, res, url) => {
      const files = Number(url.searchParams.get("files") ?? 3);
      const pages = Number(url.searchParams.get("pages") ?? 2);
      const parts = ["<!doctype html><html><head><title>index</title></head><body>"];
      for (let i = 0; i < files; i += 1) {
        parts.push(`<a href="top-${i}.bin">file number ${i}</a>`);
      }
      for (let i = 1; i <= pages; i += 1) {
        parts.push(`<a href="/site/page.html?n=${i}">page ${i}</a>`);
      }
      parts.push('<a href="http://elsewhere.invalid/off.bin">somewhere else</a>');
      parts.push('<a href="mailto:nobody@example.com">mail</a>');
      parts.push('<img src="logo.png" alt="a logo">');
      parts.push('<a href="top-0.bin">the same file again</a>');
      parts.push("</body></html>");
      const body = parts.join("\n");
      res.writeHead(200, {
        "Content-Type": "text/html; charset=utf-8",
        "Content-Length": Buffer.byteLength(body),
      });
      res.end(body);
    },
  },

  "/site/page.html": {
    description:
      "A page reached from the index. ?n= which one. Links to two files of its own and to " +
      "/site/deeper.html, which a grab that stops at one level must not fetch.",
    handler: (req, res, url) => {
      const n = url.searchParams.get("n") ?? "0";
      const body =
        `<!doctype html><html><body>` +
        `<a href="/site/sub-${n}-a.bin">a</a>` +
        `<a href="/site/sub-${n}-b.bin">b</a>` +
        `<a href="/site/deeper.html">deeper</a>` +
        `</body></html>`;
      res.writeHead(200, {
        "Content-Type": "text/html",
        "Content-Length": Buffer.byteLength(body),
      });
      res.end(body);
    },
  },

  "/site/deeper.html": {
    description:
      "One level further out than a grab goes. Fetching this at all is the failure, so its " +
      "link is named to say so.",
    handler: (req, res) => {
      const body = `<!doctype html><html><body><a href="/site/must-not-appear.bin">no</a></body></html>`;
      res.writeHead(200, {
        "Content-Type": "text/html",
        "Content-Length": Buffer.byteLength(body),
      });
      res.end(body);
    },
  },

  "/site/many.html": {
    description: "A page with ?links= links on it, for the cap. Default two thousand.",
    handler: (req, res, url) => {
      const count = Number(url.searchParams.get("links") ?? 2000);
      const parts = ["<!doctype html><html><body>"];
      for (let i = 0; i < count; i += 1) {
        parts.push(`<a href="/site/n-${i}.bin">n</a>`);
      }
      parts.push("</body></html>");
      const body = parts.join("");
      res.writeHead(200, {
        "Content-Type": "text/html",
        "Content-Length": Buffer.byteLength(body),
      });
      res.end(body);
    },
  },

  "/archive.zip": {
    description:
      "A real zip, served with ranges honoured, so an archive can be listed without being " +
      "downloaded. `?files=` sets how many it holds and `?each=` how large each one is; " +
      "`?comment=` adds an archive comment, which is the thing that stops the index sitting " +
      "at a fixed offset.",
    handler: (req, res, url) => {
      const count = Number(url.searchParams.get("files") ?? 3);
      const each = Number(url.searchParams.get("each") ?? 64);
      const comment = url.searchParams.get("comment") ?? "";
      const files = Array.from({ length: count }, (_, index) => [
        index % 4 === 3 ? `folder-${index}/` : `file-${String(index).padStart(4, "0")}.bin`,
        index % 4 === 3 ? 0 : each,
      ]);
      const zip = buildZip(files, comment);

      const match = /^bytes=(?:(\d+)-(\d*)|-(\d+))$/.exec(req.headers.range ?? "");
      if (!match) {
        res.writeHead(200, {
          "Accept-Ranges": "bytes",
          "Content-Length": String(zip.length),
          "Content-Type": "application/zip",
        });
        return res.end(zip);
      }

      let start;
      let end;
      if (match[3] !== undefined) {
        start = Math.max(0, zip.length - Number(match[3]));
        end = zip.length - 1;
      } else {
        start = Number(match[1]);
        end = match[2] === "" ? zip.length - 1 : Math.min(Number(match[2]), zip.length - 1);
      }

      res.writeHead(206, {
        "Accept-Ranges": "bytes",
        "Content-Range": `bytes ${start}-${end}/${zip.length}`,
        "Content-Length": String(end - start + 1),
        "Content-Type": "application/zip",
      });
      return res.end(zip.subarray(start, end + 1));
    },
  },

  "/head-disagrees": {
    description:
      "HEAD reports a different Content-Length than GET, which is why the engine probes with " +
      "a ranged GET instead of a HEAD.",
    handler: (req, res, url) => {
      const total = sizeFromQuery(url);
      if (req.method === "HEAD") {
        res.writeHead(200, {
          "Accept-Ranges": "bytes",
          "Content-Length": String(total * 2),
        });
        return res.end();
      }
      return serveHonestly(req, res, total);
    },
  },

  "/always-416": {
    description: "Answers every ranged request with 416 and `bytes */total`.",
    handler: (req, res, url) => sendUnsatisfiable(res, sizeFromQuery(url)),
  },
};

const server = createServer((req, res) => {
  const url = new URL(req.url, `http://${req.headers.host ?? "localhost"}`);

  if (url.pathname === "/_routes") {
    const listing = Object.entries(routes).map(([path, route]) => ({
      path,
      description: route.description,
    }));
    const payload = JSON.stringify({ routes: listing }, null, 2);
    res.writeHead(200, {
      "Content-Type": "application/json",
      "Content-Length": String(Buffer.byteLength(payload)),
    });
    return res.end(payload);
  }

  // Lets a test reset the counters that drive the "misbehave after N" routes.
  if (url.pathname === "/_reset") {
    hitCounts.clear();
    res.writeHead(204).end();
    return undefined;
  }

  // Rendition directories and segment names are generated, so they are
  // matched by shape rather than listed one by one.
  let pathname = url.pathname;
  if (/^\/hls\/[^/]+\/index\.m3u8$/.test(pathname)) {
    pathname = "/hls/index.m3u8";
  } else if (/^\/hls-aes\/seg\d+\.ts$/.test(pathname)) {
    pathname = "/hls-aes/segment";
  } else if (/^\/hls\/[^/]+\/seg\d+\.ts$/.test(pathname)) {
    pathname = "/hls/segment";
  }

  const route = routes[pathname];
  if (!route) {
    res.writeHead(404, { "Content-Length": "0" });
    return res.end();
  }

  try {
    return route.handler(req, res, url);
  } catch (error) {
    if (!res.headersSent) res.writeHead(500, { "Content-Length": "0" });
    res.end();
    console.error(`[mock] ${url.pathname} failed:`, error);
    return undefined;
  }
});

const portFlagIndex = process.argv.indexOf("--port");
const port = portFlagIndex === -1 ? DEFAULT_PORT : Number(process.argv[portFlagIndex + 1]);

server.listen(port, "127.0.0.1", () => {
  const { port: boundPort } = server.address();
  // The Rust integration tests parse this line to learn the port.
  console.log(`mock-server listening on http://127.0.0.1:${boundPort}`);
});

export { byteAt, bodyFor, parseRange, routes };
