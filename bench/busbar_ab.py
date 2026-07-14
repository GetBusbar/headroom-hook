#!/usr/bin/env python3
# Copyright (C) 2026 Busbar Inc and contributors
#
# BUSBAR-PATH A/B benchmark for headroom-hook: the same request stream driven through two busbar
# configs that differ ONLY by the hook —
#
#   baseline :  loadgen -> busbar (no hook)        -> recording mock upstream
#   hook     :  loadgen -> busbar (headroom gate)  -> recording mock upstream
#
# per ingress protocol (anthropic /v1/messages -> openai egress = cross-protocol; openai
# /v1/chat/completions -> openai = same-protocol). The mock contributes the same fixed time on
# both paths, so `hook - baseline` per percentile is the hook's whole-path cost (gate round trip
# + body re-render), and the mock's received-token tally per phase shows whether the rewrite
# actually arrived upstream — end to end, not hook-side accounting.
#
# The rig spawns everything itself: mock, hook binary (release), busbar (release; see README
# methodology notes on the loopback upstream). Stdlib only; deterministic corpus.
#
# Usage: python3 busbar_ab.py [--requests 300] [--concurrency 8] [--history-kb 11]
#                             [--busbar-bin PATH] [--out results/busbar_ab.json]

import argparse
import http.client
import json
import os
import socket
import statistics
import subprocess
import sys
import threading
import time
from pathlib import Path

import corpus

BENCH_DIR = Path(__file__).resolve().parent
HOOK_BIN = BENCH_DIR.parent / "target/release/headroom-hook"
HOOK_SOCKET = "/tmp/headroom-bench.sock"
MOCK_PORT = 9001
BUSBAR_PORT = 8080


def percentile(sorted_vals, q):
    if not sorted_vals:
        return float("nan")
    k = max(0, min(len(sorted_vals) - 1, round(q * (len(sorted_vals) - 1))))
    return sorted_vals[k]


def wait_port(port, timeout=15.0):
    deadline = time.time() + timeout
    while time.time() < deadline:
        try:
            with socket.create_connection(("127.0.0.1", port), timeout=0.25):
                return
        except OSError:
            time.sleep(0.05)
    raise TimeoutError(f"port {port} did not come up")


def wait_port_free(port, timeout=15.0):
    deadline = time.time() + timeout
    while time.time() < deadline:
        try:
            with socket.create_connection(("127.0.0.1", port), timeout=0.25):
                time.sleep(0.1)
        except OSError:
            return
    raise TimeoutError(f"port {port} did not free up")


def _busbar_dur_us(resp):
    """Parse busbar's own-clock cost from the Server-Timing header (busbar;dur=<ms>) into µs.
    This is busbar's internal processing time (total minus the upstream RTT), which INCLUDES the
    synchronous rewrite-gate call — so it is the floor-free measure of what the hook adds, on
    busbar's clock, not the Python harness's wall-clock."""
    st = resp.getheader("Server-Timing") or ""
    for part in st.split(","):
        part = part.strip()
        if part.startswith("busbar;dur="):
            try:
                return float(part.split("=", 1)[1]) * 1000.0  # ms -> µs
            except ValueError:
                return None
    return None


def run_load(path, headers, body: bytes, n, conc):
    """Drive n keep-alive POSTs at fixed concurrency. Returns (wall_us_sorted, busbar_dur_us_sorted,
    errors): wall-clock per request AND busbar's own-clock busbar;dur per request."""
    wall, dur, errors, lock = [], [], [0], threading.Lock()

    def worker(count):
        conn = http.client.HTTPConnection("127.0.0.1", BUSBAR_PORT, timeout=30)
        lwall, ldur, err = [], [], 0
        for _ in range(count):
            t0 = time.perf_counter_ns()
            try:
                conn.request("POST", path, body=body, headers=headers)
                resp = conn.getresponse()
                bd = _busbar_dur_us(resp)
                resp.read()
                if resp.status != 200:
                    err += 1
                    continue
                lwall.append((time.perf_counter_ns() - t0) / 1000.0)
                if bd is not None:
                    ldur.append(bd)
            except Exception:
                err += 1
                try:
                    conn.close()
                except Exception:
                    pass
                conn = http.client.HTTPConnection("127.0.0.1", BUSBAR_PORT, timeout=30)
        with lock:
            wall.extend(lwall)
            dur.extend(ldur)
            errors[0] += err

    threads = [threading.Thread(target=worker, args=(n // conc,)) for _ in range(conc)]
    for t in threads:
        t.start()
    for t in threads:
        t.join()
    return sorted(wall), sorted(dur), errors[0]


def mock_call(path):
    conn = http.client.HTTPConnection("127.0.0.1", MOCK_PORT, timeout=5)
    conn.request("POST" if path == "/reset" else "GET", path)
    resp = conn.getresponse()
    body = json.loads(resp.read() or b"{}")
    conn.close()
    return body


INGRESSES = {
    "anthropic_to_openai": {
        "path": "/v1/messages",
        "headers": {
            "content-type": "application/json",
            "x-api-key": "bench-token",
            "anthropic-version": "2023-06-01",
        },
    },
    "openai_to_openai": {
        "path": "/v1/chat/completions",
        "headers": {
            "content-type": "application/json",
            "authorization": "Bearer bench-token",
        },
    },
}


def measure_phase(args, warmup):
    """With busbar already up: per ingress, reset the mock tally, drive the load, read it back."""
    body = json.dumps(
        {
            "model": "bench-pool",
            "max_tokens": 64,
            "messages": corpus.history_messages("tool_log", args.history_kb * 1024),
        }
    ).encode()
    out = {}
    for name, ing in INGRESSES.items():
        run_load(ing["path"], ing["headers"], body, warmup, args.concurrency)  # warm
        mock_call("/reset")
        wall, dur, errors = run_load(
            ing["path"], ing["headers"], body, args.requests, args.concurrency
        )
        stats = mock_call("/stats")
        # busbar;dur (µs) is the HEADLINE metric: busbar's own-clock cost incl. the synchronous
        # rewrite gate, floor-free. Wall-clock is kept for context (it carries the Python/mock floor).
        out[name] = {
            "requests_ok": len(wall),
            "errors": errors,
            "busbar_dur_us": {
                "p50": round(percentile(dur, 0.50)) if dur else None,
                "p90": round(percentile(dur, 0.90)) if dur else None,
                "p99": round(percentile(dur, 0.99)) if dur else None,
                "n": len(dur),
            },
            "wall_ms": {
                "p50": round(percentile(wall, 0.50) / 1000.0, 3),
                "p90": round(percentile(wall, 0.90) / 1000.0, 3),
                "p99": round(percentile(wall, 0.99) / 1000.0, 3),
            },
            "upstream_prompt_tokens_est_total": stats["prompt_tokens_est"],
            "upstream_prompt_tokens_est_per_req": (
                round(stats["prompt_tokens_est"] / stats["requests"], 1) if stats["requests"] else None
            ),
        }
    return out


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--requests", type=int, default=300)
    ap.add_argument("--warmup", type=int, default=50)
    ap.add_argument("--concurrency", type=int, default=8)
    ap.add_argument("--history-kb", type=int, default=11)
    ap.add_argument(
        "--delay-ms",
        type=int,
        default=0,
        help="fixed mock-upstream delay (models a real provider; the SAME on both paths, so the "
        "hook delta is unchanged — it just sets the denominator for 'overhead as %% of a call')",
    )
    ap.add_argument(
        "--busbar-bin",
        default=os.environ.get(
            "BUSBAR_BIN",
            str(BENCH_DIR.parents[2] / "busbarAI/target/release/busbar"),
        ),
    )
    ap.add_argument("--out", default=str(BENCH_DIR / "results/busbar_ab.json"))
    args = ap.parse_args()

    if not Path(args.busbar_bin).exists():
        sys.exit(f"busbar binary not found: {args.busbar_bin} (set --busbar-bin or BUSBAR_BIN)")
    if not HOOK_BIN.exists():
        sys.exit(f"release hook binary missing: {HOOK_BIN} — run `cargo build --release` first")

    procs = []

    def spawn(cmd, env=None):
        p = subprocess.Popen(cmd, env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        procs.append(p)
        return p

    result = {
        "bench": "busbar_ab",
        "uname": " ".join(os.uname()),
        "busbar_bin": args.busbar_bin,
        "requests": args.requests,
        "concurrency": args.concurrency,
        "history_kb": args.history_kb,
        "upstream_delay_ms": args.delay_ms,
        "phases": {},
    }
    try:
        spawn([sys.executable, str(BENCH_DIR / "mock_upstream.py"), "--port", str(MOCK_PORT),
               "--delay-ms", str(args.delay_ms)])
        wait_port(MOCK_PORT)
        spawn([str(HOOK_BIN)], env={**os.environ, "HEADROOM_SOCKET": HOOK_SOCKET})

        for phase, cfg in [("baseline", "config.baseline.yaml"), ("hook", "config.hook.yaml")]:
            busbar = spawn(
                [args.busbar_bin],
                env={
                    **os.environ,
                    "BUSBAR_PROVIDERS": str(BENCH_DIR / "providers.mock.yaml"),
                    "BUSBAR_CONFIG": str(BENCH_DIR / cfg),
                    "BENCH_MOCK_KEY": "x",
                },
            )
            wait_port(BUSBAR_PORT)
            result["phases"][phase] = measure_phase(args, args.warmup)
            busbar.terminate()
            busbar.wait(timeout=10)
            wait_port_free(BUSBAR_PORT)

        # Per-ingress deltas: the hook's whole-path cost and the end-to-end token savings.
        deltas = {}
        for name in INGRESSES:
            base, hook = result["phases"]["baseline"][name], result["phases"]["hook"][name]
            tb, th = (
                base["upstream_prompt_tokens_est_per_req"],
                hook["upstream_prompt_tokens_est_per_req"],
            )
            bd_b, bd_h = base["busbar_dur_us"], hook["busbar_dur_us"]
            deltas[name] = {
                # busbar's own-clock cost, floor-free: base (no hook), with-hook, and the added
                # hook cost per percentile.
                "busbar_dur_us": {
                    "base": {"p50": bd_b["p50"], "p90": bd_b["p90"], "p99": bd_b["p99"]},
                    "with_hook": {"p50": bd_h["p50"], "p90": bd_h["p90"], "p99": bd_h["p99"]},
                    "hook_added": {
                        "p50": (bd_h["p50"] - bd_b["p50"]) if bd_b["p50"] is not None else None,
                        "p90": (bd_h["p90"] - bd_b["p90"]) if bd_b["p90"] is not None else None,
                        "p99": (bd_h["p99"] - bd_b["p99"]) if bd_b["p99"] is not None else None,
                    },
                },
                "tokens_per_req_baseline": tb,
                "tokens_per_req_hook": th,
                "tokens_saved_pct": round(100.0 * (tb - th) / tb, 1) if tb else None,
            }
        result["delta"] = deltas
    finally:
        for p in procs:
            p.terminate()
        for p in procs:
            try:
                p.wait(timeout=5)
            except Exception:
                p.kill()

    Path(args.out).parent.mkdir(parents=True, exist_ok=True)
    Path(args.out).write_text(json.dumps(result, indent=2) + "\n")
    print(json.dumps(result, indent=2))


if __name__ == "__main__":
    main()
