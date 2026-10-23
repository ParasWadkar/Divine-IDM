// Checks how the window writes a length of time.
//
// Reads the real function out of app/ui/app.js and runs it, so this tests what
// ships rather than a copy that could drift from it. The window itself needs
// the Tauri runtime to load, which is why it is extracted and not imported.
//
// The case that prompted it: a long download on a slow link read "51h 20m",
// which makes the reader do the arithmetic to learn it is more than two days.
// Past 24 hours it now says "2d 3h".
//
//   node duration.mjs

import { readFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const REPO = resolve(dirname(fileURLToPath(import.meta.url)), "..", "..");
const source = readFileSync(join(REPO, "app", "ui", "app.js"), "utf8");

const start = source.indexOf("function duration(seconds) {");
const end = source.indexOf("\n}\n", start) + 3;
if (start < 0) {
  console.error("duration() not found in app.js");
  process.exit(1);
}
const duration = new Function(`${source.slice(start, end)}\nreturn duration;`)();

let failures = 0;
function expect(seconds, wanted, why = "") {
  const got = duration(seconds);
  const ok = got === wanted;
  console.log(`  ${ok ? "PASS" : "FAIL"}  ${String(seconds).padStart(9)}s -> ${got.padEnd(8)}${ok ? "" : ` (wanted ${wanted})`} ${why}`);
  if (!ok) failures += 1;
}

console.log("duration()\n");
expect(0, "\u2014", "nothing to show");
expect(NaN, "\u2014", "not a number");
expect(-5, "\u2014", "negative");
expect(Infinity, "\u2014", "a stalled download has no estimate");
expect(5, "5s");
expect(59.2, "1m 0s", "rounded up as one number, not 0m 59s");
expect(61, "1m 1s");
expect(3599.4, "1h 0m", "never \"59m 60s\" or \"1h 60m\"");
expect(3600, "1h 0m");
expect(12000, "3h 20m");
expect(86399, "1d 0h", "just under a day rounds to a day, not \"24h 0m\"");
expect(86400, "1d 0h");
expect(90000, "1d 1h", "25 hours");
expect(51 * 3600 + 20 * 60, "2d 3h", "the case that prompted this: 51h 20m");
expect(10 * 86400, "10d 0h");
expect(400 * 86400, "400d 0h", "does not roll over into anything else");

console.log(failures === 0 ? "\nduration: PASS" : `\nduration: ${failures} FAILED`);
process.exit(failures === 0 ? 0 : 1);
