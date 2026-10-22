// Checks that every scheme the engine *accepts* is one it can actually
// download.
//
// This exists because of a bug it would have caught. FTP and SFTP were
// allowed through the engine's validation when the adapters were written,
// but the adapters were only wired into the command line — so a URL pasted
// into the app was accepted and then failed with "builder error for url",
// which is the HTTP client complaining about a scheme it never handles.
//
// Accepting a URL is a promise to try it. These two halves live in
// different files and nothing but a test like this holds them together.
//
//   node engine-schemes.mjs

import { spawn, spawnSync } from "node:child_process";
import { existsSync, mkdirSync, mkdtempSync, readFileSync, readdirSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const HERE = dirname(fileURLToPath(import.meta.url));
const REPO = resolve(HERE, "..", "..");
const IDM = join(REPO, "target", "release", "idm.exe");
const FTP_SERVER = join(REPO, "tests", "mock-server", "ftp-server.mjs");
const SFTP_SERVER = join(REPO, "target", "release", "stub-sftp-server.exe");
const MOCK = join(REPO, "tests", "mock-server", "server.mjs");

const FTP_PORT = 2151;
const SFTP_PORT = 2252;
const HTTP_PORT = 8191;
const SIZE = 512 * 1024;
const NAME = `file-${SIZE}.bin`;
// The stub SFTP server's built-in key, so known_hosts can be prepared.
const SFTP_HOST_KEY =
  "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIJUDOaerEPsEOGwLmsX4eH30jUsmMa8DUHwOstJbiMZo";

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
let failures = 0;

function check(label, ok, detail = "") {
  console.log(`  ${ok ? "PASS" : "FAIL"}  ${label}${detail ? "  — " + detail : ""}`);
  if (!ok) failures += 1;
  return ok;
}

function expected(length) {
  const buffer = Buffer.allocUnsafe(length);
  for (let i = 0; i < length; i += 1) {
    let x = (i + 0x9e3779b9) | 0;
    x = Math.imul(x ^ (x >>> 16), 0x85ebca6b);
    x = Math.imul(x ^ (x >>> 13), 0xc2b2ae35);
    x = x ^ (x >>> 16);
    buffer[i] = x & 0xff;
  }
  return buffer;
}

async function main() {
  for (const [label, path] of [["idm.exe", IDM], ["stub-sftp-server.exe", SFTP_SERVER]]) {
    if (!existsSync(path)) {
      console.error(`missing ${label}; run: cargo build --release --workspace`);
      process.exit(1);
    }
  }

  console.log("Divine IDM — every scheme the engine accepts\n");

  const downloads = mkdtempSync(join(tmpdir(), "idm-schemes-"));
  const stateDir = mkdtempSync(join(tmpdir(), "idm-state-"));
  const home = mkdtempSync(join(tmpdir(), "idm-home-"));
  mkdirSync(join(home, ".ssh"), { recursive: true });
  writeFileSync(
    join(home, ".ssh", "known_hosts"),
    `[127.0.0.1]:${SFTP_PORT} ${SFTP_HOST_KEY}\n`,
  );
  // A key where ssh would look for one, so what gets tested is the engine's
  // own way of authenticating — it has nobody to ask for a password.
  // Unencrypted, because an encrypted key is deliberately skipped: there is
  // no one there to type a passphrase.
  const keygen = spawnSync(
    "ssh-keygen",
    ["-t", "ed25519", "-N", "", "-C", "idm-test", "-f", join(home, ".ssh", "id_ed25519"), "-q"],
    { encoding: "utf8" },
  );
  const haveKey = keygen.status === 0 && existsSync(join(home, ".ssh", "id_ed25519"));

  const servers = [
    spawn("node", [FTP_SERVER, "--port", String(FTP_PORT)], { stdio: ["ignore", "pipe", "ignore"] }),
    spawn(SFTP_SERVER, ["--port", String(SFTP_PORT)], { stdio: ["ignore", "pipe", "ignore"] }),
    spawn("node", [MOCK, "--port", String(HTTP_PORT)], { stdio: ["ignore", "pipe", "ignore"] }),
  ];
  await sleep(2500);

  // The engine inherits the prepared home, so the SFTP host key is known
  // and the suite never touches the real ~/.ssh.
  const environment = { ...process.env, IDM_STATE_DIR: stateDir, HOME: home, USERPROFILE: home };
  const engine = spawn(IDM, ["--serve", "-o", downloads], {
    stdio: ["ignore", "pipe", "pipe"],
    env: environment,
  });
  const log = [];
  engine.stdout.on("data", (d) => log.push(d.toString()));
  engine.stderr.on("data", (d) => log.push(d.toString()));
  await sleep(2000);

  try {
    const cases = [
      ["http", `http://127.0.0.1:${HTTP_PORT}/honest?size=${SIZE}`, "http.bin"],
      ["ftp", `ftp://127.0.0.1:${FTP_PORT}/${NAME}`, "ftp.bin"],
      // The stub server accepts a password; a key would need one installed
      // on it, which is a different thing from what this test is for.
      ["sftp", `sftp://tester@127.0.0.1:${SFTP_PORT}/${NAME}`, "sftp.bin"],
    ];

    for (const [scheme, url, name] of cases) {
      const sent = spawnSync(IDM, ["--send", url, "--name", name], {
        encoding: "utf8",
        env: environment,
      });
      const accepted = sent.status === 0;
      check(
        `the engine accepts an ${scheme} url`,
        accepted,
        (sent.stderr || sent.stdout).trim().split("\n")[0],
      );
      if (!accepted) continue;

      // Accepting it is a promise to try it, so the file has to appear.
      let landed = false;
      for (let i = 0; i < 80 && !landed; i += 1) {
        await sleep(250);
        const path = join(downloads, name);
        landed = existsSync(path) && readFileSync(path).length === SIZE;
      }

      if (scheme === "sftp" && !haveKey) {
        // Without ssh-keygen there is no key to offer, so only the routing
        // can be checked. Said plainly rather than passing quietly.
        check(
          "an sftp url is routed to the sftp code",
          !log.join("").includes("builder error"),
          "no key available, so the download itself was not checked",
        );
        continue;
      }

      check(`an ${scheme} download completes`, landed, landed ? "" : "never appeared");
      if (landed) {
        check(
          `the ${scheme} bytes are correct`,
          readFileSync(join(downloads, name)).equals(expected(SIZE)),
          `${SIZE} bytes`,
        );
      }
    }

    // The failure this test was written for: a scheme accepted and then
    // handed to a client that cannot speak it.
    check(
      "no url was accepted and then given to the wrong client",
      !log.join("").includes("builder error"),
      log.join("").includes("builder error") ? "found a builder error" : "none",
    );
  } finally {
    engine.kill();
    for (const server of servers) server.kill();
    await sleep(600);
    for (const dir of [downloads, stateDir, home]) {
      try {
        rmSync(dir, { recursive: true, force: true, maxRetries: 3 });
      } catch {
        // The engine may still hold the database open.
      }
    }
    if (failures) console.log("\nengine output:\n" + log.join("").trim());
  }

  console.log(`\n${failures ? failures + " check(s) failed" : "all checks passed"}`);
  process.exit(failures ? 1 : 0);
}

main().catch((error) => {
  console.error(error);
  process.exit(1);
});
