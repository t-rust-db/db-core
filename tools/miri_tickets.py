#!/usr/bin/env python3
"""Weekly Miri findings -> GitHub issues.

Reads the JUnit report `cargo miri nextest run --profile miri` writes
(target/nextest/miri/junit.xml), classifies every failed test by what Miri
said, and opens one issue per finding with the `miri` label. A finding that
already has an open issue (same title) is left alone, so a weekly run never
files duplicates; closing the issue re-arms it.

Kinds, by the first marker found in the test's output:
  ub           "Undefined Behavior"   -- the finding Miri exists for
  leak         "memory leaked"
  unsupported  "unsupported operation" -- the test calls into something Miri
               cannot run (FFI, mmap, fcntl); fix = `#[cfg_attr(miri, ignore)]`
  timeout      nextest's slow-timeout terminated it
  failure      anything else (the test fails under Miri but not natively)

Usage:
  python3 tools/miri_tickets.py JUNIT... [--dry-run] [--run-url URL]

Writes a markdown summary to $GITHUB_STEP_SUMMARY when set. Exits 0 even
when it files issues: the issues are the report, not a red X.
"""

import argparse
import os
import re
import subprocess
import sys
import xml.etree.ElementTree as ET

LABEL = "miri"
MAX_LOG = 6000  # characters of Miri output quoted per issue

KINDS = (
    ("ub", "Undefined Behavior", "undefined behaviour"),
    ("leak", "memory leaked", "memory leak"),
    ("unsupported", "unsupported operation", "unsupported operation"),
)


def classify(text: str) -> tuple[str, str]:
    for kind, marker, label in KINDS:
        if marker in text:
            return kind, label
    if re.search(r"TIMEOUT|terminated", text):
        return "timeout", "timeout"
    return "failure", "test failure"


def excerpt(text: str) -> str:
    """The Miri error block if there is one, else the tail of the output."""
    m = re.search(r"^error: (Undefined Behavior|unsupported operation|memory leaked).*", text, re.MULTILINE | re.DOTALL)
    body = text[m.start():] if m else text
    if len(body) > MAX_LOG:
        body = body[:MAX_LOG] + "\n... (truncated)"
    return body.strip()


def failures(junit: str) -> list[dict]:
    out = []
    for case in ET.parse(junit).getroot().iter("testcase"):
        bad = case.find("failure")
        if bad is None:
            bad = case.find("error")
        if bad is None:
            continue
        text = "\n".join(
            t for t in (
                bad.get("message", ""),
                bad.text or "",
                (case.findtext("system-out") or ""),
                (case.findtext("system-err") or ""),
            ) if t
        )
        kind, label = classify(text)
        out.append({"test": case.get("name", "?"), "kind": kind, "label": label, "text": text})
    return out


def gh(*args: str) -> str:
    return subprocess.run(["gh", *args], check=True, capture_output=True, text=True).stdout


def open_titles() -> set[str]:
    raw = gh("issue", "list", "--label", LABEL, "--state", "open", "--limit", "500",
             "--json", "title", "--jq", ".[].title")
    return set(raw.splitlines())


def body_for(f: dict, run_url: str) -> str:
    fix = {
        "ub": "Find the unsafe code (ours or a dependency's) the trace points at and fix the UB.",
        "leak": "Find the allocation that is never freed (Rc cycle, `mem::forget`, leaked Box).",
        "unsupported": "The test calls something Miri cannot interpret. If that is inherent "
                       "(FFI, mmap, fcntl), mark it `#[cfg_attr(miri, ignore)]` with a reason; "
                       "otherwise restructure so the pure logic is testable without it.",
        "timeout": "Too slow under Miri (~100x interpretation overhead). Shrink the input "
                   "under `cfg!(miri)` or mark it `#[cfg_attr(miri, ignore)]` with a reason.",
        "failure": "The test passes natively but fails under Miri; compare the two outputs.",
    }[f["kind"]]
    return (
        f"The weekly Miri job reports **{f['label']}** in `{f['test']}`.\n\n"
        f"Run: {run_url or '(local)'}\n\n"
        "Reproduce:\n\n"
        "```sh\n"
        f"MIRIFLAGS=\"-Zmiri-disable-isolation\" cargo +nightly miri nextest run "
        f"-p db-core --all-features --lib -E 'test(={f['test']})'\n"
        "```\n\n"
        f"Suggested fix: {fix}\n\n"
        "<details><summary>Miri output</summary>\n\n"
        f"```\n{excerpt(f['text'])}\n```\n\n</details>\n\n"
        "---\n🤖 *Filed by the weekly Miri job (`tools/miri_tickets.py`)*\n"
    )


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("junit", nargs="+")
    ap.add_argument("--dry-run", action="store_true")
    ap.add_argument("--run-url", default="")
    a = ap.parse_args()

    found = [f for j in a.junit for f in failures(j)]
    existing = set() if a.dry_run else open_titles()
    rows = []
    for f in found:
        title = f"Miri: {f['label']} in {f['test']}"
        if title in existing:
            rows.append((f, "already open"))
            continue
        if a.dry_run:
            rows.append((f, "would file"))
            print(f"--- {title}\n{body_for(f, a.run_url)}")
            continue
        url = gh("issue", "create", "--title", title, "--label", LABEL,
                 "--body", body_for(f, a.run_url)).strip()
        rows.append((f, url))

    lines = ["## Miri", "", f"{len(found)} finding(s)." if found else "No findings.", ""]
    if rows:
        lines += ["| test | kind | issue |", "|---|---|---|"]
        lines += [f"| `{f['test']}` | {f['label']} | {r} |" for f, r in rows]
    summary = "\n".join(lines) + "\n"
    print(summary)
    if path := os.environ.get("GITHUB_STEP_SUMMARY"):
        with open(path, "a") as fh:
            fh.write(summary)
    return 0


if __name__ == "__main__":
    sys.exit(main())
