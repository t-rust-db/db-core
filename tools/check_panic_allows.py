#!/usr/bin/env python3
"""Policy gate (db-core#230): production code never opts out of the panic
lints.

Cargo.toml's `[lints.clippy]` denies `unwrap_used`, `expect_used`, `panic`,
`unreachable`, `todo`, `unimplemented`. Clippy enforces the *lint*; this
script enforces the *policy* -- that no `#[allow(...)]` re-admits one of
them in production code. Test code is exempt by construction: clippy.toml's
`allow-*-in-tests` and `src/lib.rs`'s `cfg_attr(test, ...)` scope the lints,
so a test module never needs an allow either -- any allow found in a test
region is dead weight, not a violation, and is left to review.

"Production" = every line of `src/**/*.rs` before that file's first
`#[cfg(test)]` / `#[cfg(all(test, ...))]`.

EXEMPT is empty since db-core#231 converted the last production `expect`s to
typed errors; nothing may be added to it without an issue number and reason.

Usage: python3 tools/check_panic_allows.py   (exit 1 on any violation)
"""

import re
import sys
from pathlib import Path

PANIC_LINTS = (
    "unwrap_used",
    "expect_used",
    "panic",
    "unreachable",
    "todo",
    "unimplemented",
)

# Emptied by db-core#231. Any new entry needs an issue number and a reason.
# db-core#289: two test-support sites that arrived with src/storage (#288)
# and that this gate's `#[cfg(test)]`-marker heuristic cannot see.
EXEMPT: dict[str, str] = {
    "src/storage/row/btree.rs": (
        "db-core#289: `test_minimal_db` is `#[cfg(any(test, feature = "
        "\"storage-test-support\"))]` -- the allow is needed for the "
        "feature-enabled non-test build"
    ),
    "src/storage/row/vfs/bin/lock_probe.rs": (
        "db-core#289: `[[bin]]` test helper (second OS process for fcntl "
        "contention tests) built as a production target; to move under "
        "tests/helpers/"
    ),
}

TEST_REGION = re.compile(r"^\s*#\[cfg\((all\()?test", re.M)
ALLOW = re.compile(r"#!?\[allow\(([^\]]*?)\)\]", re.S)
LINT = re.compile(r"clippy::(" + "|".join(PANIC_LINTS) + r")\b")


def production_region(path: Path) -> str:
    text = path.read_text()
    m = TEST_REGION.search(text)
    return text[: m.start()] if m else text


def main() -> int:
    violations: list[str] = []
    exempt_hits: dict[str, int] = {}
    for path in sorted(Path("src").rglob("*.rs")):
        rel = path.as_posix()
        prod = production_region(path)
        for m in ALLOW.finditer(prod):
            lints = LINT.findall(m.group(1))
            if not lints:
                continue
            line = prod.count("\n", 0, m.start()) + 1
            if rel in EXEMPT:
                exempt_hits[rel] = exempt_hits.get(rel, 0) + 1
                continue
            violations.append(f"{rel}:{line}: allow({', '.join(lints)})")

    stale = sorted(set(EXEMPT) - set(exempt_hits))
    for rel in stale:
        violations.append(f"{rel}: listed in EXEMPT but has no panic-lint allow left -- remove the entry")

    if violations:
        print("check-panic-allows: production code re-admits a panic lint (db-core#230 policy):")
        for v in violations:
            print(f"  {v}")
        return 1
    n = sum(exempt_hits.values())
    tail = f" ({n} exempt via EXEMPT in {len(exempt_hits)} files)" if n else ""
    print(f"check-panic-allows: no panic-lint allows in production src/{tail}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
