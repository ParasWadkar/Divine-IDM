// Checks that every setting the engine has is one the window sends back.
//
// This exists because of a trap that has bitten twice, and the second time
// cost an afternoon in the real window. `saveSettings` does not send the one
// control that changed: it rebuilds the WHOLE settings object from the panel
// and sends that. So a field with no control, and no module-level mirror of
// its value, is sent as whatever the object literal happens to put there --
// and the engine fills anything missing from `Default`. The symptom is that
// changing any setting silently resets an unrelated one. That is how browser
// integration used to switch itself back on, and how the download folder
// would revert.
//
// Reading the two files is the whole check. Both are the ones that ship:
// the Rust struct the engine deserialises, and the literal the window sends.
//
//   node settings-wiring.mjs

import { readFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const REPO = resolve(dirname(fileURLToPath(import.meta.url)), "..", "..");
const protocol = readFileSync(
  join(REPO, "crates", "idm-ipc", "src", "protocol.rs"),
  "utf8",
);
const app = readFileSync(join(REPO, "app", "ui", "app.js"), "utf8");

let failures = 0;
function check(ok, label, detail = "") {
  console.log(`  ${ok ? "PASS" : "FAIL"}  ${label}${detail ? `  — ${detail}` : ""}`);
  if (!ok) failures += 1;
}

/** The body of a braced block starting at `from`, balanced rather than greedy. */
function blockAt(source, from) {
  if (from < 0) throw new Error("asked for a block that is not there");
  const open = source.indexOf("{", from);
  let depth = 0;
  for (let i = open; i < source.length; i += 1) {
    if (source[i] === "{") depth += 1;
    else if (source[i] === "}") {
      depth -= 1;
      if (depth === 0) return source.slice(open + 1, i);
    }
  }
  throw new Error("unbalanced braces");
}

// ---------------------------------------------------------------------------
// What the engine has
// ---------------------------------------------------------------------------

const structAt = protocol.indexOf("pub struct Settings {");
if (structAt < 0) {
  console.error("could not find `pub struct Settings` in protocol.rs");
  process.exit(1);
}
const fields = [...blockAt(protocol, structAt).matchAll(/^\s*pub (\w+):/gm)].map(
  (match) => match[1],
);

if (fields.length === 0) {
  console.error("found the Settings struct but no fields in it");
  process.exit(1);
}

// ---------------------------------------------------------------------------
// What the window sends
// ---------------------------------------------------------------------------

const saveAt = app.indexOf("async function saveSettings(");
if (saveAt < 0) {
  console.error("could not find saveSettings() in app.js");
  process.exit(1);
}
// From the end of the signature, not the start: the parameter list holds a
// `{}` of its own (`overrides = {}`), and matching from the name would take
// that empty object for the whole function body.
const save = blockAt(app, app.indexOf(") {", saveAt));
const literalAt = save.indexOf("const settings = {");
const literal = blockAt(save, literalAt);
// field -> the expression its value comes from, so the check below can tell
// a control from a module-level variable. Every entry in this literal is one
// line; a value spread over several would need more than this.
const sent = new Map(
  [...literal.matchAll(/^\s*(\w+):\s*(.+?),?\s*$/gm)].map((match) => [match[1], match[2]]),
);

console.log("Settings: engine ↔ window\n");
console.log(`  the engine has ${fields.length} settings; the window sends ${sent.size}\n`);

for (const field of fields) {
  check(
    sent.has(field),
    field,
    sent.has(field) ? "" : "not in the object saveSettings() builds, so every save resets it",
  );
}

// The other half of the trap: a field sent from a module-level variable
// rather than a control is only correct while something keeps that variable
// current. `renderSettings` does that, and does it *before* its early return,
// because the early return fires whenever the panel has focus.
const renderAt = app.indexOf("function renderSettings(");
const render = blockAt(app, app.indexOf(") {", renderAt));
const guard = render.indexOf("return;");
const beforeGuard = render.slice(0, guard);

for (const field of fields) {
  // Read straight out of the panel, so it cannot go stale.
  if ((sent.get(field) ?? "").includes("$(")) continue;
  // Sent from a variable. That variable has to be refreshed from the
  // snapshot unconditionally, or it goes stale and the next save writes the
  // stale value over the real one.
  check(
    beforeGuard.includes(`settings?.${field}`),
    `${field} (sent from a variable, not a control)`,
    beforeGuard.includes(`settings?.${field}`)
      ? ""
      : "renderSettings() must refresh it before its early return, or it goes stale",
  );
}

console.log(`\nsettings-wiring: ${failures === 0 ? "PASS" : `FAIL (${failures})`}`);
process.exit(failures === 0 ? 0 : 1);
