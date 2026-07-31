# headroom-hook benchmarks (Docker)

One harness, one command. Every published number is produced by
[`docker_ab.py`](docker_ab.py) running the **shipped `getbusbar/busbar` image** with
the Headroom `kind: hook` dlopen plugin — built and packed from THIS checkout — loaded
into it, exactly the way a real install runs the 1.5.0 signed-plugin ABI. There is no
separate `headroom-hook` image or container: since the port to busbar's plugin ABI, a
hook is a `cdylib` busbar loads in-process, not a sidecar with its own image.

## Run it

```sh
python3 docker_ab.py --requests 1000 --concurrency 1 --history-kb 11
```

Needs Docker, Python 3 (stdlib only), and a Rust toolchain (`cargo`). It builds the
Headroom cdylib from this checkout, packs it as an unsigned dev-mode tarball with
`busbar-plugin-pack` (built from the busbarAI checkout — see `BUSBARAI_DIR` in
[`docker_ab.py`](docker_ab.py) if yours isn't laid out at the interim path
`headroom-hook/Cargo.toml` currently points at), pulls `getbusbar/busbar` if absent,
then runs a baseline phase (busbar alone, `config.baseline.yaml`) and a hook phase
(busbar with the plugin dlopen'd, `config.hook.yaml`) through the same deterministic
request stream, writing `results/docker_ab.json`. Pass `--skip-plugin-build` to reuse
an already-packed `plugins/busbar-headroom.tar.gz`.

Flags: `--requests`, `--concurrency`, `--warmup`, `--history-kb`, `--delay-ms`,
`--skip-plugin-build`. Pin the image with `BUSBAR_IMAGE=`.

**Host arch note:** the packed plugin must dlopen inside the busbar container, so it
must be built for the SAME OS/arch as that container (Linux). On a Linux CI runner
(the normal case — GitHub Actions, etc.) a plain `cargo build --release` already
matches. On a non-Linux dev host (e.g. macOS) a locally-built `.dylib` will not load
inside a Linux container; verify natively instead — build `busbar` and the plugin for
the host, pack an unsigned tarball, point `plugins.dir` at it with
`plugins.trust.allow_unsigned: true`, and run `busbar --validate` / a native request
cycle. That exercises the identical load-and-rewrite path this script drives, just
without Docker.

## What it measures, and why it's honest

- **Topology mirrors the real install.** A recording mock upstream
  ([`mock_upstream.py`](mock_upstream.py)) tallies the chars that actually *arrived*
  upstream — so a token reduction here proves the rewrite **shipped**, not that the
  hook accounted for it internally. busbar shares the mock's network namespace so the
  mock is reachable on `127.0.0.1` (busbar's plaintext-loopback carve-out). The
  Headroom plugin loads IN-PROCESS inside the one busbar container (dlopen, the signed
  `kind: hook` plugin ABI — see `../docs` in busbarAI: `docs/plugins.md`,
  `docs/hooks.md`): the ONLY difference between the baseline and hook phases is which
  `config.yaml` is mounted and whether the `plugins/` directory (containing the packed
  tarball) is bind-mounted at `/etc/busbar/plugins`. There is no second container and
  no shared socket volume to wire.
- **The number is the delta.** We report the hook's added cost on busbar's OWN clock
  (`busbar;dur`, the `Server-Timing` header). Baseline and hook phases share the same
  harness/network floor, so it cancels in `hook − baseline` and the delta is the
  hook's whole-path cost (gate call + compression, now an in-process `busbar_call`
  rather than a socket round-trip).
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

[`../scripts/docker-smoke.sh`](../scripts/docker-smoke.sh) is the release-gate check:
it builds and packs the plugin from this checkout, boots a single `getbusbar/busbar`
container with it dlopen'd, runs `busbar --validate` to prove a clean plugin load, and
sends a compressible request to assert it ships fewer tokens upstream — the check that
would have caught a plugin that builds but fails the signed-manifest / ABI checks at
load, or dlopens but doesn't actually rewrite.
