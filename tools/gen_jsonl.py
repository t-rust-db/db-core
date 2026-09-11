#!/usr/bin/env python3
"""Fixture generator (#319): JSON Lines log lines exercising `jsonl`'s
alias-table promotion, numeric Bunyan/Pino levels, one-level nesting, and
the Docker `json-file` container unwrap.

Usage: python3 tools/gen_jsonl.py [n] > tests/fixtures/stream/jsonl-Nk.log
"""

import json
import random
import sys
import time

LEVELS = [10, 20, 30, 40, 50, 60]  # Bunyan/Pino: trace..fatal
SERVICES = ["auth", "billing", "api-gateway", "worker"]
METHODS = ["GET", "POST", "PUT", "DELETE"]
PATHS = ["/health", "/v1/users", "/v1/orders", "/v1/payments"]


def pino_line(ts_ms: int) -> dict:
    return {
        "level": random.choice(LEVELS),
        "time": ts_ms,
        "msg": random.choice(
            ["request completed", "cache miss", "slow query", "retrying upstream"]
        ),
        "service": random.choice(SERVICES),
        "pid": random.randint(1000, 9999),
        "req": {
            "method": random.choice(METHODS),
            "url": random.choice(PATHS),
        },
        "res": {
            "statusCode": random.choice([200, 201, 400, 404, 500]),
        },
    }


def docker_wrapped_syslog_line(ts_ms: int) -> dict:
    payload = (
        f"<134>Sep  9 14:23:01 host app[{random.randint(100,999)}]: "
        f"{random.choice(['ok', 'timeout', 'connection reset'])}"
    )
    return {
        "log": payload + "\n",
        "stream": random.choice(["stdout", "stderr"]),
        "time": time.strftime(
            "%Y-%m-%dT%H:%M:%S", time.gmtime(ts_ms / 1000)
        )
        + f".{ts_ms % 1000:03d}Z",
    }


def main() -> None:
    n = int(sys.argv[1]) if len(sys.argv) > 1 else 1000
    now_ms = int(time.time() * 1000)
    for i in range(n):
        ts_ms = now_ms - (n - i) * 1000
        # ~10% Docker-wrapped syslog to exercise container unwrap.
        line = (
            docker_wrapped_syslog_line(ts_ms)
            if random.random() < 0.1
            else pino_line(ts_ms)
        )
        sys.stdout.write(json.dumps(line) + "\n")


if __name__ == "__main__":
    main()
