#!/usr/bin/env python3
"""Row engine scalar functions against sqlite3 (#651). Local only, like
check_column_oracle.sh: needs Homebrew's sqlite3 (3.53.4) first on PATH.

Each line of tools/row_functions_cases.sql is one statement. The row
engine's half runs through examples/row_oracle; sqlite3 runs each line in
`.mode quote` on an in-memory database. Results are compared cell by cell,
reals as doubles; an error must be an error on both sides. A line ending
in `-- nondeterministic` is only checked for erroring alike.

When every line agrees, db-core's answers are written next to the cases
as `<cases>.expected` (a nondeterministic line as `ERROR` or `OK`): the
unit test `unit_row_functions_oracle` checks the row engine against that
file in CI, where sqlite3 is not installed.

Usage: tools/check_row_functions_oracle.py [cases.sql]
"""
import subprocess
import sys

CASES = sys.argv[1] if len(sys.argv) > 1 else "tools/row_functions_cases.sql"
EXPECTED = "3.53.4"


def split_cells(row):
    """Split a `.mode quote` row on commas outside quotes."""
    cells, cur, quoted = [], "", False
    for ch in row:
        if ch == "'":
            quoted = not quoted
        if ch == "," and not quoted:
            cells.append(cur)
            cur = ""
        else:
            cur += ch
    cells.append(cur)
    return cells


def norm_cell(c):
    if c.startswith("'") or c.startswith("X'") or c == "NULL":
        return c
    try:
        real = any(x in c for x in ".eE") or c.lower().lstrip("-") in ("inf", "nan")
        return repr(float(c)) if real else c
    except ValueError:
        return c


def norm(out):
    return " | ".join(
        ",".join(norm_cell(c) for c in split_cells(row)) for row in out.split(" | ") if row != ""
    ) if out else ""


def sqlite(sql):
    p = subprocess.run(
        ["sqlite3", ":memory:", ".mode quote", sql],
        capture_output=True, encoding="utf-8", errors="replace",
    )
    if p.returncode != 0 or "error" in p.stderr.lower():
        return "ERROR"
    return " | ".join(line for line in p.stdout.splitlines())


def main():
    version = subprocess.run(["sqlite3", "--version"], capture_output=True, text=True).stdout.split()[0]
    if version != EXPECTED:
        sys.exit(f"sqlite3 on PATH is {version}, expected {EXPECTED} (Homebrew's, keg-only)")
    subprocess.run(["cargo", "build", "-q", "--release", "--example", "row_oracle"], check=True)
    ours = subprocess.run(
        ["target/release/examples/row_oracle", CASES], capture_output=True, text=True, check=True
    ).stdout.splitlines()
    lines = [l for l in open(CASES).read().splitlines() if l.strip() and not l.startswith("--")]
    assert len(ours) == len(lines), (len(ours), len(lines))
    bad = 0
    expected = []
    for sql, got in zip(lines, ours):
        want = sqlite(sql)
        nondet = sql.rstrip().endswith("-- nondeterministic")
        same = (got == "ERROR") == (want == "ERROR") if nondet else norm(got) == norm(want)
        if not same:
            bad += 1
            print(f"MISMATCH: {sql}\n  db-core: {got}\n  sqlite3: {want}")
        expected.append(("ERROR" if got == "ERROR" else "OK") if nondet else got)
    print(f"check-row-functions-oracle: {len(lines) - bad}/{len(lines)} agree with sqlite3 {EXPECTED}")
    if not bad:
        with open(CASES.removesuffix(".sql") + ".expected", "w") as f:
            f.write("".join(line + "\n" for line in expected))
    sys.exit(1 if bad else 0)


main()
