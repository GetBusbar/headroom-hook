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

The hook is a small binary you run alongside busbar; busbar connects to it over
a Unix socket. You own its lifecycle — busbar never spawns it, lazy-connects, and
reconnects across restarts, so start order doesn't matter.

### Docker (recommended): `docker compose up`

Two tiny images, one shared socket. Drop this repo's [`docker-compose.yml`](docker-compose.yml)
next to your busbar `config.yaml` (with the hook registered — see the compose
file's header) and:

```sh
docker compose up
```

busbar and the compression hook come up together and Headroom is on every
request. The images are `getbusbar/headroom-hook` and `getbusbar/busbar`.

### Prebuilt binary

Grab it from the [latest release](https://github.com/GetBusbar/headroom-hook/releases/latest):

```sh
# Linux x86_64
curl -fsSL -o headroom-hook https://github.com/GetBusbar/headroom-hook/releases/latest/download/headroom-hook-linux-x86_64
# Linux arm64
curl -fsSL -o headroom-hook https://github.com/GetBusbar/headroom-hook/releases/latest/download/headroom-hook-linux-aarch64
# macOS (Apple Silicon)
curl -fsSL -o headroom-hook https://github.com/GetBusbar/headroom-hook/releases/latest/download/headroom-hook-macos-arm64
chmod +x headroom-hook
```

Each release ships a `SHA256SUMS`; verify with `sha256sum -c SHA256SUMS`. Then run it on a socket:

```sh
HEADROOM_SOCKET=/tmp/headroom.sock ./headroom-hook
```

### Build from source

Needs a Rust toolchain ([rustup](https://rustup.rs)); the pinned `headroom-core` rev is in `Cargo.toml`.

```sh
git clone https://github.com/GetBusbar/headroom-hook && cd headroom-hook
cargo build --release      # binary: target/release/headroom-hook
```

### Settings

`HEADROOM_TARGET_RATIO` (default 0.5, the fraction of tokens to keep),
`HEADROOM_MIN_SAVINGS_PCT` (default 10, abstain below this saving), and
`HEADROOM_PRICE_UDOLLARS_PER_KTOK` (default 2500, the $-estimate price) SEED the
startup values — once busbar pushes settings over the wire, the push wins. Point
busbar at the socket (see the config block below) or register the hook live over
the admin API. That's it.

**OS support.** The hook speaks a Unix socket, so it runs on Linux and macOS
(any arch). There is no native Windows build and no HTTP mode — on Windows, run
busbar and the hook together inside WSL2 or a Linux container, where the socket
works normally.

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

```yaml
hooks:
  headroom:
    kind: gate
    socket: /tmp/headroom.sock
    prompt: rw           # the rewrite grant
    global: true         # fire on every request
    timeout_ms: 25       # ~550 µs typical; 25 ms is generous headroom
    on_error: nothing    # a broken compressor never touches a request
    settings:            # pushed to the hook as the first line of every connection
      target_ratio: 0.5
      min_savings_pct: 10
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
`FEASIBILITY.md` has the full measurements, wire-contract findings, and
packaging assessment.
