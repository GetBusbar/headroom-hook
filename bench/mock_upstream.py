#!/usr/bin/env python3
# Copyright (C) 2026 Busbar Inc and contributors
#
# Recording fixed-latency mock upstream for the headroom-hook A/B benchmark — the same pattern as
# the engine's bench/latency/mock_upstream.py, plus per-run accounting so the rig can measure what
# actually ARRIVED upstream (i.e. whether the hook's rewrite survived busbar end-to-end).
#
# Serves POST /v1/chat/completions (OpenAI-shaped, non-streaming; busbar's OpenAI writer appends
# this path to base_url). Every request's message content chars are tallied; the reply's
# usage.prompt_tokens is ceil(chars/4) — the same estimator the direct driver uses, so the token
# numbers line up across the rig.
#
#   GET  /stats -> {"requests": N, "prompt_chars": C, "prompt_tokens_est": T}
#   POST /reset -> zeroes the tally (called between the baseline and hook phases)
#
# Stdlib only. Usage: python3 mock_upstream.py --port 9001 --delay-ms 0

import argparse
import json
import math
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

_lock = threading.Lock()
_stats = {"requests": 0, "prompt_chars": 0, "prompt_tokens_est": 0}

CANNED_TEXT = "Benchmark canned completion."


def _content_chars(body: dict) -> int:
    total = 0
    for m in body.get("messages", []):
        c = m.get("content", "")
        if isinstance(c, str):
            total += len(c)
        elif isinstance(c, list):  # content-block form
            for block in c:
                if isinstance(block, dict):
                    total += len(block.get("text", "") or "")
    return total


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    delay_s = 0.0

    def log_message(self, *args):
        pass

    def do_GET(self):
        if self.path == "/stats":
            with _lock:
                body = json.dumps(_stats).encode()
            self._reply(200, body)
        else:
            self._reply(404, b"{}")

    def do_POST(self):
        n = int(self.headers.get("content-length", 0))
        raw = self.rfile.read(n) if n else b""
        if self.path == "/reset":
            with _lock:
                for k in _stats:
                    _stats[k] = 0
            self._reply(200, b"{}")
            return
        try:
            body = json.loads(raw)
        except ValueError:
            body = {}
        chars = _content_chars(body)
        tokens = max(1, math.ceil(chars / 4))
        with _lock:
            _stats["requests"] += 1
            _stats["prompt_chars"] += chars
            _stats["prompt_tokens_est"] += tokens
        if self.delay_s:
            time.sleep(self.delay_s)
        reply = json.dumps(
            {
                "id": "chatcmpl-bench-0001",
                "object": "chat.completion",
                "created": 1718000000,
                "model": "mock-model",
                "choices": [
                    {
                        "index": 0,
                        "message": {"role": "assistant", "content": CANNED_TEXT},
                        "finish_reason": "stop",
                    }
                ],
                "usage": {
                    "prompt_tokens": tokens,
                    "completion_tokens": 8,
                    "total_tokens": tokens + 8,
                },
            }
        ).encode()
        self._reply(200, reply)

    def _reply(self, status, body):
        self.send_response(status)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--port", type=int, default=9001)
    ap.add_argument("--delay-ms", type=int, default=0)
    args = ap.parse_args()
    Handler.delay_s = args.delay_ms / 1000.0
    srv = ThreadingHTTPServer(("127.0.0.1", args.port), Handler)
    print(f"mock upstream on 127.0.0.1:{args.port} (delay {args.delay_ms}ms)", flush=True)
    srv.serve_forever()


if __name__ == "__main__":
    main()
