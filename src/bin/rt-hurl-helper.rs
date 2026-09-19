//! WebSocket-side smoke-test helper for the message bus.
//!
//! Hurl is HTTP-only — it can't do a WS upgrade, let alone read frames
//! for later assertion. This binary is the WS half of the smoke test:
//! opens `/api/rt/ws`, speaks JSON-RPC 2.0, and either collects events
//! into a JSON file for shell assertions (`subscribe-and-collect`),
//! validates that an authz-denied subscribe returns the expected wire
//! error code (`expect-denied`), or exercises the collab binary-frame
//! path (`collab-sync-probe`) by sending a Yjs sync-step-1 request and
//! asserting on the sync-step-2 reply header.
//!
//! Invocation (from `tests/api/rt_bus_check.sh`):
//!
//! ```bash
//! rt-hurl-helper subscribe-and-collect \
//!   --url ws://127.0.0.1:$PORT/api/rt/ws \
//!   --token $USER_JWT \
//!   --subscribe folder:$FOLDER_A \
//!   --expect-events 1 \
//!   --timeout 3s \
//!   --output /tmp/rt_s1.json &
//!
//! rt-hurl-helper expect-denied \
//!   --url ws://127.0.0.1:$PORT/api/rt/ws \
//!   --token $USER2_JWT \
//!   --subscribe folder:$FOLDER_A \
//!   --reason no_read \
//!   --timeout 2s
//!
//! rt-hurl-helper collab-sync-probe \
//!   --url ws://127.0.0.1:$PORT/api/rt/ws \
//!   --token $USER_JWT \
//!   --file $FILE_UUID \
//!   --timeout 3s
//!
//! rt-hurl-helper collab-fanout-listen \
//!   --url ws://127.0.0.1:$PORT/api/rt/ws \
//!   --token $USER_JWT \
//!   --file $FILE_UUID \
//!   --expect-content "hello from A" \
//!   --ready-file /tmp/ready.b \
//!   --timeout 5s
//!
//! rt-hurl-helper collab-fanout-write \
//!   --url ws://127.0.0.1:$PORT/api/rt/ws \
//!   --token $USER_JWT \
//!   --file $FILE_UUID \
//!   --content "hello from A" \
//!   --timeout 5s
//! ```
//!
//! Exit codes:
//!   * 0 — expectation met.
//!   * 1 — expectation failed (wrong event, unexpected event, timeout
//!     without hitting the target, denied when expecting event,
//!     event when expecting denied).
//!   * 2 — protocol / connect error the shell can distinguish from a
//!     real assertion failure.
//!
//! JSON output shape for `subscribe-and-collect` (written to `--output`):
//!
//! ```jsonc
//! {
//!   "subscribed": ["folder:..."],
//!   "events":     [ { "topic": "folder:...", "event": "file_created",
//!                     "data": { ... } } ],
//!   "timed_out":  false
//! }
//! ```

use std::process::ExitCode;
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use yrs::updates::decoder::Decode;
use yrs::{Doc, GetString, ReadTxn, StateVector, Text, Transact, Update};

// ════════════════════════════════════════════════════════════════════════════
// CLI parsing (minimal, dependency-free)
// ════════════════════════════════════════════════════════════════════════════

struct Args {
    mode: Mode,
    url: String,
    /// Either `--token <jwt>` (Authorization: Bearer path — the original
    /// helper flow) or `--ticket <uuid>` (Sec-WebSocket-Protocol path
    /// — exercises F). Exactly one MUST be set; parse_args enforces.
    auth: WsAuth,
    subscribe: Vec<String>,
    expect_events: Option<usize>,
    reason: Option<String>,
    timeout: Duration,
    output: Option<String>,
    /// Optional path the helper `touch`es the instant EVERY requested
    /// `--subscribe` topic has been ack'd by the server. Shell tests
    /// wait on this file before firing the upload that publishes to
    /// the topic, closing the "sleep 0.4 hoping the subscribe landed
    /// in time" race that occasionally dropped events on slow /
    /// cold-cache runs. Off by default; only used by the smoke test.
    ready_file: Option<String>,
    /// `--file <uuid>` — the target file for `collab-sync-probe`,
    /// `collab-fanout-listen` and `collab-fanout-write`. Parsed to
    /// 16 raw bytes so the helper can emit the wire header
    /// (`[1 byte kind][16 bytes file_id]…`) without pulling in the
    /// uuid crate.
    file_id: Option<[u8; 16]>,
    /// `--content <string>` — text the write-side helper inserts into
    /// a fresh Yjs Doc before broadcasting the resulting UPDATE.
    /// `--expect-content <string>` — text the listen-side asserts on
    /// after decoding the incoming UPDATE.
    content: Option<String>,
    expect_content: Option<String>,
}

/// How the helper authenticates the WS upgrade. Mirrors the two paths
/// `rt_ws_handler::authenticate_upgrade` accepts.
enum WsAuth {
    Bearer(String),
    Ticket(String),
}

enum Mode {
    SubscribeAndCollect,
    ExpectDenied,
    CollabSyncProbe,
    /// Subscribe to `collab:<file_id>`, wait for one `0x01` UPDATE
    /// frame to arrive, decode it, assert its text content matches
    /// `--expect-content`. Used with a paired `collab-fanout-write`
    /// helper on a second socket to prove the actor's outbox fans out
    /// to every subscriber.
    CollabFanoutListen,
    /// Subscribe to `collab:<file_id>`, build a Yjs UPDATE that
    /// inserts `--content` into an otherwise-empty doc, send it as a
    /// `0x01` binary frame, and exit. Companion to `collab-fanout-listen`.
    CollabFanoutWrite,
}

/// Parse a canonical dashed UUID (e.g. `f47ac10b-58cc-4372-a567-0e02b2c3d479`)
/// into its 16 raw bytes. Kept dependency-free — pulling in the `uuid`
/// crate for one hex-decode would be overkill.
fn parse_uuid_bytes(s: &str) -> Result<[u8; 16], String> {
    let hex: String = s.chars().filter(|c| *c != '-').collect();
    if hex.len() != 32 {
        return Err(format!("bad uuid (want 32 hex chars, got {}): {s}", hex.len()));
    }
    let mut out = [0u8; 16];
    for (i, chunk) in hex.as_bytes().chunks(2).enumerate() {
        let s = std::str::from_utf8(chunk).map_err(|_| format!("bad uuid: {s}"))?;
        out[i] = u8::from_str_radix(s, 16).map_err(|_| format!("bad uuid hex: {s}"))?;
    }
    Ok(out)
}

fn parse_duration(s: &str) -> Result<Duration, String> {
    // Accept `<n>s`, `<n>ms`, or a bare integer (interpreted as
    // seconds). Kept small — hurl and shell are the only callers.
    let s = s.trim();
    if let Some(num) = s.strip_suffix("ms") {
        num.parse::<u64>()
            .map(Duration::from_millis)
            .map_err(|_| format!("bad duration: {s}"))
    } else if let Some(num) = s.strip_suffix('s') {
        num.parse::<u64>()
            .map(Duration::from_secs)
            .map_err(|_| format!("bad duration: {s}"))
    } else {
        s.parse::<u64>()
            .map(Duration::from_secs)
            .map_err(|_| format!("bad duration: {s}"))
    }
}

fn parse_args() -> Result<Args, String> {
    let mut it = std::env::args().skip(1);
    let mode = match it.next().as_deref() {
        Some("subscribe-and-collect") => Mode::SubscribeAndCollect,
        Some("expect-denied") => Mode::ExpectDenied,
        Some("collab-sync-probe") => Mode::CollabSyncProbe,
        Some("collab-fanout-listen") => Mode::CollabFanoutListen,
        Some("collab-fanout-write") => Mode::CollabFanoutWrite,
        Some(other) => return Err(format!("unknown mode: {other}")),
        None => return Err("mode is required".into()),
    };

    let mut url = None;
    let mut token = None;
    let mut ticket = None;
    let mut subscribe = Vec::new();
    let mut expect_events = None;
    let mut reason = None;
    let mut timeout = Duration::from_secs(3);
    let mut output = None;
    let mut ready_file = None;
    let mut file_id = None;
    let mut content = None;
    let mut expect_content = None;

    while let Some(flag) = it.next() {
        let value = it
            .next()
            .ok_or_else(|| format!("flag {flag} requires a value"))?;
        match flag.as_str() {
            "--url" => url = Some(value),
            "--token" => token = Some(value),
            "--ticket" => ticket = Some(value),
            "--subscribe" => subscribe.push(value),
            "--expect-events" => {
                expect_events = Some(
                    value
                        .parse::<usize>()
                        .map_err(|_| format!("--expect-events not a number: {value}"))?,
                );
            }
            "--reason" => reason = Some(value),
            "--timeout" => timeout = parse_duration(&value)?,
            "--output" => output = Some(value),
            "--ready-file" => ready_file = Some(value),
            "--file" => file_id = Some(parse_uuid_bytes(&value)?),
            "--content" => content = Some(value),
            "--expect-content" => expect_content = Some(value),
            other => return Err(format!("unknown flag: {other}")),
        }
    }

    // Exactly one credential MUST be set. Emitting a specific error
    // makes shell-script drift ("forgot to swap --token for --ticket")
    // debuggable at a glance.
    let auth = match (token, ticket) {
        (Some(_), Some(_)) => return Err("pass exactly one of --token or --ticket".into()),
        (Some(t), None) => WsAuth::Bearer(t),
        (None, Some(t)) => WsAuth::Ticket(t),
        (None, None) => return Err("--token or --ticket required".into()),
    };

    Ok(Args {
        mode,
        url: url.ok_or("--url required")?,
        auth,
        subscribe,
        expect_events,
        reason,
        timeout,
        output,
        ready_file,
        file_id,
        content,
        expect_content,
    })
}

// ════════════════════════════════════════════════════════════════════════════
// Main
// ════════════════════════════════════════════════════════════════════════════

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("rt-hurl-helper: {e}");
            return ExitCode::from(2);
        }
    };

    let result = match args.mode {
        Mode::SubscribeAndCollect => subscribe_and_collect(args).await,
        Mode::ExpectDenied => expect_denied(args).await,
        Mode::CollabSyncProbe => collab_sync_probe(args).await,
        Mode::CollabFanoutListen => collab_fanout_listen(args).await,
        Mode::CollabFanoutWrite => collab_fanout_write(args).await,
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(HelperError::Expectation(msg)) => {
            eprintln!("rt-hurl-helper: expectation failed: {msg}");
            ExitCode::from(1)
        }
        Err(HelperError::Protocol(msg)) => {
            eprintln!("rt-hurl-helper: protocol error: {msg}");
            ExitCode::from(2)
        }
    }
}

// ════════════════════════════════════════════════════════════════════════════
// Errors
// ════════════════════════════════════════════════════════════════════════════

enum HelperError {
    /// The wire behaved OK but didn't match what the test expected —
    /// e.g. a `subscribed` ack when we expected `denied`, or fewer
    /// events than requested before timeout. Exit 1: test failure.
    Expectation(String),
    /// Something is broken at the transport/JSON layer — connect
    /// refused, malformed frame, TLS handshake failed. Exit 2:
    /// infrastructure problem, not a test result.
    Protocol(String),
}

impl<E: std::fmt::Display> From<E> for HelperError {
    fn from(e: E) -> Self {
        HelperError::Protocol(e.to_string())
    }
}

// ════════════════════════════════════════════════════════════════════════════
// WS connection
// ════════════════════════════════════════════════════════════════════════════

/// Open a WS connection to `url` with the given [`WsAuth`] applied.
///
/// - `Bearer(jwt)` sets `Authorization: Bearer <jwt>` on the upgrade
///   — the programmatic-client path.
/// - `Ticket(uuid)` sets `Sec-WebSocket-Protocol: oxi.ticket.<uuid>`
///   — the browser-equivalent path used by F's smoke scenarios.
///
/// The subprotocol prefix matches
/// `infrastructure::services::rt_ticket_store::SUBPROTOCOL_PREFIX`; kept
/// as a literal here so the test binary has no dependency on the
/// application crate.
async fn connect_ws(
    url: &str,
    auth: &WsAuth,
) -> Result<
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    HelperError,
> {
    let mut req = url
        .into_client_request()
        .map_err(|e| HelperError::Protocol(format!("bad url: {e}")))?;
    match auth {
        WsAuth::Bearer(token) => {
            let bearer = format!("Bearer {token}");
            req.headers_mut().insert(
                "Authorization",
                HeaderValue::from_str(&bearer)
                    .map_err(|e| HelperError::Protocol(format!("bad token: {e}")))?,
            );
        }
        WsAuth::Ticket(ticket) => {
            let subprotocol = format!("oxi.ticket.{ticket}");
            req.headers_mut().insert(
                "Sec-WebSocket-Protocol",
                HeaderValue::from_str(&subprotocol)
                    .map_err(|e| HelperError::Protocol(format!("bad ticket: {e}")))?,
            );
        }
    }
    let (ws, _resp) = tokio_tungstenite::connect_async(req)
        .await
        .map_err(|e| HelperError::Protocol(format!("connect failed: {e}")))?;
    Ok(ws)
}

// ════════════════════════════════════════════════════════════════════════════
// Mode: subscribe-and-collect
// ════════════════════════════════════════════════════════════════════════════

async fn subscribe_and_collect(args: Args) -> Result<(), HelperError> {
    if args.subscribe.is_empty() {
        return Err(HelperError::Protocol(
            "--subscribe required for subscribe-and-collect".into(),
        ));
    }
    let expect_events = args.expect_events.unwrap_or(0);

    let mut ws = connect_ws(&args.url, &args.auth).await?;

    // Subscribe to every requested topic; track pending request ids so
    // we know when all acks have arrived before we start counting
    // events.
    let mut subscribed: Vec<String> = Vec::new();
    let mut pending_subs: std::collections::HashMap<u64, String> = std::collections::HashMap::new();
    for (i, topic) in args.subscribe.iter().enumerate() {
        let req_id = (i as u64) + 1;
        let frame = json!({
            "jsonrpc": "2.0",
            "id": req_id,
            "method": "rt.subscribe",
            "params": { "topic": topic },
        });
        ws.send(Message::Text(frame.to_string().into())).await?;
        pending_subs.insert(req_id, topic.clone());
    }

    let mut events: Vec<Value> = Vec::new();
    // Server-initiated eviction notifications (`rt.revoked`) — captured
    // separately from `rt.event` so scenarios can assert on eviction
    // scoping (evicted topic vs. surviving topic) without conflating
    // them with real content events.
    let mut revoked: Vec<Value> = Vec::new();
    // Count server-initiated protocol Pings so scenarios can assert the
    // keepalive fires. tokio-tungstenite queues an auto-Pong on the next
    // write path, so we don't need to send one ourselves; we just observe
    // the frame.
    let mut pings_received: usize = 0;
    let mut timed_out = false;

    let deadline = tokio::time::Instant::now() + args.timeout;

    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            timed_out = true;
            break;
        }
        // Exit early: all acks received AND enough events collected.
        if pending_subs.is_empty() && events.len() >= expect_events {
            break;
        }

        let msg = match timeout(remaining, ws.next()).await {
            Ok(Some(Ok(m))) => m,
            Ok(Some(Err(e))) => {
                return Err(HelperError::Protocol(format!("ws error: {e}")));
            }
            Ok(None) => {
                return Err(HelperError::Protocol("connection closed by peer".into()));
            }
            Err(_) => {
                timed_out = true;
                break;
            }
        };

        let text = match msg {
            Message::Text(t) => t,
            Message::Ping(_) => {
                // Server-initiated keepalive — observable proof that the
                // interval is firing. tokio-tungstenite queues an
                // auto-Pong on the next flush; nothing to do here.
                pings_received += 1;
                continue;
            }
            _ => continue, // pong/binary/close — not asserted on
        };
        let value: Value = serde_json::from_str(&text)
            .map_err(|e| HelperError::Protocol(format!("bad frame: {e}: {text}")))?;

        // Response to a subscribe request?
        if let Some(id_num) = value.get("id").and_then(|v| v.as_u64()) {
            let topic = pending_subs.remove(&id_num);
            if let Some(err) = value.get("error") {
                return Err(HelperError::Expectation(format!(
                    "subscribe to {} denied: {}",
                    topic.as_deref().unwrap_or("<unknown>"),
                    err,
                )));
            }
            if let Some(topic) = topic {
                subscribed.push(topic);
            }
            // Every requested subscribe is now ack'd — signal the
            // orchestrator that publishes targeted at these topics
            // will land on a live subscriber. See `Args::ready_file`
            // for the race this closes. Empty content is fine; the
            // shell only checks existence, not payload. Errors are
            // logged to stderr but not fatal: the smoke test's
            // `wait_ready` timeout will surface the failure with
            // more context than a mid-run panic here.
            if pending_subs.is_empty()
                && let Some(path) = args.ready_file.as_deref()
                && let Err(e) = std::fs::write(path, b"")
            {
                eprintln!("rt-hurl-helper: could not touch --ready-file {path}: {e}");
            }
            continue;
        }

        // Notification (id-less)?
        let method = value.get("method").and_then(|v| v.as_str()).unwrap_or("");
        match method {
            "rt.event" => {
                if let Some(params) = value.get("params") {
                    events.push(params.clone());
                }
            }
            "rt.revoked" => {
                // Server evicted one of our subscriptions. Record for
                // the shell to assert on; do NOT increment `events` —
                // eviction is orthogonal to content delivery.
                if let Some(params) = value.get("params") {
                    revoked.push(params.clone());
                }
            }
            _ => {
                // Unknown notification method — ignored. `rt.pong` and
                // future server-pushed methods land here silently.
            }
        }
    }

    // Assertion: at least `expect_events` collected before timeout.
    let met = events.len() >= expect_events;

    // Always write output (even on failure) so the shell can diff.
    if let Some(path) = args.output.as_ref() {
        let summary = json!({
            "subscribed": subscribed,
            "events": events,
            "revoked": revoked,
            "pings_received": pings_received,
            "timed_out": timed_out,
        });
        std::fs::write(path, serde_json::to_vec_pretty(&summary).unwrap())
            .map_err(|e| HelperError::Protocol(format!("write output: {e}")))?;
    }

    if !met {
        return Err(HelperError::Expectation(format!(
            "expected {} events, got {} ({}timeout)",
            expect_events,
            events.len(),
            if timed_out { "with " } else { "no " }
        )));
    }
    Ok(())
}

// ════════════════════════════════════════════════════════════════════════════
// Mode: expect-denied
// ════════════════════════════════════════════════════════════════════════════

async fn expect_denied(args: Args) -> Result<(), HelperError> {
    let topic = args
        .subscribe
        .first()
        .ok_or_else(|| HelperError::Protocol("--subscribe required for expect-denied".into()))?
        .clone();

    let mut ws = connect_ws(&args.url, &args.auth).await?;

    let req_id: u64 = 1;
    let frame = json!({
        "jsonrpc": "2.0",
        "id": req_id,
        "method": "rt.subscribe",
        "params": { "topic": topic },
    });
    ws.send(Message::Text(frame.to_string().into())).await?;

    // Wait for the id-matched response with an `error` object. Any
    // notification arriving before the response is skipped — the
    // server should not fan out to a subscription that hasn't been
    // acked yet, but the check is robust to that ordering anyway.
    let deadline = tokio::time::Instant::now() + args.timeout;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Err(HelperError::Expectation(
                "timeout without a subscribe response".into(),
            ));
        }

        let msg = match timeout(remaining, ws.next()).await {
            Ok(Some(Ok(m))) => m,
            Ok(Some(Err(e))) => return Err(HelperError::Protocol(format!("ws error: {e}"))),
            Ok(None) => return Err(HelperError::Protocol("connection closed by peer".into())),
            Err(_) => {
                return Err(HelperError::Expectation(
                    "timeout without a subscribe response".into(),
                ));
            }
        };
        let Message::Text(text) = msg else { continue };
        let value: Value = serde_json::from_str(&text)
            .map_err(|e| HelperError::Protocol(format!("bad frame: {e}: {text}")))?;

        // Match by id.
        let Some(id_num) = value.get("id").and_then(|v| v.as_u64()) else {
            continue;
        };
        if id_num != req_id {
            continue;
        }

        // Expect: error object present.
        let Some(err) = value.get("error") else {
            return Err(HelperError::Expectation(format!(
                "expected `error` object, got: {value}"
            )));
        };
        let message = err.get("message").and_then(|v| v.as_str()).unwrap_or("");
        if let Some(want) = args.reason.as_ref()
            && message != want
        {
            return Err(HelperError::Expectation(format!(
                "expected reason `{want}`, got `{message}` (full error: {err})"
            )));
        }
        return Ok(());
    }
}

// ════════════════════════════════════════════════════════════════════════════
// Mode: collab-sync-probe
// ════════════════════════════════════════════════════════════════════════════
//
// Sends one Yjs sync-step-1 request as a binary frame and asserts the server
// answers with a well-formed sync-step-2 reply on the same file. Exercises
// the full C2 path in one probe: parse the wire header, spawn the actor,
// seed content via the reader, encode `state_as_update_v1(&sv)`, wrap the
// payload back in a 0x03 frame, and ship it to the socket.
//
// Wire format is `collab_wire.rs`:
//   [1 byte kind = 0x03 SYNC][16 bytes file_id BE][payload = state vector]
// The reply carries the same header (kind 0x03, same file_id) and a
// non-empty payload — even for an empty doc, `encode_state_as_update_v1`
// emits the 2-byte "empty update" marker, so a zero-length payload is a
// regression (missing route wiring or the reader failed silently).

async fn collab_sync_probe(args: Args) -> Result<(), HelperError> {
    let file_id = args.file_id.ok_or_else(|| {
        HelperError::Protocol("--file <uuid> required for collab-sync-probe".into())
    })?;

    let mut ws = connect_ws(&args.url, &args.auth).await?;

    // Build sync-step-1: [0x03][file_id 16 bytes][state vector = 0x00].
    //
    // "I know nothing yet" is NOT a zero-length payload — Yjs's
    // `StateVector::encode_v1` starts with a varint count of clients,
    // and an empty state vector encodes to exactly one byte, `0x00`.
    // A zero-length payload here fails `StateVector::decode_v1` with
    // "unexpected end of buffer" and the server tears the socket down
    // (see `collab.protocol_violation` audit event). This one byte is
    // the wire equivalent of the client's "fresh doc, send me
    // everything you have".
    let mut req = Vec::with_capacity(18);
    req.push(0x03); // kind::SYNC — the collab_wire.rs constant, inlined
    req.extend_from_slice(&file_id);
    req.push(0x00); // StateVector::default().encode_v1() == [0x00]
    ws.send(Message::Binary(req.into())).await?;

    // Await the first binary frame within the deadline. Text frames are
    // legal on the same socket (server-initiated `rt.event` or
    // `rt.revoked` notifications, keepalive Pings), so drain them
    // without asserting until a Binary arrives or the timer trips.
    let deadline = tokio::time::Instant::now() + args.timeout;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Err(HelperError::Expectation(
                "timeout without a binary reply on the collab file".into(),
            ));
        }

        let msg = match timeout(remaining, ws.next()).await {
            Ok(Some(Ok(m))) => m,
            Ok(Some(Err(e))) => return Err(HelperError::Protocol(format!("ws error: {e}"))),
            // A close mid-probe is a real regression — the server rejected
            // the frame and tore the socket down. Report as expectation
            // failure so the shell test surfaces the audit line, not as a
            // protocol error (which reads as infra breakage).
            Ok(None) => {
                return Err(HelperError::Expectation(
                    "connection closed before a binary reply arrived".into(),
                ));
            }
            Err(_) => {
                return Err(HelperError::Expectation(
                    "timeout without a binary reply on the collab file".into(),
                ));
            }
        };

        let bytes = match msg {
            Message::Binary(b) => b,
            // Text / Ping / Pong / Close(before-drain) — ignore and keep
            // reading. This tolerates the auto-sub notifications the WS
            // installs at open time and any server keepalive.
            _ => continue,
        };

        // Frame layout: kind(1) + file_id(16) + payload(≥1).
        if bytes.len() < 17 {
            return Err(HelperError::Expectation(format!(
                "reply frame too short: {} bytes (want header + payload)",
                bytes.len()
            )));
        }
        let reply_kind = bytes[0];
        if reply_kind != 0x03 {
            return Err(HelperError::Expectation(format!(
                "reply kind 0x{reply_kind:02x}, want 0x03 (SYNC)"
            )));
        }
        let mut reply_file_id = [0u8; 16];
        reply_file_id.copy_from_slice(&bytes[1..17]);
        if reply_file_id != file_id {
            return Err(HelperError::Expectation(
                "reply file_id does not match request".into(),
            ));
        }
        let payload_len = bytes.len() - 17;
        if payload_len == 0 {
            return Err(HelperError::Expectation(
                "reply payload is empty — sync-step-2 should carry the encoded diff (even the \
                 empty-update marker is 2 bytes)"
                    .into(),
            ));
        }

        if let Some(path) = args.output.as_ref() {
            let summary = json!({
                "kind": reply_kind,
                "file_id_matches": true,
                "payload_len": payload_len,
            });
            std::fs::write(path, serde_json::to_vec_pretty(&summary).unwrap())
                .map_err(|e| HelperError::Protocol(format!("write output: {e}")))?;
        }

        return Ok(());
    }
}

// ════════════════════════════════════════════════════════════════════════════
// Mode: collab-fanout-listen / collab-fanout-write
// ════════════════════════════════════════════════════════════════════════════
//
// These two paired modes exercise the actor's fan-out path: an UPDATE
// applied on one socket must reach every other socket subscribed to
// the same `Topic::Collab(file_id)`. Both first speak JSON-RPC to
// clear the subscribe-time `Read` AuthZ gate — the server's forwarder
// task is what wires the broadcast receiver to the WS out queue, so
// unless you're subscribed you don't receive.

/// JSON-RPC subscribe to a single topic and wait for the id-matched
/// ack. Returns cleanly on `result`, errors on `error`. Shared by
/// both fanout modes so the wire-level handshake is captured in one
/// place.
async fn subscribe_topic(
    ws: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    topic: &str,
    request_deadline: tokio::time::Instant,
) -> Result<(), HelperError> {
    let req_id: u64 = 1;
    let frame = json!({
        "jsonrpc": "2.0",
        "id": req_id,
        "method": "rt.subscribe",
        "params": { "topic": topic },
    });
    ws.send(Message::Text(frame.to_string().into())).await?;

    // Drain non-ack frames (server-initiated notifications, pings,
    // binary events) until we see the ack keyed on `id`.
    loop {
        let remaining =
            request_deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Err(HelperError::Expectation(format!(
                "timeout waiting for subscribe ack on {topic}"
            )));
        }
        let msg = match timeout(remaining, ws.next()).await {
            Ok(Some(Ok(m))) => m,
            Ok(Some(Err(e))) => return Err(HelperError::Protocol(format!("ws error: {e}"))),
            Ok(None) => return Err(HelperError::Protocol("connection closed by peer".into())),
            Err(_) => {
                return Err(HelperError::Expectation(format!(
                    "timeout waiting for subscribe ack on {topic}"
                )));
            }
        };
        let Message::Text(text) = msg else { continue };
        let value: Value = serde_json::from_str(&text)
            .map_err(|e| HelperError::Protocol(format!("bad frame: {e}: {text}")))?;
        let Some(id_num) = value.get("id").and_then(|v| v.as_u64()) else {
            continue;
        };
        if id_num != req_id {
            continue;
        }
        if let Some(err) = value.get("error") {
            return Err(HelperError::Expectation(format!(
                "subscribe denied: {err}"
            )));
        }
        return Ok(());
    }
}

async fn collab_fanout_listen(args: Args) -> Result<(), HelperError> {
    let file_id = args.file_id.ok_or_else(|| {
        HelperError::Protocol("--file <uuid> required for collab-fanout-listen".into())
    })?;
    let expected = args.expect_content.clone().ok_or_else(|| {
        HelperError::Protocol("--expect-content <string> required for collab-fanout-listen".into())
    })?;

    let mut ws = connect_ws(&args.url, &args.auth).await?;
    let deadline = tokio::time::Instant::now() + args.timeout;

    // 1. Clear the subscribe-time AuthZ gate. Without this, no
    //    forwarder is installed on the server side and the broadcast
    //    never reaches this socket.
    let topic = format!("collab:{}", uuid_bytes_to_dashed(&file_id));
    subscribe_topic(&mut ws, &topic, deadline).await?;

    // 2. Touch the ready-file so the orchestrator knows to fire the
    //    paired writer. Same convention as `subscribe-and-collect`.
    if let Some(path) = args.ready_file.as_deref()
        && let Err(e) = std::fs::write(path, b"")
    {
        eprintln!("rt-hurl-helper: could not touch --ready-file {path}: {e}");
    }

    // 3. Wait for a binary `0x01` UPDATE frame keyed on our file_id.
    //    Text frames on this socket during this window would be
    //    unrelated notifications; drain and ignore.
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Err(HelperError::Expectation(format!(
                "timeout waiting for a 0x01 fan-out frame on collab:{}",
                uuid_bytes_to_dashed(&file_id)
            )));
        }
        let msg = match timeout(remaining, ws.next()).await {
            Ok(Some(Ok(m))) => m,
            Ok(Some(Err(e))) => return Err(HelperError::Protocol(format!("ws error: {e}"))),
            Ok(None) => {
                return Err(HelperError::Expectation(
                    "connection closed before a fan-out frame arrived".into(),
                ));
            }
            Err(_) => {
                return Err(HelperError::Expectation(
                    "timeout waiting for a 0x01 fan-out frame".into(),
                ));
            }
        };
        let bytes = match msg {
            Message::Binary(b) => b,
            _ => continue,
        };
        if bytes.len() < 17 {
            return Err(HelperError::Expectation(format!(
                "fan-out frame too short: {} bytes",
                bytes.len()
            )));
        }
        if bytes[0] != 0x01 {
            // Not an UPDATE — could be a SYNC reply from an unrelated
            // in-flight request; ignore and keep waiting.
            continue;
        }
        let mut reply_file_id = [0u8; 16];
        reply_file_id.copy_from_slice(&bytes[1..17]);
        if reply_file_id != file_id {
            continue;
        }
        // Decode the payload as a Yjs update and apply to a fresh Doc
        // to extract the resulting text. This is the strongest
        // assertion we can make at the wire level — a lax "payload
        // non-empty" check would miss a fan-out that broadcasts the
        // wrong bytes.
        let update = Update::decode_v1(&bytes[17..]).map_err(|e| {
            HelperError::Expectation(format!("payload is not a valid Yjs update: {e}"))
        })?;
        let doc = Doc::new();
        {
            let mut txn = doc.transact_mut();
            txn.apply_update(update).map_err(|e| {
                HelperError::Expectation(format!("apply_update failed: {e}"))
            })?;
        }
        let text_ref = doc.get_or_insert_text("content");
        let got = text_ref.get_string(&doc.transact());
        if got != expected {
            return Err(HelperError::Expectation(format!(
                "fan-out content mismatch: got {got:?}, want {expected:?}"
            )));
        }
        return Ok(());
    }
}

async fn collab_fanout_write(args: Args) -> Result<(), HelperError> {
    let file_id = args.file_id.ok_or_else(|| {
        HelperError::Protocol("--file <uuid> required for collab-fanout-write".into())
    })?;
    let content = args.content.clone().ok_or_else(|| {
        HelperError::Protocol("--content <string> required for collab-fanout-write".into())
    })?;

    let mut ws = connect_ws(&args.url, &args.auth).await?;
    let deadline = tokio::time::Instant::now() + args.timeout;

    // 1. Subscribe: clears the Read gate. Without this the server
    //    accepts the binary frame (the router doesn't require a
    //    subscribe) but the write-side integration keeps the two
    //    handshakes together — it's what the frontend will do.
    let topic = format!("collab:{}", uuid_bytes_to_dashed(&file_id));
    subscribe_topic(&mut ws, &topic, deadline).await?;

    // 2. Build a Yjs UPDATE that inserts `content` at position 0 of
    //    an otherwise-empty Doc. The client Doc is local to this
    //    process; the server's actor has its own Doc and will apply
    //    the incoming update against it. Origin skip is intentionally
    //    not filtered — this sender will also see the update come
    //    back through its own broadcast subscription (idempotent).
    let client_doc = Doc::new();
    {
        let text_ref = client_doc.get_or_insert_text("content");
        let mut txn = client_doc.transact_mut();
        text_ref.insert(&mut txn, 0, &content);
    }
    let update_bytes = client_doc
        .transact()
        .encode_state_as_update_v1(&StateVector::default());

    // 3. Wrap in the collab wire header: [0x01][file_id][update].
    let mut frame = Vec::with_capacity(17 + update_bytes.len());
    frame.push(0x01);
    frame.extend_from_slice(&file_id);
    frame.extend_from_slice(&update_bytes);
    ws.send(Message::Binary(frame.into())).await?;

    // 4. Best-effort clean close so the server's forwarder tears
    //    down promptly, not on TCP timeout.
    let _ = ws.close(None).await;
    Ok(())
}

/// Format 16 raw UUID bytes as canonical dashed hex — inverse of
/// `parse_uuid_bytes`. Used to build the JSON-RPC topic string from
/// the same file_id bytes the binary frame carries. Dependency-free
/// like its inverse.
fn uuid_bytes_to_dashed(b: &[u8; 16]) -> String {
    let mut out = String::with_capacity(36);
    for (i, byte) in b.iter().enumerate() {
        if matches!(i, 4 | 6 | 8 | 10) {
            out.push('-');
        }
        out.push_str(&format!("{byte:02x}"));
    }
    out
}
