#!/usr/bin/env python3
"""Sampling profile of one `make perf` bench binary (ADR 0015, tier 6).

Records the bench under Instruments' Time Profiler (`xctrace`, macOS),
exports the `time-profile` table and prints two rankings:

  SELF       -- where the CPU actually is (leaf frames), all symbols
  INCLUSIVE  -- db_core functions, by time spent in them or below

Usage: python3 tools/perf_profile.py <bench> [--budget-ms N] [--top N]
       (bench is one of: parser, codegen, vm_opcodes)

The bench profile carries line tables (`[profile.bench] debug` in
Cargo.toml), so frames resolve to function names without changing the
codegen the timings are taken from. Traces and XML land under
`target/perf/profile/`.
"""

import argparse
import collections
import glob
import os
import re
import subprocess
import sys
import xml.etree.ElementTree as ET

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))


def bench_binary(name: str) -> str:
    subprocess.run(["cargo", "bench", "--no-run", "--bench", name], cwd=ROOT, check=True)
    cands = [p for p in glob.glob(f"{ROOT}/target/release/deps/{name}-*") if not p.endswith(".d")]
    if not cands:
        sys.exit(f"no bench binary for {name}")
    return max(cands, key=os.path.getmtime)


def record(name: str, binary: str, budget_ms: int) -> str:
    out = f"{ROOT}/target/perf/profile"
    os.makedirs(out, exist_ok=True)
    trace = f"{out}/{name}.trace"
    subprocess.run(["rm", "-rf", trace], check=True)
    env = dict(os.environ, PERF_BUDGET_MS=str(budget_ms))
    # xctrace exits non-zero even on a clean run; the .trace is what matters.
    subprocess.run(
        ["xctrace", "record", "--template", "Time Profiler", "--output", trace, "--launch", "--", binary, "--bench"],
        env=env, cwd=ROOT, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, check=False,
    )
    xml_path = f"{out}/{name}.xml"
    subprocess.run(
        ["xctrace", "export", "--input", trace, "--output", xml_path,
         "--xpath", '/trace-toc/run[@number="1"]/data/table[@schema="time-profile"]'],
        check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
    )
    return xml_path


def aggregate(xml_path: str):
    root = ET.parse(xml_path).getroot()
    frames, backtraces, weights = {}, {}, {}
    self_t, incl_t = collections.Counter(), collections.Counter()
    total = 0

    def frame_name(fr):
        if "ref" in fr.attrib:
            return frames[fr.attrib["ref"]]
        frames[fr.attrib["id"]] = fr.attrib.get("name") or "?"
        return frames[fr.attrib["id"]]

    def backtrace(bt):
        if "ref" in bt.attrib:
            return backtraces[bt.attrib["ref"]]
        fs = [frame_name(f) for f in bt.findall("frame")]
        backtraces[bt.attrib["id"]] = fs
        return fs

    for row in root.iter("row"):
        w = row.find("weight")
        if "ref" in w.attrib:
            wv = weights[w.attrib["ref"]]
        else:
            wv = int(w.text)
            weights[w.attrib["id"]] = wv
        tb = row.find("tagged-backtrace")
        bt = tb.find("backtrace") if tb is not None else None
        if bt is None:
            continue
        fs = backtrace(bt)
        if not fs:
            continue
        total += wv
        self_t[fs[0]] += wv
        for f in set(fs):
            incl_t[f] += wv
    return total, self_t, incl_t


def short(name: str) -> str:
    name = re.sub(r"::h[0-9a-f]{16}$", "", name)
    name = name.replace("$LT$", "<").replace("$GT$", ">").replace("$u20$", " ").replace("$C$", ",")
    name = name.replace("$u7b$$u7b$closure$u7d$$u7d$", "{closure}").replace("..", "::")
    return name


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("bench", choices=["parser", "codegen", "vm_opcodes"])
    ap.add_argument("--budget-ms", type=int, default=1500, help="PERF_BUDGET_MS per benchmark while recording")
    ap.add_argument("--top", type=int, default=20)
    args = ap.parse_args()

    xml_path = record(args.bench, bench_binary(args.bench), args.budget_ms)
    total, self_t, incl_t = aggregate(xml_path)
    print(f"\n{args.bench}: {total / 1e6:.0f} ms sampled ({xml_path})")
    print("\nSELF time, top")
    for name, w in self_t.most_common(args.top):
        print(f"  {100 * w / total:5.1f}%  {short(name)[:110]}")
    print("\nINCLUSIVE time, db_core functions, top")
    shown = 0
    for name, w in incl_t.most_common():
        if "db_core" not in name:
            continue
        print(f"  {100 * w / total:5.1f}%  {short(name)[:110]}")
        shown += 1
        if shown >= args.top:
            break


if __name__ == "__main__":
    main()
