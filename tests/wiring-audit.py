"""Finds public items that nothing outside their own file ever calls.

Rust's dead_code lint stays quiet about `pub` items in a library, because
anything outside the crate might use them. In a workspace with no external
users that silence is a blind spot, and it has already hidden three
features that were written, tested, and never wired up: the post-download
action, the connection-count profile, and the FTP resume fallback.

This is a heuristic, not a proof. It counts references from outside the
defining file, ignoring test modules and doc comments. Anything it reports
needs a human look: a trait method is called through the trait, and a
constructor may exist for a caller yet to be written.
"""

import pathlib
import re
import sys
from collections import defaultdict

ROOT = pathlib.Path(r"D:\_Divine\Projects\Active\IDM")
SOURCE_DIRS = [ROOT / "crates", ROOT / "app"]

# Names too common or too conventional to say anything.
IGNORE = {
    "new", "default", "fmt", "clone", "from", "into", "drop", "eq", "hash",
    "next", "len", "is_empty", "iter", "get", "set", "run", "handle", "build",
    "main", "parse", "read", "write", "close", "open", "flush", "init",
}


def rust_files():
    for base in SOURCE_DIRS:
        for path in base.rglob("*.rs"):
            if "target" in path.parts:
                continue
            yield path


def strip_tests_and_comments(text: str) -> str:
    """Removes doc comments and #[cfg(test)] modules.

    A reference from a test does not count: the whole point is to find
    things only the tests use.
    """
    out = []
    depth = None
    brace = 0
    for line in text.split(chr(10)):
        stripped = line.strip()

        if depth is None and re.match(r"#\[cfg\(test\)\]", stripped):
            depth = 0
            continue
        if depth is not None:
            brace += line.count("{") - line.count("}")
            if brace <= 0 and "{" in line:
                # Module opened and closed on one line.
                depth = None
                brace = 0
            elif brace <= 0 and depth == 0 and "}" in line:
                depth = None
                brace = 0
            continue

        if stripped.startswith("//"):
            continue
        out.append(line)
    return chr(10).join(out)


def main():
    # name -> list of (file, kind)
    declared = defaultdict(list)
    bodies = {}

    declaration = re.compile(
        r"^\s*pub(?:\(crate\))?\s+(?:async\s+)?(fn|struct|enum|trait|const)\s+([A-Za-z_][A-Za-z0-9_]*)"
    )

    for path in rust_files():
        text = path.read_text(encoding="utf-8", errors="replace")
        bodies[path] = strip_tests_and_comments(text)
        for line in text.split(chr(10)):
            match = declaration.match(line)
            if match:
                kind, name = match.group(1), match.group(2)
                if name in IGNORE or name.startswith("_"):
                    continue
                declared[name].append((path, kind))

    unused = []
    for name, sites in sorted(declared.items()):
        pattern = re.compile(r"\b" + re.escape(name) + r"\b")
        owners = {path for path, _ in sites}

        external = 0
        for path, body in bodies.items():
            if path in owners:
                continue
            external += len(pattern.findall(body))

        if external == 0:
            for path, kind in sites:
                unused.append((str(path.relative_to(ROOT)), kind, name))

    print(f"{len(unused)} public item(s) with no reference outside their own file:" + chr(10))
    by_file = defaultdict(list)
    for file, kind, name in unused:
        by_file[file].append(f"{kind} {name}")
    for file in sorted(by_file):
        print(f"  {file}")
        for item in sorted(by_file[file]):
            print(f"      {item}")


if __name__ == "__main__":
    main()
