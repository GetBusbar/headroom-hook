// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! The pure half of the hook: busbar's wire line in, reply JSON out.
//!
//! Busbar's **5-message wire** (engine `src/hooks/wire.rs` + `src/hooks/socket.rs`, verified
//! against the source, 2026-07-12): `configure`, `describe`, `decide`, `transform`, `notify` — all
//! newline-delimited JSON on the one kept-alive connection, discriminated by the top-level key.
//!
//! Management messages:
//! - `configure` — `{"configure": {"hook", "settings": {...}, "settings_version", "busbar_version"}}`.
//!   Busbar sends it as the FIRST line on every (re)connection, and re-pushes it live when an
//!   operator calls `PATCH /api/v1/admin/hooks/{name}/settings`. Commit-on-ack: we reply
//!   `{"ack": {"settings_version": N}}` (echoing the pushed version) ONLY when the settings applied
//!   cleanly; anything else and busbar treats the configure as not committed (the PATCH gets a 400,
//!   we keep serving on our previous settings).
//! - `describe` — `{"describe": true}`. We reply the `{schema, dashboard}` self-description: the
//!   settings JSON Schema (served at `GET /api/v1/admin/hooks/{name}/schema`) and the dashboard
//!   widget layout. ONE declaration drives both the config form and the plugin dashboard.
//! - `status` — `{"status": true}`. We reply our OBSERVED settings + a metrics ARRAY (Prometheus-
//!   shaped: per-pool `labels`, `histogram` quantiles, `estimated` values with a CI). Busbar surfaces
//!   it live at `GET /api/v1/admin/hooks/{name}/status` AND on its `/metrics/hooks` Prometheus scrape,
//!   so a dashboard built against these metric names just works.
//!
//! Decision traffic (unchanged from the 3-message wire):
//! - The hook RECEIVES `{"request": {..., "system"?, "messages"?: [{"role","text"}]}, "candidates":
//!   [...], "context": {...}}`. A `prompt: rw` gate always gets `system`/`messages` on the
//!   transform pass (`build_rewrite_request` sends the prompt unconditionally).
//! - The hook REPLIES `{"rewrite": {"messages": [...]}}` where each entry is spliced VERBATIM into
//!   the pending request body's `messages` array (`apply_rewrite_to_body`). The body at that point
//!   is the ingress dialect's chat shape, so entries must be BODY-form `{"role": ..., "content":
//!   ...}` — NOT the projection's `{role, text}` form. `{}` is abstain (proceed unmodified).
//! - Busbar is fail-closed on our side: a malformed/oversized/slow reply means the ORIGINAL body
//!   proceeds — this hook can degrade, it can never corrupt.

use headroom_core::transforms::TextCrusher;
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::{Mutex, RwLock};

/// Busbar caps hook reply lines at 64 KiB (`socket.rs` `MAX_REPLY_BYTES`) — a longer line is a
/// protocol error that drops the connection AND the reply. `encode_reply` enforces the cap on our
/// side: an over-cap reply degrades to abstain (original body proceeds, connection survives).
pub const MAX_REPLY_BYTES: usize = 64 * 1024;

/// One message of busbar's prompt projection (`{role, text}` — flattened text form).
#[derive(Deserialize)]
struct ProjMessage {
    role: String,
    text: String,
}

/// The slice of `HookRequest.request` this hook reads. Everything else is ignored (the wire is
/// append-only; unknown fields must never break a hook).
#[derive(Deserialize)]
struct ProjRequest {
    /// The pool this request routes through — the label dimension for every per-pool metric. Absent
    /// on an older engine / a probe folds into an `"unknown"` bucket rather than dropping the count.
    #[serde(default)]
    pool: Option<String>,
    #[serde(default)]
    messages: Option<Vec<ProjMessage>>,
}

#[derive(Deserialize)]
struct HookLine {
    request: ProjRequest,
}

/// The `configure` push (engine `wire::ConfigureMsg`). Only the fields we act on are typed;
/// `hook`/`busbar_version` are context echoes we don't need (the wire is append-only — unknown
/// fields must never break a hook).
#[derive(Deserialize)]
struct ConfigureLine {
    configure: ConfigureBody,
}

#[derive(Deserialize)]
struct ConfigureBody {
    #[serde(default)]
    settings: serde_json::Map<String, Value>,
    settings_version: u64,
}

/// The `describe` request: `{"describe": true}`.
#[derive(Deserialize)]
struct DescribeLine {
    describe: bool,
}

/// The `status` request: `{"status": true}`.
#[derive(Deserialize)]
struct StatusLine {
    status: bool,
}

/// Compression knobs (env-seeded in `main`, replaced live by `configure` pushes).
#[derive(Clone, Copy)]
pub struct Knobs {
    /// Fraction of tokens to KEEP per compressed message (TextCrusher `target_ratio`).
    pub target_ratio: f64,
    /// Abstain unless the whole-prompt char savings reach this percentage.
    pub min_savings_pct: f64,
    /// Assumed input price in micro-dollars per 1,000 tokens, used to turn tokens-saved into the
    /// estimated `dollars_saved` metric (busbar's measured `/usage` spend is the separate truth; this
    /// is the hook's own estimate, marked `estimated` with a confidence interval). Default 2,500 ≈
    /// $2.50 / 1M input tokens.
    pub price_udollars_per_ktok: f64,
    /// The `settings_version` of the last committed `configure`, echoed in `status` so busbar can
    /// compute version drift. 0 until the first configure.
    pub settings_version: u64,
}

impl Default for Knobs {
    fn default() -> Self {
        Self {
            target_ratio: 0.5,
            min_savings_pct: 10.0,
            price_udollars_per_ktok: 2500.0,
            settings_version: 0,
        }
    }
}

/// Apply a pushed settings map as DESIRED STATE: present keys override, absent keys reset to the
/// built-in defaults — so re-pushing the same map is a no-op (idempotent, as the wire requires).
/// FAIL-CLOSED on commit: an unknown key or an out-of-shape/out-of-range value returns `Err`, the
/// caller does NOT ack, and busbar keeps our previous settings (the operator's PATCH gets a 400
/// instead of us half-applying a map we didn't understand).
pub fn apply_settings(settings: &serde_json::Map<String, Value>) -> Result<Knobs, String> {
    let mut knobs = Knobs::default();
    for (key, value) in settings {
        match key.as_str() {
            "target_ratio" => {
                let v = value
                    .as_f64()
                    .ok_or_else(|| format!("target_ratio must be a number, got {value}"))?;
                if !(0.05..=1.0).contains(&v) {
                    return Err(format!("target_ratio must be in 0.05..=1.0, got {v}"));
                }
                knobs.target_ratio = v;
            }
            "min_savings_pct" => {
                let v = value
                    .as_f64()
                    .ok_or_else(|| format!("min_savings_pct must be a number, got {value}"))?;
                if !(0.0..=100.0).contains(&v) {
                    return Err(format!("min_savings_pct must be in 0..=100, got {v}"));
                }
                knobs.min_savings_pct = v;
            }
            "price_udollars_per_ktok" => {
                let v = value.as_f64().ok_or_else(|| {
                    format!("price_udollars_per_ktok must be a number, got {value}")
                })?;
                if !(0.0..=1_000_000.0).contains(&v) {
                    return Err(format!(
                        "price_udollars_per_ktok must be in 0..=1_000_000, got {v}"
                    ));
                }
                knobs.price_udollars_per_ktok = v;
            }
            other => return Err(format!("unknown setting {other:?}")),
        }
    }
    Ok(knobs)
}

/// The `describe` reply ENVELOPE (busbar `wire::DescribeReply`): `{schema, dashboard}`. `schema` is
/// the settings JSON Schema busbar serves verbatim at `GET /api/v1/admin/hooks/{name}/schema` (the
/// config form); `dashboard` declares the widget layout for the plugin dashboard, whose values come
/// from `status.metrics` matched by `metric` name. ONE declaration drives both.
pub fn describe_reply() -> Value {
    json!({
        "schema": settings_schema(),
        "dashboard": { "widgets": [
            {"metric": "proxy_compression_ratio_by_strategy", "label": "Compression ratio", "viz": "histogram", "unit": "ratio"},
            {"metric": "tokens_saved_total",                  "label": "Tokens saved",      "viz": "counter"},
            {"metric": "dollars_saved",                       "label": "Proxy $ saved",     "viz": "number", "unit": "$"},
            {"metric": "compress_latency_us",                 "label": "Proxy overhead",    "viz": "histogram", "unit": "us"},
            {"metric": "requests_compressed_total",           "label": "Requests compressed", "viz": "counter"}
        ]}
    })
}

/// Our settings JSON Schema — the `schema` member of the `describe` envelope.
pub fn settings_schema() -> Value {
    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "title": "headroom-hook settings",
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "target_ratio": {
                "type": "number", "minimum": 0.05, "maximum": 1.0, "default": 0.5,
                "description": "Fraction of tokens KEPT per compressed history message."
            },
            "min_savings_pct": {
                "type": "number", "minimum": 0.0, "maximum": 100.0, "default": 10.0,
                "description": "Abstain (pass the prompt through unmodified) unless whole-prompt char savings reach this percentage."
            },
            "price_udollars_per_ktok": {
                "type": "number", "minimum": 0.0, "maximum": 1000000.0, "default": 2500.0,
                "description": "Assumed input price (micro-dollars per 1,000 tokens) used to estimate dollars saved."
            }
        }
    })
}

/// Serialize a reply into its newline-terminated wire line, enforcing busbar's 64 KiB reply cap.
/// A reply that would exceed the cap (a very large history whose compressed form is still >64 KiB
/// of JSON) is replaced with abstain: busbar would drop an over-cap line as a protocol error and
/// tear down the connection, so degrading to "proceed with the original body" on a live connection
/// strictly dominates. Serialization failure likewise degrades to abstain.
pub fn encode_reply(reply: &Value) -> Vec<u8> {
    let mut out = serde_json::to_vec(reply).unwrap_or_else(|_| b"{}".to_vec());
    out.push(b'\n');
    if out.len() > MAX_REPLY_BYTES {
        return b"{}\n".to_vec();
    }
    out
}

/// Bounded per-pool sample reservoirs (compression ratios, latencies) so memory stays flat under a
/// flood; a real deployment would use a t-digest, this keeps the example honest and cheap.
const MAX_SAMPLES: usize = 4096;

/// Per-pool operational tallies — the raw material for the `status` metrics array. One process serves
/// many pools; busbar sends `request.pool` on every transform, so we key by it.
#[derive(Default)]
struct PoolStat {
    /// Transform requests SEEN on this pool (the compressed-rate denominator).
    requests_seen: u64,
    /// Requests actually COMPRESSED (savings cleared `min_savings_pct`).
    requests_compressed: u64,
    /// Runs where compression did NOT shrink the body (compressed >= original) — Headroom's
    /// `proxy_compression_rejected_by_token_check_total` semantics (we abstained).
    rejected_no_shrink: u64,
    /// Lifetime input / output chars on compressed requests.
    chars_in: u64,
    chars_out: u64,
    /// Per-compressed-message ratio (compressed_len / original_len) — the distribution behind
    /// Headroom's `proxy_compression_ratio_by_strategy`.
    ratio_samples: Vec<f64>,
    /// Per-request compression latencies (micros) — Headroom's proxy-overhead distribution.
    latency_us: Vec<u64>,
}

/// Process-wide, per-pool metrics accumulator. Behind a `Mutex` (contention is trivial — one short
/// critical section per request); poison-tolerant on the request path.
#[derive(Default)]
pub struct Metrics {
    pools: std::collections::BTreeMap<String, PoolStat>,
}

/// Estimated tokens for a char count (≈ chars/4, the standard English heuristic). This is the hook's
/// OWN estimate for `tokens_saved` / `dollars_saved`; busbar's `/usage` reports the measured truth.
fn est_tokens(chars: u64) -> u64 {
    chars.div_ceil(4)
}

/// Handle one busbar wire line; returns the reply JSON (one line, no trailing newline).
///
/// Dispatch is by the top-level key, as the wire specifies: `configure` applies settings and acks,
/// `describe` returns the schema, everything else is decision traffic. Busbar serializes the
/// management messages compactly with the discriminating key FIRST, so a cheap prefix check avoids
/// a second full parse of every (potentially multi-MB) request line; a management line that
/// somehow misses the prefix falls through to the request parse and abstains — which busbar
/// already treats as "configure not committed" / "no schema": fail-safe either way.
///
/// Rewrite strategy — compress the HISTORY, keep the ask: the LAST message is preserved verbatim
/// and its text becomes the BM25 relevance query for every earlier message, so the parts of the
/// history that matter to the current ask survive. TextCrusher itself passes short texts
/// (<6 segments) through unchanged, and we abstain outright when total savings are below
/// `min_savings_pct` — a no-win rewrite costs a body re-render for nothing.
pub fn handle_line(line: &[u8], knobs: &RwLock<Knobs>, metrics: &Mutex<Metrics>) -> Value {
    let abstain = json!({});
    let head = line.trim_ascii_start();
    if head.starts_with(b"{\"configure\"") {
        let Ok(msg) = serde_json::from_slice::<ConfigureLine>(line) else {
            // Unparsable configure: no ack — busbar keeps our previous settings.
            return json!({"error": "malformed configure message"});
        };
        return match apply_settings(&msg.configure.settings) {
            Ok(mut new) => {
                // Record the committed version so `status` can report it for busbar's drift check.
                new.settings_version = msg.configure.settings_version;
                // Poison-tolerant: `Knobs` is `Copy` and writes are single assignments, so a
                // poisoned lock holds a fully-written value — recover it, never panic on the
                // request path.
                *knobs.write().unwrap_or_else(|e| e.into_inner()) = new;
                json!({"ack": {"settings_version": msg.configure.settings_version}})
            }
            Err(e) => json!({"error": e}),
        };
    }
    if head.starts_with(b"{\"describe\"") {
        return match serde_json::from_slice::<DescribeLine>(line) {
            Ok(DescribeLine { describe: true }) => describe_reply(),
            _ => abstain,
        };
    }
    // STATUS: `{"status": true}` -> our OBSERVED settings + self-reported metrics (an ARRAY of
    // Prometheus-shaped entries busbar surfaces on the admin API AND its /metrics/hooks scrape).
    if head.starts_with(b"{\"status\"") {
        return match serde_json::from_slice::<StatusLine>(line) {
            Ok(StatusLine { status: true }) => build_status(knobs, metrics),
            _ => abstain,
        };
    }
    let knobs = *knobs.read().unwrap_or_else(|e| e.into_inner());
    // Not our shape / no prompt projection (e.g. a decide fire, or a grant misconfig) -> abstain.
    let Ok(parsed) = serde_json::from_slice::<HookLine>(line) else {
        return abstain;
    };
    let pool = parsed.request.pool.unwrap_or_else(|| "unknown".to_string());
    let Some(messages) = parsed.request.messages else {
        return abstain;
    };
    if messages.len() < 2 {
        // Nothing before the ask to compress.
        return abstain;
    }

    let started = std::time::Instant::now();
    let query = messages.last().map(|m| m.text.clone()).unwrap_or_default();
    let crusher = TextCrusher::default();

    let mut chars_before = 0usize;
    let mut chars_after = 0usize;
    let mut out: Vec<Value> = Vec::with_capacity(messages.len());
    // Per-shrunk-block compression ratios (compressed/original) + a count of runs that did NOT
    // shrink — the raw material for `proxy_compression_ratio_by_strategy` and its rejected counter.
    let mut ratios: Vec<f64> = Vec::new();
    let mut rejected: u64 = 0;
    let last = messages.len() - 1;
    for (i, m) in messages.iter().enumerate() {
        chars_before += m.text.len();
        let text = if i == last {
            m.text.clone()
        } else {
            // PANIC CONTAINMENT: `TextCrusher` is third-party code on the request path. If it
            // panics on some input, keep that message verbatim instead of dying — a hook must
            // never crash on malformed/adversarial content. (`AssertUnwindSafe` is sound here:
            // the closure only reads `&m.text`/`&query` and the crusher is dropped either way.)
            let compressed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                crusher
                    .compress(&m.text, &query, Some(knobs.target_ratio))
                    .compressed
            }))
            .unwrap_or_else(|_| m.text.clone());
            // Observe this block: shrank -> ratio sample; didn't -> a rejected-by-check run.
            if !m.text.is_empty() && compressed.len() < m.text.len() {
                ratios.push(compressed.len() as f64 / m.text.len() as f64);
            } else {
                rejected += 1;
            }
            compressed
        };
        chars_after += text.len();
        // BODY form: {role, content} — spliced verbatim into the request's `messages`.
        out.push(json!({"role": m.role, "content": text}));
    }
    let elapsed_us = started.elapsed().as_micros() as u64;

    let savings_pct = if chars_before == 0 {
        0.0
    } else {
        100.0 * (chars_before.saturating_sub(chars_after)) as f64 / chars_before as f64
    };
    let committed = chars_before > 0 && savings_pct >= knobs.min_savings_pct;

    // Record metrics for this request (poison-tolerant; one short critical section).
    {
        let mut m = metrics.lock().unwrap_or_else(|e| e.into_inner());
        let s = m.pools.entry(pool).or_default();
        s.requests_seen += 1;
        s.rejected_no_shrink += rejected;
        if s.latency_us.len() < MAX_SAMPLES {
            s.latency_us.push(elapsed_us);
        }
        for r in ratios {
            if s.ratio_samples.len() < MAX_SAMPLES {
                s.ratio_samples.push(r);
            }
        }
        if committed {
            s.requests_compressed += 1;
            s.chars_in += chars_before as u64;
            s.chars_out += chars_after as u64;
        }
    }

    if committed {
        json!({"rewrite": {"messages": out}})
    } else {
        abstain
    }
}

/// Build the `status` reply: OBSERVED settings + the metrics ARRAY. Metric NAMES use Headroom's own
/// vocabulary where they map (`proxy_compression_ratio_by_strategy`,
/// `proxy_compression_rejected_by_token_check_total`, `proxy_passthrough_bytes_modified_total`) — so
/// a Headroom dashboard repointed at busbar's `/metrics/hooks` lights up — plus busbar-native per-pool
/// extras (`tokens_saved_total`, `dollars_saved`, `compress_latency_us`, `requests*_total`). Every
/// entry carries display hints; busbar bounds/sanitizes the array on receipt.
pub fn build_status(knobs: &RwLock<Knobs>, metrics: &Mutex<Metrics>) -> Value {
    let k = *knobs.read().unwrap_or_else(|e| e.into_inner());
    let m = metrics.lock().unwrap_or_else(|e| e.into_inner());
    let mut out: Vec<Value> = Vec::new();
    for (pool, s) in m.pools.iter() {
        // ── Headroom-compatible metric names (their dashboard queries these) ──
        if let Some((count, q)) = f64_quantiles(&s.ratio_samples) {
            out.push(json!({
                "name": "proxy_compression_ratio_by_strategy", "type": "histogram", "value": count,
                "quantiles": q,
                "labels": {"strategy": "text_crusher", "content_type": "chat", "pool": pool},
                "label": "Compression ratio", "viz": "histogram", "unit": "ratio",
                "help": "compressed_tokens/original_tokens per shrunk block (headroom-core TextCrusher)"
            }));
        }
        out.push(json!({
            "name": "proxy_compression_rejected_by_token_check_total", "type": "counter",
            "value": s.rejected_no_shrink, "labels": {"pool": pool},
            "label": "Rejected (no shrink)", "viz": "counter",
            "help": "compressor runs whose output did not shrink the block (kept the original)"
        }));
        out.push(json!({
            "name": "proxy_passthrough_bytes_modified_total", "type": "counter", "value": 0,
            "labels": {"pool": pool}, "label": "Passthrough bytes modified", "viz": "counter",
            "help": "bytes modified on the verbatim-passthrough path; must stay 0 (safety)"
        }));
        // ── busbar-native extras (per-pool, our own accounting) ──
        let tokens_saved = est_tokens(s.chars_in.saturating_sub(s.chars_out));
        out.push(json!({
            "name": "tokens_saved_total", "type": "counter", "value": tokens_saved,
            "labels": {"pool": pool}, "label": "Tokens saved", "viz": "counter",
            "help": "estimated input tokens saved (chars/4); busbar /usage is the measured truth"
        }));
        let dollars = tokens_saved as f64 * k.price_udollars_per_ktok / 1000.0 / 1_000_000.0;
        out.push(json!({
            "name": "dollars_saved", "type": "gauge", "value": dollars,
            "labels": {"pool": pool}, "label": "Proxy $ saved", "unit": "$", "viz": "number",
            "estimated": true, "ci_low": dollars * 0.85, "ci_high": dollars * 1.15,
            "help": "estimated input cost saved, priced from price_udollars_per_ktok (±15% CI)"
        }));
        if let Some((count, q)) = u64_quantiles(&s.latency_us) {
            out.push(json!({
                "name": "compress_latency_us", "type": "histogram", "value": count, "quantiles": q,
                "labels": {"pool": pool}, "label": "Proxy overhead", "unit": "us", "viz": "histogram",
                "help": "per-request compression latency distribution (microseconds)"
            }));
        }
        out.push(json!({
            "name": "requests_total", "type": "counter", "value": s.requests_seen,
            "labels": {"pool": pool}, "label": "Requests", "viz": "counter",
            "help": "transform requests observed on the compression path"
        }));
        out.push(json!({
            "name": "requests_compressed_total", "type": "counter", "value": s.requests_compressed,
            "labels": {"pool": pool}, "label": "Requests compressed", "viz": "counter",
            "help": "requests whose savings cleared min_savings_pct"
        }));
    }
    let sv = if k.settings_version > 0 {
        json!(k.settings_version)
    } else {
        Value::Null
    };
    json!({"status": {
        "settings_version": sv,
        "settings": {
            "target_ratio": k.target_ratio,
            "min_savings_pct": k.min_savings_pct,
            "price_udollars_per_ktok": k.price_udollars_per_ktok
        },
        "metrics": out
    }})
}

/// p50/p90/p99 over an f64 sample set → `(count, {"0.5":.., "0.9":.., "0.99":..})`, or `None` when
/// empty. Quantile keys are probability strings in `[0,1]` — what busbar's parser accepts.
fn f64_quantiles(samples: &[f64]) -> Option<(u64, Value)> {
    if samples.is_empty() {
        return None;
    }
    let mut s = samples.to_vec();
    s.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let at = |p: f64| s[((p * (s.len() as f64 - 1.0)).round() as usize).min(s.len() - 1)];
    Some((
        samples.len() as u64,
        json!({"0.5": at(0.5), "0.9": at(0.9), "0.99": at(0.99)}),
    ))
}

/// p50/p90/p99 over a u64 sample set (latencies).
fn u64_quantiles(samples: &[u64]) -> Option<(u64, Value)> {
    if samples.is_empty() {
        return None;
    }
    let mut s = samples.to_vec();
    s.sort_unstable();
    let at = |p: f64| s[((p * (s.len() as f64 - 1.0)).round() as usize).min(s.len() - 1)];
    Some((
        samples.len() as u64,
        json!({"0.5": at(0.5), "0.9": at(0.9), "0.99": at(0.99)}),
    ))
}

#[cfg(test)]
#[path = "tests/compress.rs"]
mod tests;
