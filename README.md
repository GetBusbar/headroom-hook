# headroom-hook

**v1.** Compresses LLM chat history inside [busbar](https://getbusbar.com)
using [headroom](https://github.com/headroomlabs-ai/headroom)'s Rust
compression core (`TextCrusher`, pure BM25: no model, no network, no proxy).
A rewrite gate on busbar's hook wire; targets busbar 1.3.0 and pins a
`headroom-core` rev.

## Benchmark

Measured the way busbar measures itself: from **busbar's own clock**. Busbar reports
its internal processing time — total request time minus the upstream round-trip — in
a standard `Server-Timing: busbar;dur=<ms>` header on every response. The Headroom
gate runs *synchronously* inside that window (busbar calls the hook and waits for the
rewrite before dispatch), so `busbar;dur` captures exactly what the hook adds, on
busbar's clock, with none of the benchmark harness's own round-trip in it. Every
number below is reproducible — see [`bench/`](bench/README.md).

### Added latency

`busbar;dur`, in µs, concurrency 1, 1,000 requests per row, 11 KB noisy tool-log
history, openai → openai:

| | p50 | p90 | p99 |
|---|--:|--:|--:|
| Busbar alone | `22` | `25` | `30` |
| Busbar + Headroom | `569` | `601` | `634` |
| **Headroom's added cost** | **`547`** | **`576`** | **`604`** |

Cross-protocol (anthropic → openai) is within noise: `34` base / `584` with-hook /
`550` added, p50.

Two things this shows. **Busbar itself is tens of µs** (`22` µs p50 here; its own
[benchmark](https://getbusbar.com/docs/benchmark/) sweeps `37`–`93` µs across
protocols and payloads) — the gateway is not where your latency goes. And **Headroom
adds ~`547` µs** to compress an 11 KB history, with a **tight tail**: p99 is only
~1.1× p50, because busbar and the hook are single Rust binaries with no garbage
collector — nothing in the path pauses to sweep memory. Compression cost scales with
history size (direct socket driver, p50): `150` µs at 2 KB, `380` µs at 8 KB, `720` µs
at 16 KB, `2,900` µs at 64 KB.

On a two-second inference call, `547` µs is **0.03%** of the request.

### Token savings (context, not the headline)

This is **not** a compression benchmark — measuring how well Headroom compresses is
the [Headroom project's](https://headroomlabs-ai.github.io/headroom/) job, and it
reports higher ratios than the ~50% seen here (66–94% on some content types), with
more to gain from tuning the keep-ratio. What matters here is that the plumbing is
sound: the mock upstream tallies the tokens it actually received, so these are the
prompts that *really shipped* to the provider, not hook-side accounting.

| content (11 KB) | input tokens before | after | saved |
|---|--:|--:|--:|
| noisy tool log | `2,832` | `1,422` | **49.8%** |
| RAG dump | `4,211` | `2,127` | **49.5%** |
| short chat | `22` | `22` | 0% (abstains) |

Short conversational chats pass through byte-identical (100% abstain over 100 short
chats; 0% over 100 compressible histories) — nothing to trim, nothing touched. And if
the hook is ever slow, wrong, or down, the request proceeds with its original body.

### Next to a proxy

Headroom's own HTTP proxy reports, from
[production telemetry](https://headroomlabs-ai.github.io/headroom/benchmarks/)
(50,000+ sessions, 250+ instances), a **52 ms median** overhead — which, as they
rightly note, is negligible against multi-second inference. Running the same
compression core as a busbar gate, the added cost measures **547 µs** on busbar's
clock. Both are small next to the model call; we make no claim about how their proxy
is deployed — only that the same core, run as a socket gate on busbar's path, measures
in the hundreds of microseconds. Full credit to the
[Headroom](https://headroomlabs-ai.github.io/headroom/) project for the core; busbar
just puts it in front of every model you call.

Reproduce every number: see [`bench/README.md`](bench/README.md).

## Install and run

Headroom is a **busbar plugin**: a signed cdylib busbar `dlopen`s in-process (busbar's
plugin ABI). There is no standalone binary and no socket to wire up — busbar loads the
library directly and calls it as an in-process rewrite gate. Two ways to get it
running:

### 1. Bundled image (simplest — one container, zero config)

If you came to busbar specifically to run Headroom and just want it working, this is
the path: busbar + Headroom in one container, pre-installed and pre-wired.

```sh
docker run -d -p 8080:8080 \
  -e ANTHROPIC_KEY -e BUSBAR_ADMIN_TOKEN \
  getbusbar/busbar-headroom
```

Every request through the default pool is compressed immediately — no plugin install
step. See [`docker/bundle/config.yaml`](docker/bundle/config.yaml) for the baked-in
default config (one Anthropic provider, one pool, Headroom wired as a global
`prompt: rw` gate) and [`docker/bundle/Dockerfile`](docker/bundle/Dockerfile) for how
the image is built — busbar compiled from source with the same PGO release build
busbar's own official image uses, Headroom's cdylib built and signed alongside it in
the same CI run. Mount your own `config.yaml` over `/etc/busbar/config.yaml` to
replace the default entirely; copy the `plugins:`/`pools.*.hooks` blocks from the
baked-in default into yours to keep Headroom wired. Published by
[`.github/workflows/docker-bundle.yml`](.github/workflows/docker-bundle.yml).

This image is distinct from `getbusbar/busbar` (busbarAI's own plugin-free image) and
supersedes the old `getbusbar/headroom-hook` standalone image described later in this
repo's history (that image predates busbar's dlopen plugin ABI and is no longer
buildable — see the root `Dockerfile`'s header).

### 2. Plugin drop-in (if you already run busbar)

If you already have a busbar deployment — [`getbusbar/busbar`](https://github.com/GetBusbar/busbar)
or your own build — install Headroom into it the same way you'd install any other
first-party plugin:

1. Grab the signed tarball for your platform from this repo's
   [Releases](https://github.com/GetBusbar/headroom-hook/releases)
   (`busbar-headroom-<arch>.tar.gz`).
2. Drop it into busbar's plugin directory and enable plugins in your `config.yaml`:

   ```yaml
   plugins:
     enabled: true
     dir: /etc/busbar/plugins   # or wherever you dropped the tarball
   ```
3. Wire it in as a hook wherever you want compression — globally or per-pool:

   ```yaml
   pools:
     default:
       hooks:
         - { module: busbar-headroom, kind: gate, prompt: rw, timeout_ms: 50 }
       members:
         - model: claude
   ```

Because the tarball is signed with busbar's release key, it loads as first-party with
zero extra trust configuration (`plugins.trust.allow_unsigned` is only needed for
unsigned/dev builds).

### Build from source

Needs a Rust toolchain ([rustup](https://rustup.rs)); the pinned `headroom-core` rev is
in `Cargo.toml`. This builds the cdylib only (`crate-type = ["cdylib", "rlib"]` — there
is no `[[bin]]`, no standalone executable), which you then sign/pack with busbar's own
`busbar-plugin-pack` tool before busbar will load it as first-party (or load it
unsigned in dev mode via `plugins.trust.allow_unsigned`):

```sh
git clone https://github.com/GetBusbar/headroom-hook && cd headroom-hook
cargo build --release      # cdylib: target/release/libheadroom_hook.so (.dylib on macOS)
```

### Settings

`target_ratio` (default 0.5, the fraction of tokens to keep), `min_savings_pct`
(default 10, abstain below this saving), and `price_udollars_per_ktok` (default 2500,
the $-estimate price) seed the startup values, then live-update as busbar pushes
settings — retune without a restart over the admin API's hook-settings endpoint. See
[Metrics](#metrics) below for the matching status/metrics surface.

**OS support.** As a dlopen'd cdylib, Headroom runs wherever busbar itself runs and
can load a `.so`/`.dylib` built for the target platform — no native Windows build yet
(build and load inside WSL2 or a Linux container on Windows, same as any other
first-party hook plugin).

## The wire (busbar's 5-message hook protocol)

Newline-delimited JSON on one kept-alive Unix-socket connection,
dispatched by the top-level key:

| message | direction | this hook's reply |
|---|---|---|
| `configure` | busbar → hook, **first line of every connection** + live on a settings PATCH | `{"ack":{"settings_version":N}}` echoing the pushed version — only if the settings applied cleanly (commit-on-ack) |
| `describe` | busbar → hook, any time | `{schema, dashboard}` — the settings JSON Schema (`GET /api/v1/admin/hooks/headroom/schema`) + the dashboard widget layout |
| `status` | busbar → hook, any time | `{status:{settings, metrics:[…]}}` — observed settings + self-reported metrics (see [Metrics](#metrics)) |
| decide / transform | busbar → hook, per request | `{"rewrite":{...}}` or `{}` (abstain) |
| notify | busbar → hook, fire-and-forget (taps only) | none read |

> **Note:** this table describes the message *shape* busbar's hook contract still
> speaks conceptually (configure/describe/status/transform/notify); the *transport* it
> travels over changed from the Unix-socket wire below to an in-process dlopen call
> (`HookHandler` trait methods) when this hook was ported to busbar's signed plugin ABI
> — see [Install and run](#install-and-run) above. The settings PATCH example and
> config block further down are current; the raw socket framing is historical.

Settings (`target_ratio`, `min_savings_pct`, `price_udollars_per_ktok`)
arrive as desired state: a key absent from the push resets to its default, a
rejected push (unknown key, bad value) is never acked and busbar keeps the
previous settings. Retune live, no restart:

```sh
curl -X PATCH localhost:8080/api/v1/admin/hooks/headroom/settings \
  -d '{"target_ratio": 0.3, "min_savings_pct": 20}'
```

## Metrics

The hook reports its own operational metrics on the `status` message, and busbar surfaces them two
ways from that one source:

- **Live JSON** — `GET /api/v1/admin/hooks/headroom/status` queries the hook on the spot (you set the
  resolution by how often you poll: hit it every second, get one-second-fresh data).
- **Prometheus** — busbar's `/metrics/hooks` scrape renders the same metrics as standard text, with
  the metric **names verbatim** and an automatic `hook="headroom"` label.

The names follow Headroom's own vocabulary where they map —
`proxy_compression_ratio_by_strategy{strategy,content_type}`,
`proxy_compression_rejected_by_token_check_total`, `proxy_passthrough_bytes_modified_total` — so a
dashboard built against Headroom points at busbar and lights up. Alongside them, busbar-native
per-pool extras: `tokens_saved_total`, `dollars_saved` (an estimate, priced off
`price_udollars_per_ktok`, marked `estimated` with a confidence interval — busbar's `/usage` is the
*measured* spend), `compress_latency_us` (a p50/p90/p99 histogram), and the request counters. Every
series carries a `pool` label, so one process serving N pools shows N rows.

## Wire into busbar (fleet-wide)

Current config shape (`kind: hook` dlopen plugin, not the retired `socket:` gate — see
[Install and run](#install-and-run) above for the full drop-in steps):

```yaml
plugins:
  enabled: true
  dir: /etc/busbar/plugins   # where you dropped busbar-headroom-<arch>.tar.gz

pools:
  default:
    hooks:
      - { module: busbar-headroom, kind: gate, prompt: rw, timeout_ms: 50 }
    members:
      - model: claude
```

## A/B test it (same binary, two pools)

The clean experiment: one busbar, two pools over the same model — one with
the hook, one without — and point half your traffic at each. Compare
per-pool tokens and latency in `/metrics` or `GET /api/v1/admin/usage`.

```yaml
pools:
  with-headroom:
    hooks: [headroom]    # drop `global: true` from the hook definition
    members: [ { target: claude-sonnet, weight: 1 } ]
  baseline:
    members: [ { target: claude-sonnet, weight: 1 } ]
```

> **Status:** pool-scoped rewrite gates are not fired by the current
> 1.3.0-dev engine yet (the transform pass is global-only), and on
> same-protocol passthrough the engine's pristine-bytes fast path can skip
> the rewritten body — both found while building this POC and tracked for
> the engine. Until then, A/B with two busbar instances (one with
> `global: true`, one without), which is how the numbers above were
> measured.

`scripts/smoke.sh` proves the socket protocol without busbar: the
configure-ack handshake, a describe, and a rewrite round-trip on one
connection, in busbar's order.
The `bench/` directory has the full measurement rig and results.
