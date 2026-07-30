// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! Tests for `compress` (kept out of the core file so it reads lean). Ported from the old
//! Unix-socket hook's `src/tests/compress.rs`, adapted from `handle_line(&bytes, &knobs, &metrics)`
//! to the new plugin's pure `run_transform(&payload, &knobs, &metrics)` call surface — building the
//! `transform` payload shape directly (`{"request": {"pool", "messages": [{"role","text"}, ...]}}`)
//! instead of a whole socket-wire line.

use super::*;

/// Test convenience: wrap knobs the way `Headroom::new` does.
fn locked(knobs: Knobs) -> RwLock<Knobs> {
    RwLock::new(knobs)
}

/// Call `run_transform` with a throwaway metrics accumulator (tests that assert metrics build
/// their own).
fn transform(payload: &Value, knobs: &RwLock<Knobs>) -> Value {
    run_transform(payload, knobs, &Mutex::new(Metrics::default()))
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

/// Build the `transform` payload the engine's `hooks::wire` projects: `{"request": {"pool",
/// "messages": [{"role","text"}, ...]}}` — the same shape the old socket wire's `HookLine{request}`
/// read (minus the `candidates`/`context` envelope we never used).
fn transform_payload(messages: Vec<(&str, String)>) -> Value {
    let msgs: Vec<Value> = messages
        .iter()
        .map(|(r, t)| json!({"role": r, "text": t}))
        .collect();
    json!({"request": {"pool": "p", "messages": msgs}})
}

/// A long history compresses; the reply is the rewrite arm in BODY form, ask kept verbatim.
#[test]
fn compresses_history_keeps_ask() {
    let payload = transform_payload(vec![
        ("user", log_dump(40)),
        ("assistant", log_dump(40)),
        ("user", "why did the deployment fail".to_string()),
    ]);
    let knobs = locked(Knobs {
        target_ratio: 0.4,
        min_savings_pct: 10.0,
        ..Knobs::default()
    });
    let reply = transform(&payload, &knobs);
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
    let payload = transform_payload(vec![
        ("user", "hello".to_string()),
        ("assistant", "hi".to_string()),
        ("user", "how are you".to_string()),
    ]);
    let reply = transform(&payload, &locked(Knobs::default()));
    assert_eq!(reply, json!({}), "short prompts must pass through");
}

/// A shape-only payload (no prompt grant / a decide fire) abstains: no `messages` at all, and no
/// `request` key at all.
#[test]
fn no_prompt_projection_abstains() {
    let payload = json!({"request": {"pool": "p"}});
    assert_eq!(transform(&payload, &locked(Knobs::default())), json!({}));
    assert_eq!(
        transform(&json!({}), &locked(Knobs::default())),
        json!({}),
        "no request key at all (grant absent) must abstain"
    );
}

/// Garbage in -> abstain, never a panic.
#[test]
fn malformed_payload_abstains() {
    assert_eq!(
        transform(
            &json!({"request": "not an object"}),
            &locked(Knobs::default())
        ),
        json!({})
    );
    assert_eq!(
        transform(&json!(null), &locked(Knobs::default())),
        json!({})
    );
}

/// `apply_settings` is DESIRED STATE: a key absent from the push resets to the built-in default —
/// so an empty map is "back to defaults", and re-pushing the same map is a no-op.
#[test]
fn configure_is_desired_state() {
    let applied = apply_settings(&serde_json::Map::new()).expect("empty map applies cleanly");
    assert_eq!(applied.target_ratio, 0.5);
    assert_eq!(applied.min_savings_pct, 10.0);
}

/// A settings map we can't cleanly apply must return `Err` (the caller must NOT commit, keeping the
/// previous live settings): unknown key, wrong type, out-of-range value.
#[test]
fn bad_configure_never_acks_and_keeps_settings() {
    let bad_maps = [
        json!({"bogus_knob": 1}),
        json!({"target_ratio": "half"}),
        json!({"target_ratio": 2.0}),
        json!({"min_savings_pct": -1}),
    ];
    for v in bad_maps {
        let settings = v.as_object().unwrap();
        assert!(
            apply_settings(settings).is_err(),
            "must reject {settings:?}"
        );
    }
}

/// `describe` returns the `{schema, dashboard}` ENVELOPE (the config form + the widget layout, one
/// declaration driving both).
#[test]
fn describe_returns_schema_and_dashboard_envelope() {
    let reply = describe_reply();
    assert_eq!(reply["schema"]["type"], "object");
    assert!(reply["schema"]["properties"]["target_ratio"].is_object());
    assert!(reply["schema"]["properties"]["price_udollars_per_ktok"].is_object());
    let widgets = reply["dashboard"]["widgets"].as_array().expect("widgets");
    assert!(
        widgets
            .iter()
            .any(|w| w["metric"] == "headroom_tokens_saved_total")
    );
}

/// `status` returns observed settings + the metrics array using Headroom's REAL `/metrics` names,
/// types, and units (counters + the `headroom_overhead_ms_*` millisecond summary — no histograms,
/// matching the running proxy) plus busbar-native per-pool extras (a `pool` label, estimated $ with
/// a CI). Drives a compressing request first so the counters are non-zero.
#[test]
fn status_reports_headroom_named_metrics() {
    let knobs = locked(Knobs {
        target_ratio: 0.4,
        min_savings_pct: 10.0,
        ..Knobs::default()
    });
    let metrics = Mutex::new(Metrics::default());
    let payload = transform_payload(vec![
        ("user", log_dump(40)),
        ("assistant", log_dump(40)),
        ("user", "why did the deployment fail".to_string()),
    ]);
    assert!(
        run_transform(&payload, &knobs, &metrics)
            .get("rewrite")
            .is_some(),
        "the request must compress so counters advance"
    );

    let status = build_status(&knobs, &metrics);
    assert_eq!(status["status"]["settings"]["target_ratio"], 0.4);
    let m = status["status"]["metrics"]
        .as_array()
        .expect("metrics array");
    let by_name = |n: &str| m.iter().find(|e| e["name"] == n).cloned();

    // Headroom's REAL Prometheus names/types — the drop-in dashboard compatibility.
    let saved = by_name("headroom_tokens_saved_total").expect("headroom_tokens_saved_total");
    assert_eq!(saved["type"], "counter");
    assert!(saved["value"].as_u64().unwrap() > 0);
    assert_eq!(saved["labels"]["pool"], "p"); // transform_payload routes pool "p"
    assert!(by_name("headroom_tokens_input_total").is_some());
    assert!(
        by_name("headroom_requests_total").unwrap()["value"]
            .as_u64()
            .unwrap()
            >= 1
    );

    // overhead is Headroom's real ms SUMMARY: two counters (_sum,_count) + two gauges (_min,_max).
    assert_eq!(
        by_name("headroom_overhead_ms_sum").unwrap()["type"],
        "counter"
    );
    let cnt = by_name("headroom_overhead_ms_count").expect("overhead count");
    assert_eq!(cnt["type"], "counter");
    assert!(cnt["value"].as_u64().unwrap() >= 1);
    assert_eq!(
        by_name("headroom_overhead_ms_min").unwrap()["type"],
        "gauge"
    );
    assert_eq!(
        by_name("headroom_overhead_ms_max").unwrap()["type"],
        "gauge"
    );
    // No histograms — the real proxy has none.
    assert!(
        m.iter().all(|e| e["type"] != "histogram"),
        "Headroom emits no histograms"
    );
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

/// A gate that has served nothing still answers `status` cleanly: observed settings + an empty
/// metrics array (busbar renders no series, fail-open).
#[test]
fn status_with_no_traffic_is_clean() {
    let knobs = locked(Knobs::default());
    let status = build_status(&knobs, &Mutex::new(Metrics::default()));
    assert!(status["status"]["metrics"].as_array().unwrap().is_empty());
    assert_eq!(status["status"]["settings"]["min_savings_pct"], 10.0);
}

/// Latency: compress a realistic multi-KB history and print the wall time (feasibility datum; run
/// with `cargo test -- --nocapture latency`). Asserts a generous ceiling so CI still gates.
#[test]
fn latency_multi_kb_prompt() {
    let payload = transform_payload(vec![
        ("user", log_dump(120)),      // ~8 KB
        ("assistant", log_dump(120)), // ~8 KB
        ("user", "why did the deployment fail".to_string()),
    ]);
    let knobs = locked(Knobs::default());
    // Warm once (lazy statics inside headroom), then time.
    let _ = transform(&payload, &knobs);
    let start = std::time::Instant::now();
    let iters = 20;
    for _ in 0..iters {
        let _ = transform(&payload, &knobs);
    }
    let per = start.elapsed() / iters;
    println!("latency: {per:?} per ~16KB history compress");
    assert!(
        per < std::time::Duration::from_millis(250),
        "compression should be well under a serving-acceptable deadline, got {per:?}"
    );
}
