#!/usr/bin/env python3
"""Fixture generator (#320): `key=value` (Heroku/Go kit/zap console) log
lines exercising `logfmt`'s alias-table promotion (shared with `jsonl`)
and typed inference (int/float/bool/string).

Usage: python3 tools/gen_logfmt.py [n] > tests/fixtures/stream/logfmt-Nk.log
"""

import random
import sys
import time

LEVELS = ["trace", "debug", "info", "warn", "error", "fatal"]
SERVICES = ["auth", "billing", "api-gateway", "worker"]
MESSAGES = ["request slow", "started", "shutting down", "connection reset"]


def line(ts_ms: int) -> str:
    ts = time.strftime("%Y-%m-%dT%H:%M:%S", time.gmtime(ts_ms / 1000)) + "Z"
    fields = [
        f"ts={ts}",
        f"level={random.choice(LEVELS)}",
        f'msg="{random.choice(MESSAGES)}"',
        f"service={random.choice(SERVICES)}",
        f"duration={random.randint(1, 900)}ms",
        f"status={random.choice([200, 201, 400, 404, 500])}",
    ]
    if random.random() < 0.2:
        fields.append("debug")  # bare key -> true
    return " ".join(fields)


def main() -> None:
    n = int(sys.argv[1]) if len(sys.argv) > 1 else 1000
    now_ms = int(time.time() * 1000)
    for i in range(n):
        ts_ms = now_ms - (n - i) * 1000
        sys.stdout.write(line(ts_ms) + "\n")


if __name__ == "__main__":
    main()
