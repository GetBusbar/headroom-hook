#!/usr/bin/env bash
# Smoke test for the getbusbar/busbar-headroom bundled image: pulls the just-published image,
# boots it against a local mock upstream (mock_upstream.py) instead of a real provider, and proves
# two things a bare `docker run` + HTTP 200 would NOT prove:
#   1. busbar actually loaded headroom as a plugin (checked over the real admin API, not just
#      "the container is still running").
#   2. headroom actually compressed a real request — the mock upstream's captured body is
#      genuinely shorter than what the client sent, the same externally-observable proof
#      tests/e2e_admin_api.rs uses against a bare binary, now against the packaged container.
#
# Usage: run.sh <image-ref>   (e.g. getbusbar/busbar-headroom:2.0.1)
set -euo pipefail

IMAGE="${1:?usage: run.sh <image-ref>}"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORK="$(mktemp -d)"
trap 'docker rm -f smoke-headroom smoke-mock-upstream >/dev/null 2>&1 || true; rm -rf "$WORK"' EXIT

ADMIN_TOKEN="smoke-test-admin-token"
MOCK_PORT=18080
CAPTURED_LEN_FILE="$WORK/captured_len"

echo "== pulling $IMAGE =="
docker pull "$IMAGE"

echo "== starting mock upstream on :$MOCK_PORT =="
python3 "$SCRIPT_DIR/mock_upstream.py" "$MOCK_PORT" "$CAPTURED_LEN_FILE" &
MOCK_PID=$!
trap 'kill "$MOCK_PID" >/dev/null 2>&1 || true; docker rm -f smoke-headroom >/dev/null 2>&1 || true; rm -rf "$WORK"' EXIT

sed "s/MOCK_UPSTREAM_PORT/$MOCK_PORT/" "$SCRIPT_DIR/config.yaml" > "$WORK/config.yaml"
sed "s/MOCK_UPSTREAM_PORT/$MOCK_PORT/" "$SCRIPT_DIR/providers.yaml" > "$WORK/providers.yaml"

echo "== starting $IMAGE =="
docker run -d --name smoke-headroom --network host \
  -e BUSBAR_ADMIN_TOKEN="$ADMIN_TOKEN" \
  -e MOCK_KEY="unused-mock-provider-key" \
  -v "$WORK/config.yaml:/etc/busbar/config.yaml:ro" \
  -v "$WORK/providers.yaml:/etc/busbar/providers.yaml:ro" \
  "$IMAGE"

echo "== waiting for busbar to come up =="
for _ in $(seq 1 30); do
  if curl -fsS "http://127.0.0.1:8080/health" >/dev/null 2>&1; then
    break
  fi
  sleep 1
done
if ! curl -fsS "http://127.0.0.1:8080/health" >/dev/null 2>&1; then
  echo "FAIL: busbar never became healthy" >&2
  docker logs smoke-headroom >&2 || true
  exit 1
fi

echo "== check 1: headroom is loaded as a plugin (real admin API) =="
INFO_JSON="$(curl -fsS -H "Authorization: Bearer $ADMIN_TOKEN" "http://127.0.0.1:8080/api/v1/admin/plugins?type=hooks")"
if ! echo "$INFO_JSON" | grep -q "busbar-headroom"; then
  echo "FAIL: busbar-headroom is not in the loaded plugins list" >&2
  echo "$INFO_JSON" >&2
  docker logs smoke-headroom >&2 || true
  exit 1
fi
echo "ok: busbar-headroom is a loaded plugin"

echo "== check 2: a real request through the gate is genuinely compressed =="
# A noisy, repetitive "tool log" — mirrors tests/e2e_admin_api.rs's log_dump() shape, well above
# headroom's default min_savings_pct threshold so a working gate MUST compress it.
HISTORY=""
for i in $(seq 1 200); do
  HISTORY="${HISTORY}[tool_call $i] fetched 200 rows from users table, no errors, 42ms elapsed. "
done
SENT_LEN=${#HISTORY}

REQ_BODY="$(python3 -c "
import json, sys
print(json.dumps({
    'model': 'claude',
    'messages': [
        {'role': 'user', 'content': sys.argv[1]},
        {'role': 'user', 'content': 'why did the deployment fail'},
    ],
}))
" "$HISTORY")"

HTTP_STATUS="$(curl -s -o "$WORK/completion_resp.json" -w '%{http_code}' \
  -X POST "http://127.0.0.1:8080/v1/chat/completions" \
  -H "Content-Type: application/json" \
  -d "$REQ_BODY")"

if [ "$HTTP_STATUS" != "200" ]; then
  echo "FAIL: chat completion returned HTTP $HTTP_STATUS, not 200" >&2
  cat "$WORK/completion_resp.json" >&2 || true
  docker logs smoke-headroom >&2 || true
  exit 1
fi

if [ ! -s "$CAPTURED_LEN_FILE" ]; then
  echo "FAIL: the mock upstream never received a request — the gate or routing is broken" >&2
  docker logs smoke-headroom >&2 || true
  exit 1
fi
CAPTURED_LEN="$(cat "$CAPTURED_LEN_FILE")"

echo "sent request body: $SENT_LEN chars of history; upstream received a $CAPTURED_LEN-byte body"
if [ "$CAPTURED_LEN" -ge "$SENT_LEN" ]; then
  echo "FAIL: upstream body ($CAPTURED_LEN bytes) is not smaller than what the client sent ($SENT_LEN chars) — headroom did not compress this request" >&2
  docker logs smoke-headroom >&2 || true
  exit 1
fi
echo "ok: upstream body is smaller than the sent history — headroom compressed the request"

echo "== check 3: headroom's own metrics confirm the gate ran =="
METRICS="$(curl -fsS "http://127.0.0.1:8080/metrics/hooks")"
if ! echo "$METRICS" | grep -q "^headroom_"; then
  echo "FAIL: no headroom_* metrics on /metrics/hooks — the gate's own instrumentation never fired" >&2
  echo "$METRICS" >&2
  docker logs smoke-headroom >&2 || true
  exit 1
fi
echo "ok: headroom_* metrics present on /metrics/hooks"

echo "== SMOKE TEST PASSED: $IMAGE boots, loads headroom, and compresses real requests =="
