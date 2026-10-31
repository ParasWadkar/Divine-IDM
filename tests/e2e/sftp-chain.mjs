// Downloads over SFTP against a real SSH server, and checks that host keys
// are actually verified.
//
// The host key checks are the point. An SFTP client that accepts whatever
// key a server offers has no security at all — anything able to answer on
// that address reads the password and the file — and "we verify host keys"
// is exactly the sort of claim that is easy to make and easy to get wrong.
// So this runs the client against a known key, an unknown one, and a
// changed one, and requires the right answer to each.
//
//   node sftp-chain.mjs

import { spawn, spawnSync } from "node:child_process";
import {
  existsSync, mkdirSync, mkdtempSync, readFileSync, readdirSync, rmSync, writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const HERE = dirname(fileURLToPath(import.meta.url));
const REPO = resolve(HERE, "..", "..");
const IDM = join(REPO, "target", "release", "idm.exe");
const SERVER = join(REPO, "target", "release", "stub-sftp-server.exe");

const PORT = 2242;
const USER = "tester";
const PASSWORD = "correct horse";
// The server's built-in key, fixed so these cases can be told apart.
const HOST_KEY =
  "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIJUDOaerEPsEOGwLmsX4eH30jUsmMa8DUHwOstJbiMZo";
// A different, valid key — what a server that was rebuilt would present.
const OTHER_KEY =
  "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIJFqUqJ0AnVZ3lqZkVbF0fXKjJ8kP3sYvJ0dLqz5Nabc";

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
let failures = 0;

function check(label, ok, detail = "") {
  console.log(`  ${ok ? "PASS" : "FAIL"}  ${label}${detail ? "  — " + detail : ""}`);
  if (!ok) failures += 1;
  return ok;
}

function byteAt(offset) {
  let x = (offset + 0x9e3779b9) | 0;
  x = Math.imul(x ^ (x >>> 16), 0x85ebca6b);
  x = Math.imul(x ^ (x >>> 13), 0xc2b2ae35);
  x = x ^ (x >>> 16);
  return x & 0xff;
}

function expected(length) {
  const buffer = Buffer.allocUnsafe(length);
  for (let i = 0; i < length; i += 1) buffer[i] = byteAt(i);
  return buffer;
}

/**
 * Runs the client with a known_hosts file of our choosing.
 *
 * HOME and USERPROFILE both point at a directory built for the case under
 * test, so the person running this suite keeps their own known_hosts.
 */
function fetch(home, name, out, extra = []) {
  return spawnSync(
    IDM,
    [`sftp://${USER}@127.0.0.1:${PORT}/${name}`, "-o", out, "--password", PASSWORD, ...extra],
    { encoding: "utf8", env: { ...process.env, HOME: home, USERPROFILE: home } },
  );
}

/** A home directory whose known_hosts contains the given lines. */
function homeWith(lines) {
  const home = mkdtempSync(join(tmpdir(), "idm-home-"));
  mkdirSync(join(home, ".ssh"), { recursive: true });
  writeFileSync(join(home, ".ssh", "known_hosts"), lines.join("\n") + (lines.length ? "\n" : ""));
  return home;
}

function onlyFile(dir) {
  const files = readdirSync(dir).filter((name) => !name.endsWith(".idmpart"));
  return files.length === 1 ? join(dir, files[0]) : null;
}

async function main() {
  for (const [label, path] of [["idm.exe", IDM], ["stub-sftp-server.exe", SERVER]]) {
    if (!existsSync(path)) {
      console.error(`missing ${label}; run: cargo build --release --workspace`);
      process.exit(1);
    }
  }

  console.log("DivinePulse — SFTP\n");
  const server = spawn(SERVER, ["--port", String(PORT)], { stdio: ["ignore", "pipe", "pipe"] });
  await sleep(1500);

  const SIZE = 512 * 1024;
  const NAME = `file-${SIZE}.bin`;
  const homes = [];
  const outs = [];

  try {
    // 1. A host already in known_hosts: the ordinary case.
    const known = homeWith([`[127.0.0.1]:${PORT} ${HOST_KEY}`]);
    homes.push(known);
    let out = mkdtempSync(join(tmpdir(), "idm-sftp-"));
    outs.push(out);

    let result = fetch(known, NAME, out);
    let file = onlyFile(out);
    check(
      "a file downloads over SFTP",
      result.status === 0 && Boolean(file),
      (result.stderr || result.stdout).trim().split("\n")[0],
    );
    if (file) {
      check("the bytes are correct", readFileSync(file).equals(expected(SIZE)), `${SIZE} bytes`);
    }

    // 2. Resume. Unlike FTP there is no REST to refuse: every read names
    //    its own offset, so the server has no say.
    out = mkdtempSync(join(tmpdir(), "idm-sftp-"));
    outs.push(out);
    const already = 200 * 1024;
    writeFileSync(join(out, `${NAME}.idmpart`), expected(already));

    result = fetch(known, NAME, out);
    file = onlyFile(out);
    const said = (result.stderr || result.stdout).trim();
    check(
      "a part file is resumed",
      result.status === 0 && /already on disk/.test(said),
      said.split("\n")[0],
    );
    if (file) {
      check(
        "a resumed file is correct end to end",
        readFileSync(file).equals(expected(SIZE)),
        `${readFileSync(file).length} bytes`,
      );
    }

    // 3. An unknown host is refused, and says how to check it.
    const empty = homeWith([]);
    homes.push(empty);
    out = mkdtempSync(join(tmpdir(), "idm-sftp-"));
    outs.push(out);

    result = fetch(empty, NAME, out);
    const unknownSaid = (result.stderr || result.stdout).trim();
    check(
      "an unknown host is refused rather than trusted",
      result.status !== 0 && /known_hosts/.test(unknownSaid),
      unknownSaid.split("\n").pop(),
    );
    check(
      "and the refusal prints a fingerprint to check",
      /SHA256:/.test(unknownSaid),
      /SHA256:[^ ]*/.exec(unknownSaid)?.[0] ?? "no fingerprint",
    );
    check("nothing was downloaded", onlyFile(out) === null, readdirSync(out).join(", "));

    // 4. --accept-new-host-key trusts it and records it, as ssh does.
    const learning = homeWith([]);
    homes.push(learning);
    out = mkdtempSync(join(tmpdir(), "idm-sftp-"));
    outs.push(out);

    result = fetch(learning, NAME, out, ["--accept-new-host-key"]);
    check(
      "accepting a new host key downloads the file",
      result.status === 0 && Boolean(onlyFile(out)),
      (result.stderr || result.stdout).trim().split("\n")[0],
    );
    const recorded = readFileSync(join(learning, ".ssh", "known_hosts"), "utf8");
    check(
      "and records the key so the next time needs no flag",
      recorded.includes(HOST_KEY.split(" ")[1]),
      recorded.trim().slice(0, 48) + "…",
    );

    // 5. A changed key is refused outright. This is the case that matters:
    //    either the server was rebuilt or something is in the middle, and
    //    only the person can tell which.
    const changed = homeWith([`[127.0.0.1]:${PORT} ${OTHER_KEY}`]);
    homes.push(changed);
    out = mkdtempSync(join(tmpdir(), "idm-sftp-"));
    outs.push(out);

    result = fetch(changed, NAME, out);
    const changedSaid = (result.stderr || result.stdout).trim();
    check(
      "a changed host key is refused",
      result.status !== 0,
      changedSaid.split("\n").pop(),
    );
    check(
      "and says the key changed rather than something vague",
      /changed/i.test(changedSaid),
      /changed/i.test(changedSaid) ? "explained" : changedSaid.slice(0, 60),
    );
    check("nothing was downloaded", onlyFile(out) === null, readdirSync(out).join(", "));

    // 6. A wrong password is refused without echoing it back.
    out = mkdtempSync(join(tmpdir(), "idm-sftp-"));
    outs.push(out);
    result = spawnSync(
      IDM,
      [
        `sftp://${USER}@127.0.0.1:${PORT}/${NAME}`, "-o", out,
        "--password", "hunter2",
      ],
      { encoding: "utf8", env: { ...process.env, HOME: known, USERPROFILE: known } },
    );
    const rejected = (result.stderr || result.stdout).trim();
    check(
      "a wrong password is refused without repeating it",
      result.status !== 0 && !rejected.includes("hunter2"),
      rejected.split("\n").pop(),
    );
  } finally {
    server.kill();
    await sleep(400);
    for (const dir of [...homes, ...outs]) {
      try {
        rmSync(dir, { recursive: true, force: true, maxRetries: 3 });
      } catch {
        // Not worth failing a run over.
      }
    }
  }

  console.log(`\n${failures ? failures + " check(s) failed" : "all checks passed"}`);
  process.exit(failures ? 1 : 0);
}

main().catch((error) => {
  console.error(error);
  process.exit(1);
});
