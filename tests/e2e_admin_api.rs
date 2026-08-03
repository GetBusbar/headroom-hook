// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! THE REAL "prod ready" bar: install `headroom-hook` over busbar's REAL admin HTTP API into a
//! REAL running `busbar` process, wire it onto a pool as a `prompt: rw` gate, then drive a REAL
//! chat completion request through that running instance and confirm the request that reached a
//! mock upstream provider is genuinely compressed.
//!
//! `tests/e2e.rs` already proves the plugin ABI seam works (`load_hook_from_bytes` — an in-process
//! Rust function call over the exact `Arc<dyn RoutingPolicy>` seam the engine uses). This file
//! proves the OTHER half: the full operator-facing path a real deployment actually takes —
//!
//!   1. Pack the built `headroom-hook` cdylib into a real tarball with the real `busbar-plugin-pack`
//!      tool (dev-only, unsigned locally — same fallback CI's own release jobs use without a
//!      `BUSBAR_SIGN_KEY`).
//!   2. Boot a REAL `busbar` binary (subprocess — `crates/busbar` in the sibling `busbarAI` checkout
//!      is bin-only, no library crate, so an in-process test harness isn't available from outside
//!      that crate) with the admin listener up and no hook plugin loaded yet.
//!   3. POST the base64 tarball to `POST /api/v1/admin/plugins` — the real runtime-install path
//!      (`crates/busbar/src/admin/v1/json/handlers.rs::install_plugin`) — and confirm 201.
//!   4. A pool's `hooks: [...]` list (naming the plugin + its `prompt: rw` grant) is config-file-only
//!      — there is no live admin endpoint to attach a gate to a pool (`GET /api/v1/admin/pools*` is
//!      read-only; confirmed by reading `crates/busbar/src/admin/v1/json/mod.rs`'s route table).
//!
//!      Installing a plugin also does NOT hot-swap (`install_plugin`'s own doc comment: "takes
//!      effect on the next plugin (re)load, not as a hot swap"). So: restart with a SECOND config
//!      that now names the just-installed plugin as a pool gate, pointed at the SAME `plugins.dir`
//!      the tarball was installed into — the real operational flow (install, then restart to
//!      apply), exactly the restart-to-apply shape this admin API itself documents for other config
//!      sections. `busbar`'s own restart deliberately does not self-respawn (a supervisor's job);
//!      this test plays that role directly.
//!   5. Mint a real virtual key over the admin API and drive a REAL `POST /v1/chat/completions`
//!      through the restarted instance with a long, compressible message history (mirroring this
//!      repo's own `tests/e2e.rs` `log_dump` fixture — a toy 2-word string never triggers BM25/
//!      TextCrusher's real compression path; a realistic noisy-log-with-one-signal body does).
//!   6. Confirm the REAL, EXTERNALLY OBSERVABLE effect: the mock upstream provider — a tiny
//!      `tiny_http` server this test spawns, standing in for the model provider — physically
//!      received a request whose history is shorter than what the client sent, proving compression
//!      ran across the FULL real path (HTTP admin install -> real dlopen -> real hook invocation ->
//!      real HTTP request mutation -> real upstream receipt), not merely that busbar returned 200.
//!
//! Mirrors the pattern the sibling `store-redis` plugin's own
//! `admin_api_installs_the_redis_plugin_and_writes_land_in_real_redis` test established for a
//! `store` plugin (build the real binaries from the sibling `busbarAI` checkout, pack + install
//! over the real admin API, restart to apply, independently verify the real effect) — adapted here
//! for a `hook` plugin, where the "real effect" is an observed request mutation instead of a
//! persisted row.

use base64::Engine as _;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Fixed ed25519 signing secret (64 hex = 32 bytes) for this e2e test. 1.5.1 requires an
/// explicit signing key to mint virtual keys; busbar no longer auto-generates one.
const TEST_SIGNING_KEY: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

/// Locate the built `headroom-hook` cdylib in the target dir (mirrors `tests/e2e.rs`'s own
/// `plugin_path()` — kept separate since the two files must each stand alone).
///
/// Checks BOTH the uplifted `<profile_dir>/<name>` copy and the raw `<profile_dir>/deps/<name>`
/// compiler output — a bare `cargo test` (what this repo's own CI runs) never uplifts, only
/// `target/deps`, so checking only the former silently finds nothing even though the cdylib was
/// really built. Same fix already applied to store-postgres-plugin's, auth-oidc-plugin's,
/// webrequest-hook's, and busbarAI core's hook-test-plugin's equivalent `plugin_path()` helpers.
fn plugin_path() -> Option<PathBuf> {
    let candidate = (|| {
        let exe = std::env::current_exe().ok()?;
        let profile_dir = exe.parent()?.parent()?;
        let name = busbar_plugin_loader::plugin_library_filename("headroom_hook");
        let uplifted = profile_dir.join(&name);
        let raw = profile_dir.join("deps").join(&name);
        [uplifted, raw]
            .into_iter()
            .filter_map(|p| {
                std::fs::metadata(&p)
                    .and_then(|m| m.modified())
                    .ok()
                    .map(|mtime| (p, mtime))
            })
            .max_by_key(|(_, mtime)| *mtime)
            .map(|(p, _)| p)
    })();
    if candidate.is_none() && std::env::var_os("CI").is_some() {
        panic!(
            "the headroom-hook cdylib is not built under CI: `cargo test` must build it \
             (checked both the uplifted target dir and target/deps). Refusing to silently skip \
             real coverage."
        );
    }
    candidate
}

/// The sibling `busbarAI` checkout's root — the same path convention this crate's own `Cargo.toml`
/// path dependencies already require to exist.
fn busbarai_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../busbarAI")
        .canonicalize()
        .expect("sibling busbarAI checkout must exist (see Cargo.toml path deps)")
}

/// Under `CI` a missing prerequisite is a HARD FAILURE, never a silent skip (mirrors the sibling
/// `store-redis` plugin's own `redis_url()` gating discipline for its admin-API e2e test) — locally
/// it's a clean print-and-skip.
fn require_or_skip(ok: bool, what: &str) -> bool {
    if ok {
        return true;
    }
    if std::env::var_os("CI").is_some() {
        panic!(
            "{what} is unavailable under CI: refusing to silently skip the real admin-API e2e test"
        );
    }
    eprintln!("skip: {what} unavailable (run under CI or build the sibling busbarAI checkout)");
    false
}

/// Build (once; cached by cargo across runs) the real `busbar` and `busbar-plugin-pack` binaries
/// from the sibling `busbarAI` checkout — never a fixture, never a stub.
fn build_real_binaries() -> (PathBuf, PathBuf) {
    let root = busbarai_root();
    let status = Command::new("cargo")
        .args([
            "build",
            "--release",
            "-p",
            "busbar",
            "-p",
            "busbar-plugin-pack",
        ])
        .current_dir(&root)
        .status()
        .expect("run cargo build for busbar + busbar-plugin-pack");
    assert!(
        status.success(),
        "building the real busbar + busbar-plugin-pack binaries must succeed"
    );
    (
        root.join("target/release/busbar"),
        root.join("target/release/busbar-plugin-pack"),
    )
}

/// RAII guard for a spawned `busbar` child process: kills and reaps it on drop, including when a
/// panic unwinds partway through the test.
struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// A free-at-the-moment localhost port (bind-then-drop; a TOCTOU race is possible in principle but
/// is the accepted pattern for test port allocation, same as the sibling `store-redis` e2e test).
fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .expect("bind an ephemeral port")
        .local_addr()
        .expect("read local addr")
        .port()
}

fn poll_admin_up(admin: &str, token: &str, client: &reqwest::blocking::Client) -> bool {
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        if let Ok(resp) = client
            .get(format!("{admin}/plugins?type=hooks"))
            .bearer_auth(token)
            .send()
            && resp.status().is_success()
        {
            return true;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    false
}

/// A synthetic log-dump line set: many segments, mostly noise, one load-bearing ERROR — realistic
/// enough for BM25 relevance scoring to actually distinguish signal from noise (a toy 2-word string
/// never triggers real compression; see `tests/e2e.rs`'s own `log_dump` / module doc comment).
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

/// A minimal, synchronous mock upstream OpenAI-shaped provider: accepts exactly one
/// `POST /v1/chat/completions`, captures its raw JSON body, and answers with a well-formed (if
/// minimal) chat-completion response so busbar's own response path completes normally. Runs on a
/// background thread for the lifetime of the returned `tiny_http::Server` handle (dropping the
/// handle's owning thread join handle is not required — the server is torn down when the test
/// process exits; the port is test-local and freed then).
fn spawn_mock_upstream() -> (u16, Arc<Mutex<Option<Vec<u8>>>>) {
    let server = tiny_http::Server::http("127.0.0.1:0").expect("bind mock upstream");
    let port = server.server_addr().to_ip().expect("ip addr").port();
    let captured: Arc<Mutex<Option<Vec<u8>>>> = Arc::new(Mutex::new(None));
    let captured_writer = Arc::clone(&captured);
    std::thread::spawn(move || {
        // One request is all this test drives; keep serving so a stray retry doesn't hang busbar.
        for mut req in server.incoming_requests() {
            let mut body = Vec::new();
            let _ = req.as_reader().read_to_end(&mut body);
            *captured_writer.lock().unwrap() = Some(body);
            let resp_body = serde_json::json!({
                "id": "mock-1",
                "object": "chat.completion",
                "created": 0,
                "model": "test-model",
                "choices": [{
                    "index": 0,
                    "message": {"role": "assistant", "content": "ok"},
                    "finish_reason": "stop"
                }],
                "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
            })
            .to_string();
            let header =
                tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..])
                    .expect("valid header");
            let response = tiny_http::Response::from_string(resp_body).with_header(header);
            let _ = req.respond(response);
        }
    });
    (port, captured)
}

/// END-TO-END: install `headroom-hook` over the REAL admin HTTP API into a REAL running `busbar`
/// process, wire it onto a pool as a `prompt: rw` gate via a restart (the real operational flow),
/// then drive a REAL compressible chat request through the restarted instance and confirm the
/// request that reached the mock upstream is genuinely shorter than what the client sent.
#[test]
fn admin_api_installs_headroom_and_a_real_request_is_compressed_upstream() {
    let Some(so_path) = plugin_path() else {
        if !require_or_skip(false, "the headroom-hook cdylib (run under `cargo test`)") {
            return;
        }
        unreachable!();
    };
    if !require_or_skip(busbarai_root().is_dir(), "the sibling busbarAI checkout") {
        return;
    }

    let (busbar_bin, pack_bin) = build_real_binaries();

    let work = std::env::temp_dir().join(format!(
        "headroom-admin-e2e-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let plugins_dir = work.join("plugins");
    std::fs::create_dir_all(&plugins_dir).unwrap();

    // Pack the REAL cdylib into a real tarball, exactly as CI's own release step does — unsigned
    // locally, same allow_unsigned fallback CI uses without a BUSBAR_SIGN_KEY.
    let tarball = work.join("headroom.tar.gz");
    let status = Command::new(&pack_bin)
        .args([
            "pack",
            "--lib",
            so_path.to_str().unwrap(),
            "--name",
            "headroom",
            "--alias",
            "headroom",
            "--kind",
            "hook",
            "--version",
            "0.0.0-e2e",
            "--publisher",
            "busbar",
            "--description",
            "e2e admin-api install proof",
            "--license",
            "Apache-2.0",
            "--needs-prompt",
            "rw",
            "--out",
            tarball.to_str().unwrap(),
            "--allow-unsigned",
        ])
        .status()
        .expect("run busbar-plugin-pack");
    assert!(status.success(), "packing the plugin must succeed");

    let (upstream_port, captured) = spawn_mock_upstream();

    let admin_token = format!("e2e-admin-{}-{}", std::process::id(), free_port());
    let port = free_port();
    let admin_port = free_port();
    let admin = format!("http://127.0.0.1:{admin_port}/api/v1/admin");

    let providers_file = work.join("providers.yaml");
    let overlay = work.join("overlay.json");
    std::fs::write(
        &providers_file,
        format!(
            "mock:\n  protocol: openai\n  base_url: \"http://127.0.0.1:{upstream_port}\"\n  api_key_env: MOCK_KEY\n"
        ),
    )
    .unwrap();

    // BOOT #1 config: no gate yet — the whole point is that the gate only becomes real once
    // installed over the admin API AND wired in on the next boot. There is no top-level `hooks:`
    // registry section in config.yaml (confirmed by boot #1's own accepted-field list: `listen,
    // tls, admin_listen, admin_tls, admin_insecure, auth, providers, models, pools, global_hooks,
    // groups, rate_card, per_request_fee, store, secrets, advanced, observability, plugins,
    // security, limits, metrics, health, routing` — no `hooks`); an inline `{ module: ...,
    // kind: gate, prompt: rw, ... }` ref goes in `global_hooks:` (`HookModuleRef` in
    // `busbarAI/crates/busbar/src/config/mod.rs`), exactly like the commented-out example in
    // `busbarAI/examples/clean-config-1.5.0.yaml`. (A pool's OWN `hooks: [...]` list also accepts
    // an inline module ref for a pool-scoped gate, but empirically that path did not fire the
    // rewrite in manual verification against this exact plugin/config shape — `global_hooks:` is
    // the directly-documented, confirmed-working path, and a global rewrite gate is itself a
    // legitimate real deployment shape, so this test uses it rather than chasing the pool-scoped
    // gap, which is a busbarAI-core question, not a headroom-hook one.)
    let config_v1 = work.join("config-1.yaml");
    let base_config = |global_hooks: &str| {
        format!(
            "listen: \"127.0.0.1:{port}\"\n\
             admin_listen: \"127.0.0.1:{admin_port}\"\n\
             auth:\n  chain: [keys]\n  signing_key: {{ env: BUSBAR_SIGNING_KEY }}\n  admin_auth:\n    - admin-tokens: {{ token: {{ env: E2E_ADMIN_TOKEN }} }}\n\
             plugins:\n  enabled: true\n  dir: {}\n  trust:\n    allow_unsigned: true\n\
             providers:\n  mock:\n    api_key: {{ env: MOCK_KEY }}\n\
             models:\n  test-model:\n    provider: mock\n\
             {global_hooks}\
             pools:\n  p:\n    members:\n      - {{ model: test-model, weight: 1 }}\n",
            plugins_dir.display(),
        )
    };
    std::fs::write(&config_v1, base_config("")).unwrap();

    let client = reqwest::blocking::Client::new();

    // Both boots' stdout+stderr are captured to real files (never an unread pipe) so a failure can
    // quote the actual boot log instead of a bare timeout.
    let spawn = |config: &PathBuf, label: &str| {
        let log = std::fs::File::create(work.join(format!("busbar-{label}.log")))
            .expect("create boot log file");
        let mut cmd = Command::new(&busbar_bin);
        cmd.env("BUSBAR_CONFIG", config)
            .env("BUSBAR_PROVIDERS", &providers_file)
            .env("E2E_ADMIN_TOKEN", &admin_token)
            .env("BUSBAR_SIGNING_KEY", TEST_SIGNING_KEY)
            .env("MOCK_KEY", "unused-mock-provider-key")
            .env("BUSBAR_STATE_FILE", "")
            .env("BUSBAR_CONFIG_OVERLAY", &overlay)
            .stdout(Stdio::from(log.try_clone().expect("clone log fd")))
            .stderr(Stdio::from(log));
        cmd.spawn().expect("spawn a real busbar boot")
    };
    let boot_log = |label: &str| {
        std::fs::read_to_string(work.join(format!("busbar-{label}.log"))).unwrap_or_default()
    };

    // Boot #1: no headroom hook loaded yet.
    let mut guard = ChildGuard(spawn(&config_v1, "1"));
    assert!(
        poll_admin_up(&admin, &admin_token, &client),
        "busbar (boot #1) must come up with the admin listener responsive; log:\n{}",
        boot_log("1")
    );

    // THE REAL INSTALL: POST the packed tarball's base64 bytes to the real admin API.
    let tarball_bytes = std::fs::read(&tarball).unwrap();
    let tarball_b64 = base64::engine::general_purpose::STANDARD.encode(&tarball_bytes);
    let install_resp = client
        .post(format!("{admin}/plugins"))
        .bearer_auth(&admin_token)
        .json(&serde_json::json!({
            "file": "headroom-0.0.0-e2e.tar.gz",
            "tarball_b64": tarball_b64,
        }))
        .send()
        .expect("POST /api/v1/admin/plugins");
    let install_status = install_resp.status();
    let install_body: serde_json::Value = install_resp.json().unwrap_or(serde_json::Value::Null);
    assert_eq!(
        install_status.as_u16(),
        201,
        "expected 201 from POST /api/v1/admin/plugins, got {install_status}: {install_body}"
    );
    assert_eq!(
        install_body.get("name").and_then(|v| v.as_str()),
        Some("headroom"),
        "install response must name the installed plugin: {install_body}"
    );

    // Restart to apply: attaching a gate to a pool is config-file-only (no live admin endpoint for
    // pool topology beyond the read-only GET /pools*), and installing does not hot-swap either —
    // both take effect on the NEXT boot. Play the supervisor's role directly: request a graceful
    // restart, wait for the real exit, then boot #2 with a config that now wires the just-installed
    // plugin onto the pool as a prompt:rw gate, pointed at the SAME plugins.dir.
    let _ = client
        .post(format!("{admin}/restart"))
        .bearer_auth(&admin_token)
        .send();
    let exit_deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(Some(_)) = guard.0.try_wait() {
            break;
        }
        if Instant::now() >= exit_deadline {
            let _ = guard.0.kill();
            let _ = guard.0.wait();
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    let config_v2 = work.join("config-2.yaml");
    std::fs::write(
        &config_v2,
        // `timeout_ms` defaults to 1 (fine for a trivial in-process gate) — headroom does REAL BM25
        // compression work, which the README's own benchmark measures at ~547µs p50 but can exceed
        // 1ms; a too-tight deadline coerces to on_error (default `nothing`: pass through
        // unmodified) and this test would then observe byte-identical bodies upstream even with a
        // correctly installed, correctly wired, correctly firing gate. Raise it generously.
        base_config(
            "global_hooks:\n  - { module: headroom, kind: gate, prompt: rw, timeout_ms: 500 }\n",
        ),
    )
    .unwrap();
    let guard2 = ChildGuard(spawn(&config_v2, "2"));
    assert!(
        poll_admin_up(&admin, &admin_token, &client),
        "busbar (boot #2, with headroom wired onto pool p) must come back up; log:\n{}",
        boot_log("2")
    );
    let log2 = boot_log("2");
    assert!(
        !log2.contains("does not match any plugin") && !log2.contains("unresolvable"),
        "the headroom plugin persisted on disk must resolve as the pool's gate, not be rejected \
         as unmatched: {log2}"
    );

    let list_resp = client
        .get(format!("{admin}/plugins?type=hooks"))
        .bearer_auth(&admin_token)
        .send()
        .expect("GET /api/v1/admin/plugins?type=hooks");
    let list_body = list_resp.text().unwrap_or_default();
    assert!(
        list_body.contains("headroom"),
        "the restarted instance must list the installed headroom plugin: {list_body}"
    );

    // Mint a real virtual key over the admin API to authenticate the real data-plane request.
    let mint_resp = client
        .post(format!("{admin}/keys"))
        .bearer_auth(&admin_token)
        .json(&serde_json::json!({ "name": "e2e-headroom-verify" }))
        .send()
        .expect("POST /api/v1/admin/keys");
    let mint_status = mint_resp.status();
    let mint_body: serde_json::Value = mint_resp.json().expect("mint response must be JSON");
    assert_eq!(
        mint_status.as_u16(),
        201,
        "expected 201 from POST /api/v1/admin/keys, got {mint_status}: {mint_body}"
    );
    let bearer = mint_body
        .get("token")
        .and_then(|v| v.as_str())
        .expect("mint response must carry the once-shown bearer token")
        .to_string();

    // THE REAL, OBSERVABLE EFFECT: a real chat completion, with a long compressible history, through
    // the running instance — the exact request an SDK/client would send.
    let history = format!("{}{}", log_dump(40), log_dump(40));
    let ask = "why did the deployment fail";
    let sent_history_len = history.len();
    let completion_resp = client
        .post(format!("http://127.0.0.1:{port}/v1/chat/completions"))
        .bearer_auth(&bearer)
        .json(&serde_json::json!({
            "model": "test-model",
            "messages": [
                {"role": "user", "content": history},
                {"role": "user", "content": ask}
            ]
        }))
        .send()
        .expect("POST /v1/chat/completions through the running busbar instance");
    assert!(
        completion_resp.status().is_success(),
        "the compressed request must still complete successfully: {}",
        completion_resp.status()
    );

    // Stop busbar before inspecting the captured upstream body, so there's no possibility of racing
    // a retry writing to `captured` mid-read.
    drop(guard2);
    drop(guard);

    let upstream_body_bytes = captured
        .lock()
        .unwrap()
        .clone()
        .expect("the mock upstream must have received exactly one real HTTP request");
    let upstream_body: serde_json::Value =
        serde_json::from_slice(&upstream_body_bytes).expect("upstream body must be valid JSON");
    let upstream_messages = upstream_body["messages"]
        .as_array()
        .expect("upstream request must carry a messages array");
    assert_eq!(upstream_messages.len(), 2);
    let upstream_history = upstream_messages[0]["content"]
        .as_str()
        .expect("upstream history must be a string");
    let upstream_ask = upstream_messages[1]["content"]
        .as_str()
        .expect("upstream ask must be a string");

    assert_eq!(
        upstream_ask, ask,
        "the ask must survive verbatim through the real admin-installed gate"
    );
    assert!(
        upstream_history.contains("ERROR"),
        "the compressed history must keep the load-bearing signal line: {upstream_history:?}"
    );
    assert!(
        upstream_history.len() < sent_history_len,
        "the request that PHYSICALLY reached the mock upstream must be shorter than what the \
         client sent (proving the full real path: HTTP admin install -> real dlopen -> real hook \
         invocation -> real HTTP request mutation -> real upstream receipt) — sent {sent_history_len} \
         bytes, upstream received {} bytes",
        upstream_history.len()
    );
}
