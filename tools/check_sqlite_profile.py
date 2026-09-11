#!/usr/bin/env python3
"""Charter gate (ADR 0000, db-core#324): the SQLite profile is what it claims.

The *SQLite profile* is the exact feature set sqlite-rs builds db-core with.
Everything that profile compiles is on the safe-SQLite audit path, so three
facts about it are checked here -- measured from what `cargo` actually
compiles, never from a hand-maintained module list:

  (a) dependency closure: the profile's `cargo tree` is first-party only;
  (b) `unsafe`: exactly the named carve-outs, with their exact counts;
  (c) mode isolation: no batch / column / stream module is compiled at all.

The compiled-file set comes from rustc's dep-info (`--emit=dep-info`), so a
new `mod` or a feature implication that drags another mode in shows up as a
concrete file, with no way to hide behind the feature graph. This is the
check that would have caught `codegen-row = [.., "vm-batch"]` compiling
`vm/batch.rs` into every sqlite-rs binary for months.

Usage: python3 tools/check_sqlite_profile.py   (exit 1 on any violation)
"""

import glob
import os
import re
import subprocess
import sys
import tempfile

PROFILE = ["parser-row", "vm-row", "codegen-row", "storage-row", "engine-row"]

# Every crate the profile may resolve to. Nothing from crates.io.
FIRST_PARTY = {"db-core"}

# The named `unsafe` carve-outs: file -> number of `unsafe` sites. Adding a
# site anywhere on the profile is an ADR 0000 decision, not a lint fix.
ALLOWED_UNSAFE = {
    # ADR-0031 (sqlite-rs) vendored fcntl FFI: `fsync`, `fcntl` byte-range locks.
    "src/storage/row/vfs/fcntl.rs": 2,
}

# Paths that must never appear in the profile's compiled set (ADR 0000 §Invariants (c)).
OTHER_MODES = re.compile(
    r"^src/(vm/batch|vm/engine|vm/stream|codegen/batch|storage/column|storage/stream|engine/column|engine/stream)"
)

UNSAFE = re.compile(r"\bunsafe (\{|fn|impl|extern)")
COMMENT = re.compile(r"^\s*//")

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))


def run(args: list[str]) -> str:
    proc = subprocess.run(args, cwd=ROOT, capture_output=True, text=True)
    if proc.returncode != 0:
        print(f"check-sqlite-profile: `{' '.join(args)}` failed:\n{proc.stderr}")
        sys.exit(1)
    return proc.stdout


def compiled_files() -> list[str]:
    features = ",".join(PROFILE)
    with tempfile.TemporaryDirectory() as tmp:
        out = os.path.join(tmp, "profile")
        run(
            [
                "cargo", "rustc", "-q", "--lib", "--no-default-features",
                "--features", features, "--", "--emit=dep-info", "-o", out,
            ]
        )
        dep_files = glob.glob(os.path.join(tmp, "*.d"))
        if not dep_files:
            print("check-sqlite-profile: rustc emitted no dep-info file")
            sys.exit(1)
        with open(dep_files[0], encoding="utf-8") as f:
            text = f.read()
    # rustc writes source paths relative to the crate root (`src/lib.rs`);
    # tolerate absolute ones too.
    files = set()
    for token in text.replace("\\\n", " ").split():
        if not token.endswith(".rs"):
            continue
        rel = os.path.relpath(token, ROOT) if os.path.isabs(token) else token
        if rel.startswith("src/"):
            files.add(rel)
    return sorted(files)


def dependency_closure() -> list[str]:
    features = ",".join(PROFILE)
    out = run(
        ["cargo", "tree", "-e", "normal", "--no-default-features", "--features", features, "--prefix", "none"]
    )
    crates = set()
    for line in out.splitlines():
        name = line.strip().split(" ")[0]
        if name:
            crates.add(name)
    return sorted(crates)


def unsafe_sites(path: str) -> list[int]:
    sites = []
    with open(os.path.join(ROOT, path), encoding="utf-8") as f:
        for lineno, line in enumerate(f, 1):
            if COMMENT.match(line):
                continue
            if UNSAFE.search(line):
                sites.append(lineno)
    return sites


def main() -> int:
    violations: list[str] = []

    crates = dependency_closure()
    foreign = [c for c in crates if c not in FIRST_PARTY]
    if foreign:
        violations.append(
            "(a) dependency closure is not first-party only: " + ", ".join(foreign)
        )

    files = compiled_files()
    if not files:
        violations.append("no compiled files found for the profile -- dep-info parsing failed")

    other = [p for p in files if OTHER_MODES.match(p)]
    if other:
        violations.append(
            "(c) another execution mode is compiled into the SQLite profile:\n    "
            + "\n    ".join(other)
        )

    found: dict[str, list[int]] = {}
    for p in files:
        sites = unsafe_sites(p)
        if sites:
            found[p] = sites
    for p, sites in sorted(found.items()):
        allowed = ALLOWED_UNSAFE.get(p)
        if allowed is None:
            violations.append(
                f"(b) `unsafe` outside the named carve-outs: {p} lines {sites}"
            )
        elif len(sites) != allowed:
            violations.append(
                f"(b) `unsafe` count changed in {p}: {len(sites)} sites {sites}, charter allows {allowed}"
            )
    for p in ALLOWED_UNSAFE:
        if p in files and p not in found:
            violations.append(
                f"(b) named carve-out {p} has no `unsafe` left -- remove it from ALLOWED_UNSAFE"
            )

    if violations:
        print("check-sqlite-profile: the SQLite profile violates ADR 0000:")
        for v in violations:
            print(f"  {v}")
        return 1

    unsafe_total = sum(len(s) for s in found.values())
    print(
        f"check-sqlite-profile: ok -- {len(files)} files compiled, closure = "
        f"{{{', '.join(crates)}}}, unsafe = {unsafe_total} site(s) in {len(found)} named file(s), "
        f"no batch/column/stream module"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
