#!/usr/bin/env python3
# Copyright (C) 2026 Busbar Inc and contributors
#
# Docker A/B benchmark for the Headroom hook — the same methodology as ../busbar_ab.py, but every
# component runs from the SHIPPED images (getbusbar/busbar, getbusbar/headroom-hook), driven exactly
# the way a user's `docker compose up` install runs. It measures the hook's added cost on busbar's
# OWN clock (`busbar;dur`), so the harness/network floor cancels in the with/without-hook delta.
#
# Topology (why it faithfully mirrors the real install):
#   * mock upstream — a recording OpenAI-shaped mock on 127.0.0.1:9001; tallies the chars that
#     actually ARRIVED upstream, proving the rewrite shipped rather than being hook-side accounting.
#   * busbar — the getbusbar/busbar image, sharing the mock container's NETWORK NAMESPACE so the
#     mock is reachable at 127.0.0.1 (inside busbar's plaintext-loopback carve-out, same as a
#     local-model deployment). Its :8080 is published through the mock container.
#   * headroom — the getbusbar/headroom-hook image, sharing a Unix-socket VOLUME with busbar
#     (/run/busbar), exactly as the published docker-compose.yml wires them.
#
# Baseline phase runs busbar alone; hook phase adds the headroom container + the gate in the config.
# Same request stream (deterministic ../corpus.py) through both; we report the per-percentile delta.
#
#   python3 docker_ab.py --requests 1000 --concurrency 1 --history-kb 11 [--delay-ms 0]
#
# Stdlib only (+ the docker CLI). Writes results/docker_ab.json and prints a summary.

import argparse
import http.client
import json
import os
import subprocess
import sys
import time
import urllib.request

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)  # corpus.py is a sibling in this folder
import corpus  # noqa: E402

BUSBAR_IMAGE = os.environ.get("BUSBAR_IMAGE", "getbusbar/busbar:latest")
HOOK_IMAGE = os.environ.get("HOOK_IMAGE", "getbusbar/headroom-hook:latest")
MOCK = "hb-mock"
BUSBAR = "hb-busbar"
HOOK = "hb-hook"
# INTERNAL ports (inside the shared netns — fixed, never collide): busbar listens on
# 8080 (see config*.yaml), the mock on 9001 (providers.mock.yaml points busbar there).
PORT = 8080
MOCK_PORT = 9001
# The only HOST-published port; override on a busy host with BENCH_PORT. The driver
# and healthz poll this; the mock's /stats,/reset are reached in-container via docker exec.
HOST_PORT = int(os.environ.get("BENCH_PORT", "8080"))


def sh(*args, check=True, quiet=True):
    kw = {"stdout": subprocess.DEVNULL, "stderr": subprocess.DEVNULL} if quiet else {}
    return subprocess.run(args, check=check, **kw)


def rm(*names):
    for n in names:
        sh("docker", "rm", "-f", n, check=False)


def percentile(vals, q):
    if not vals:
        return None
    s = sorted(vals)
    i = min(len(s) - 1, max(0, round(q * (len(s) - 1))))
    return s[i]


def busbar_dur_us(server_timing):
    # Server-Timing: busbar;dur=<milliseconds> -> integer microseconds.
    for part in (server_timing or "").split(","):
        part = part.strip()
        if part.startswith("busbar;dur="):
            try:
                return round(float(part.split("=", 1)[1]) * 1000.0)
            except ValueError:
                return None
    return None


def wait_ready(timeout=25.0):
    deadline = time.time() + timeout
    while time.time() < deadline:
        try:
            with urllib.request.urlopen(f"http://localhost:{HOST_PORT}/healthz", timeout=1) as r:
                if r.status == 200:
                    return True
        except Exception:
            time.sleep(0.3)
    return False


def openai_body(history_kb):
    # The deterministic tool-log history as an OpenAI /v1/chat/completions body.
    msgs = corpus.history_messages("tool_log", history_kb * 1024)
    return json.dumps({"model": "bench-pool", "messages": msgs}).encode()


def mock_get(path):
    # The mock binds 127.0.0.1 inside its own netns, so reach its control endpoints
    # (/stats, /reset) from INSIDE that container (it ships python).
    method = "POST" if path == "/reset" else "GET"
    code = (
        "import urllib.request as u,sys;"
        f"r=u.urlopen(u.Request('http://127.0.0.1:{MOCK_PORT}{path}',method='{method}'));"
        "sys.stdout.write(r.read().decode())"
    )
    out = subprocess.run(
        ["docker", "exec", MOCK, "python", "-c", code],
        capture_output=True, text=True, check=True,
    ).stdout
    return json.loads(out) if out.strip() else {}


def load(body, n, conc, warmup):
    """Drive n keep-alive POSTs (after `warmup`), collect busbar;dur µs samples."""
    hdrs = {"authorization": "Bearer bench-token", "content-type": "application/json"}

    def run(count):
        durs = []
        conn = http.client.HTTPConnection("localhost", HOST_PORT, timeout=30)
        for _ in range(count):
            conn.request("POST", "/v1/chat/completions", body=body, headers=hdrs)
            r = conn.getresponse()
            st = r.getheader("Server-Timing")
            r.read()
            d = busbar_dur_us(st)
            if d is not None:
                durs.append(d)
        conn.close()
        return durs

    if conc <= 1:
        run(warmup)
        return run(n)
    # simple threaded fan-out for concurrency > 1
    import threading

    run(warmup)
    out, per = [], n // conc
    lock = threading.Lock()

    def w():
        d = run(per)
        with lock:
            out.extend(d)

    ts = [threading.Thread(target=w) for _ in range(conc)]
    [t.start() for t in ts]
    [t.join() for t in ts]
    return out


def start_mock(delay_ms):
    rm(MOCK)
    # Publish only busbar's data port (busbar shares this netns and listens on PORT
    # internally). The mock's own port stays internal — reached via docker exec.
    sh("docker", "run", "-d", "--name", MOCK, "-p", f"{HOST_PORT}:{PORT}",
       "-v", f"{HERE}/mock_upstream.py:/mock.py:ro",
       "python:3.12-slim", "python", "/mock.py", "--port", str(MOCK_PORT),
       "--delay-ms", str(delay_ms))
    time.sleep(2)


def start_busbar(config, with_hook):
    rm(BUSBAR, HOOK)
    if with_hook:
        sh("docker", "run", "-d", "--name", HOOK, "-v", "hb-sock:/run/busbar", HOOK_IMAGE)
        time.sleep(1)
    args = ["docker", "run", "-d", "--name", BUSBAR, "--network", f"container:{MOCK}",
            "-e", "BENCH_MOCK_KEY=x", "-e", "BUSBAR_STATE_FILE=",
            "-v", f"{HERE}/{config}:/etc/busbar/config.yaml:ro",
            "-v", f"{HERE}/providers.mock.yaml:/etc/busbar/providers.yaml:ro"]
    if with_hook:
        args += ["-v", "hb-sock:/run/busbar"]
    args += [BUSBAR_IMAGE]
    sh(*args)
    if not wait_ready():
        print("busbar did not become ready; logs:", file=sys.stderr)
        sh("docker", "logs", BUSBAR, check=False, quiet=False)
        sys.exit(1)


def measure(config, with_hook, args):
    start_busbar(config, with_hook)
    body = openai_body(args.history_kb)
    mock_get("/reset")
    durs = load(body, args.requests, args.concurrency, args.warmup)
    stats = mock_get("/stats")
    tokens = round(stats.get("prompt_tokens_est", 0) / max(1, stats.get("requests", 1)))
    return {
        "busbar_dur_us": {q: percentile(durs, p) for q, p in (("p50", .5), ("p90", .9), ("p99", .99))},
        "samples": len(durs),
        "tokens_per_req": tokens,
    }


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--requests", type=int, default=1000)
    ap.add_argument("--concurrency", type=int, default=1)
    ap.add_argument("--warmup", type=int, default=50)
    ap.add_argument("--history-kb", type=int, default=11)
    ap.add_argument("--delay-ms", type=int, default=0)
    args = ap.parse_args()

    sh("docker", "volume", "rm", "hb-sock", check=False)
    sh("docker", "volume", "create", "hb-sock")
    try:
        start_mock(args.delay_ms)
        base = measure("config.baseline.yaml", False, args)
        hook = measure("config.hook.yaml", True, args)
    finally:
        rm(MOCK, BUSBAR, HOOK)
        sh("docker", "volume", "rm", "hb-sock", check=False)

    delta = {q: (hook["busbar_dur_us"][q] - base["busbar_dur_us"][q])
             for q in ("p50", "p90", "p99")}
    saved_pct = round(100 * (1 - hook["tokens_per_req"] / max(1, base["tokens_per_req"])), 1)
    result = {
        "config": vars(args),
        "images": {"busbar": BUSBAR_IMAGE, "hook": HOOK_IMAGE},
        "baseline": base, "hook": hook,
        "added_busbar_dur_us": delta,
        "tokens": {"baseline": base["tokens_per_req"], "hook": hook["tokens_per_req"],
                   "saved_pct": saved_pct},
    }
    os.makedirs(f"{HERE}/results", exist_ok=True)
    with open(f"{HERE}/results/docker_ab.json", "w") as f:
        json.dump(result, f, indent=2)
    print(json.dumps(result, indent=2))
    print(f"\nADDED busbar;dur (hook cost)  p50={delta['p50']}µs "
          f"p90={delta['p90']}µs p99={delta['p99']}µs")
    print(f"tokens/req {base['tokens_per_req']} -> {hook['tokens_per_req']}  ({saved_pct}% saved)")


if __name__ == "__main__":
    main()
