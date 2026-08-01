// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! The pure half of the plugin: busbar's `transform` payload in, reply JSON out — plus the
//! `configure`/`describe`/`status` support types the [`crate::Headroom`] gate wires into
//! [`busbar_plugin_sdk::HookHandler`].
//!
//! This module has NO knowledge of the dlopen ABI or the SDK trait; it is exhaustively unit
//! testable on its own (kept as a straight port of the original Unix-socket hook's pure logic —
//! `headroom-hook` v1.0.14's `src/compress.rs` — now driven by the new plugin's method calls
//! instead of a raw wire line).
//!
//! ## Payload shape (unchanged from the old socket wire, for the `transform` op)
//!
//! The engine's `hooks::wire` projection carries the granted prompt as `{"request": {"pool"?,
//! "messages"?: [{"role", "text"}]}}` — the SAME shape the old socket wire's `HookLine{request}`
//! read (just no longer wrapped in a `candidates`/`context` envelope we ignored anyway). We read
//! ONLY `text` on input messages (never `content` — the old code never did), and we emit BODY-form
//! `{"role", "content"}` on output, verbatim from the old code. That asymmetry is deliberate and
//! real, not a bug: the engine splices our reply straight into the pending request body's
//! `messages` array, which is BODY-form, not projection-form.
//!
//! Rewrite strategy — compress the HISTORY, keep the ask: the LAST message is preserved verbatim
//! and its text becomes the BM25 relevance query for every earlier message, so the parts of the
//! history that matter to the current ask survive. TextCrusher itself passes short texts (<6
//! segments) through unchanged, and we abstain outright when total savings are below
//! `min_savings_pct` — a no-win rewrite costs a body re-render for nothing.

use headroom_core::transforms::TextCrusher;
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::{Mutex, RwLock};

/// One message of the engine's prompt projection (`{role, text}` — flattened text form).
#[derive(Deserialize)]
struct ProjMessage {
    role: String,
    text: String,
}

/// The slice of the `transform` payload's `request` this gate reads. Everything else is ignored
/// (the projection is append-only; unknown fields must never break a hook).
#[derive(Deserialize)]
struct ProjRequest {
    /// The pool this request routes through — the label dimension for every per-pool metric.
    /// Absent on an older engine / a probe folds into an `"unknown"` bucket rather than dropping
    /// the count.
    #[serde(default)]
    pool: Option<String>,
    #[serde(default)]
    messages: Option<Vec<ProjMessage>>,
}

/// The whole `transform` payload shape: `{"request": {...}}`.
#[derive(Deserialize)]
struct HookLine {
    request: ProjRequest,
}

/// Compression knobs (config-seeded at `open`, replaced live by `configure` pushes).
#[derive(Clone, Copy)]
pub struct Knobs {
    /// Fraction of tokens to KEEP per compressed message (TextCrusher `target_ratio`).
    pub target_ratio: f64,
    /// Abstain unless the whole-prompt char savings reach this percentage.
    pub min_savings_pct: f64,
    /// Assumed input price in micro-dollars per 1,000 tokens, used to turn tokens-saved into the
    /// estimated `dollars_saved` metric (busbar's measured `/usage` spend is the separate truth;
    /// this is the hook's own estimate, marked `estimated` with a confidence interval). Default
    /// 2,500 ≈ $2.50 / 1M input tokens.
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
/// built-in defaults — so re-pushing the same map is a no-op (idempotent). FAIL-CLOSED on commit:
/// an unknown key or an out-of-shape/out-of-range value returns `Err`, the caller does NOT commit,
/// and the gate keeps its previous settings (the operator's `configure` push is NACKed instead of
/// us half-applying a map we didn't understand).
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

/// The `describe` reply ENVELOPE: `{schema, dashboard}`. `schema` is the settings JSON Schema the
/// engine serves verbatim at `GET /api/v1/admin/hooks/{name}/schema` (the config form); `dashboard`
/// declares the widget layout for the plugin dashboard, whose values come from `status.metrics`
/// matched by `metric` name. ONE declaration drives both.
pub fn describe_reply() -> Value {
    json!({
        "schema": settings_schema(),
        "dashboard": { "widgets": [
            {"metric": "headroom_tokens_saved_total",   "label": "Tokens saved",      "viz": "counter"},
            {"metric": "headroom_tokens_input_total",   "label": "Input tokens",      "viz": "counter"},
            {"metric": "headroom_overhead_ms_sum",      "label": "Overhead (ms)",     "viz": "counter", "unit": "ms"},
            {"metric": "dollars_saved",                 "label": "Proxy $ saved",     "viz": "number", "unit": "$"},
            {"metric": "headroom_requests_total",       "label": "Requests",          "viz": "counter"}
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

/// Per-pool operational tallies — the raw material for the `status` metrics array. Field names and
/// units mirror Headroom's real `/metrics` exposition so busbar re-exposes them 1:1. One process
/// serves many pools; the engine sends `request.pool` on every transform, so we key by it.
#[derive(Default)]
struct PoolStat {
    /// Transform requests SEEN on this pool — `headroom_requests_total`.
    requests_seen: u64,
    /// Input chars SEEN across every transform — `headroom_tokens_input_total` (via `est_tokens`).
    chars_seen: u64,
    /// Lifetime input / output chars on compressed requests — the `headroom_tokens_saved_total` base.
    chars_in: u64,
    chars_out: u64,
    /// Headroom-processing overhead per request, in whole microseconds — reduced into the
    /// `headroom_overhead_ms_*` millisecond summary (sum / count / min / max).
    overhead_us_sum: u128,
    overhead_us_count: u64,
    overhead_us_min: u64,
    overhead_us_max: u64,
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

/// Whole microseconds folded to fractional milliseconds, for the `headroom_overhead_ms_*` summary.
/// Pulled out of `build_status` so the unit conversion is directly unit-testable without needing a
/// real (non-deterministic) wall-clock sample.
fn us_to_ms(us: u128) -> f64 {
    us as f64 / 1000.0
}

/// Estimated dollars saved from tokens saved, priced at `price_udollars_per_ktok` (micro-dollars
/// per 1,000 tokens). Pulled out of `build_status` so the pricing arithmetic is directly
/// unit-testable with exact expected values instead of only exercised indirectly via `status`.
fn estimate_dollars_saved(tokens_saved: u64, price_udollars_per_ktok: f64) -> f64 {
    tokens_saved as f64 * price_udollars_per_ktok / 1000.0 / 1_000_000.0
}

/// The `transform` op: compress the granted prompt's history, keep the ask. Returns `{}` (abstain)
/// whenever there is no grant, no/too-short history, or the savings don't clear `min_savings_pct`;
/// otherwise `{"rewrite": {"messages": [...]}}` with each entry in BODY form
/// (`{"role", "content"}`), spliced verbatim into the pending request by the engine.
pub fn run_transform(payload: &Value, knobs: &RwLock<Knobs>, metrics: &Mutex<Metrics>) -> Value {
    let abstain = json!({});
    // Not our shape / no prompt projection (e.g. a decide fire, or a grant misconfig) -> abstain.
    let Ok(parsed) = serde_json::from_value::<HookLine>(payload.clone()) else {
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

    let knobs_v = *knobs.read().unwrap_or_else(|e| e.into_inner());
    let started = std::time::Instant::now();
    let query = messages.last().map(|m| m.text.clone()).unwrap_or_default();
    let crusher = TextCrusher::default();

    let mut chars_before = 0usize;
    let mut chars_after = 0usize;
    let mut out: Vec<Value> = Vec::with_capacity(messages.len());
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
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                crusher
                    .compress(&m.text, &query, Some(knobs_v.target_ratio))
                    .compressed
            }))
            .unwrap_or_else(|_| m.text.clone())
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
    let committed = chars_before > 0 && savings_pct >= knobs_v.min_savings_pct;

    // Record metrics for this request (poison-tolerant; one short critical section).
    {
        let mut m = metrics.lock().unwrap_or_else(|e| e.into_inner());
        let s = m.pools.entry(pool).or_default();
        s.requests_seen += 1;
        s.chars_seen += chars_before as u64;
        // Fold this request's processing time into the overhead summary (µs; rendered as ms).
        s.overhead_us_sum += elapsed_us as u128;
        s.overhead_us_min = if s.overhead_us_count == 0 {
            elapsed_us
        } else {
            s.overhead_us_min.min(elapsed_us)
        };
        s.overhead_us_max = s.overhead_us_max.max(elapsed_us);
        s.overhead_us_count += 1;
        if committed {
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

/// Build the `status` reply: OBSERVED settings + the metrics ARRAY. Metric NAMES, TYPES, and UNITS
/// mirror Headroom's REAL `/metrics` exposition verbatim (verified against the running proxy):
/// `headroom_requests_total`, `headroom_tokens_input_total`, `headroom_tokens_saved_total` are
/// counters, and `headroom_overhead_ms_{sum,count,min,max}` is the millisecond overhead summary
/// Headroom uses (two counters + two gauges — Headroom has no native histograms). So when busbar
/// re-exposes these on its Prometheus scrape, a Grafana dashboard built for Headroom reads them off
/// busbar with no query change. Plus one busbar-native extra (`dollars_saved`, estimated + CI).
/// busbar bounds/sanitizes the array on receipt.
pub fn build_status(knobs: &RwLock<Knobs>, metrics: &Mutex<Metrics>) -> Value {
    let k = *knobs.read().unwrap_or_else(|e| e.into_inner());
    let m = metrics.lock().unwrap_or_else(|e| e.into_inner());
    let mut out: Vec<Value> = Vec::new();
    for (pool, s) in m.pools.iter() {
        let tokens_saved = est_tokens(s.chars_in.saturating_sub(s.chars_out));
        out.push(json!({
            "name": "headroom_requests_total", "type": "counter", "value": s.requests_seen,
            "labels": {"pool": pool}, "label": "Requests", "viz": "counter",
            "help": "Total number of requests"
        }));
        out.push(json!({
            "name": "headroom_tokens_input_total", "type": "counter",
            "value": est_tokens(s.chars_seen), "labels": {"pool": pool}, "label": "Input tokens",
            "viz": "counter", "help": "Total input tokens"
        }));
        out.push(json!({
            "name": "headroom_tokens_saved_total", "type": "counter", "value": tokens_saved,
            "labels": {"pool": pool}, "label": "Tokens saved", "viz": "counter",
            "help": "Tokens saved by optimization"
        }));
        // headroom_overhead_ms_* — Headroom's real millisecond overhead SUMMARY (not a histogram):
        // two counters (_sum, _count) + two gauges (_min, _max), µs folded to ms to match the unit.
        if s.overhead_us_count > 0 {
            out.push(json!({
                "name": "headroom_overhead_ms_sum", "type": "counter", "value": us_to_ms(s.overhead_us_sum),
                "labels": {"pool": pool}, "label": "Overhead (sum)", "unit": "ms", "viz": "counter",
                "help": "Sum of Headroom processing overhead in milliseconds"
            }));
            out.push(json!({
                "name": "headroom_overhead_ms_count", "type": "counter", "value": s.overhead_us_count,
                "labels": {"pool": pool}, "label": "Overhead (count)", "viz": "counter",
                "help": "Count of observed Headroom overhead samples"
            }));
            out.push(json!({
                "name": "headroom_overhead_ms_min", "type": "gauge",
                "value": us_to_ms(s.overhead_us_min as u128), "labels": {"pool": pool},
                "label": "Overhead (min)", "unit": "ms", "viz": "number",
                "help": "Minimum observed Headroom overhead in milliseconds"
            }));
            out.push(json!({
                "name": "headroom_overhead_ms_max", "type": "gauge",
                "value": us_to_ms(s.overhead_us_max as u128), "labels": {"pool": pool},
                "label": "Overhead (max)", "unit": "ms", "viz": "number",
                "help": "Maximum observed Headroom overhead in milliseconds"
            }));
        }
        // busbar-native extra (non-conflicting name): estimated $ saved with a confidence interval.
        let dollars = estimate_dollars_saved(tokens_saved, k.price_udollars_per_ktok);
        out.push(json!({
            "name": "dollars_saved", "type": "gauge", "value": dollars,
            "labels": {"pool": pool}, "label": "Proxy $ saved", "unit": "$", "viz": "number",
            "estimated": true, "ci_low": dollars * 0.85, "ci_high": dollars * 1.15,
            "help": "estimated input cost saved, priced from price_udollars_per_ktok (±15% CI)"
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

#[cfg(test)]
#[path = "tests/compress.rs"]
mod tests;
