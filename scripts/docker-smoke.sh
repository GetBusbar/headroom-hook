#!/usr/bin/env bash
# Release-gate smoke test for the headroom-hook DOCKER IMAGE. Boots the image the way
# `docker compose up` does — hook + busbar sharing a Unix-socket volume, against a recording
# mock upstream — and asserts the hook ACTUALLY RUNS AND COMPRESSES. This catches the class of
# failure that a `cargo test` can't: a binary that builds fine but can't LOAD in the runtime image
# (glibc/CXXABI mismatch) or can't CREATE its socket (volume ownership) — both of which leave
# `docker compose up` silently un-compressing (busbar fail-opens to passthrough).
#
#   scripts/docker-smoke.sh <hook-image> [busbar-image]
#
# Exit 0 only if: the hook container stays UP, the socket appears, and a compressible request
# ships FEWER tokens upstream than it arrived with (i.e. the rewrite reached the provider).
set -euo pipefail

HOOK_IMAGE="${1:?usage: docker-smoke.sh <hook-image> [busbar-image]}"
BUSBAR_IMAGE="${2:-getbusbar/busbar:latest}"
HERE="$(cd "$(dirname "$0")/../bench" && pwd)"
M=smoke-mock B=smoke-busbar H=smoke-hook V=smoke-sock
PORT=8080 MOCKP=9001

cleanup() { docker rm -f "$M" "$B" "$H" >/dev/null 2>&1 || true; docker volume rm "$V" >/dev/null 2>&1 || true; }
trap cleanup EXIT
cleanup
docker volume create "$V" >/dev/null

echo "→ mock upstream"
docker run -d --name "$M" -p "$PORT:$PORT" -v "$HERE/mock_upstream.py:/mock.py:ro" \
  python:3.12-slim python /mock.py --port "$MOCKP" >/dev/null
sleep 2

echo "→ hook ($HOOK_IMAGE)"
docker run -d --name "$H" -v "$V:/run/busbar" "$HOOK_IMAGE" >/dev/null
sleep 3
# BUG CLASS 1 & 2: a glibc-mismatch or socket-permission failure exits the container here.
if [ "$(docker inspect -f '{{.State.Running}}' "$H")" != "true" ]; then
  echo "✗ FAIL: hook container is not running — it crashed on startup:"; docker logs "$H" 2>&1 | tail -5; exit 1
fi
# The socket must actually exist in the shared volume (proves bind() succeeded as the runtime user).
if ! docker run --rm -v "$V:/run/busbar" busybox test -S /run/busbar/headroom.sock; then
  echo "✗ FAIL: hook did not create /run/busbar/headroom.sock"; docker logs "$H" 2>&1 | tail -5; exit 1
fi
echo "  ✓ hook up, socket present"

echo "→ busbar ($BUSBAR_IMAGE)"
docker run -d --name "$B" --network "container:$M" -e BENCH_MOCK_KEY=x -e BUSBAR_STATE_FILE= \
  -v "$HERE/config.hook.yaml:/etc/busbar/config.yaml:ro" \
  -v "$HERE/providers.mock.yaml:/etc/busbar/providers.yaml:ro" \
  -v "$V:/run/busbar" "$BUSBAR_IMAGE" >/dev/null
for i in $(seq 1 30); do curl -fsS "http://localhost:$PORT/healthz" >/dev/null 2>&1 && break; sleep 0.5; done

# Drive a deterministic COMPRESSIBLE history (two noisy tool-log messages + a short ask).
BODY="$(cd "$HERE" && python3 -c "import corpus,json;print(json.dumps({'model':'bench-pool','messages':corpus.history_messages('tool_log',11*1024)}))")"
docker exec "$M" python -c "import urllib.request as u;u.urlopen(u.Request('http://127.0.0.1:$MOCKP/reset',method='POST'))"
for i in 1 2 3; do
  curl -fsS -o /dev/null -X POST "http://localhost:$PORT/v1/chat/completions" \
    -H "authorization: Bearer bench-token" -H 'content-type: application/json' -d "$BODY"
done
READ="$(docker exec "$M" python -c "import urllib.request as u,json;print(json.load(u.urlopen('http://127.0.0.1:$MOCKP/stats'))['prompt_tokens_est'])")"

# 3 uncompressed requests would be ~8496 tokens; compressed is ~4266. Assert a real reduction shipped.
echo "  tokens that reached the provider over 3 reqs: $READ (uncompressed would be ~8496)"
if [ "$READ" -ge 7000 ]; then
  echo "✗ FAIL: no compression reached the provider — hook is not rewriting."; docker logs "$H" 2>&1 | tail -5; exit 1
fi
echo "✓ PASS: docker image runs and compresses (rewrite shipped upstream)."
