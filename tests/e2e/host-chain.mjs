// Drives the native messaging host exactly as a browser does, and walks the
// whole chain: pairing request, approval, download, bytes on disk.
//
// Chrome's `sendNativeMessage` spawns a *fresh host process per message* and
// closes it after one reply. That detail matters enough to reproduce faithfully
// here rather than hold one process open: a host that remembers anything
// between messages works when tested with a persistent connection and fails
// the moment a real browser uses it.
//
//   node host-chain.mjs

import { spawn, spawnSync } from "node:child_process";
import { existsSync, mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const HERE = dirname(fileURLToPath(import.meta.url));
const REPO = resolve(HERE, "..", "..");
const IDM = join(REPO, "target", "release", "idm.exe");
const HOST = join(REPO, "target", "release", "idm-host.exe");
const MOCK = join(REPO, "tests", "mock-server", "server.mjs");

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
let failures = 0;

function check(label, ok, detail = "") {
  console.log(`  ${ok ? "PASS" : "FAIL"}  ${label}${detail ? "  — " + detail : ""}`);
  if (!ok) failures += 1;
  return ok;
}

/** One message to a freshly spawned host, one reply, as Chrome does it. */
function askHost(message) {
  return new Promise((resolve_) => {
    const child = spawn(HOST, ["chrome-extension://testextensionidtestextensionid/"], {
      stdio: ["pipe", "pipe", "pipe"],
    });
    const chunks = [];
    child.stdout.on("data", (c) => chunks.push(c));

    const body = Buffer.from(JSON.stringify(message), "utf8");
    const header = Buffer.alloc(4);
    header.writeUInt32LE(body.length); // native byte order on x64
    child.stdin.write(Buffer.concat([header, body]));

    const done = () => {
      const all = Buffer.concat(chunks);
      if (all.length < 4) return resolve_({ type: "no_reply" });
      const size = all.readUInt32LE(0);
      try {
        resolve_(JSON.parse(all.subarray(4, 4 + size).toString("utf8")));
      } catch (error) {
        resolve_({ type: "unparseable", error: String(error) });
      }
    };

    // Give the host a moment to answer, then close stdin so it exits.
    setTimeout(() => {
      child.stdin.end();
      setTimeout(done, 400);
    }, 900);
  });
}

function startMock(port, size) {
  const child = spawn("node", [MOCK, "--port", String(port)], {
    stdio: ["ignore", "pipe", "ignore"],
  });
  return new Promise((resolve_) => {
    child.stdout.on("data", (d) => {
      if (d.toString().includes("listening")) resolve_(child);
    });
    setTimeout(() => resolve_(child), 3000);
  });
}

async function main() {
  for (const [label, path] of [["idm.exe", IDM], ["idm-host.exe", HOST]]) {
    if (!existsSync(path)) {
      console.error(`missing ${label}; run: cargo build --release --workspace`);
      process.exit(1);
    }
  }

  const downloads = mkdtempSync(join(tmpdir(), "idm-e2e-"));
  // Its own state directory. Sharing the real one means a run that dies
  // partway can leave settings behind that break every later run, and can
  // damage a real install's pairings and history.
  const stateDir = mkdtempSync(join(tmpdir(), "idm-state-"));
  process.env.IDM_STATE_DIR = stateDir;
  const port = 8123;
  const size = 4 * 1024 * 1024;

  console.log("Divine IDM — end-to-end chain\n");
  const mock = await startMock(port, size);
  const engine = spawn(IDM, ["--serve", "-o", downloads], {
    stdio: ["ignore", "pipe", "pipe"],
  });
  const engineLog = [];
  engine.stderr.on("data", (d) => engineLog.push(d.toString()));
  await sleep(1200);

  try {
    // 1. An unknown extension says hello and must get nothing but "pending".
    const hello = await askHost({
      type: "hello",
      client_id: "e2e-test-client",
      label: "Chrome on this PC",
    });
    const pending = check(
      "unknown extension gets pairing_pending, not access",
      hello.type === "pairing_pending",
      JSON.stringify(hello),
    );
    const requestId = hello.request_id;

    // 2. Before approval, a download must be refused.
    const early = await askHost({
      type: "download",
      client_id: "e2e-test-client",
      url: `http://127.0.0.1:${port}/honest?size=${size}`,
      size_hint: size,
    });
    check(
      "download refused while unpaired",
      early.type === "declined",
      JSON.stringify(early),
    );

    // 3. Approve from the CLI, which the engine treats as a non-browser client.
    if (pending && requestId) {
      const approved = spawnSync(IDM, ["--approve", requestId], { encoding: "utf8" });
      check(
        "CLI can approve the pending request",
        /approved/i.test(approved.stderr + approved.stdout),
        (approved.stderr || approved.stdout).trim().split("\n")[0],
      );
    }

    // 4. Now the same extension should be accepted.
    const afterPairing = await askHost({
      type: "hello",
      client_id: "e2e-test-client",
      label: "Chrome on this PC",
    });
    check(
      "paired extension is greeted as ready",
      afterPairing.type === "ready",
      JSON.stringify(afterPairing),
    );

    // 5. And a download should be taken.
    const queued = await askHost({
      type: "download",
      client_id: "e2e-test-client",
      url: `http://127.0.0.1:${port}/honest?size=${size}`,
      filename: "chain.bin",
      size_hint: size,
    });
    check("download accepted once paired", queued.type === "accepted", JSON.stringify(queued));

    // 6. The bytes should land, correct.
    let landed = false;
    const target = join(downloads, "chain.bin");
    for (let i = 0; i < 60 && !landed; i += 1) {
      await sleep(250);
      landed = existsSync(target) && readFileSync(target).length === size;
    }
    check("file lands with the right size", landed, landed ? "" : "never appeared");

    if (landed) {
      const got = readFileSync(target);
      let ok = true;
      for (let i = 0; i < got.length && ok; i += 4093) {
        let x = (i + 0x9e3779b9) | 0;
        x = Math.imul(x ^ (x >>> 16), 0x85ebca6b);
        x = Math.imul(x ^ (x >>> 13), 0xc2b2ae35);
        x = x ^ (x >>> 16);
        if (got[i] !== (x & 0xff)) ok = false;
      }
      check("file contents verify against the generator", ok);
    }

    // 7. Revocation must bite immediately.
    spawnSync(IDM, ["--revoke", "e2e-test-client"], { encoding: "utf8" });
    const afterRevoke = await askHost({
      type: "download",
      client_id: "e2e-test-client",
      url: `http://127.0.0.1:${port}/honest?size=${size}`,
      size_hint: size,
    });
    check(
      "revoked extension is refused again",
      afterRevoke.type === "declined",
      JSON.stringify(afterRevoke),
    );
  } finally {
    engine.kill();
    mock.kill();
    for (const dir of [downloads, stateDir]) {
      try {
        rmSync(dir, { recursive: true, force: true, maxRetries: 3 });
      } catch {
        // The engine may still hold the database open. A temporary
        // directory left behind is not worth failing a run over.
      }
    }
    if (failures && engineLog.length) {
      console.log("\nengine output:\n" + engineLog.join("").trim());
    }
  }

  console.log(`\n${failures ? failures + " check(s) failed" : "all checks passed"}`);
  process.exit(failures ? 1 : 0);
}

main().catch((error) => {
  console.error(error);
  process.exit(1);
});
