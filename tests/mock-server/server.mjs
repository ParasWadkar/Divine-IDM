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
import { createCipheriv } from "node:crypto";
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

/** The well-behaved baseline: strong ETag, honours Range, accurate lengths. */
function serveHonestly(req, res, total, extraHeaders = {}) {
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
    return res.end(bodyFor(0, total));
  }

  res.writeHead(206, {
    ...headers,
    "Content-Range": `bytes ${range.start}-${range.end - 1}/${total}`,
    "Content-Length": String(range.end - range.start),
  });
  if (req.method === "HEAD") return res.end();
  return res.end(bodyFor(range.start, range.end));
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
