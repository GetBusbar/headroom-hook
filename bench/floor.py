#!/usr/bin/env python3
# Copyright (C) 2026 Busbar Inc and contributors
#
# HARNESS FLOOR: the round-trip latency of the benchmark rig ITSELF — a stdlib
# http.client POST to the mock upstream directly, with no busbar and no hook in
# the loop. This is the noise floor the A/B is measured against: it is well above
# busbar's own tens-of-µs overhead, which is exactly why the A/B reports the
# with/without-hook DELTA (that cancels this floor) rather than an absolute
# per-request number. Reading: "the rig can't resolve busbar's ~50µs solo cost,
# but it measures the hook's added cost cleanly."
#
# Usage: python3 floor.py [--requests 400] [--warmup 50] [--port 9001]

import argparse
import http.client
import json
import os
import subprocess
import sys
import time
from pathlib import Path

BENCH_DIR = Path(__file__).resolve().parent


def measure(port, path, body, headers, n, warm):
    lat = []
    for i in range(n + warm):
        c = http.client.HTTPConnection("127.0.0.1", port, timeout=30)
        t = time.perf_counter()
        c.request("POST", path, body=body, headers=headers)
        r = c.getresponse()
        r.read()
        dt = (time.perf_counter() - t) * 1e6  # µs
        c.close()
        if i >= warm:
            lat.append(dt)
    lat.sort()
    return lat


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--requests", type=int, default=400)
    ap.add_argument("--warmup", type=int, default=50)
    ap.add_argument("--port", type=int, default=9001)
    args = ap.parse_args()

    mock = subprocess.Popen(
        [sys.executable, str(BENCH_DIR / "mock_upstream.py"), "--port", str(args.port),
         "--delay-ms", "0"]
    )
    try:
        time.sleep(1.0)
        body = json.dumps(
            {"model": "m", "messages": [{"role": "user", "content": "hi"}], "max_tokens": 16}
        ).encode()
        lat = measure(
            args.port, "/v1/chat/completions", body,
            {"Content-Type": "application/json"}, args.requests, args.warmup,
        )
        p50 = lat[len(lat) // 2]
        p90 = lat[int(len(lat) * 0.90)]
        p99 = lat[int(len(lat) * 0.99)]
        print(
            json.dumps(
                {
                    "what": "harness floor (loadgen -> mock, no busbar, no hook)",
                    "requests": len(lat),
                    "p50_us": round(p50),
                    "p90_us": round(p90),
                    "p99_us": round(p99),
                }
            )
        )
    finally:
        mock.terminate()


if __name__ == "__main__":
    main()
