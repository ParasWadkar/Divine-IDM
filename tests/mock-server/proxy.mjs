// A forward proxy that insists on a password.
//
// Exists for one assertion: that the password saved against a proxy address
// actually reaches the wire as `Proxy-Authorization`, and reaches the proxy
// rather than the origin. That is wiring which cannot be proven by a unit
// test and which would fail silently -- a download through an
// unauthenticated proxy works, so nothing else would notice the header
// going missing.
//
// It does not forward anything. A real upstream would make this a test of
// two servers and a network path; answering in place keeps it a test of the
// header. The body is the same generated sequence the main mock server
// produces, so the bytes can be checked against the same expectation.
//
//   node proxy.mjs --port 0

import { createServer } from "node:http";

const USER = "proxyuser";
const PASS = "proxypass";
const REALM = "proxy@example.org";

/** Matches `byteAt` in server.mjs and `byte_at` in the Rust tests. */
function byteAt(offset) {
  let x = (offset + 0x9e3779b9) >>> 0;
  x = Math.imul(x ^ (x >>> 16), 0x85ebca6b) >>> 0;
  x = Math.imul(x ^ (x >>> 13), 0xc2b2ae35) >>> 0;
  return (x ^ (x >>> 16)) & 0xff;
}

function bodyFor(start, end) {
  const out = Buffer.allocUnsafe(end - start);
  for (let i = start; i < end; i += 1) out[i - start] = byteAt(i);
  return out;
}

const expected = "Basic " + Buffer.from(`${USER}:${PASS}`).toString("base64");

const server = createServer((req, res) => {
  // A proxy receives the absolute form: `GET http://host/path HTTP/1.1`.
  // Anything else means this was reached directly rather than as a proxy,
  // which is worth failing loudly rather than quietly serving.
  if (!/^https?:\/\//i.test(req.url)) {
    res.writeHead(400, { "Content-Length": "0" });
    return res.end();
  }

  if (req.headers["proxy-authorization"] !== expected) {
    // 407 and `Proxy-Authenticate`, which are the proxy's own versions of
    // 401 and `WWW-Authenticate` and must not be confused with them.
    res.writeHead(407, {
      "Proxy-Authenticate": `Basic realm="${REALM}"`,
      "Content-Length": "0",
    });
    return res.end();
  }

  // The site's own password must NOT have come along for the ride. A
  // client that put it on every request would leak it to every proxy in
  // between, so this is asserted here rather than hoped for.
  if (req.headers.authorization) {
    res.writeHead(400, { "Content-Length": "0", "X-Leaked-Site-Credential": "yes" });
    return res.end();
  }

  const url = new URL(req.url);
  const total = Number(url.searchParams.get("size") ?? 1 << 20);
  const range = req.headers.range;
  const match = /^bytes=(\d+)-(\d*)$/.exec(range ?? "");

  if (!match) {
    res.writeHead(200, {
      "Accept-Ranges": "bytes",
      "Content-Length": String(total),
      "Content-Type": "application/octet-stream",
    });
    return res.end(bodyFor(0, total));
  }

  const start = Number(match[1]);
  const end = match[2] === "" ? total - 1 : Math.min(Number(match[2]), total - 1);
  res.writeHead(206, {
    "Accept-Ranges": "bytes",
    "Content-Range": `bytes ${start}-${end}/${total}`,
    "Content-Length": String(end - start + 1),
    "Content-Type": "application/octet-stream",
  });
  return res.end(bodyFor(start, end + 1));
});

const flag = process.argv.indexOf("--port");
server.listen(flag === -1 ? 0 : Number(process.argv[flag + 1]), "127.0.0.1", () => {
  // Parsed by the Rust test to learn the port, same shape as server.mjs.
  console.log(`mock-server listening on http://127.0.0.1:${server.address().port}`);
});
