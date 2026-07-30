// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! End-to-end coverage of the `headroom-hook` compression gate loaded over the REAL
//! `busbar-plugin-loader` `open_hook` seam (`load_hook_from_bytes`) — the exact seam busbarAI's
//! engine sees: an `Arc<dyn RoutingPolicy>` indistinguishable from a compiled-in policy, whose
//! `transform` compresses the granted prompt with the REAL `headroom-core` BM25 `TextCrusher`
//! engine, and whose reply is parsed through the engine's own fail-closed `hooks::wire`
//! normalizers.
//!
//! Ported from `busbarAI/crates/headroom-hook-plugin/tests/e2e.rs` (the placeholder plugin's e2e
//! harness/style — its compression algorithm was fake, only the harness shape is mirrored here) and
//! from the old socket hook's `src/tests/compress.rs` fixtures (`log_dump`), since a toy 2-word
//! string never triggers BM25/TextCrusher's real compression path — `TextCrusher` itself passes
//! short texts (<6 segments) through unchanged.

use busbar_api::{
    Candidate, RoutingContext, RoutingDecision, RoutingPolicy, RoutingRequest, TransformOutcome,
};
use busbar_plugin_loader::hook::{HookProjectors, load_hook_from_bytes};
use std::sync::Arc;
use std::time::Duration;

/// Locate the built `headroom-hook` cdylib in the target dir (mirrors the loader's hook_plugin_path).
/// `[package] name = "headroom-hook"` -> cargo's cdylib filename is `libheadroom_hook.{so,dylib}`.
fn plugin_path() -> Option<std::path::PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let profile_dir = exe.parent()?.parent()?;
    let name = busbar_plugin_loader::plugin_library_filename("headroom_hook");
    let candidate = profile_dir.join(&name);
    candidate.exists().then_some(candidate)
}

/// A synthetic log-dump line set: many segments, mostly noise, one load-bearing ERROR — realistic
/// enough for BM25 relevance scoring to actually distinguish signal from noise (unlike a toy
/// 2-word string, which TextCrusher passes through unchanged).
fn log_dump(lines: usize) -> String {
    let mut s = String::new();
    for i in 0..lines {
        if i == lines / 2 {
            s.push_str("ERROR: deployment canary failed with status 503 on us-east-1.\n");
        } else {
            s.push_str(&format!(
                "Routine step {i} completed in the staging environment without incident.\n"
            ));
        }
    }
    s
}

/// The engine-side projectors: the same tiny fail-closed shims the loader's own hook test uses,
/// standing in for the engine's real `hooks::wire`. The `transform` projector carries the granted
/// prompt as `request.system` + `request.messages` (`{role, text}`), and `transform_outcome` maps a
/// `{"rewrite":{"messages":[...]}}` reply to a Rewrite (reject > rewrite > abstain).
fn projectors() -> Arc<HookProjectors> {
    Arc::new(HookProjectors {
        decide: Box::new(|req, cands, _ctx| {
            serde_json::json!({
                "request": { "pool": req.pool },
                "candidates": cands.iter().map(|c| serde_json::json!({"idx": c.idx})).collect::<Vec<_>>(),
            })
        }),
        transform: Box::new(|req| {
            // Project the prompt EXACTLY as the core does when prompt: rw is granted: system + the
            // message bodies. When `req.prompt` is None (grant absent) the projection carries no
            // prompt keys, so the gate sees nothing to compress and abstains.
            serde_json::json!({
                "request": {
                    "pool": req.pool,
                    "system": req.prompt.as_ref().and_then(|p| p.system.as_ref().map(|s| s.as_ref().to_string())),
                    "messages": req.prompt.as_ref().map(|p| {
                        p.messages.iter().map(|(r, t)| {
                            serde_json::json!({"role": r.as_ref(), "text": t.as_ref()})
                        }).collect::<Vec<_>>()
                    }),
                }
            })
        }),
        normalize: Box::new(|v, cands| {
            let Some(order) = v.get("order").and_then(|o| o.as_array()) else {
                return RoutingDecision::Abstain;
            };
            let valid: std::collections::HashSet<usize> = cands.iter().map(|c| c.idx).collect();
            RoutingDecision::from_ranked(
                order.iter().filter_map(|x| x.as_u64().map(|x| x as usize)),
                &valid,
            )
        }),
        transform_outcome: Box::new(|v| {
            match v
                .get("rewrite")
                .and_then(|r| r.get("messages"))
                .and_then(|m| m.as_array())
            {
                Some(msgs) if !msgs.is_empty() => {
                    TransformOutcome::Rewrite(busbar_api::RewriteReply {
                        messages: msgs.clone(),
                        tools: Vec::new(),
                    })
                }
                _ => TransformOutcome::Abstain,
            }
        }),
        status: Box::new(|v| {
            v.get("status").map(|s| busbar_api::HookStatus {
                settings_version: s.get("settings_version").and_then(|x| x.as_u64()),
                settings: s.get("settings").and_then(|x| x.as_object()).cloned(),
                metrics: s.get("metrics").and_then(|m| m.as_array()).cloned(),
            })
        }),
        describe_schema: Box::new(|v| v.get("schema").cloned()),
    })
}

/// Load the gate over the real loader seam with the given `settings` JSON.
fn load(settings: &str) -> Arc<dyn RoutingPolicy> {
    let path = plugin_path().expect("headroom-hook cdylib built under --workspace");
    let bytes = std::fs::read(&path).expect("read headroom-hook cdylib");
    load_hook_from_bytes(
        &bytes,
        settings,
        "headroom",
        "hook",
        "headroom",
        projectors(),
    )
    .expect("load the headroom-hook plugin over the ABI")
}

/// A request with a MULTI-message, realistic history (grant given): the projector carries system +
/// every message body. `run_transform` needs >= 2 messages (history + ask) to have anything to
/// compress, and TextCrusher needs a real multi-segment body to meaningfully shrink — a log dump,
/// not a toy string.
fn req_with_history(history: String, ask: &str) -> RoutingRequest<'static> {
    let total_chars = history.len() + ask.len();
    RoutingRequest {
        pool: "p",
        ingress_protocol: "anthropic",
        requested_model: None,
        message_count: 2,
        tool_count: 0,
        has_tools: false,
        total_chars,
        system_chars: 0,
        max_tokens: None,
        stream: false,
        prompt: Some(busbar_api::PromptProjection {
            system: None,
            messages: vec![
                ("user".into(), history.into()),
                ("user".into(), ask.to_string().into()),
            ],
        }),
        identity: None,
    }
}

/// A request with NO prompt (grant absent): the projector carries no prompt keys → the gate abstains.
fn req_without_prompt() -> RoutingRequest<'static> {
    RoutingRequest {
        pool: "p",
        ingress_protocol: "anthropic",
        requested_model: None,
        message_count: 1,
        tool_count: 0,
        has_tools: false,
        total_chars: 0,
        system_chars: 0,
        max_tokens: None,
        stream: false,
        prompt: None,
        identity: None,
    }
}

fn cand(idx: usize) -> Candidate<'static> {
    Candidate {
        idx,
        model: "m",
        provider: "prov",
        weight: 1,
        context_max: None,
        tier: None,
        cost_per_mtok: None,
        tags: &[],
        latency_ms: None,
        available_concurrency: 1,
        budget_remaining: None,
        rate_headroom: None,
    }
}

fn ctx() -> RoutingContext<'static> {
    RoutingContext {
        pool: "p",
        budget_remaining: None,
        budget: &[],
    }
}

const BUDGET: Duration = Duration::from_secs(5);

/// END-TO-END: load the gate, `transform` a realistic compressible history (grant given) → a
/// Rewrite carrying the shrunk message bodies in BODY form (`{"role","content"}` — the deliberate
/// asymmetry from the old socket hook: input is read from `text`, output is emitted as `content`).
#[tokio::test]
async fn transform_compresses_granted_history() {
    if plugin_path().is_none() {
        eprintln!("skip: headroom-hook cdylib not built (run under --workspace)");
        return;
    }
    let policy = load("{}");
    let history = format!("{}{}", log_dump(40), log_dump(40));
    let req = req_with_history(history.clone(), "why did the deployment fail");
    match policy.transform(&req, BUDGET).await {
        TransformOutcome::Rewrite(rw) => {
            assert_eq!(rw.messages.len(), 2);
            assert_eq!(rw.messages[1]["content"], "why did the deployment fail");
            assert!(
                rw.messages[0]["content"]
                    .as_str()
                    .unwrap()
                    .contains("ERROR")
            );
            assert!(
                rw.messages[0].get("text").is_none(),
                "reply must be BODY form (content), not projection form (text)"
            );
            assert!(
                rw.messages[0]["content"].as_str().unwrap().len() < history.len(),
                "the history must actually shrink"
            );
        }
        other => panic!("expected Rewrite, got {other:?}"),
    }
}

/// `transform` of an already-tight prompt → Abstain (nothing to compress; the original body proceeds).
#[tokio::test]
async fn transform_abstains_when_nothing_to_compress() {
    if plugin_path().is_none() {
        return;
    }
    let policy = load("{}");
    let req = req_with_history("hi".to_string(), "how are you");
    assert!(matches!(
        policy.transform(&req, BUDGET).await,
        TransformOutcome::Abstain
    ));
}

/// `transform` with NO prompt projected (operator grant absent) → Abstain. The gate can never coerce
/// content: without the projection it simply has nothing to rewrite.
#[tokio::test]
async fn transform_abstains_without_grant() {
    if plugin_path().is_none() {
        return;
    }
    let policy = load("{}");
    assert!(matches!(
        policy.transform(&req_without_prompt(), BUDGET).await,
        TransformOutcome::Abstain
    ));
}

/// Abstain when `min_savings_pct` is pushed to 100 via `configure` — even a real, shrinking history
/// abstains once the savings bar is unreachable.
#[tokio::test]
async fn transform_abstains_when_min_savings_unreachable() {
    if plugin_path().is_none() {
        return;
    }
    let policy = load("{}");
    let mut settings = serde_json::Map::new();
    settings.insert("min_savings_pct".into(), serde_json::json!(100.0));
    policy
        .configure("headroom", &settings, 1, BUDGET)
        .await
        .expect("a well-formed configure push acks");

    let history = format!("{}{}", log_dump(40), log_dump(40));
    let req = req_with_history(history, "why did the deployment fail");
    assert!(matches!(
        policy.transform(&req, BUDGET).await,
        TransformOutcome::Abstain
    ));
}

/// `decide` abstains over the seam (the gate ranks nothing).
#[tokio::test]
async fn decide_abstains() {
    if plugin_path().is_none() {
        return;
    }
    let policy = load("{}");
    let d = policy
        .decide(
            &req_with_history("hi".to_string(), "x"),
            &[cand(0)],
            &ctx(),
            BUDGET,
        )
        .await
        .expect("decide ok");
    assert_eq!(d, RoutingDecision::Abstain);
}

/// `describe` returns the settings schema + dashboard envelope; `status` reports observed settings +
/// Headroom-named metrics after a real compressing transform (tokens_saved > 0), with no prompt
/// content surfaced.
#[tokio::test]
async fn describe_and_status_over_the_seam() {
    if plugin_path().is_none() {
        return;
    }
    let policy = load(r#"{"target_ratio":0.4}"#);
    let schema = policy.describe(BUDGET).await.expect("describe schema");
    assert_eq!(schema["type"], "object");
    assert!(schema["properties"]["target_ratio"].is_object());

    // Run a real compressing transform so status has something honest to report.
    let history = format!("{}{}", log_dump(40), log_dump(40));
    let req = req_with_history(history, "why did the deployment fail");
    match policy.transform(&req, BUDGET).await {
        TransformOutcome::Rewrite(_) => {}
        other => panic!("expected Rewrite to drive status, got {other:?}"),
    }

    let status = policy.status(BUDGET).await.expect("status");
    let settings = status.settings.expect("status settings");
    assert_eq!(settings.get("target_ratio").unwrap(), 0.4);
    let metrics = status.metrics.expect("metrics");
    let saved = metrics
        .iter()
        .find(|m| m["name"] == "headroom_tokens_saved_total")
        .expect("headroom_tokens_saved_total metric");
    assert!(
        saved["value"].as_u64().unwrap() > 0,
        "compression saved tokens"
    );
}

/// `configure` over the seam acks a well-formed settings push and NACKs a malformed one.
#[tokio::test]
async fn configure_acks_and_nacks_over_the_seam() {
    if plugin_path().is_none() {
        return;
    }
    let policy = load("{}");
    let mut good = serde_json::Map::new();
    good.insert("target_ratio".into(), serde_json::json!(0.3));
    policy
        .configure("headroom", &good, 2, BUDGET)
        .await
        .expect("a well-formed settings push acks");

    let mut bad = serde_json::Map::new();
    bad.insert("target_ratio".into(), serde_json::json!(2.0));
    assert!(
        policy.configure("headroom", &bad, 3, BUDGET).await.is_err(),
        "an out-of-range target_ratio must NACK (malformed push does not commit)"
    );
}
