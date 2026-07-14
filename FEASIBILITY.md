# Headroom hook — feasibility assessment

*2026-07-12. Prototype in this directory; measurements on an M-series Mac,
headroom-core pinned to `c41cf444` (the rev evaluated in `../NOTES.md`).*

## Verdict

**Feasible and cheap to ship as a separate binary.** The compression is
fast (sub-millisecond for typical histories), quality is real (32–58% token
reduction on compressible content, clean pass-through on short chats), and
the wire contract fits busbar's rewrite arm exactly. The two real costs are
the **dependency tree** (330 crates for what is lexically a BM25 scorer)
and **API churn risk** (0.1.0, unpublished, mid-port). Neither blocks a
prototype; both inform packaging (below).

## Measurements

| Metric | Value |
|---|---|
| Unique crates pulled (`cargo tree`, this binary) | **330** — incl. `ort` (ONNX), `fastembed`, `image`, `tokenizers`, `hf-hub`, `rusqlite`; upstream has no feature gates, so TextCrusher-only use pays for all of it (build-time only: nothing ML runs or downloads at runtime) |
| Clean debug build (warm registry) | ~18 s wall / 99 s CPU |
| Release build | ~30 s; binary **3.4 MB** (debug 11 MB) |
| Compress latency, ~16 KB two-message history | **0.70 ms** release (7.8 ms debug) per `handle_line` |
| Recommended `timeout_ms` | **25** (default 1 ms is far too low for a content hook; 25 ms covers ~500 KB of history at measured throughput) |

### Compression quality (three samples)

| Sample | Result |
|---|---|
| 40-line deployment log + "why did the deployment fail" | 2820 → 1197 chars (**58% saved**); the one ERROR line kept (highest BM25 vs query) |
| 8 RAG doc-chunks (~5 KB) + "why are payment gateway requests timing out" | 616 → 196 tokens (**68% saved**, 21/56 segments); timeout-related chunks kept |
| 3-message short chat | **abstains** — `{}`, byte-identical request proceeds (TextCrusher's <6-segment guard + the hook's min-savings gate) |

## End-to-end measurements (real busbar in the loop)

Debug busbar + mock upstream + release hook, 300 requests per cell, 11 KB
three-message body (two noisy-log history messages + a short ask):

| Path | Tokens | p50 latency |
|---|---|---|
| Cross-protocol (anthropic ingress → openai egress), `global: true` | 2,836 → **1,173 (59% saved)** | 0.72 → 1.41 ms (**+0.69 ms**) |
| Same-protocol (anthropic → anthropic), `global: true` | 2,836 → 2,836 (**0%** — see engine bug below) | +0.74 ms |
| Pool-scoped `hooks: [headroom]` (either protocol) | unchanged (**0%** — see engine gap below) | +1.0 ms |

**Two engine findings (for the 1.3 build to fix):**

1. **Same-protocol passthrough discards global rewrites.** The per-hop
   serializer's pristine-bytes short-circuit (`forward/mod.rs`, "Request
   SHORT-CIRCUIT") re-emits the retained ORIGINAL request bytes when a
   same-protocol hop triggered none of invalidators #1–#4 — but the
   phase-1 rewrite pass mutated only the parsed `Value`, so the rewrite
   is silently dropped on exactly the most common path. The rewrite pass
   must invalidate pristineness (or re-serialize into the retained bytes).
2. **Pool-scoped rewrite gates never fire the transform pass.** Rewrite
   hooks resolve from `global_hooks` only (`resolve_rewrite_hooks`); a
   `prompt: rw` gate named in a pool's `hooks: [...]` list fires as a
   phase-2 DECISION gate, its rewrite-only reply normalizes to Abstain,
   and the request pays the gate latency for nothing. Per-pool rewrite is
   the natural A/B-test shape, so this is worth wiring (or loudly
   rejecting at boot until it is).

## headroom's own benchmarks, compared

Their published numbers ([benchmarks page](https://headroomlabs-ai.github.io/headroom/benchmarks/),
proxy in production, 250+ instances): **52 ms median added latency** (P90
309 ms, P99 4.2 s, mean 161 ms) and **4.8% median compression** (short
conversational turns dominate; heavy tool-use sessions see 40–80%). Their
`compress()` SDK call alone is 1–2 ms on tool outputs with ~66% savings on
compressible content and 0% on already-compact content — consistent with
what we measure from the same core.

Read: the compression core is fast and good; their *Python proxy wrapper*
is where the production latency (and its ugly tail) comes from. Running
the same core as a busbar socket gate is ~75× cheaper at the median
(0.69 ms vs 52 ms) with no added network hop and busbar's fail-safe
semantics — which is precisely the "busbar is where your AI middleware
runs" pitch, demonstrated.

## Wire-contract findings (verified against engine source)

1. **Framing**: newline-delimited JSON over the Unix socket, connection
   kept alive and reused; one `HookRequest` line in, one reply line out.
   Matches `src/routing/socket.rs` exactly; proven by `scripts/smoke.sh`.
2. **Shape asymmetry (doc-worthy)**: the hook *receives* messages in
   projection form `{role, text}` but must *reply* in **body form**
   `{role, content}` — `apply_rewrite_to_body` splices the reply array
   verbatim into the request's `messages`. Works fine, but nothing on the
   wire says so; hook authors will trip on it. Worth a line in
   `docs/hooks.md` and/or an engine-side normalization.
3. **64 KiB reply cap**: `socket.rs` caps reply lines at 64 KiB. A large
   history that compresses to >64 KiB of JSON is dropped by busbar →
   `on_error` → original body proceeds (safe, but silently uncompressed —
   the biggest prompts, where compression matters most, are exactly the
   ones that hit it). Engine option: raise/configure the cap for
   transform replies.
4. **System prompt is not rewritable**: the rewrite arm carries
   `messages` + `tools` only. Fine for history compression; a
   system-prompt compressor would need an engine extension.
5. **Decide fires**: a `prompt: rw` gate named in a *pool's* `hooks:` list
   also receives decision fires; replying `{}`/rewrite-only normalizes to
   Abstain, so one handler serves both. (As a `global: true` rewrite gate
   it only gets transform fires.)

## Risks

- **API churn**: `headroom-core` is 0.1.0, unpublished, "Phase B" mid-port.
  The pin protects builds; expect manual bumps. `TextCrusher::compress`'s
  signature is simple enough that churn is absorbable in one file.
- **Dep supply-chain surface**: 330 crates is a lot of audit surface for a
  security-adjacent product. Mitigations below.
- **Latency under adversarial input**: BM25 is O(n); the 8 MB inbound line
  cap + busbar's own body limits bound the worst case (~50 ms at measured
  throughput). `timeout_ms: 25` keeps busbar safe regardless (fail-open to
  the uncompressed body).

## Packaging options

| Option | Pros | Cons |
|---|---|---|
| **A. Separate repo/crate shipping a static binary (this shape)** | Zero engine coupling; operator installs one binary + 4 lines of YAML; busbar's fail-safe means it can never take the gateway down | 330-crate build we must audit/build; rev-pinned churn |
| B. Vendor just the TextCrusher transform (~single-file lexical scorer) | Tiny dep tree (serde only); full audit control | Fork maintenance; loses the upcoming pipeline/CCR features that make Headroom interesting |
| C. Upstream feature flags (ask headroomlabs for `transforms`-only feature) | Best of A+B | Depends on upstream; they're mid-port |
| D. Example in busbar's docs only | No maintenance | Nothing shippable/marketable; no "Hook Store" artifact |

**Recommendation: A now (this prototype is that shape), pursue C in
parallel** — file the upstream feature-flag ask; if accepted, A's dep tree
collapses and B becomes moot. B remains the fallback if upstream churn
becomes unmanageable. The partnership/positioning question (co-marketing
with Headroom vs quiet integration) stays a product decision — out of
scope here.

## What was tested

- 5 unit tests (compression + abstain paths + malformed input + latency
  ceiling) — green.
- `scripts/smoke.sh`: end-to-end **transport** smoke (real socket, real
  NDJSON framing, real reply parse). A full busbar-in-the-loop e2e (mock
  upstream, assert compressed body arrives) was not wired — the engine's
  own `apply_rewrite_to_body` and socket-transform tests already cover the
  busbar side of the seam; the smoke test covers ours.
