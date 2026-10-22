// Downloads over FTP against a server written for the purpose, including
// the ways a real one misbehaves.
//
// Two of these checks matter more than the plain download. `REST` is how FTP
// resumes and plenty of servers refuse it, so the client has to notice and
// start again rather than fail. `SIZE` is optional too, and a client that
// insists on knowing the length first cannot talk to a server that will not
// say.
//
//   node ftp-chain.mjs

import { spawn, spawnSync } from "node:child_process";
import { existsSync, mkdtempSync, readFileSync, readdirSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const HERE = dirname(fileURLToPath(import.meta.url));
const REPO = resolve(HERE, "..", "..");
const IDM = join(REPO, "target", "release", "idm.exe");
const SERVER = join(REPO, "tests", "mock-server", "ftp-server.mjs");

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

function startServer(port, extra = []) {
  const child = spawn("node", [SERVER, "--port", String(port), ...extra], {
    stdio: ["ignore", "pipe", "ignore"],
  });
  return new Promise((done) => {
    child.stdout.on("data", (d) => {
      if (d.toString().includes("listening")) done(child);
    });
    setTimeout(() => done(child), 2500);
  });
}

function fetch(port, name, out, extra = []) {
  return spawnSync(IDM, [`ftp://127.0.0.1:${port}/${name}`, "-o", out, ...extra], {
    encoding: "utf8",
  });
}

/** The only file in a directory, if there is exactly one. */
function onlyFile(dir) {
  const files = readdirSync(dir);
  return files.length === 1 ? join(dir, files[0]) : null;
}

async function main() {
  if (!existsSync(IDM)) {
    console.error("missing idm.exe; run: cargo build --release --workspace");
    process.exit(1);
  }

  console.log("Divine IDM — FTP\n");
  const SIZE = 512 * 1024;
  const NAME = `file-${SIZE}.bin`;
  const servers = [];

  try {
    // 1. An ordinary download.
    const plain = await startServer(2131);
    servers.push(plain);
    let out = mkdtempSync(join(tmpdir(), "idm-ftp-"));

    let result = fetch(2131, NAME, out);
    let file = onlyFile(out);
    check(
      "a file downloads over FTP",
      result.status === 0 && Boolean(file),
      (result.stderr || result.stdout).trim().split("\n")[0],
    );
    if (file) {
      check("the bytes are correct", readFileSync(file).equals(expected(SIZE)), `${SIZE} bytes`);
    }
    rmSync(out, { recursive: true, force: true });

    // 2. Resume: part of the file is already there, and REST should mean
    //    only the remainder is fetched.
    out = mkdtempSync(join(tmpdir(), "idm-ftp-"));
    const already = 200 * 1024;
    writeFileSync(join(out, `${NAME}.idmpart`), expected(already));

    result = fetch(2131, NAME, out);
    file = onlyFile(out);
    const said = (result.stderr || result.stdout).trim();
    check(
      "a part file is resumed rather than restarted",
      result.status === 0 && /already on disk/.test(said),
      said.split("\n")[0],
    );
    if (file) {
      check(
        "a resumed file is still correct end to end",
        readFileSync(file).equals(expected(SIZE)),
        `${readFileSync(file).length} bytes`,
      );
    }
    rmSync(out, { recursive: true, force: true });

    // 3. A server that refuses REST. Starting again costs time and nothing
    //    else; refusing to download at all would be the wrong answer.
    const noRest = await startServer(2132, ["--no-rest"]);
    servers.push(noRest);
    out = mkdtempSync(join(tmpdir(), "idm-ftp-"));
    writeFileSync(join(out, `${NAME}.idmpart`), expected(already));

    result = fetch(2132, NAME, out);
    file = onlyFile(out);
    check(
      "a server refusing REST still yields a correct file",
      result.status === 0 && Boolean(file) && readFileSync(file).equals(expected(SIZE)),
      (result.stderr || result.stdout).trim().split("\n")[0],
    );
    rmSync(out, { recursive: true, force: true });

    // 4. A server that refuses SIZE. Not knowing the length means no
    //    percentage, not no download.
    const noSize = await startServer(2133, ["--no-size"]);
    servers.push(noSize);
    out = mkdtempSync(join(tmpdir(), "idm-ftp-"));

    result = fetch(2133, NAME, out);
    file = onlyFile(out);
    check(
      "a server refusing SIZE still yields a correct file",
      result.status === 0 && Boolean(file) && readFileSync(file).equals(expected(SIZE)),
      (result.stderr || result.stdout).trim().split("\n")[0],
    );
    rmSync(out, { recursive: true, force: true });

    // 5. A directory address is refused before anything is attempted.
    out = mkdtempSync(join(tmpdir(), "idm-ftp-"));
    result = spawnSync(IDM, [`ftp://127.0.0.1:2131/pub/`, "-o", out], { encoding: "utf8" });
    check(
      "a directory address is refused with a reason",
      result.status !== 0 && /directory/.test(result.stderr + result.stdout),
      (result.stderr || result.stdout).trim().split("\n")[0],
    );
    rmSync(out, { recursive: true, force: true });
  } finally {
    for (const server of servers) server.kill();
    await sleep(400);
  }

  console.log(`\n${failures ? failures + " check(s) failed" : "all checks passed"}`);
  process.exit(failures ? 1 : 0);
}

main().catch((error) => {
  console.error(error);
  process.exit(1);
});
