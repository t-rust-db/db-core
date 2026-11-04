#!/usr/bin/env python3
"""Weekly Miri findings -> GitHub issues, one per root cause.

Reads the JUnit reports `cargo miri nextest run --profile miri` writes
(target/nextest/miri/junit.xml, one per partition), groups the failed tests
by root cause, and opens one `miri` issue per cause listing the tests it
hit. A cause that already has an open issue (same title) is left alone, so
a weekly run never files duplicates; closing the issue re-arms it.

Root cause = kind + what Miri (or the panic) said + the first `src/` file
in the trace, with ids, sizes and line numbers normalised away, so 150
tests tripping over the same unsupported syscall in one helper are one
issue, not 150.

Kinds:
  ub           "Undefined Behavior"   -- the finding Miri exists for
  leak         "memory leaked"
  unsupported  "unsupported operation" -- Miri cannot run it (FFI, mmap,
               fcntl, ...); fix = add the test to the miri profile's
               default-filter in .config/nextest.toml, with the reason
  timeout      nextest's slow-timeout terminated it (all timeouts: one issue)
  failure      the test fails under Miri but not natively (panic site)

Usage:
  python3 tools/miri_tickets.py JUNIT... [--dry-run] [--run-url URL]

Writes a markdown summary to $GITHUB_STEP_SUMMARY when set. Exits 0 even
when it files issues: the issues are the report, not a red X.
"""

import argparse
import json
import os
import re
import subprocess
import sys
import xml.etree.ElementTree as ET
from collections import defaultdict

LABEL = "miri"
MAX_LOG = 5000  # characters of Miri output quoted per issue
MAX_TESTS = 50  # tests listed per issue

FIX = {
    "ub": "Find the unsafe code (ours or a dependency's) the trace points at and fix the UB.",
    "leak": "Find the allocation that is never freed (Rc cycle, `mem::forget`, `Box::leak`). "
    "If the leak is deliberate test scaffolding, exclude the test in `.config/nextest.toml`.",
    "unsupported": "Miri cannot interpret this operation. If that is inherent (FFI, mmap, "
    "fcntl, process spawn), add the tests to the `miri` profile's `default-filter` in "
    "`.config/nextest.toml` with the reason; otherwise restructure so the logic is "
    "testable without it.",
    "timeout": "Too slow under Miri (~100x interpretation overhead). Shrink the input under "
    "`cfg!(miri)` or add the tests to the `default-filter` in `.config/nextest.toml`.",
    "failure": "Passes natively, fails under Miri. Miri returns short reads/writes and "
    "imprecise `powi`/libm results on purpose: a failure here usually means the code "
    "assumes something the platform does not guarantee.",
}


def output_of(case: ET.Element, bad: ET.Element) -> str:
    parts = (bad.get("message", ""), bad.text or "",
             case.findtext("system-out") or "", case.findtext("system-err") or "")
    return "\n".join(p for p in parts if p)


def normalise(s: str) -> str:
    s = re.sub(r"alloc\d+", "allocN", s)
    s = re.sub(r"\d+", "N", s)
    return s.strip()[:100]


def first_src(text: str) -> str:
    m = re.search(r"\n\s+at (src/[^:\s]+)", text)
    return m.group(1) if m else ""


def root_cause(bad: ET.Element, text: str) -> tuple[str, str]:
    """(kind, signature) -- the signature becomes the issue title."""
    if bad.get("type") == "test timeout":
        return "timeout", "tests exceed the time limit"
    m = re.search(r"^error: (Undefined Behavior|unsupported operation|memory leaked)(.*)$", text, re.MULTILINE)
    if m:
        kind = {"Undefined Behavior": "ub", "unsupported operation": "unsupported",
                "memory leaked": "leak"}[m.group(1)]
        head = m.group(1) if kind == "leak" else m.group(1) + m.group(2)
        # An unsupported operation is the same finding wherever it's called
        # from; UB and leaks are pinned to the file that hit them.
        where = "" if kind == "unsupported" else first_src(text)
        return kind, normalise(head) + (f" ({where})" if where else "")
    m = re.search(r"panicked at (src/[^:]+):\d+:\d+:\n(.*)", text)
    if m:
        return "failure", f"panic in {m.group(1)}: {normalise(m.group(2))}"
    return "failure", "test fails without a Miri error or panic"


def excerpt(text: str) -> str:
    m = re.search(r"^(error: (Undefined Behavior|unsupported operation|memory leaked)|.*panicked at)",
                  text, re.MULTILINE)
    body = text[m.start():] if m else text[-MAX_LOG:]
    if len(body) > MAX_LOG:
        body = body[:MAX_LOG] + "\n... (truncated)"
    return body.strip()


def collect(paths: list[str]) -> dict[tuple[str, str], list[tuple[str, str]]]:
    groups: dict[tuple[str, str], list[tuple[str, str]]] = defaultdict(list)
    for path in paths:
        for case in ET.parse(path).getroot().iter("testcase"):
            bad = case.find("failure")
            if bad is None:
                bad = case.find("error")
            if bad is None:
                continue
            text = output_of(case, bad)
            groups[root_cause(bad, text)].append((case.get("name", "?"), text))
    return groups


def gh(*args: str) -> str:
    return subprocess.run(["gh", *args], check=True, capture_output=True, text=True).stdout


def open_titles(repo: str) -> set[str]:
    # REST, not `gh issue list`: the search index behind the latter lags
    # minutes behind a fresh issue.
    raw = gh("api", "--paginate", f"repos/{repo}/issues?labels={LABEL}&state=open&per_page=100",
             "--jq", ".[].title")
    return set(raw.splitlines())


def create(repo: str, title: str, body: str) -> str:
    payload = json.dumps({"title": title, "body": body, "labels": [LABEL]})
    out = subprocess.run(["gh", "api", f"repos/{repo}/issues", "--input", "-", "--jq", ".html_url"],
                         input=payload, check=True, capture_output=True, text=True)
    return out.stdout.strip()


def body_for(kind: str, sig: str, tests: list[tuple[str, str]], run_url: str) -> str:
    names = sorted(n for n, _ in tests)
    listed = "\n".join(f"- `{n}`" for n in names[:MAX_TESTS])
    if len(names) > MAX_TESTS:
        listed += f"\n- ... and {len(names) - MAX_TESTS} more (see the run's JUnit artifacts)"
    first = names[0]
    sample = next(t for n, t in tests if n == first)
    return (
        f"The weekly Miri job hit **{sig}** in {len(names)} test(s).\n\n"
        f"Run: {run_url or '(local)'}\n\n"
        f"{listed}\n\n"
        "Reproduce:\n\n"
        "```sh\n"
        f"make miri MIRI_ARGS=\"-E 'test(={first})'\"\n"
        "```\n\n"
        f"Suggested fix: {FIX[kind]}\n\n"
        f"<details><summary>Miri output for <code>{first}</code></summary>\n\n"
        f"```\n{excerpt(sample)}\n```\n\n</details>\n\n"
        "---\n🤖 *Filed by the weekly Miri job (`tools/miri_tickets.py`)*\n"
    )


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("junit", nargs="+")
    ap.add_argument("--dry-run", action="store_true")
    ap.add_argument("--run-url", default="")
    ap.add_argument("--repo", default=os.environ.get("GITHUB_REPOSITORY", "t-rust-db/db-core"))
    a = ap.parse_args()

    groups = collect(a.junit)
    existing = set() if a.dry_run else open_titles(a.repo)
    rows = []
    # UB first: if the API gives out halfway, the findings that matter are in.
    order = {"ub": 0, "leak": 1, "failure": 2, "unsupported": 3, "timeout": 4}
    for (kind, sig), tests in sorted(groups.items(), key=lambda kv: (order[kv[0][0]], kv[0][1])):
        title = f"Miri: {sig}"[:200]
        if title in existing:
            status = "already open"
        elif a.dry_run:
            status = "would file"
            print(f"--- {title}\n{body_for(kind, sig, tests, a.run_url)}")
        else:
            status = create(a.repo, title, body_for(kind, sig, tests, a.run_url))
        rows.append((kind, sig, len(tests), status))

    failed = sum(n for _, _, n, _ in rows)
    lines = ["## Miri", ""]
    if rows:
        lines += [f"{failed} failed test(s), {len(rows)} root cause(s).", "",
                  "| kind | root cause | tests | issue |", "|---|---|---|---|"]
        lines += [f"| {k} | {s.replace('|', '/')} | {n} | {st} |" for k, s, n, st in rows]
    else:
        lines.append("No findings.")
    summary = "\n".join(lines) + "\n"
    print(summary)
    if path := os.environ.get("GITHUB_STEP_SUMMARY"):
        with open(path, "a") as fh:
            fh.write(summary)
    return 0


if __name__ == "__main__":
    sys.exit(main())
