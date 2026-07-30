#!/usr/bin/env bash
# Release-gate smoke test for the headroom-hook PLUGIN, built and packed from THIS checkout, dlopen'd
# into a single `getbusbar/busbar` container — the 1.5.0 signed `kind: hook` plugin ABI. There is no
# separate headroom-hook image or container and no shared Unix-socket volume anymore: the port to
# busbar's dlopen plugin ABI means a hook is a cdylib busbar loads in-process. This asserts the hook
# ACTUALLY LOADS AND COMPRESSES, catching the class of failure `cargo test` can't:
#   - the cdylib builds but fails busbar's manifest/ABI checks at load (wrong abi_version, bad
#     signature handling, wrong busbar_plugin_kind) — `busbar --validate`, run first, catches this
#     before any traffic flows;
#   - the plugin loads but doesn't actually rewrite (a silent no-op gate) — the token-count assertion
#     against the real mock upstream catches this.
#
#   scripts/docker-smoke.sh [busbar-image]
#
# Exit 0 only if: `busbar --validate` reports the plugin loaded, the container stays UP, and a
# compressible request ships FEWER tokens upstream than it arrived with (i.e. the rewrite reached
# the provider).
set -euo pipefail

BUSBAR_IMAGE="${1:-getbusbar/busbar:latest}"
HERE="$(cd "$(dirname "$0")/../bench" && pwd)"
REPO="$(cd "$HERE/.." && pwd)"
M=smoke-mock B=smoke-busbar
PORT=8080 MOCKP=9001

cleanup() { docker rm -f "$M" "$B" >/dev/null 2>&1 || true; }
trap cleanup EXIT
cleanup

echo "→ build + pack the headroom-hook plugin from this checkout"
python3 -c "import sys; sys.path.insert(0, '$HERE'); import docker_ab; docker_ab.prep_plugin()"
test -f "$HERE/plugins/busbar-headroom.tar.gz" || { echo "✗ FAIL: plugin tarball was not produced"; exit 1; }

echo "→ mock upstream"
docker run -d --name "$M" -p "$PORT:$PORT" -v "$HERE/mock_upstream.py:/mock.py:ro" \
  python:3.12-slim python /mock.py --port "$MOCKP" >/dev/null
sleep 2

echo "→ busbar --validate ($BUSBAR_IMAGE, config.hook.yaml + packed plugin)"
docker run --rm --entrypoint /busbar \
  -v "$HERE/config.hook.yaml:/etc/busbar/config.yaml:ro" \
  -v "$HERE/providers.mock.yaml:/etc/busbar/providers.yaml:ro" \
  -v "$HERE/plugins:/etc/busbar/plugins:ro" \
  "$BUSBAR_IMAGE" --validate || { echo "✗ FAIL: busbar --validate rejected the plugin/config"; exit 1; }
echo "  ✓ validate: plugin loaded cleanly"

echo "→ busbar ($BUSBAR_IMAGE)"
docker run -d --name "$B" --network "container:$M" -e BENCH_MOCK_KEY=x -e BUSBAR_STATE_FILE= \
  -v "$HERE/config.hook.yaml:/etc/busbar/config.yaml:ro" \
  -v "$HERE/providers.mock.yaml:/etc/busbar/providers.yaml:ro" \
  -v "$HERE/plugins:/etc/busbar/plugins:ro" \
  "$BUSBAR_IMAGE" >/dev/null
for i in $(seq 1 30); do curl -fsS "http://localhost:$PORT/healthz" >/dev/null 2>&1 && break; sleep 0.5; done
# BUG CLASS: a boot/dlopen failure leaves the container exited even though --validate passed offline
# (e.g. a runtime-only glibc/CXXABI mismatch); catch it before asserting on traffic.
if [ "$(docker inspect -f '{{.State.Running}}' "$B")" != "true" ]; then
  echo "✗ FAIL: busbar container is not running — it crashed on startup:"; docker logs "$B" 2>&1 | tail -10; exit 1
fi

# Drive a deterministic COMPRESSIBLE history (two noisy tool-log messages + a short ask).
BODY="$(cd "$HERE" && python3 -c "import corpus,json;print(json.dumps({'model':'bench-pool','messages':corpus.history_messages('tool_log',11*1024)}))")"
docker exec "$M" python -c "import urllib.request as u;u.urlopen(u.Request('http://127.0.0.1:$MOCKP/reset',method='POST'))"
for i in 1 2 3; do
  curl -fsS -o /dev/null -X POST "http://localhost:$PORT/v1/chat/completions" \
    -H 'content-type: application/json' -d "$BODY"
done
READ="$(docker exec "$M" python -c "import urllib.request as u,json;print(json.load(u.urlopen('http://127.0.0.1:$MOCKP/stats'))['prompt_tokens_est'])")"

# 3 uncompressed requests would be ~8496 tokens; compressed (target_ratio 0.5) is ~4266. Assert a
# real reduction shipped, i.e. the rewrite reached the provider, not just hook-side accounting.
echo "  tokens that reached the provider over 3 reqs: $READ (uncompressed would be ~8496)"
if [ "$READ" -ge 7000 ]; then
  echo "✗ FAIL: no compression reached the provider — plugin loaded but is not rewriting."
  docker logs "$B" 2>&1 | tail -10
  exit 1
fi
echo "✓ PASS: plugin loads under busbar --validate and the image compresses (rewrite shipped upstream)."
