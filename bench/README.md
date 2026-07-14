# headroom-hook benchmarks (Docker)

One harness, one command. Every published number is produced by
[`docker_ab.py`](docker_ab.py) running the **shipped images** — `getbusbar/busbar`
and `getbusbar/headroom-hook` — exactly the way a `docker compose up` install runs
them. No local build, no native binary: what you benchmark is what you deploy.

## Run it

```sh
python3 docker_ab.py --requests 1000 --concurrency 1 --history-kb 11
```

Needs Docker and Python 3 (stdlib only). It pulls the images if absent, runs a
baseline phase (busbar alone) and a hook phase (busbar + the Headroom gate) through
the same deterministic request stream, and writes `results/docker_ab.json`.

Flags: `--requests`, `--concurrency`, `--warmup`, `--history-kb`, `--delay-ms`.
Pin images with `BUSBAR_IMAGE=` / `HOOK_IMAGE=` env vars.

## What it measures, and why it's honest

- **Topology mirrors the real install.** A recording mock upstream
  ([`mock_upstream.py`](mock_upstream.py)) tallies the chars that actually *arrived*
  upstream — so a token reduction here proves the rewrite **shipped**, not that the
  hook accounted for it internally. busbar shares the mock's network namespace so the
  mock is reachable on `127.0.0.1` (busbar's plaintext-loopback carve-out); the hook
  shares busbar's Unix-socket volume, exactly as `docker-compose.yml` wires them.
- **The number is the delta.** We report the hook's added cost on busbar's OWN clock
  (`busbar;dur`, the `Server-Timing` header). Baseline and hook phases share the same
  harness/network floor, so it cancels in `hook − baseline` and the delta is the
  hook's whole-path cost (gate round-trip + compression).
- **Deterministic input.** [`corpus.py`](corpus.py) generates byte-identical noisy
  tool-log history at `target_ratio: 0.5`, so two runs produce the same inputs.

## Honesty notes

- Absolute `busbar;dur` scales with the host: on a small VM (e.g. a 2-core laptop
  Docker VM) it reads high; on a real multi-core host it approaches busbar's native
  tens-of-µs. **Report the host's core count with any absolute number**, or lean on
  the delta, which is far more stable across hosts.
- Savings are content- and setting-dependent: the corpus is compressible tool logs /
  RAG dumps. Ordinary conversation isn't compressible and the hook abstains on it.

## Smoke test

[`../scripts/docker-smoke.sh <hook-image>`](../scripts/docker-smoke.sh) is the
release-gate check: it boots the image the compose way and fails unless the hook
runs, creates its socket, and a compressible request ships fewer tokens upstream —
the check that would have caught a binary that builds but can't load or bind.
