#!/usr/bin/env python3
"""Charter gate (ADR 0000, db-core#324, db-core#403): a named profile is what it claims.

A *profile* is a feature set some downstream consumer builds db-core with.
Everything a profile compiles is on that consumer's audit path, so two facts
about it are checked here -- measured from what `cargo` actually compiles,
never from a hand-maintained module list:

  (a) dependency closure: the profile's `cargo tree` is first-party only;
  (b) `unsafe`: exactly the named carve-outs, with their exact counts.

The SQLite profile additionally checks:

  (c) mode isolation: no batch / column / stream module is compiled at all.

The compiled-file set comes from rustc's dep-info (`--emit=dep-info`), so a
new `mod` or a feature implication that drags another mode in shows up as a
concrete file, with no way to hide behind the feature graph. This is the
check that would have caught `codegen-row = [.., "vm-batch"]` compiling
`vm/batch.rs` into every sqlite-rs binary for months.

Usage: python3 tools/check_sqlite_profile.py [PROFILE]   (exit 1 on any violation)

PROFILE defaults to "sqlite" and must be a key of PROFILES below.
"""

import glob
import os
import re
import subprocess
import sys
import tempfile

# Every crate any profile may resolve to. Nothing from crates.io.
FIRST_PARTY = {"db-core"}

UNSAFE = re.compile(r"\bunsafe (\{|fn|impl|extern)")
COMMENT = re.compile(r"^\s*//")

# Paths that must never appear in the SQLite profile's compiled set
# (ADR 0000 §Invariants (c)).
OTHER_MODES = re.compile(
    r"^src/(vm/batch|vm/engine|vm/stream|codegen/batch|storage/column|storage/stream|engine/column|engine/stream)"
)

PROFILES = {
    "sqlite": {
        "features": ["parser-row", "vm-row", "codegen-row", "storage-row", "engine-row"],
        # The named `unsafe` carve-outs: file -> number of `unsafe` sites.
        # Adding a site anywhere on the profile is an ADR 0000 decision,
        # not a lint fix.
        "allowed_unsafe": {
            # ADR-0031 (sqlite-rs) vendored fcntl FFI: `fsync`, `fcntl` byte-range locks.
            "src/storage/row/vfs/fcntl.rs": 2,
        },
        "other_modes": OTHER_MODES,
    },
    # db-core#403: the log engine's execution mode -- measured to check
    # ADR-0000's invariants (a)/(b) hold here too, not just on the SQLite
    # side. No mode-isolation check: the stream profile legitimately
    # compiles vm-batch/codegen-batch (its planner reuses their types).
    "stream": {
        "features": [
            "parser-column",
            "vm-batch",
            "vm-stream",
            "codegen-batch",
            "codegen-stream",
            "storage-stream",
            "engine-stream",
        ],
        "allowed_unsafe": {},
        "other_modes": None,
    },
    # db-core#405: the Parquet analytics mode. Unlike the SQLite/stream
    # profiles, this one is not first-party-only by design (ADR 0000 §The
    # column profile) -- memmap2/ruzstd are accepted third-party
    # dependencies, so invariant (a) is not checked here. Only (b) is.
    "column": {
        "features": ["parser-column", "storage-column", "engine-column"],
        "allowed_unsafe": {
            # ADR-0016/ADR-0000: `memmap2::Mmap::map`, Safety comment at
            # the call site.
            "src/storage/column/mmap.rs": 1,
        },
        "other_modes": None,
        "check_dependency_closure": False,
    },
}

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))


def run(name: str, args: list[str]) -> str:
    proc = subprocess.run(args, cwd=ROOT, capture_output=True, text=True)
    if proc.returncode != 0:
        print(f"check-{name}-profile: `{' '.join(args)}` failed:\n{proc.stderr}")
        sys.exit(1)
    return proc.stdout


def compiled_files(name: str, features: list[str]) -> list[str]:
    feature_str = ",".join(features)
    with tempfile.TemporaryDirectory() as tmp:
        out = os.path.join(tmp, "profile")
        run(
            name,
            [
                "cargo", "rustc", "-q", "--lib", "--no-default-features",
                "--features", feature_str, "--", "--emit=dep-info", "-o", out,
            ],
        )
        dep_files = glob.glob(os.path.join(tmp, "*.d"))
        if not dep_files:
            print(f"check-{name}-profile: rustc emitted no dep-info file")
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


def dependency_closure(name: str, features: list[str]) -> list[str]:
    feature_str = ",".join(features)
    out = run(
        name,
        ["cargo", "tree", "-e", "normal", "--no-default-features", "--features", feature_str, "--prefix", "none"],
    )
    crates = set()
    for line in out.splitlines():
        crate_name = line.strip().split(" ")[0]
        if crate_name:
            crates.add(crate_name)
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


def check_profile(name: str, profile: dict) -> int:
    features = profile["features"]
    allowed_unsafe = profile["allowed_unsafe"]
    other_modes = profile["other_modes"]

    violations: list[str] = []

    crates = dependency_closure(name, features)
    if profile.get("check_dependency_closure", True):
        foreign = [c for c in crates if c not in FIRST_PARTY]
        if foreign:
            violations.append(
                "(a) dependency closure is not first-party only: " + ", ".join(foreign)
            )

    files = compiled_files(name, features)
    if not files:
        violations.append("no compiled files found for the profile -- dep-info parsing failed")

    if other_modes is not None:
        other = [p for p in files if other_modes.match(p)]
        if other:
            violations.append(
                "(c) another execution mode is compiled into the profile:\n    "
                + "\n    ".join(other)
            )

    found: dict[str, list[int]] = {}
    for p in files:
        sites = unsafe_sites(p)
        if sites:
            found[p] = sites
    for p, sites in sorted(found.items()):
        allowed = allowed_unsafe.get(p)
        if allowed is None:
            violations.append(
                f"(b) `unsafe` outside the named carve-outs: {p} lines {sites}"
            )
        elif len(sites) != allowed:
            violations.append(
                f"(b) `unsafe` count changed in {p}: {len(sites)} sites {sites}, charter allows {allowed}"
            )
    for p in allowed_unsafe:
        if p in files and p not in found:
            violations.append(
                f"(b) named carve-out {p} has no `unsafe` left -- remove it from ALLOWED_UNSAFE"
            )

    if violations:
        print(f"check-{name}-profile: the {name} profile violates ADR 0000:")
        for v in violations:
            print(f"  {v}")
        return 1

    unsafe_total = sum(len(s) for s in found.values())
    mode_note = ", no batch/column/stream module" if other_modes is not None else ""
    print(
        f"check-{name}-profile: ok -- {len(files)} files compiled, closure = "
        f"{{{', '.join(crates)}}}, unsafe = {unsafe_total} site(s) in {len(found)} named file(s)"
        f"{mode_note}"
    )
    return 0


def main() -> int:
    name = sys.argv[1] if len(sys.argv) > 1 else "sqlite"
    profile = PROFILES.get(name)
    if profile is None:
        print(f"check-sqlite-profile: unknown profile {name!r}, expected one of {sorted(PROFILES)}")
        return 1
    return check_profile(name, profile)


if __name__ == "__main__":
    sys.exit(main())
