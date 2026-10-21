// Generates the keypair that fixes this extension's ID.
//
// A Chromium extension's ID is the first 16 bytes of the SHA-256 of its
// public key, hex-encoded with 0-f mapped onto a-p. An unpacked extension
// with no key in its manifest gets an ID derived from its *folder path*
// instead, so it changes whenever the folder moves and differs on every
// machine. Since the native messaging manifest has to name the ID in
// `allowed_origins`, a path-derived ID would mean re-registering the host
// after every move and a different registration on each friend's PC.
//
// Embedding the public key in manifest.json pins the ID everywhere. The
// private key is only needed to sign a .crx; it is written outside version
// control and is not required for loading unpacked.
//
//   node extension/keygen.mjs            # create if absent, print the ID
//   node extension/keygen.mjs --force    # new key, NEW ID (re-register the host)

import { generateKeyPairSync, createHash, createPublicKey } from "node:crypto";
import { existsSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const HERE = dirname(fileURLToPath(import.meta.url));
const KEY_DIR = join(HERE, "keys");
const PRIVATE = join(KEY_DIR, "chromium.pem");
const PUBLIC = join(KEY_DIR, "chromium.pub.txt");

/** The ID Chromium will derive from this SPKI DER public key. */
export function extensionId(spkiDer) {
  const digest = createHash("sha256").update(spkiDer).digest();
  let id = "";
  for (const byte of digest.subarray(0, 16)) {
    // Two hex digits per byte, each mapped into a-p.
    id += String.fromCharCode(97 + (byte >> 4), 97 + (byte & 0x0f));
  }
  return id;
}

function spkiOf(privatePem) {
  return createPublicKey(privatePem).export({ type: "spki", format: "der" });
}

export function loadOrCreate({ force = false } = {}) {
  mkdirSync(KEY_DIR, { recursive: true });

  if (force || !existsSync(PRIVATE)) {
    // 2048-bit RSA: what Chromium's own packer uses.
    const { privateKey } = generateKeyPairSync("rsa", { modulusLength: 2048 });
    writeFileSync(PRIVATE, privateKey.export({ type: "pkcs8", format: "pem" }));
  }

  const spki = spkiOf(readFileSync(PRIVATE, "utf8"));
  const key = spki.toString("base64");
  const id = extensionId(spki);
  writeFileSync(PUBLIC, `${id}\n${key}\n`);
  return { key, id };
}

if (process.argv[1] && import.meta.url.endsWith(process.argv[1].replace(/\\/g, "/"))) {
  const { key, id } = loadOrCreate({ force: process.argv.includes("--force") });
  console.log(`extension id: ${id}`);
  console.log(`private key:  ${PRIVATE}  (keep this; it is not in git)`);
  console.log(`manifest key: ${key.slice(0, 32)}...`);
}
