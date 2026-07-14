#!/bin/sh
# Copyright (C) 2026 Busbar Inc and contributors
#
# Transport-level smoke test: start the hook on a temp socket and speak busbar's exact
# newline-delimited JSON wire at it, in busbar's order — configure first (commit-on-ack),
# then describe, then a transform line — and show the rewrite. Proves the socket framing,
# the configure-ack handshake, and the reply shapes without running busbar.
set -eu
cd "$(dirname "$0")/.."

SOCK="$(mktemp -d)/headroom.sock"
HEADROOM_SOCKET="$SOCK" cargo run --quiet --release &
HOOK_PID=$!
trap 'kill $HOOK_PID 2>/dev/null || true' EXIT
# wait for the socket file
for _ in $(seq 1 50); do [ -S "$SOCK" ] && break; sleep 0.1; done

python3 - "$SOCK" <<'EOF'
import json, socket, sys

s = socket.socket(socket.AF_UNIX)
s.connect(sys.argv[1])

def round_trip(msg):
    s.sendall((json.dumps(msg) + "\n").encode())
    buf = b""
    while not buf.endswith(b"\n"):
        buf += s.recv(65536)
    return json.loads(buf)

# 1. configure — busbar's FIRST line on every connection; the ack must echo the version.
ack = round_trip({"configure": {
    "hook": "headroom",
    "settings": {"target_ratio": 0.4, "min_savings_pct": 10.0},
    "settings_version": 1, "busbar_version": "1.3.0",
}})
assert ack == {"ack": {"settings_version": 1}}, ack
print("configure acked: settings_version 1 (target_ratio 0.4 pushed live)")

# 2. describe — the settings schema (served at GET /admin/v1/hooks/{name}/schema).
schema = round_trip({"describe": True})
assert set(schema["properties"]) == {"target_ratio", "min_savings_pct"}, schema
print("describe answered: schema with", sorted(schema["properties"]))

# 3. transform — the rewrite arm, using the settings pushed in step 1.
noise = "\n".join(
    f"Routine step {i} completed in the staging environment without incident."
    if i != 20 else
    "ERROR: deployment canary failed with status 503 on us-east-1."
    for i in range(40)
)
req = {
    "request": {
        "pool": "smart", "ingress_protocol": "anthropic",
        "message_count": 2, "has_tools": False, "total_chars": len(noise),
        "stream": False,
        "messages": [
            {"role": "user", "text": noise},
            {"role": "user", "text": "why did the deployment fail"},
        ],
    },
    "candidates": [], "context": {"pool": "smart", "budget_remaining": None},
}
reply = round_trip(req)
msgs = reply["rewrite"]["messages"]
print(f"history: {len(noise)} chars -> {len(msgs[0]['content'])} chars")
print(f"ask kept verbatim: {msgs[1]['content']!r}")
assert "ERROR" in msgs[0]["content"], "the load-bearing line must survive"
assert msgs[1]["content"] == "why did the deployment fail"
print("SMOKE OK: configure-ack + describe + rewrite on one NDJSON connection")
EOF
