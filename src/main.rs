// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! `headroom-hook` — a busbar GATE that compresses chat history with headroom-core.
//!
//! Speaks busbar's Unix-socket hook transport: newline-delimited JSON on a kept-alive, reused
//! connection (engine `src/hooks/socket.rs`), the full **5-message wire** — `configure`,
//! `describe`, `decide`, `transform`, `notify`. Busbar sends `configure` as the FIRST line on
//! every connection (and re-pushes it live on `PATCH /api/v1/admin/hooks/{name}/settings`); we apply
//! the settings and ack, echoing the pushed `settings_version` (commit-on-ack). `describe` returns
//! our `{schema, dashboard}` self-description; `status` returns observed settings + a metrics array
//! (surfaced on busbar's admin API and its `/metrics/hooks` scrape). Everything else is decision traffic:
//! register as a `kind: gate` with `prompt: rw` (see ../README.md for the YAML) and busbar fires
//! us on the phase-1 transform pass, splicing the returned `rewrite.messages` into the request
//! body before the routing decision and dispatch. All dispatch lives in `compress::handle_line`.
//!
//! Config (env seeds the startup values; a `configure` push replaces them live):
//!   HEADROOM_SOCKET           socket path to own        (default /tmp/headroom.sock)
//!   HEADROOM_TARGET_RATIO     fraction of tokens kept   (default 0.5)
//!   HEADROOM_MIN_SAVINGS_PCT  abstain below this        (default 10)
//!   HEADROOM_PRICE_UDOLLARS_PER_KTOK  $ estimate price  (default 2500)
//!
//! Lifecycle: on startup a live instance already owning the socket path is detected (by
//! connecting) and refused — a stale socket file (dead previous instance) is removed so the bind
//! succeeds. SIGINT/SIGTERM remove the socket file and exit cleanly; busbar's lazy-connect +
//! reconnect-once semantics ride out a hook restart with zero failed requests.

mod compress;

use compress::{Knobs, Metrics, encode_reply, handle_line};
use std::sync::{Arc, Mutex, RwLock};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

/// Request-line cap: busbar caps OUR reply at 64 KiB; inbound projections of big prompts can be
/// larger, so allow a roomy but bounded line (a hostile peer cannot drive unbounded allocation).
const MAX_REQUEST_BYTES: usize = 8 * 1024 * 1024;

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let socket =
        std::env::var("HEADROOM_SOCKET").unwrap_or_else(|_| "/tmp/headroom.sock".to_string());
    let knobs = Knobs {
        target_ratio: env_f64("HEADROOM_TARGET_RATIO", 0.5).clamp(0.05, 1.0),
        min_savings_pct: env_f64("HEADROOM_MIN_SAVINGS_PCT", 10.0).clamp(0.0, 100.0),
        price_udollars_per_ktok: env_f64("HEADROOM_PRICE_UDOLLARS_PER_KTOK", 2500.0)
            .clamp(0.0, 1_000_000.0),
        settings_version: 0,
    };
    let listener = bind_socket(&socket)?;
    eprintln!(
        "headroom-hook v{} (headroom-core {}) listening on {socket} (target_ratio {}, min_savings {}%)",
        env!("CARGO_PKG_VERSION"),
        env!("HEADROOM_CORE_REF"),
        knobs.target_ratio,
        knobs.min_savings_pct
    );
    // The LIVE settings: env-seeded, replaced whenever busbar pushes a `configure` (which it does
    // as the first line of every connection, and mid-connection on a settings PATCH). Shared
    // across connections so a push on any one applies process-wide.
    let knobs = Arc::new(RwLock::new(knobs));
    // Per-pool metrics, accumulated across ALL connections (the process serves many pools; busbar
    // sends `request.pool` on every transform). Read back on a `status` query.
    let metrics = Arc::new(Mutex::new(Metrics::default()));
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                match accepted {
                    Ok((stream, _)) => {
                        let knobs = knobs.clone();
                        let metrics = metrics.clone();
                        tokio::spawn(async move { serve_conn(stream, knobs, metrics).await });
                    }
                    // A transient accept failure (EMFILE, ECONNABORTED, ...) must not kill the
                    // hook — log, back off briefly so a persistent condition can't spin the CPU,
                    // and keep listening. Busbar retries its connect on the next decision.
                    Err(e) => {
                        eprintln!("headroom-hook: accept failed (retrying): {e}");
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    }
                }
            }
            _ = tokio::signal::ctrl_c() => break,
            _ = sigterm.recv() => break,
        }
    }
    let _ = std::fs::remove_file(&socket);
    Ok(())
}

/// Bind the socket path, single-instance by path: if a hook already ANSWERS on the path, refuse to
/// start (removing its file would split-brain the deployment — busbar's kept-alive connection
/// stays with the old instance while new connections reach nobody). If the file exists but nothing
/// answers (a stale socket from a crashed/killed previous run), remove it and bind.
fn bind_socket(socket: &str) -> std::io::Result<UnixListener> {
    if std::fs::symlink_metadata(socket).is_ok() {
        if std::os::unix::net::UnixStream::connect(socket).is_ok() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AddrInUse,
                format!("another instance is already serving {socket}"),
            ));
        }
        // Stale socket file from a previous run: remove so bind succeeds.
        let _ = std::fs::remove_file(socket);
    }
    UnixListener::bind(socket)
}

/// One kept-alive busbar connection: wire line -> reply line, until EOF. Every message —
/// configure/describe included — is one line in, one line out, so a single loop serves the whole
/// 5-message wire (notify is the exception: busbar never reads a tap reply, but we are a gate and
/// are never registered as a tap).
///
/// Each line is dispatched on the blocking pool: compression is CPU-bound (BM25 over up to 8 MB
/// of history), and running it inline would stall the reactor for every other connection. The
/// blocking task also acts as a panic bulkhead — if `handle_line` panics despite its own
/// containment, the JoinError degrades to abstain and the connection lives on.
async fn serve_conn(stream: UnixStream, knobs: Arc<RwLock<Knobs>>, metrics: Arc<Mutex<Metrics>>) {
    let (r, mut w) = stream.into_split();
    let mut reader = BufReader::new(r);
    let mut line: Vec<u8> = Vec::with_capacity(4096);
    loop {
        line.clear();
        // Bounded read: take() caps how much one line may pull.
        let mut limited = tokio::io::AsyncReadExt::take(&mut reader, MAX_REQUEST_BYTES as u64);
        match limited.read_until(b'\n', &mut line).await {
            Ok(0) => return,                           // busbar closed the connection
            Ok(_) if !line.ends_with(b"\n") => return, // over-cap or truncated: drop the conn
            Ok(_) => {}
            Err(_) => return,
        }
        let owned = std::mem::take(&mut line);
        let k = knobs.clone();
        let met = metrics.clone();
        let (out, returned) = match tokio::task::spawn_blocking(move || {
            let out = encode_reply(&handle_line(&owned, &k, &met));
            (out, owned)
        })
        .await
        {
            Ok((out, returned)) => (out, Some(returned)),
            Err(_) => (b"{}\n".to_vec(), None), // panicked task: abstain, keep serving
        };
        // Reuse the line buffer across iterations (avoids re-growing to prompt size every line).
        line = returned.unwrap_or_default();
        if w.write_all(&out).await.is_err() || w.flush().await.is_err() {
            return;
        }
    }
}

fn env_f64(name: &str, default: f64) -> f64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

#[cfg(test)]
#[path = "tests/main.rs"]
mod tests;
