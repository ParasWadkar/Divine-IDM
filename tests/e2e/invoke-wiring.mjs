// Fails if the window calls a Tauri command that does not exist, or passes it
// an argument by a name the Rust side will not recognise.
//
// This exists because of a bug that passed every test and every lint: the
// Schedule editor sent
//
//   { days, start_minute, end_minute, bytes_per_sec, stop }
//
// and Tauri renames command arguments to camelCase, so the Rust parameter is
// `startMinute`. The call failed at run time with "invalid args", which looks
// to somebody using the app like the Save button doing nothing. Nothing on
// either side of the boundary could see it: the JS is valid, the Rust is
// valid, and the only thing wrong is that the two disagree about a name.
//
// So this reads both sides and compares them. Deliberately a text scan rather
// than anything clever -- no build step, no npm, and a parser for two
// languages would be a worse thing to maintain than the bug it prevents.
//
//   node invoke-wiring.mjs

import { readFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const REPO = resolve(dirname(fileURLToPath(import.meta.url)), "..", "..");
const UI = join(REPO, "app", "ui", "app.js");
const MAIN = join(REPO, "app", "src-tauri", "src", "main.rs");

let failures = 0;
function check(label, ok, detail = "") {
  console.log(`${ok ? "  ok  " : "  FAIL"}  ${label}${detail ? ` — ${detail}` : ""}`);
  if (!ok) failures += 1;
}

console.log("invoke-wiring");

const rust = readFileSync(MAIN, "utf8");
const js = readFileSync(UI, "utf8");

const line = (text, at) => text.slice(0, at).split("\n").length;
const camelOf = (name) => name.replace(/_([a-z])/g, (_, letter) => letter.toUpperCase());
const snakeOf = (name) => name.replace(/[A-Z]/g, (letter) => `_${letter.toLowerCase()}`);

/** Reads from `at` (which must be an opener) to its match. */
function balanced(text, at, open, close) {
  let depth = 0;
  for (let i = at; i < text.length; i += 1) {
    if (text[i] === open) depth += 1;
    else if (text[i] === close) {
      depth -= 1;
      if (depth === 0) return i;
    }
  }
  return -1;
}

/** Splits an argument list on the commas that are not inside <>, () or []. */
function topLevelPieces(text) {
  const pieces = [];
  let depth = 0;
  let current = "";
  for (const character of text) {
    if ("<([".includes(character)) depth += 1;
    else if (">)]".includes(character)) depth -= 1;
    if (character === "," && depth === 0) {
      pieces.push(current);
      current = "";
      continue;
    }
    current += character;
  }
  pieces.push(current);
  return pieces;
}

// --- what Rust offers ------------------------------------------------------

/** Arguments Tauri supplies itself, which the window never passes. */
const INJECTED = new Set(["state", "app", "window", "webview"]);

/** Command name -> the argument names the window may pass, camelCased. */
const commands = new Map();

const ATTRIBUTE = "#[tauri::command]";
for (let at = rust.indexOf(ATTRIBUTE); at !== -1; at = rust.indexOf(ATTRIBUTE, at + 1)) {
  // The next `fn name(` after the attribute. Anything between the two is an
  // attribute of its own, which never contains `fn `.
  const signature = /\bfn\s+([a-z_0-9]+)\s*\(/.exec(rust.slice(at, at + 400));
  if (!signature) {
    check("every #[tauri::command] is followed by a function", false, `main.rs:${line(rust, at)}`);
    continue;
  }
  const name = signature[1];
  const open = at + signature.index + signature[0].length - 1;
  const close = balanced(rust, open, "(", ")");
  if (close === -1) {
    check(`${name}'s argument list is readable`, false, `main.rs:${line(rust, at)}`);
    continue;
  }

  const allowed = new Set();
  for (const piece of topLevelPieces(rust.slice(open + 1, close))) {
    const bare = piece.trim().split(":")[0].trim();
    if (!bare || INJECTED.has(bare)) continue;
    // camelCase only: Tauri renames command arguments, so the snake_case
    // spelling in the Rust signature is not a name the window may send.
    allowed.add(camelOf(bare));
  }
  commands.set(name, allowed);
}

check("the command list was read from main.rs", commands.size > 10, `${commands.size} commands`);

// --- what is registered ----------------------------------------------------

const handlerAt = rust.indexOf("generate_handler![");
check("generate_handler! was found", handlerAt !== -1);
const handlerEnd = handlerAt === -1 ? -1 : balanced(rust, rust.indexOf("[", handlerAt), "[", "]");
const registered = new Set(
  (handlerAt === -1 ? "" : rust.slice(rust.indexOf("[", handlerAt) + 1, handlerEnd))
    .split(",")
    .map((name) => name.trim())
    .filter((name) => /^[a-z_0-9]+$/.test(name)),
);

const unregistered = [...commands.keys()].filter((name) => !registered.has(name));
check(
  "every command is in generate_handler!",
  unregistered.length === 0,
  unregistered.join(", "),
);

// --- what the window calls -------------------------------------------------

const calls = [];
let skipped = 0;
const invocation = /\binvoke\(\s*"([a-z_0-9]+)"\s*(,)?/g;
for (const match of js.matchAll(invocation)) {
  const name = match[1];
  const after = match.index + match[0].length;
  if (!match[2]) {
    calls.push({ name, keys: [], at: match.index });
    continue;
  }
  const open = js.indexOf("{", after);
  const closeParen = js.indexOf(")", after);
  if (open === -1 || (closeParen !== -1 && closeParen < open)) {
    // Arguments built elsewhere and passed by variable. Nothing to compare
    // here; the spelling pass below is what covers these.
    skipped += 1;
    calls.push({ name, keys: null, at: match.index });
    continue;
  }

  const end = balanced(js, open, "{", "}");
  const keys = [];
  for (const piece of topLevelPieces(js.slice(open + 1, end === -1 ? js.length : end))) {
    const key =
      /^\s*([A-Za-z_$][\w$]*)\s*:/.exec(piece) ?? /^\s*([A-Za-z_$][\w$]*)\s*$/.exec(piece);
    if (key) keys.push(key[1]);
  }
  calls.push({ name, keys, at: match.index });
}

check("the window's invoke calls were found", calls.length > 10, `${calls.length} calls`);

const seen = new Set();
for (const call of calls) {
  if (!seen.has(call.name)) {
    seen.add(call.name);
    check(
      `the window's "${call.name}" is a real command`,
      commands.has(call.name),
      commands.has(call.name)
        ? ""
        : `app.js:${line(js, call.at)} — no #[tauri::command] by that name`,
    );
  }
  if (!commands.has(call.name) || call.keys === null) continue;

  const allowed = commands.get(call.name);
  for (const key of call.keys) {
    check(
      `"${call.name}" is passed ${key}`,
      allowed.has(key),
      allowed.has(key)
        ? ""
        : `app.js:${line(js, call.at)} — it takes ${[...allowed].join(", ") || "no arguments"}`,
    );
  }
}

// --- the spelling, wherever it is written ----------------------------------
//
// The pass above only reaches calls whose arguments are an object literal
// written at the call. The bug that prompted all this was in a call whose
// arguments came from a variable built three functions away, which no text
// scan is going to follow. So this looks for the mistake itself: a
// snake_case object key anywhere in the window that is the Rust spelling of
// a command argument, which is never a name Tauri will accept.
//
// Property *reads* are not keys, which is what keeps the engine's own
// snake_case replies -- `rule.start_minute`, `entry.is_directory` -- out of
// this.

const renamed = new Map();
for (const [command, allowed] of commands) {
  for (const name of allowed) {
    const snake = snakeOf(name);
    if (snake !== name) renamed.set(snake, { command, camel: name });
  }
}

check("some arguments are renamed by Tauri at all", renamed.size > 0, `${renamed.size} of them`);

for (const [snake, { command, camel }] of renamed) {
  const found = new RegExp(`(^|[{,\\s])${snake}\\s*:`, "m").exec(js);
  check(
    `${snake} is not written as an argument name`,
    found === null,
    found === null ? "" : `app.js:${line(js, found.index)} — ${command} takes ${camel}`,
  );
}

console.log(
  `\ninvoke-wiring: ${failures === 0 ? "PASS" : `${failures} check(s) failed`}` +
    (skipped > 0 ? ` (${skipped} call(s) pass their arguments by variable)` : ""),
);
process.exit(failures === 0 ? 0 : 1);
