// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! Tests for `compress` (kept out of the core file so it reads lean).

use super::*;

/// Test convenience: wrap knobs the way `main` does.
fn locked(knobs: Knobs) -> RwLock<Knobs> {
    RwLock::new(knobs)
}

/// Call the dispatcher with a throwaway metrics accumulator (tests that assert metrics build
/// their own).
fn call(line: &[u8], knobs: &RwLock<Knobs>) -> Value {
    handle_line(line, knobs, &Mutex::new(Metrics::default()))
}

/// A synthetic log-dump line set: many segments, mostly noise, one load-bearing ERROR.
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

fn wire_line(messages: Vec<(&str, String)>) -> Vec<u8> {
    let msgs: Vec<Value> = messages
        .iter()
        .map(|(r, t)| json!({"role": r, "text": t}))
        .collect();
    serde_json::to_vec(&json!({
        "request": {
            "pool": "p", "ingress_protocol": "anthropic", "message_count": msgs.len(),
            "has_tools": false, "total_chars": 0, "stream": false,
            "messages": msgs
        },
        "candidates": [],
        "context": {"pool": "p", "budget_remaining": null}
    }))
    .unwrap()
}

/// A long history compresses; the reply is the rewrite arm in BODY form, ask kept verbatim.
#[test]
fn compresses_history_keeps_ask() {
    let line = wire_line(vec![
        ("user", log_dump(40)),
        ("assistant", log_dump(40)),
        ("user", "why did the deployment fail".to_string()),
    ]);
    let knobs = locked(Knobs {
        target_ratio: 0.4,
        min_savings_pct: 10.0,
        ..Knobs::default()
    });
    let reply = call(&line, &knobs);
    let msgs = reply["rewrite"]["messages"]
        .as_array()
        .expect("rewrite arm");
    assert_eq!(msgs.len(), 3);
    assert_eq!(msgs[2]["content"], "why did the deployment fail");
    assert!(msgs[0]["content"].as_str().unwrap().contains("ERROR"));
    // target_ratio 0.4 keeps ~40% of tokens; assert a conservative 60% char ceiling.
    assert!(
        msgs[0]["content"].as_str().unwrap().len() < log_dump(40).len() * 6 / 10,
        "history must actually shrink"
    );
    // Body form, not projection form.
    assert!(msgs[0].get("text").is_none());
}

/// A short chat abstains (`{}`) — nothing worth compressing.
#[test]
fn short_chat_abstains() {
    let line = wire_line(vec![
        ("user", "hello".to_string()),
        ("assistant", "hi".to_string()),
        ("user", "how are you".to_string()),
    ]);
    let reply = call(&line, &locked(Knobs::default()));
    assert_eq!(reply, json!({}), "short prompts must pass through");
}

/// A shape-only projection (no prompt grant / a decide fire) abstains.
#[test]
fn no_prompt_projection_abstains() {
    let line = serde_json::to_vec(&json!({
        "request": {"pool": "p", "ingress_protocol": "openai", "message_count": 1,
                    "has_tools": false, "total_chars": 10, "stream": false},
        "candidates": [], "context": {"pool": "p", "budget_remaining": null}
    }))
    .unwrap();
    assert_eq!(call(&line, &locked(Knobs::default())), json!({}));
}

/// Garbage in -> abstain, never a panic (busbar treats our failure as proceed-unmodified,
/// but we should not crash the process either).
#[test]
fn malformed_line_abstains() {
    assert_eq!(call(b"not json", &locked(Knobs::default())), json!({}));
    assert_eq!(call(b"{}", &locked(Knobs::default())), json!({}));
}

/// The configure-ack handshake: a pushed settings map is applied LIVE and the ack echoes the
/// pushed `settings_version` — the shape busbar's commit-on-ack requires. Behavior proof: a
/// history that rewrites under the defaults abstains once `min_savings_pct: 100` is pushed.
#[test]
fn configure_acks_version_and_applies_settings_live() {
    let knobs = locked(Knobs::default());
    let req = wire_line(vec![
        ("user", log_dump(40)),
        ("user", "why did the deployment fail".to_string()),
    ]);
    assert!(
        call(&req, &knobs).get("rewrite").is_some(),
        "defaults must rewrite this history"
    );

    let cfg = serde_json::to_vec(&json!({"configure": {
        "hook": "headroom",
        "settings": {"target_ratio": 0.4, "min_savings_pct": 100.0},
        "settings_version": 7,
        "busbar_version": "1.3.0"
    }}))
    .unwrap();
    let reply = call(&cfg, &knobs);
    assert_eq!(reply, json!({"ack": {"settings_version": 7}}));
    assert_eq!(knobs.read().unwrap().target_ratio, 0.4);
    assert_eq!(knobs.read().unwrap().min_savings_pct, 100.0);
    // The same connection's NEXT request line sees the pushed settings.
    assert_eq!(
        call(&req, &knobs),
        json!({}),
        "min_savings_pct 100 must abstain"
    );
}

/// Configure is DESIRED STATE: a key absent from the push resets to the built-in default —
/// so an empty map is "back to defaults", and re-pushing the same map is a no-op.
#[test]
fn configure_is_desired_state() {
    let knobs = locked(Knobs {
        target_ratio: 0.2,
        min_savings_pct: 90.0,
        ..Knobs::default()
    });
    let cfg =
        br#"{"configure":{"hook":"headroom","settings":{},"settings_version":3,"busbar_version":"1.3.0"}}"#;
    assert_eq!(call(cfg, &knobs), json!({"ack": {"settings_version": 3}}));
    assert_eq!(knobs.read().unwrap().target_ratio, 0.5);
    assert_eq!(knobs.read().unwrap().min_savings_pct, 10.0);
}

/// A configure we can't cleanly apply must NOT ack (busbar then keeps our previous settings and
/// fails the operator's PATCH): unknown key, wrong type, out-of-range value, missing version.
#[test]
fn bad_configure_never_acks_and_keeps_settings() {
    for cfg in [
        r#"{"configure":{"settings":{"bogus_knob":1},"settings_version":9}}"#,
        r#"{"configure":{"settings":{"target_ratio":"half"},"settings_version":9}}"#,
        r#"{"configure":{"settings":{"target_ratio":2.0},"settings_version":9}}"#,
        r#"{"configure":{"settings":{"min_savings_pct":-1},"settings_version":9}}"#,
        r#"{"configure":{"settings":{}}}"#, // no settings_version: nothing to echo
        r#"{"configure":"garbage"}"#,
    ] {
        let knobs = locked(Knobs {
            target_ratio: 0.3,
            min_savings_pct: 25.0,
            ..Knobs::default()
        });
        let reply = call(cfg.as_bytes(), &knobs);
        assert!(reply.get("ack").is_none(), "must not ack {cfg}: {reply}");
        assert_eq!(
            knobs.read().unwrap().target_ratio,
            0.3,
            "settings must survive a rejected configure: {cfg}"
        );
    }
}

/// `describe` returns the `{schema, dashboard}` ENVELOPE (busbar reads `.schema` for the config
/// form and `.dashboard` for the widget layout); the explicit non-describe shape abstains.
#[test]
fn describe_returns_schema_and_dashboard_envelope() {
    let knobs = locked(Knobs::default());
    let reply = call(br#"{"describe":true}"#, &knobs);
    assert_eq!(reply["schema"]["type"], "object");
    assert!(reply["schema"]["properties"]["target_ratio"].is_object());
    assert!(reply["schema"]["properties"]["price_udollars_per_ktok"].is_object());
    // the dashboard widget layout is declared alongside the schema (one declaration drives both).
    let widgets = reply["dashboard"]["widgets"].as_array().expect("widgets");
    assert!(widgets.iter().any(|w| w["metric"] == "headroom_tokens_saved_total"));
    assert_eq!(call(br#"{"describe":false}"#, &knobs), json!({}));
}

/// A `status` query returns observed settings + the metrics array using Headroom's REAL `/metrics`
/// names, types, and units (counters + the `headroom_overhead_ms_*` millisecond summary — no
/// histograms, matching the running proxy) plus busbar-native per-pool extras (a `pool` label,
/// estimated $ with a CI). Drives a compressing request first so the counters are non-zero.
#[test]
fn status_reports_headroom_named_metrics() {
    let knobs = locked(Knobs {
        target_ratio: 0.4,
        min_savings_pct: 10.0,
        ..Knobs::default()
    });
    let metrics = Mutex::new(Metrics::default());
    let req = wire_line(vec![
        ("user", log_dump(40)),
        ("assistant", log_dump(40)),
        ("user", "why did the deployment fail".to_string()),
    ]);
    assert!(
        handle_line(&req, &knobs, &metrics).get("rewrite").is_some(),
        "the request must compress so counters advance"
    );

    let status = handle_line(br#"{"status":true}"#, &knobs, &metrics);
    assert_eq!(status["status"]["settings"]["target_ratio"], 0.4);
    let m = status["status"]["metrics"]
        .as_array()
        .expect("metrics array");
    let by_name = |n: &str| m.iter().find(|e| e["name"] == n).cloned();

    // Headroom's REAL Prometheus names/types — the drop-in dashboard compatibility.
    let saved = by_name("headroom_tokens_saved_total").expect("headroom_tokens_saved_total");
    assert_eq!(saved["type"], "counter");
    assert!(saved["value"].as_u64().unwrap() > 0);
    assert_eq!(saved["labels"]["pool"], "p"); // wire_line routes pool "p"
    assert!(by_name("headroom_tokens_input_total").is_some());
    assert!(by_name("headroom_requests_total").unwrap()["value"].as_u64().unwrap() >= 1);

    // overhead is Headroom's real ms SUMMARY: two counters (_sum,_count) + two gauges (_min,_max).
    assert_eq!(by_name("headroom_overhead_ms_sum").unwrap()["type"], "counter");
    let cnt = by_name("headroom_overhead_ms_count").expect("overhead count");
    assert_eq!(cnt["type"], "counter");
    assert!(cnt["value"].as_u64().unwrap() >= 1);
    assert_eq!(by_name("headroom_overhead_ms_min").unwrap()["type"], "gauge");
    assert_eq!(by_name("headroom_overhead_ms_max").unwrap()["type"], "gauge");
    // No histograms — the real proxy has none.
    assert!(m.iter().all(|e| e["type"] != "histogram"), "Headroom emits no histograms");
    assert!(by_name("headroom_compression_ratio").is_none());
    assert!(by_name("headroom_latency_seconds").is_none());

    // busbar-native extra: estimated $ with a bounding CI.
    let dollars = by_name("dollars_saved").expect("dollars_saved");
    assert_eq!(dollars["estimated"], true);
    let (lo, val, hi) = (
        dollars["ci_low"].as_f64().unwrap(),
        dollars["value"].as_f64().unwrap(),
        dollars["ci_high"].as_f64().unwrap(),
    );
    assert!(lo <= val && val <= hi, "CI must bound the value");
}

/// A hook that has served nothing still answers `status` cleanly: observed settings + an empty
/// metrics array (busbar renders no series, fail-open).
#[test]
fn status_with_no_traffic_is_clean() {
    let knobs = locked(Knobs::default());
    let status = handle_line(
        br#"{"status":true}"#,
        &knobs,
        &Mutex::new(Metrics::default()),
    );
    assert!(status["status"]["metrics"].as_array().unwrap().is_empty());
    assert_eq!(status["status"]["settings"]["min_savings_pct"], 10.0);
}

/// `encode_reply` enforces busbar's 64 KiB reply-line cap: an in-cap reply passes through
/// newline-terminated; an over-cap reply degrades to abstain (`{}`) instead of a line busbar
/// would kill the connection over.
#[test]
fn encode_reply_enforces_busbar_reply_cap() {
    let small = encode_reply(&json!({"ack": {"settings_version": 1}}));
    assert_eq!(small, b"{\"ack\":{\"settings_version\":1}}\n");

    let huge = json!({"rewrite": {"messages": [{"role": "user", "content": "x".repeat(MAX_REPLY_BYTES)}]}});
    assert_eq!(
        encode_reply(&huge),
        b"{}\n",
        "an over-cap reply must degrade to abstain"
    );
}

/// Latency: compress a realistic multi-KB history and print the wall time (feasibility datum;
/// run with `cargo test -- --nocapture latency`). Asserts a generous ceiling so CI still gates.
#[test]
fn latency_multi_kb_prompt() {
    let line = wire_line(vec![
        ("user", log_dump(120)),      // ~8 KB
        ("assistant", log_dump(120)), // ~8 KB
        ("user", "why did the deployment fail".to_string()),
    ]);
    let knobs = locked(Knobs::default());
    // Warm once (lazy statics inside headroom), then time.
    let _ = call(&line, &knobs);
    let start = std::time::Instant::now();
    let iters = 20;
    for _ in 0..iters {
        let _ = call(&line, &knobs);
    }
    let per = start.elapsed() / iters;
    println!("latency: {per:?} per ~16KB history compress");
    assert!(
        per < std::time::Duration::from_millis(250),
        "compression should be well under a serving-acceptable deadline, got {per:?}"
    );
}
