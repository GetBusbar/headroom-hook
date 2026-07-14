# Headroom → busbar integration notes

*2026-07-12 — findings from evaluating [headroom](https://github.com/headroomlabs-ai/headroom)
and building the hook in this directory.*

## What headroom is

Context compression for LLM traffic: compresses tool outputs, logs, RAG
chunks, files, and history before they reach the model. Claims 60–95% token
reduction. Apache 2.0. Ships as a Python library/CLI, a proxy server, agent
wrappers, an MCP server — and a Rust core.

## Key finding: the compression core is Rust

The project started in Python and is mid-port to Rust. The Rust crates are
the *new* canonical implementation, parity-tested byte-for-byte against the
Python originals (`headroom-parity` crate, fixture-locked).

| Part | Lines | Relevance |
|---|---|---|
| `crates/headroom-core` (Rust) | ~37k | **The piece we use** — all compression engines |
| `crates/headroom-proxy` (Rust) | ~30k | Separate crate; never touched |
| Python package | ~176k | Original impl + proxy (40k) + CLI/memory/evals |
| Python compression-relevant | ~25k | `transforms/` + `compression/` + `ccr/` |

The proxy is physically separate from the compression in both languages —
no entanglement to cut through.

## How a Rust binary hooks in (the POC in this repo)

```toml
[dependencies]
headroom-core = { git = "https://github.com/headroomlabs-ai/headroom",
                  rev = "c41cf444c74a9d190ab5922836122d0d10bc988c" }
```

```rust
use headroom_core::transforms::TextCrusher;

let crusher = TextCrusher::default();
let result = crusher.compress(&prompt, &query, Some(0.4));
let new_prompt = result.compressed;  // + token/segment stats on `result`
```

Verified working (`cargo run`): a 12-segment deployment log compressed
128 → 54 tokens, keeping the ERROR line because it scored highest against
the query "why did the deployment fail". No proxy, no Python, no network,
no model download.

## No LLM/model config needed

`TextCrusher` is purely lexical — BM25 relevance vs the query + recency +
salience (error markers, numbers). It never needs to know which LLM the
prompt is destined for. The `model=` parameter in headroom's Python
`compress()` is only used for tokenizer-accurate savings *counting* and
provider message formats, not compression decisions.

All knobs, all optional:

- Per call: `target_ratio` (keep fraction, default 0.5) and `query`
  (`""` = rank by recency+salience only).
- `TextCrusherConfig`: scoring weights (`w_recency` 1.0, `w_relevance` 2.0,
  `w_salience` 1.5), `min_segment_chars` 12, `near_dup_threshold` 0.85,
  `min_segments_for_crush` 6.

Built-in safety: inputs with fewer than 6 segments pass through **unchanged**
— short prompts are automatically left alone.

## What else the crate offers (next steps)

- `CompressionPipeline` — auto-routing orchestrator: `detect_content_type()`
  picks JSON/log/diff/text, dispatches to `JsonMinifier` (lossless),
  `SmartCrusher` (JSON arrays), `LogOffload`, `DiffOffload`.
- CCR (Compress-Cache-Retrieve) — offloads are *reversible*: dropped bytes
  land in a `CcrStore` (in-memory / SQLite / Redis backends included) keyed
  by hash; compressed output carries retrieval markers so the LLM can fetch
  originals via a tool call. Optional — reformat-only use never touches it.
- `compress_anthropic_live_zone` / `compress_openai_chat_live_zone` —
  message-array-level compression (closest to what a gateway like busbar
  would want).

## Caveats

- **Unpublished + churning**: `headroom-core` is 0.1.0, not on crates.io,
  actively mid-port ("Phase B" comments). Pin a git rev; expect API movement.
- **Fat dependency tree**: no feature gates yet, so even TextCrusher-only
  use pulls ~470 crates including `ort` (ONNX runtime), `fastembed`,
  `image`, `rusqlite`, `tokenizers`, `hf-hub`. For real busbar integration:
  ask upstream for feature flags, or vendor just the transforms we need.
- The ONNX/HF paths exist in the crate but nothing downloads at runtime
  unless you use the ML compressor; TextCrusher is pure BM25.

## Busbar wire: 3 messages → 5 (2026-07-12)

The engine's hook wire grew two management messages on top of
decide/transform/notify, and the Headroom hook now speaks all five
(engine `src/routing/wire.rs` + `src/routing/socket.rs`, `docs/hooks.md`):

- **`configure`** — `{"configure": {hook, settings, settings_version,
  busbar_version}}`, the FIRST line busbar sends on every socket
  (re)connection and re-pushed live on
  `PATCH /admin/v1/hooks/{name}/settings`. Commit-on-ack: the hook must
  reply `{"ack": {"settings_version": N}}` echoing the pushed version or
  busbar treats the configure as not committed (the PATCH 400s, the hook's
  previous settings stay authoritative). The hook applies the map as
  desired state (absent key = default) and refuses to ack a map it can't
  cleanly apply (unknown key, bad type/range).
- **`describe`** — `{"describe": true}`, any time; the hook replies its
  settings JSON Schema, served verbatim at
  `GET /admin/v1/hooks/{name}/schema`. Optional — an ignoring hook just
  reports `schema: null` — but Headroom answers (target_ratio,
  min_savings_pct).

All five ride the same newline-delimited JSON connection; dispatch is by
the top-level key (`configure` / `describe` / `request`). Env vars now only
seed the startup knobs; a configure push replaces them live, process-wide.

## Repo state

- Lives in its own repo, `GetBusbar/headroom-hook` (was extracted from the
  short-lived `GetBusbar/Hooks` monorepo — one repo per hook, for per-repo
  access control).
- Run: `cargo run --release`, or `docker compose up` (busbar + hook together).
