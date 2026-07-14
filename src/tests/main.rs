// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! Tests for `main` (kept out of the core file so it reads lean).

use super::*;

/// The connection-open handshake over a real socket, exactly as busbar drives it: `configure`
/// is the FIRST line, we ack echoing the pushed `settings_version`, and the SAME connection
/// then serves `describe` and decision traffic with the pushed settings live.
#[tokio::test]
async fn configure_ack_handshake_then_traffic_on_one_connection() {
    let (client, server) = UnixStream::pair().unwrap();
    let knobs = Arc::new(RwLock::new(Knobs::default()));
    let served = knobs.clone();
    let met = Arc::new(Mutex::new(Metrics::default()));
    tokio::spawn(async move { serve_conn(server, served, met).await });

    let (r, mut w) = client.into_split();
    let mut reader = BufReader::new(r);
    let mut line = String::new();
    let mut round_trip = async |msg: &str, line: &mut String| {
        w.write_all(msg.as_bytes()).await.unwrap();
        w.write_all(b"\n").await.unwrap();
        line.clear();
        reader.read_line(line).await.unwrap();
        serde_json::from_str::<serde_json::Value>(line).unwrap()
    };

    // 1. configure first — commit-on-ack, version echoed.
    let ack = round_trip(
        r#"{"configure":{"hook":"headroom","settings":{"target_ratio":0.3,"min_savings_pct":42.0},"settings_version":7,"busbar_version":"1.3.0"}}"#,
        &mut line,
    )
    .await;
    assert_eq!(ack["ack"]["settings_version"], 7);
    assert_eq!(knobs.read().unwrap().min_savings_pct, 42.0);

    // 2. describe mid-connection — the {schema, dashboard} envelope comes back.
    let desc = round_trip(r#"{"describe":true}"#, &mut line).await;
    assert_eq!(desc["schema"]["type"], "object");
    assert!(desc["schema"]["properties"]["target_ratio"].is_object());
    assert!(desc["dashboard"]["widgets"].is_array());

    // 3. decision traffic still round-trips on the same connection (shape-only projection,
    //    no prompt grant -> abstain).
    let reply = round_trip(
        r#"{"request":{"pool":"p","ingress_protocol":"anthropic","message_count":1,"has_tools":false,"total_chars":5,"stream":false},"candidates":[],"context":{"pool":"p","budget_remaining":null}}"#,
        &mut line,
    )
    .await;
    assert_eq!(reply, serde_json::json!({}));
}

/// A request line that exceeds the 8 MiB inbound cap (a hostile or desynced peer) drops the
/// connection — bounded allocation, no reply, no panic; the process keeps serving new
/// connections.
#[tokio::test]
async fn over_cap_request_line_drops_connection() {
    let (client, server) = UnixStream::pair().unwrap();
    let knobs = Arc::new(RwLock::new(Knobs::default()));
    let met = Arc::new(Mutex::new(Metrics::default()));
    tokio::spawn(async move { serve_conn(server, knobs, met).await });

    let (mut r, mut w) = client.into_split();
    // 8 MiB + 1 of 'a' with no newline: past the cap, never a complete line.
    let flood = vec![b'a'; MAX_REQUEST_BYTES + 1];
    // The server may drop the connection mid-write; either way the read below must see EOF.
    let _ = w.write_all(&flood).await;
    let _ = w.flush().await;
    let mut buf = Vec::new();
    let n = tokio::io::AsyncReadExt::read_to_end(&mut r, &mut buf)
        .await
        .unwrap_or(0);
    assert_eq!(n, 0, "server must drop the connection without replying");
}

/// `bind_socket` lifecycle: a STALE socket file is silently replaced; a LIVE instance on the
/// same path is refused with `AddrInUse` (never hijacked).
#[tokio::test]
async fn bind_socket_replaces_stale_and_refuses_live() {
    let dir = std::env::temp_dir().join(format!("headroom-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("h.sock");
    let path_str = path.to_string_lossy().into_owned();

    // Stale: create a socket file with no listener behind it (bind then drop the listener —
    // the file outlives it).
    {
        let l = std::os::unix::net::UnixListener::bind(&path).unwrap();
        drop(l);
    }
    assert!(path.exists(), "stale socket file should linger");
    let live = bind_socket(&path_str).expect("stale socket must be replaced");

    // Live: a second bind on the same path must be refused while `live` is accepting.
    let err = bind_socket(&path_str).expect_err("live socket must be refused");
    assert_eq!(err.kind(), std::io::ErrorKind::AddrInUse);

    drop(live);
    let _ = std::fs::remove_dir_all(&dir);
}
