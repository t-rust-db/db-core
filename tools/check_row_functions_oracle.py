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

With `--random [N [SEED]]` it instead compares N random REAL-to-text and
printf (`%f`/`%e`/`%g`, with `!`, `#`, widths, precisions up to 30)
statements byte for byte, one sqlite3 run for all of them (#652).

Usage: tools/check_row_functions_oracle.py [cases.sql | --random [N [SEED]]]
"""
import math
import random
import struct
import subprocess
import sys

RANDOM = len(sys.argv) > 1 and sys.argv[1] == "--random"
CASES = "tools/row_functions_cases.sql" if RANDOM or len(sys.argv) < 2 else sys.argv[1]
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


def random_real(rng):
    k = rng.random()
    if k < 0.3:
        return struct.unpack("<d", struct.pack("<Q", rng.getrandbits(63)))[0]
    if k < 0.5:
        return rng.uniform(-1, 1) * 10 ** rng.randint(-20, 25)
    if k < 0.65:
        return float(rng.randint(1, 10 ** rng.randint(1, 22)))
    if k < 0.8:
        return round(rng.uniform(0, 1000), rng.randint(0, 8))
    if k < 0.9:
        # exact ties: sqlite3 rounds them by its own approximation
        return float(rng.randint(1, 10 ** rng.randint(1, 15)) * rng.choice([1, 5, 25, 10, 50])) / rng.choice([1, 2, 4, 8, 10])
    return rng.choice([0.1, 1 / 3, 2 / 3, 1e15, 1e16, 1e17, 9.999999999999999e22, 5e-324,
                       1.7976931348623157e308, 123456789012345678.0, 0.000123456, 9999999999999998.0])


def random_statement(rng):
    x = random_real(rng)
    while not math.isfinite(x):
        x = random_real(rng)
    lit = repr(x)
    if "e" not in lit and "." not in lit:
        lit += ".0"
    if rng.random() < 0.25:
        return f"SELECT CAST({lit} AS TEXT), {lit} || '', length({lit}), printf('%s', {lit})"
    flags = rng.choice(["", "!", "!", "#", "+", "!0", "-"])
    width = rng.choice(["", "", "12", "25"])
    prec = rng.choice(["", f".{rng.randint(0, 30)}", f".{rng.randint(14, 22)}"])
    return f"SELECT printf('%{flags}{width}{prec}{rng.choice('feg')}', {lit})"


def random_main(n, seed):
    """db-core against sqlite3 on n random statements, text compared exactly."""
    rng = random.Random(seed)
    lines = [random_statement(rng) for _ in range(n)]
    subprocess.run(["cargo", "build", "-q", "--release", "--example", "row_oracle"], check=True)
    path = f"target/random_real_cases_{seed}.sql"
    with open(path, "w", encoding="utf-8") as f:
        f.write("".join(l + "\n" for l in lines))
    ours = subprocess.run(
        ["target/release/examples/row_oracle", path], capture_output=True, text=True, check=True
    ).stdout.splitlines()
    theirs = subprocess.run(
        ["sqlite3", "-cmd", ".mode quote", ":memory:", ".read /dev/stdin"],
        input="".join(l + ";\n" for l in lines), capture_output=True, text=True, check=True,
    ).stdout.splitlines()
    assert len(ours) == len(theirs) == n, (len(ours), len(theirs), n)
    bad = [(l, a, b) for l, a, b in zip(lines, ours, theirs) if a != b]
    for l, a, b in bad[:20]:
        print(f"MISMATCH: {l}\n  db-core: {a}\n  sqlite3: {b}")
    print(f"check-row-functions-oracle --random: {n - len(bad)}/{n} agree with sqlite3 {EXPECTED} (seed {seed})")
    sys.exit(1 if bad else 0)


def main():
    version = subprocess.run(["sqlite3", "--version"], capture_output=True, text=True).stdout.split()[0]
    if version != EXPECTED:
        sys.exit(f"sqlite3 on PATH is {version}, expected {EXPECTED} (Homebrew's, keg-only)")
    if RANDOM:
        random_main(int(sys.argv[2]) if len(sys.argv) > 2 else 5000, int(sys.argv[3]) if len(sys.argv) > 3 else 1)
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
        with open(CASES.removesuffix(".sql") + ".expected", "w", encoding="utf-8", newline="\n") as f:
            f.write("".join(line + "\n" for line in expected))
    sys.exit(1 if bad else 0)


main()
