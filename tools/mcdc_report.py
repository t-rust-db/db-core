#!/usr/bin/env python3
"""MC/DC harvest dashboard — summarizes `cargo-mvl-mcdc harvest`'s raw output
(a JSON array of DischargeRecord followed by a summary line and a plain-text
`undischarged: ...` listing) into a short dashboard by default, or a binary
per-obligation action list with `--verbose`.

`vectors_required` doubles as a leaf-count signal: 0 means compiler-void
(free discharge, exhaustive `match`), 2 means a single-leaf `if`/`while`
(plain branch coverage, not a real MC/DC candidate), and 3+ means a genuine
multi-leaf `&&`/`||` decision — the obligations this project's tagged-test
convention (db-core#111, ported from sqlite-rs's own #52) actually targets.
`leafs = vectors_required - 1`.

Ported from sqlite-rs's `tools/mcdc_report.py`, unchanged except for the
dashboard header: db-core scans all of `src/` (`--all-features`, see the
Makefile's `mcdc-obligations` target), not a curated file list.

Usage:
    cargo-mvl-mcdc harvest --obligations=FILE --run-dir=. | python3 tools/mcdc_report.py
    cargo-mvl-mcdc harvest --obligations=FILE --run-dir=. | python3 tools/mcdc_report.py --verbose
"""

import argparse
import json
import sys

GREEN = "\033[32m"
YELLOW = "\033[33m"
RED = "\033[31m"
BOLD = "\033[1m"
RESET = "\033[0m"


def color(enabled: bool, code: str, text: str) -> str:
    return f"{code}{text}{RESET}" if enabled else text


def parse_records(raw: str) -> list[dict]:
    start = raw.index("[")
    depth = 0
    for i in range(start, len(raw)):
        if raw[i] == "[":
            depth += 1
        elif raw[i] == "]":
            depth -= 1
            if depth == 0:
                return json.loads(raw[start : i + 1])
    raise ValueError("unterminated JSON array in harvest output")


def summary_line(use_color: bool, multi_leaf: list[dict], undischarged: list[dict]) -> str:
    """Final pass/fail line — the one thing a CI gate or a human skimming
    scrollback needs: are all real (multi-leaf) MC/DC obligations in the
    scanned file set discharged, and if not, which files still owe vectors.
    """
    if not undischarged:
        return color(use_color, GREEN, f"SUMMARY: PASS — {len(multi_leaf)}/{len(multi_leaf)} multi-leaf obligations discharged")

    files = sorted({r["file"] for r in undischarged})
    file_list = ", ".join(files)
    discharged_count = len(multi_leaf) - len(undischarged)
    return color(
        use_color,
        RED,
        f"SUMMARY: FAIL — {discharged_count}/{len(multi_leaf)} multi-leaf obligations discharged; "
        f"{len(undischarged)} outstanding in {len(files)} file(s): {file_list}",
    )


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--verbose", action="store_true", help="binary action list instead of the dashboard table")
    parser.add_argument("--no-color", action="store_true", help="disable ANSI color (auto-disabled when not a tty)")
    args = parser.parse_args()
    use_color = sys.stdout.isatty() and not args.no_color

    raw = sys.stdin.read()
    records = parse_records(raw)

    void = [r for r in records if r["compiler_void"]]
    single_leaf = [r for r in records if not r["compiler_void"] and r["vectors_required"] == 2]
    multi_leaf = [r for r in records if not r["compiler_void"] and r["vectors_required"] >= 3]
    total_discharged = sum(1 for r in records if r["discharged"])

    print(color(use_color, BOLD, "MC/DC dashboard (db-core#111 — all of src/, --all-features, no curated file list)"))
    print(f"  total obligations:        {len(records)}")
    print(f"  compiler-void (free):     {len(void)}")
    print(f"  single-leaf branches:     {len(single_leaf)} (plain branch coverage, not tagged by convention)")
    print(f"  multi-leaf (real MC/DC):  {len(multi_leaf)} total, "
          f"{sum(1 for r in multi_leaf if r['discharged'])} discharged")
    print(f"  overall discharged:       {total_discharged}/{len(records)} "
          f"({100 * total_discharged / len(records):.1f}%)")

    undischarged = [r for r in multi_leaf if not r["discharged"]]

    if not args.verbose:
        if multi_leaf:
            print()
            print("  multi-leaf obligations (leafs = conditions, vectors = leafs + 1 required cases):")
            for r in multi_leaf:
                leafs = r["vectors_required"] - 1
                mark = color(use_color, GREEN, "OK") if r["discharged"] else color(use_color, RED, "--")
                print(f"    [{mark}] {r['id']:<14} {leafs} leafs, "
                      f"{r['vectors_discharged']}/{r['vectors_required']} vectors fulfilled  "
                      f"({r['file']}:{r['line']})")
        print()
        print(summary_line(use_color, multi_leaf, undischarged))
        if not undischarged:
            print("Run with VERBOSE=1 for a binary per-obligation action list.")
        return 1 if undischarged else 0

    # Single-leaf branches and compiler-void obligations are out of scope by
    # convention (see dashboard counts above) — verbose mode only breaks
    # down the multi-leaf obligations, since those are the only ones this
    # ticket's tagged-test convention actually targets.
    print()
    for r in multi_leaf:
        leafs = r["vectors_required"] - 1
        passing_vectors = {t["vector"] for t in r["tagged_tests"] if t["passed"]}
        failing_tests = [t for t in r["tagged_tests"] if not t["passed"]]
        tag = f"{r['id']} [{leafs} leafs, {r['vectors_discharged']}/{r['vectors_required']} vectors]"
        loc = f"({r['file']}:{r['line']})"

        if r["discharged"]:
            print(f"{color(use_color, GREEN, 'DISCHARGED')}  {tag} {loc}")
            continue

        missing_vectors = sorted(set(range(1, r["vectors_required"] + 1)) - passing_vectors)
        for v in missing_vectors:
            print(f"{color(use_color, YELLOW, 'ADD TEST')}    {tag} {loc} -- "
                  f"tag a test mcdc__{r['id']}__v{v}_<description>")
        for t in failing_tests:
            print(f"{color(use_color, RED, 'FIX TEST')}    {tag} {loc} -- {t['name']} is failing")

    print()
    print(summary_line(use_color, multi_leaf, undischarged))
    return 1 if undischarged else 0


if __name__ == "__main__":
    sys.exit(main())
