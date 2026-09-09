//! WebSocket-side smoke-test helper for the realtime message bus.
//!
//! Hurl is HTTP-only — it can't do a WS upgrade, let alone read frames
//! for later assertion. This binary is the WS half of the smoke test:
//! opens `/api/rt/ws`, speaks JSON-RPC 2.0, and either collects events
//! into a JSON file for shell assertions (`subscribe-and-collect`) or
//! validates that an authz-denied subscribe returns the expected wire
//! error code (`expect-denied`).
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

// ════════════════════════════════════════════════════════════════════════════
// CLI parsing (minimal, dependency-free)
// ════════════════════════════════════════════════════════════════════════════

struct Args {
    mode: Mode,
    url: String,
    token: String,
    subscribe: Vec<String>,
    expect_events: Option<usize>,
    reason: Option<String>,
    timeout: Duration,
    output: Option<String>,
}

enum Mode {
    SubscribeAndCollect,
    ExpectDenied,
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
        Some(other) => return Err(format!("unknown mode: {other}")),
        None => return Err("mode is required".into()),
    };

    let mut url = None;
    let mut token = None;
    let mut subscribe = Vec::new();
    let mut expect_events = None;
    let mut reason = None;
    let mut timeout = Duration::from_secs(3);
    let mut output = None;

    while let Some(flag) = it.next() {
        let value = it
            .next()
            .ok_or_else(|| format!("flag {flag} requires a value"))?;
        match flag.as_str() {
            "--url" => url = Some(value),
            "--token" => token = Some(value),
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
            other => return Err(format!("unknown flag: {other}")),
        }
    }

    Ok(Args {
        mode,
        url: url.ok_or("--url required")?,
        token: token.ok_or("--token required")?,
        subscribe,
        expect_events,
        reason,
        timeout,
        output,
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

/// Open a WS connection to `url` with the given bearer token attached
/// via `Authorization: Bearer <jwt>`. Programmatic client — this is the
/// path native clients (this helper, future sync-client integrations)
/// take. Browser clients that can't set the header will use the
/// `Sec-WebSocket-Protocol` subprotocol fallback (Phase A follow-up).
async fn connect_ws(
    url: &str,
    token: &str,
) -> Result<
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    HelperError,
> {
    let mut req = url
        .into_client_request()
        .map_err(|e| HelperError::Protocol(format!("bad url: {e}")))?;
    let bearer = format!("Bearer {token}");
    req.headers_mut().insert(
        "Authorization",
        HeaderValue::from_str(&bearer)
            .map_err(|e| HelperError::Protocol(format!("bad token: {e}")))?,
    );
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

    let mut ws = connect_ws(&args.url, &args.token).await?;

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
            continue;
        }

        // Notification (id-less)?
        let method = value.get("method").and_then(|v| v.as_str()).unwrap_or("");
        if method == "rt.event"
            && let Some(params) = value.get("params")
        {
            events.push(params.clone());
        }
        // Other notifications (`rt.revoked`, `rt.pong`) — ignored for
        // subscribe-and-collect. They can be added to the output
        // schema when scenarios need them.
    }

    // Assertion: at least `expect_events` collected before timeout.
    let met = events.len() >= expect_events;

    // Always write output (even on failure) so the shell can diff.
    if let Some(path) = args.output.as_ref() {
        let summary = json!({
            "subscribed": subscribed,
            "events": events,
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

    let mut ws = connect_ws(&args.url, &args.token).await?;

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
