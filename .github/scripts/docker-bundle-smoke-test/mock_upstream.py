#!/usr/bin/env python3
"""A minimal, synchronous mock upstream OpenAI-shaped provider, mirroring
tests/e2e_admin_api.rs's `spawn_mock_upstream()` for the docker-bundle smoke test: accepts
`POST /v1/chat/completions`, writes the raw request body's length to a file (so the smoke-test
shell script can compare it against what the client sent, proving headroom actually shrank it
before it reached the upstream), and answers with a well-formed chat-completion response so
busbar's own response path completes normally.

Usage: mock_upstream.py <port> <captured-length-file>
"""

import http.server
import json
import sys


def make_handler(captured_len_path):
    class Handler(http.server.BaseHTTPRequestHandler):
        def do_POST(self):
            length = int(self.headers.get("Content-Length", 0))
            body = self.rfile.read(length)
            with open(captured_len_path, "w") as f:
                f.write(str(len(body)))
            resp = json.dumps(
                {
                    "id": "mock-1",
                    "object": "chat.completion",
                    "created": 0,
                    "model": "test-model",
                    "choices": [
                        {
                            "index": 0,
                            "message": {"role": "assistant", "content": "ok"},
                            "finish_reason": "stop",
                        }
                    ],
                    "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2},
                }
            ).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(resp)))
            self.end_headers()
            self.wfile.write(resp)

        def log_message(self, fmt, *args):
            pass  # keep CI logs focused on the smoke test's own echoes

    return Handler


if __name__ == "__main__":
    port = int(sys.argv[1])
    captured_len_path = sys.argv[2]
    server = http.server.HTTPServer(("0.0.0.0", port), make_handler(captured_len_path))
    server.serve_forever()
