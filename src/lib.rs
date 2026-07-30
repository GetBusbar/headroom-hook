// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! `headroom-hook` — busbar's first-party **`headroom`** `kind: hook` plugin: a signed, trusted,
//! dlopen'd `prompt: rw` REWRITE GATE that compresses LLM chat history with the REAL
//! `headroom-core` engine (BM25 `TextCrusher`), not a placeholder.
//!
//! Formerly a standalone Unix-socket hook service (`main.rs`, now deleted) speaking busbar's
//! 5-message wire (`configure`/`describe`/`decide`/`transform`/`notify`) as newline-delimited JSON
//! over a kept-alive connection. Now a `cdylib` implementing the plugin SDK's [`HookHandler`]
//! trait directly — the wire-line dispatch is gone (the engine calls in-process through the dlopen
//! ABI), but the compression logic in [`compress`] is a straight port of the old socket hook's
//! `handle_line`/`build_status`/`apply_settings`.
//!
//! ## Ops
//!
//! - **`transform`** — the compression pass ([`compress::run_transform`]). Reads `system`-adjacent
//!   `request.messages` from the projection (only present when the operator granted `prompt: rw`),
//!   keeps the LAST message verbatim as the ask, and BM25-compresses every earlier message against
//!   it. Returns `{"rewrite": {"messages": [...]}}` (BODY form) IFF whole-prompt char savings clear
//!   `min_savings_pct`; otherwise `{}` abstain. Never fails the request — `TextCrusher` (third-party
//!   code) runs under `catch_unwind` per message.
//! - **`decide` / `notify`** — ABSTAIN / no-op. This hook is a rewrite gate, not a router or a tap
//!   (the old wire never used it for either).
//! - **`configure`** — applies a pushed settings map as DESIRED STATE ([`compress::apply_settings`]):
//!   present keys override, absent keys reset to the built-in default, and an unknown key or an
//!   out-of-range/wrong-type value NACKs (the live `Knobs` are left untouched).
//! - **`describe`** — the `{schema, dashboard}` self-description ([`compress::describe_reply`]).
//! - **`status`** — observed settings + a Prometheus-shaped metrics array using Headroom's REAL
//!   metric names ([`compress::build_status`]), plus the `headroom-core` git ref this binary was
//!   built against (stamped by `build.rs` from `Cargo.lock`).
//!
//! ## The manifest `needs`
//!
//! Headroom is a `prompt: rw` gate: its signed packaging manifest declares `needs.prompt = rw`. Even
//! with that declared, the engine only projects the prompt into the `transform` payload when the
//! OPERATOR also grants `prompt: rw`; absent that grant the payload carries no `request.messages`
//! and this gate abstains — it can never coerce content.

mod compress;

use busbar_plugin_sdk::HookHandler;
use compress::{Knobs, Metrics, build_status, describe_reply, run_transform};
use serde_json::{Map, Value, json};
use std::sync::{Mutex, RwLock};

/// The `headroom-core` git ref this binary was built against (tag when pinned to a release, else a
/// short commit) — stamped by `build.rs` from `Cargo.lock`.
const HEADROOM_CORE_REF: &str = env!("HEADROOM_CORE_REF");

/// The live gate: compression knobs behind an `RwLock` (the SDK's [`HookHandler`] methods take
/// `&self` — the engine may drive `transform`/`configure` concurrently — so committing a `configure`
/// push is interior mutability, mirroring the old socket hook's `Arc<RwLock<Knobs>>`) plus the
/// process-wide per-pool metrics accumulator.
struct Headroom {
    knobs: RwLock<Knobs>,
    metrics: Mutex<Metrics>,
}

impl Headroom {
    fn new(knobs: Knobs) -> Self {
        Headroom {
            knobs: RwLock::new(knobs),
            metrics: Mutex::new(Metrics::default()),
        }
    }
}

impl HookHandler for Headroom {
    /// `transform` — compress the granted history, keep the ask. See [`compress::run_transform`].
    fn transform(&self, payload: &Value) -> Value {
        run_transform(payload, &self.knobs, &self.metrics)
    }

    /// `decide` — ABSTAIN. Headroom ranks nothing; it is a rewrite gate, not a router.
    fn decide(&self, _payload: &Value) -> Value {
        json!({})
    }

    /// `notify` — no-op. Headroom is not a tap; it observes nothing out of band.
    fn notify(&self, _payload: &Value) {}

    /// `configure` — apply a pushed settings map as DESIRED STATE and commit it live iff it applies
    /// cleanly; an unknown key or an out-of-shape/out-of-range value NACKs (returns `false`) and the
    /// live knobs are left untouched, matching the old wire's commit-on-ack fail-closed contract.
    fn configure(&self, settings: &Map<String, Value>, settings_version: u64) -> bool {
        match compress::apply_settings(settings) {
            Ok(mut new) => {
                new.settings_version = settings_version;
                // Poison-tolerant: `Knobs` is `Copy` and writes are single assignments, so a
                // poisoned lock holds a fully-written value — recover it, never panic on the
                // request path.
                *self.knobs.write().unwrap_or_else(|e| e.into_inner()) = new;
                true
            }
            Err(_) => false,
        }
    }

    /// `describe` — the `{schema, dashboard}` self-description envelope.
    fn describe(&self) -> Value {
        describe_reply()
    }

    /// `status` — observed settings + the metrics array, plus the `headroom-core` build ref.
    fn status(&self) -> Value {
        let mut reply = build_status(&self.knobs, &self.metrics);
        if let Some(status) = reply.get_mut("status").and_then(|s| s.as_object_mut()) {
            status.insert(
                "headroom_core_ref".to_string(),
                Value::String(HEADROOM_CORE_REF.to_string()),
            );
        }
        reply
    }
}

/// Construct the gate from the engine-passed JSON config (the operator's `settings:` map, the same
/// shape a `configure` push carries). An empty/missing config uses the built-in defaults. A
/// MALFORMED config (bad JSON, unknown key, out-of-range value) is a fail-closed LOAD error — better
/// a loud load failure than a silently mis-parsed knob.
fn open(cfg: &str) -> Result<Box<dyn HookHandler>, String> {
    let knobs = if cfg.trim().is_empty() {
        Knobs::default()
    } else {
        let settings: Map<String, Value> = serde_json::from_str(cfg)
            .map_err(|e| format!("headroom-hook: invalid plugin config: {e}"))?;
        compress::apply_settings(&settings)
            .map_err(|e| format!("headroom-hook: invalid settings: {e}"))?
    };
    Ok(Box::new(Headroom::new(knobs)))
}

busbar_plugin_sdk::export_hook_plugin!(open);

#[cfg(test)]
#[path = "tests/lib.rs"]
mod tests;
