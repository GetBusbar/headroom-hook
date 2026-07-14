#!/usr/bin/env python3
# Copyright (C) 2026 Busbar Inc and contributors
#
# DIRECT-DRIVER benchmark of headroom-hook: speaks busbar's exact NDJSON wire at the hook's
# socket, with no busbar in the loop. Measures the three hook-intrinsic numbers precisely:
#
#   1. per-call cost — wall time of one transform round trip (write line, read reply) for
#      histories of 2/8/16/64 KB, p50/p90/p99 over N timed calls after warmup;
#   2. token savings by content type — estimated input tokens before vs after the rewrite for
#      noisy tool logs, RAG dumps, and short chats (est. ceil(chars/4); ratios are what matter);
#   3. abstain behavior — abstain rate over a corpus of short chats (must be 100%: the hook
#      passes short prompts through untouched) and over the compressible corpus (must be 0%).
#
# The hook binary is spawned by this script (release build, private socket), configured over the
# wire exactly as busbar would (configure first line, commit-on-ack), so the measured path is the
# production path minus busbar itself. Stdlib only; deterministic corpus (corpus.py).
#
# Usage: python3 hook_bench.py [--iters 300] [--out results/hook_direct.json]

import argparse
import json
import os
import socket
import statistics
import subprocess
import sys
import tempfile
import time
from pathlib import Path

import corpus

HOOK_BIN = Path(__file__).resolve().parents[1] / "target/release/headroom-hook"


def percentile(sorted_vals, q):
    if not sorted_vals:
        return float("nan")
    k = max(0, min(len(sorted_vals) - 1, round(q * (len(sorted_vals) - 1))))
    return sorted_vals[k]


class Hook:
    """Spawn the hook on a private socket and speak the wire at it."""

    def __init__(self, settings=None):
        self.dir = tempfile.mkdtemp(prefix="hrbench-")
        self.path = os.path.join(self.dir, "h.sock")
        self.proc = subprocess.Popen(
            [str(HOOK_BIN)],
            env={**os.environ, "HEADROOM_SOCKET": self.path},
            stderr=subprocess.DEVNULL,
        )
        for _ in range(100):
            if os.path.exists(self.path):
                break
            time.sleep(0.02)
        self.sock = socket.socket(socket.AF_UNIX)
        self.sock.connect(self.path)
        self.buf = b""
        # Configure exactly as busbar does: first line, commit-on-ack.
        ack = self.round_trip(
            (
                json.dumps(
                    {
                        "configure": {
                            "hook": "headroom",
                            "settings": settings or {"target_ratio": 0.5, "min_savings_pct": 10},
                            "settings_version": 1,
                            "busbar_version": "1.3.0",
                        }
                    }
                )
                + "\n"
            ).encode()
        )
        assert json.loads(ack)["ack"]["settings_version"] == 1, ack

    def round_trip(self, line: bytes) -> bytes:
        self.sock.sendall(line)
        while b"\n" not in self.buf:
            chunk = self.sock.recv(1 << 20)
            if not chunk:
                raise ConnectionError("hook closed the connection")
            self.buf += chunk
        reply, self.buf = self.buf.split(b"\n", 1)
        return reply

    def close(self):
        self.sock.close()
        self.proc.terminate()
        self.proc.wait(timeout=5)


def bench_latency(hook, iters):
    """Per-call wall time (µs) by history size: 2/8/16/64 KB tool-log histories."""
    out = []
    for kb in (2, 8, 16, 64):
        line = corpus.wire_line(corpus.history_messages("tool_log", kb * 1024))
        for _ in range(max(10, iters // 10)):  # warmup (lazy statics, allocator, page cache)
            hook.round_trip(line)
        samples = []
        for _ in range(iters):
            t0 = time.perf_counter_ns()
            hook.round_trip(line)
            samples.append((time.perf_counter_ns() - t0) / 1000.0)
        samples.sort()
        out.append(
            {
                "history_kb": kb,
                "wire_line_bytes": len(line),
                "iters": iters,
                "p50_us": round(percentile(samples, 0.50), 1),
                "p90_us": round(percentile(samples, 0.90), 1),
                "p99_us": round(percentile(samples, 0.99), 1),
                "mean_us": round(statistics.fmean(samples), 1),
            }
        )
    return out


def savings_case(hook, name, messages):
    """One rewrite round trip: estimated tokens before/after + abstain flag."""
    before = sum(corpus.est_tokens(m["content"]) for m in messages)
    reply = json.loads(hook.round_trip(corpus.wire_line(messages)))
    if "rewrite" in reply:
        after = sum(corpus.est_tokens(m["content"]) for m in reply["rewrite"]["messages"])
        abstained = False
    else:
        after = before
        abstained = True
    return {
        "case": name,
        "tokens_before_est": before,
        "tokens_after_est": after,
        "saved_pct": round(100.0 * (before - after) / before, 1) if before else 0.0,
        "abstained": abstained,
    }


def bench_savings(hook):
    return [
        savings_case(hook, "tool_log_11kb", corpus.history_messages("tool_log", 11 * 1024)),
        savings_case(hook, "tool_log_64kb", corpus.history_messages("tool_log", 64 * 1024)),
        savings_case(hook, "rag_dump_5kb", corpus.history_messages("rag_dump", 5 * 1024)),
        savings_case(hook, "rag_dump_16kb", corpus.history_messages("rag_dump", 16 * 1024)),
        savings_case(hook, "short_chat", corpus.short_chat_messages(0)),
    ]


def bench_abstain(hook, n=100):
    """Abstain rate on a corpus of n short chats (expected 100%) and n compressible histories
    (expected 0%)."""
    short_abstains = 0
    for i in range(n):
        reply = json.loads(hook.round_trip(corpus.wire_line(corpus.short_chat_messages(i))))
        short_abstains += "rewrite" not in reply
    long_abstains = 0
    for i in range(n):
        kb = 4 + (i % 12)  # 4–15 KB, mixed flavors
        flavor = "tool_log" if i % 2 == 0 else "rag_dump"
        reply = json.loads(hook.round_trip(corpus.wire_line(corpus.history_messages(flavor, kb * 1024))))
        long_abstains += "rewrite" not in reply
    return {
        "short_chats": n,
        "short_chat_abstain_pct": round(100.0 * short_abstains / n, 1),
        "compressible_histories": n,
        "compressible_abstain_pct": round(100.0 * long_abstains / n, 1),
    }


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--iters", type=int, default=300)
    ap.add_argument("--out", default=str(Path(__file__).parent / "results/hook_direct.json"))
    args = ap.parse_args()

    if not HOOK_BIN.exists():
        sys.exit(f"release binary missing: {HOOK_BIN} — run `cargo build --release` first")

    hook = Hook()
    try:
        result = {
            "bench": "hook_direct",
            "uname": " ".join(os.uname()),
            "hook_binary": str(HOOK_BIN),
            "settings": {"target_ratio": 0.5, "min_savings_pct": 10},
            "latency_by_history_size": bench_latency(hook, args.iters),
            "token_savings_by_content": bench_savings(hook),
            "abstain": bench_abstain(hook),
        }
    finally:
        hook.close()

    Path(args.out).parent.mkdir(parents=True, exist_ok=True)
    Path(args.out).write_text(json.dumps(result, indent=2) + "\n")
    print(json.dumps(result, indent=2))


if __name__ == "__main__":
    main()
