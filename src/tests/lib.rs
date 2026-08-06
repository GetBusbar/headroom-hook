use super::*;

/// `open` accepts empty/missing config and a valid settings map; a malformed config or an
/// out-of-range/unknown-key settings map fails closed.
#[test]
fn open_config_handling() {
    assert!(open("").is_ok(), "empty config uses defaults");
    assert!(open("{}").is_ok(), "no settings uses defaults");
    assert!(open(r#"{"target_ratio":0.3}"#).is_ok());
    assert!(open("{ not json").is_err(), "malformed config fails closed");
    assert!(
        open(r#"{"target_ratio":2.0}"#).is_err(),
        "out-of-range value fails closed"
    );
    assert!(
        open(r#"{"bogus_knob":1}"#).is_err(),
        "unknown key fails closed"
    );
}

/// `decide` and `notify` are inert (Headroom is not a router or a tap).
#[test]
fn decide_and_notify_are_inert() {
    let h = Headroom::new(Knobs::default());
    assert_eq!(h.decide(&json!({})), json!({}));
    h.notify(&json!({"anything": true})); // no panic, no effect
}

/// `configure` applies pushed settings LIVE and returns `true`/ACK on a clean apply; the next
/// `transform` on the SAME instance sees the pushed settings. Behavior proof: a history that
/// rewrites under the defaults abstains once `min_savings_pct: 100` is pushed.
#[test]
fn configure_applies_settings_live() {
    let h = Headroom::new(Knobs::default());
    let payload = json!({
        "request": {
            "pool": "p",
            "messages": [
                {"role": "user", "text": "Routine step completed without incident.\n".repeat(200)},
                {"role": "user", "text": "why did the deployment fail"}
            ]
        }
    });
    assert!(
        h.transform(&payload).get("rewrite").is_some(),
        "defaults must rewrite this history"
    );

    let mut settings = Map::new();
    settings.insert("target_ratio".into(), json!(0.4));
    settings.insert("min_savings_pct".into(), json!(100.0));
    assert!(h.configure(&settings, 7), "a well-formed push must ACK");
    assert_eq!(h.knobs.read().unwrap().target_ratio, 0.4);
    assert_eq!(h.knobs.read().unwrap().min_savings_pct, 100.0);
    // The same instance's NEXT transform sees the pushed settings.
    assert_eq!(
        h.transform(&payload),
        json!({}),
        "min_savings_pct 100 must abstain"
    );
}

/// `configure` is DESIRED STATE: a key absent from the push resets to the built-in default.
#[test]
fn configure_is_desired_state() {
    let h = Headroom::new(Knobs {
        target_ratio: 0.2,
        min_savings_pct: 90.0,
        ..Knobs::default()
    });
    assert!(
        h.configure(&Map::new(), 3),
        "empty map (back to defaults) must ACK"
    );
    assert_eq!(h.knobs.read().unwrap().target_ratio, 0.5);
    assert_eq!(h.knobs.read().unwrap().min_savings_pct, 10.0);
}

/// A settings push we can't cleanly apply must NACK (`false`) and leave the live knobs
/// untouched: unknown key, wrong type, out-of-range value.
#[test]
fn bad_configure_never_acks_and_keeps_settings() {
    let bad_maps = [
        json!({"bogus_knob": 1}),
        json!({"target_ratio": "half"}),
        json!({"target_ratio": 2.0}),
        json!({"min_savings_pct": -1}),
    ];
    for v in bad_maps {
        let h = Headroom::new(Knobs {
            target_ratio: 0.3,
            min_savings_pct: 25.0,
            ..Knobs::default()
        });
        let settings = v.as_object().unwrap();
        assert!(!h.configure(settings, 9), "must NACK {settings:?}");
        assert_eq!(
            h.knobs.read().unwrap().target_ratio,
            0.3,
            "settings must survive a rejected configure: {settings:?}"
        );
    }
}

/// `status` surfaces the `headroom-core` build ref alongside the ported settings/metrics shape.
#[test]
fn status_surfaces_headroom_core_ref() {
    let h = Headroom::new(Knobs::default());
    let status = h.status();
    assert_eq!(status["status"]["headroom_core_ref"], HEADROOM_CORE_REF);
    // ...and the constant is a REAL ref, not the silent-failure sentinel. Comparing the reported
    // value against the constant that produced it cannot fail on its own: it passes just as
    // happily on "unknown", which is what a build that could not read its own Cargo.lock stamps
    // in. This is the assertion that notices.
    assert_ne!(
        HEADROOM_CORE_REF, "unknown",
        "the build could not determine the headroom-core git ref, so status reports build \
         provenance that is indistinguishable from genuinely unpinned"
    );
    assert!(
        !HEADROOM_CORE_REF.trim().is_empty(),
        "headroom_core_ref must carry something an operator can act on"
    );
}

/// PANIC CONTAINMENT. The vendored `headroom-core` compress path has no `unwrap()`/`expect(`/
/// `panic!`/raw indexing, so there is no input that makes `TextCrusher` itself panic and this test
/// does not drive a real one. It exercises the EXACT SAME containment pattern
/// `compress::run_transform` uses
/// (`std::panic::catch_unwind(AssertUnwindSafe(|| ...)).unwrap_or_else(|_| fallback)`) with a
/// closure that deliberately panics, proving the containment code itself degrades to the
/// fallback value instead of propagating the panic — the property `run_transform` relies on.
#[test]
fn panic_containment_pattern_degrades_to_fallback() {
    let fallback = "verbatim fallback".to_string();
    let result: String = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> String {
        panic!("simulated TextCrusher panic")
    }))
    .unwrap_or_else(|_| fallback.clone());
    assert_eq!(
        result, fallback,
        "a panicking compress call must degrade to the verbatim fallback, never propagate"
    );
}
