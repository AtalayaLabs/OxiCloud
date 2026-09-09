//! Realtime bus WebSocket handler — the endpoint every WS session
//! multiplexes over. See `docs/plan/message-bus.md § Wire protocol`.
//!
//! # Wire
//!
//! JSON-RPC 2.0 for control + events (text frames). Binary frames are
//! reserved for the Yjs sync protocol (collab editor, Phase A follow-up)
//! and are IGNORED in MVP.
//!
//! Methods accepted in MVP:
//! - `rt.subscribe { topic }` → `{ subscribed: "<topic>" }` or JSON-RPC error.
//! - `rt.unsubscribe { topic }` → `{ unsubscribed: "<topic>" }`.
//! - `rt.ping` → `{ pong: true }`.
//!
//! Server-initiated notifications:
//! - `rt.event { topic, event, data, actor, ts }` — an event published to
//!   a topic the caller is subscribed to.
//!
//! # Auth
//!
//! Route sits under `protected_api` (see `src/interfaces/api/routes.rs`)
//! so `auth_middleware` runs first. Cookie AND `Authorization: Bearer`
//! paths both produce a `CurrentUserId` extension the handler extracts.
//! Browser-side subprotocol bearer (`Sec-WebSocket-Protocol:
//! authorization.bearer.<jwt>`) is a Phase-A follow-up — the MVP relies
//! on the Authorization header, which programmatic clients (the
//! `rt-hurl-helper` smoke test) set directly.
//!
//! # Limits
//!
//! Per-connection outbound `mpsc::Sender` bounded to 512 — full → close.
//! Per-connection subscription cap 128. Frame-size cap not enforced in
//! MVP; the underlying tokio-tungstenite default is 64 MiB which is more
//! than adequate for JSON-RPC control traffic.

use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::Response;
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use uuid::Uuid;

use crate::application::ports::authorization_ports::AuthorizationEngine;
use crate::application::ports::realtime_ports::{
    AuthzCheck, BusResource, ParseTopicErr, RealtimeBus, RealtimeEvent, Topic, error_code,
};
use crate::common::di::AppState;
use crate::domain::services::authorization::{Permission, Resource, Subject};
use crate::interfaces::middleware::auth::CurrentUserId;

/// Max simultaneous subscriptions on a single WS session. Beyond this the
/// server responds `-32005 sub_limit` and the client is expected to
/// unsubscribe before subscribing to another topic.
const MAX_SUBSCRIPTIONS_PER_CONNECTION: usize = 128;

/// Outbound mpsc capacity per session. When full → we close the WS
/// (client reconnects, refetches). Sized so a subscriber blocked on the
/// socket layer doesn't back-pressure into the bus's broadcast ring.
const OUTBOUND_CHANNEL_CAPACITY: usize = 512;

// ════════════════════════════════════════════════════════════════════════════
// JSON-RPC 2.0 envelope types
// ════════════════════════════════════════════════════════════════════════════

/// Marker constant for the `jsonrpc` field.
const JSONRPC_V2: &str = "2.0";

/// Inbound JSON-RPC envelope — deserialize-tolerant so a client can
/// send `rt.ping` without `params`, or a notification without an `id`.
///
/// Response/notification serialization uses the more strongly-typed
/// [`RpcResponse`] and [`RpcNotification`] types below.
#[derive(Debug, Deserialize)]
struct RpcRequest {
    #[serde(rename = "jsonrpc")]
    _jsonrpc: Option<String>,
    /// `id` is `None` for notifications (which the client-side of MVP
    /// never sends). We accept it in the shape but do not treat missing
    /// `id` as a permitted request — every `rt.*` method requires
    /// `id`-correlated responses in MVP.
    id: Option<Value>,
    method: Option<String>,
    #[serde(default)]
    params: Value,
}

/// Server → client response for a request (both success and error use
/// this shape; exactly one of `result`/`error` is populated per spec).
#[derive(Debug, Serialize)]
struct RpcResponse<'a> {
    jsonrpc: &'static str,
    id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<RpcError<'a>>,
}

#[derive(Debug, Serialize)]
struct RpcError<'a> {
    code: i32,
    message: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<Value>,
}

/// Server → client notification (id-less). Emitted for `rt.event` (bus
/// fan-out) and `rt.revoked` (subscription eviction).
#[derive(Debug, Serialize)]
struct RpcNotification<'a> {
    jsonrpc: &'static str,
    method: &'a str,
    params: Value,
}

// ════════════════════════════════════════════════════════════════════════════
// Handler entrypoint
// ════════════════════════════════════════════════════════════════════════════

/// `GET /api/rt/ws` — WS upgrade handler. Sits under `protected_api` so
/// [`CurrentUserId`] resolves against a valid session before we reach
/// `on_upgrade`.
///
/// Returns whatever `WebSocketUpgrade::on_upgrade` produces (an HTTP 101
/// Switching Protocols with the WebSocket handshake headers).
pub async fn rt_ws_handler(
    ws: WebSocketUpgrade,
    CurrentUserId(caller_id): CurrentUserId,
    State(state): State<Arc<AppState>>,
) -> Response {
    ws.on_upgrade(move |socket| handle_session(socket, caller_id, state))
}

// ════════════════════════════════════════════════════════════════════════════
// Session loop
// ════════════════════════════════════════════════════════════════════════════

/// A per-topic subscription: the join handle for the reader task that
/// drains the bus stream into `out_tx`. Dropping does NOT abort a spawned
/// tokio task — we must call `.abort()` explicitly on unsubscribe.
struct Sub {
    reader: JoinHandle<()>,
}

impl Drop for Sub {
    fn drop(&mut self) {
        // Belt-and-braces: if `remove()` bypasses `.abort()` for some
        // future call path, dropping still stops the reader.
        self.reader.abort();
    }
}

async fn handle_session(mut socket: WebSocket, caller_id: Uuid, state: Arc<AppState>) {
    // Outbound queue — every path that produces a text frame for the
    // client enqueues here; the writer half of the select drains.
    let (out_tx, mut out_rx) = mpsc::channel::<String>(OUTBOUND_CHANNEL_CAPACITY);

    // Active subscriptions on this session. Keyed by the wire-form topic
    // string so an incoming `rt.unsubscribe` with the same string is
    // recognised without re-parsing.
    let mut subs: HashMap<String, Sub> = HashMap::new();

    loop {
        tokio::select! {
            // biased: process outbound before inbound so an event burst
            // doesn't get overtaken by a control-frame handshake.
            biased;

            outbound = out_rx.recv() => {
                match outbound {
                    Some(text) => {
                        if socket.send(Message::Text(text.into())).await.is_err() {
                            break;
                        }
                    }
                    None => break, // out_tx dropped — unreachable but safe
                }
            }

            incoming = socket.recv() => {
                match incoming {
                    Some(Ok(Message::Text(txt))) => {
                        if let Some(reply) =
                            handle_text_frame(&txt, caller_id, &state, &mut subs, &out_tx).await
                            && socket.send(Message::Text(reply.into())).await.is_err() {
                                break;
                            }
                    }
                    Some(Ok(Message::Binary(_))) => {
                        // Reserved for Yjs sync protocol frames (collab
                        // editor, Phase A follow-up). Silently ignored in
                        // MVP so a future client that speaks binary
                        // frames on the same connection isn't rejected.
                    }
                    Some(Ok(Message::Ping(_) | Message::Pong(_))) => {
                        // Handled by axum's WebSocket state machine.
                    }
                    Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                }
            }
        }
    }

    // Session cleanup: abort every subscription reader task.
    subs.clear();
}

// ════════════════════════════════════════════════════════════════════════════
// Frame handling
// ════════════════════════════════════════════════════════════════════════════

/// Parse one inbound text frame as a JSON-RPC 2.0 request and dispatch
/// it. Returns the response string to send back (empty option if the
/// dispatch already enqueued via `out_tx`).
async fn handle_text_frame(
    text: &str,
    caller_id: Uuid,
    state: &Arc<AppState>,
    subs: &mut HashMap<String, Sub>,
    out_tx: &mpsc::Sender<String>,
) -> Option<String> {
    // Parse envelope. On malformed JSON: reply with an id-less error per
    // JSON-RPC 2.0 (id = null when the request couldn't be parsed).
    let req: RpcRequest = match serde_json::from_str(text) {
        Ok(r) => r,
        Err(_) => {
            return Some(error_response(
                Value::Null,
                error_code::INVALID_REQUEST,
                "invalid_request",
                None,
            ));
        }
    };

    let id = req.id.unwrap_or(Value::Null);
    let Some(method) = req.method else {
        return Some(error_response(
            id,
            error_code::INVALID_REQUEST,
            "invalid_request",
            None,
        ));
    };

    match method.as_str() {
        "rt.subscribe" => {
            Some(handle_subscribe(id, req.params, caller_id, state, subs, out_tx).await)
        }
        "rt.unsubscribe" => Some(handle_unsubscribe(id, req.params, subs)),
        "rt.ping" => Some(success_response(id, serde_json::json!({ "pong": true }))),
        _ => Some(error_response(
            id,
            error_code::METHOD_NOT_FOUND,
            "method_not_found",
            Some(serde_json::json!({ "method": method })),
        )),
    }
}

async fn handle_subscribe(
    id: Value,
    params: Value,
    caller_id: Uuid,
    state: &Arc<AppState>,
    subs: &mut HashMap<String, Sub>,
    out_tx: &mpsc::Sender<String>,
) -> String {
    // Extract topic.
    let topic_str = match params.get("topic").and_then(Value::as_str) {
        Some(s) => s.to_owned(),
        None => {
            return error_response(
                id,
                error_code::INVALID_PARAMS,
                "invalid_params",
                Some(serde_json::json!({ "missing": "topic" })),
            );
        }
    };

    // Guard against runaway subscribers pinning server memory.
    if subs.len() >= MAX_SUBSCRIPTIONS_PER_CONNECTION && !subs.contains_key(&topic_str) {
        audit_denied(caller_id, &topic_str, "sub_limit");
        return error_response(
            id,
            error_code::SUB_LIMIT,
            "sub_limit",
            Some(serde_json::json!({ "topic": topic_str })),
        );
    }

    // Idempotent: re-subscribing to an already-active topic acks with
    // no side effects. Client reconnect logic can replay its topic set
    // without dedup.
    if subs.contains_key(&topic_str) {
        return success_response(id, serde_json::json!({ "subscribed": topic_str }));
    }

    // Parse topic.
    let topic = match Topic::parse(&topic_str) {
        Ok(t) => t,
        Err(ParseTopicErr::BadUuid) | Err(ParseTopicErr::Unknown) => {
            // Both parse failures collapse to `topic_forbidden` on the
            // wire — the caller cannot distinguish "unknown shape" from
            // "shape known but resource doesn't exist" without hinting
            // an enumeration oracle.
            audit_denied(caller_id, &topic_str, "unknown_topic");
            return error_response(
                id,
                error_code::TOPIC_FORBIDDEN,
                "topic_forbidden",
                Some(serde_json::json!({ "topic": topic_str })),
            );
        }
    };

    // AuthZ dispatch — one match arm per gate class. Adding a new topic
    // variant with a new gate shape is a compile error here.
    match topic.required_perm() {
        AuthzCheck::ResourceRead { resource } => {
            let domain_resource = match resource {
                BusResource::Folder(uuid) => Resource::Folder(uuid),
            };
            if state
                .authorization
                .require(Subject::User(caller_id), Permission::Read, domain_resource)
                .await
                .is_err()
            {
                // Anti-enum: "no such resource" and "no read" collapse
                // to the same wire code. Audit records the truth.
                audit_denied(caller_id, &topic_str, "no_read");
                return error_response(
                    id,
                    error_code::NO_READ,
                    "no_read",
                    Some(serde_json::json!({ "topic": topic_str })),
                );
            }
        }
        AuthzCheck::IdentityMatch { user_id } => {
            if user_id != caller_id {
                audit_denied(caller_id, &topic_str, "identity_mismatch");
                return error_response(
                    id,
                    error_code::TOPIC_FORBIDDEN,
                    "topic_forbidden",
                    Some(serde_json::json!({ "topic": topic_str })),
                );
            }
        }
    }

    // AuthZ passed — install the subscription and spawn a reader task
    // that forwards bus events to the outbound channel as `rt.event`
    // notifications.
    let stream = RealtimeBus::subscribe(state.bus.as_ref(), &topic);
    let topic_wire = topic_str.clone();
    let out_tx_task = out_tx.clone();
    let reader = tokio::spawn(async move {
        let mut stream = stream;
        while let Some(event) = stream.next().await {
            let notification = event_notification(&topic_wire, &event);
            if out_tx_task.send(notification).await.is_err() {
                // Session's outbound channel closed — receiver dropped.
                break;
            }
        }
    });
    subs.insert(topic_str.clone(), Sub { reader });

    success_response(id, serde_json::json!({ "subscribed": topic_str }))
}

fn handle_unsubscribe(id: Value, params: Value, subs: &mut HashMap<String, Sub>) -> String {
    let Some(topic_str) = params.get("topic").and_then(Value::as_str) else {
        return error_response(
            id,
            error_code::INVALID_PARAMS,
            "invalid_params",
            Some(serde_json::json!({ "missing": "topic" })),
        );
    };
    // Idempotent: removing a topic the session isn't subscribed to is
    // still a success ack, per plan.
    subs.remove(topic_str);
    success_response(id, serde_json::json!({ "unsubscribed": topic_str }))
}

// ════════════════════════════════════════════════════════════════════════════
// Envelope helpers
// ════════════════════════════════════════════════════════════════════════════

fn success_response(id: Value, result: Value) -> String {
    serde_json::to_string(&RpcResponse {
        jsonrpc: JSONRPC_V2,
        id,
        result: Some(result),
        error: None,
    })
    .expect("RpcResponse always serializes")
}

fn error_response(id: Value, code: i32, message: &str, data: Option<Value>) -> String {
    serde_json::to_string(&RpcResponse {
        jsonrpc: JSONRPC_V2,
        id,
        result: None,
        error: Some(RpcError {
            code,
            message,
            data,
        }),
    })
    .expect("RpcResponse always serializes")
}

/// Build an `rt.event` JSON-RPC notification for a bus event.
///
/// Payload discipline (see plan): thin facts only. The `RealtimeEvent`'s
/// own `#[serde(tag = "event")]` shape provides `event` + variant fields
/// under one flat object; we lift them into `params.data` alongside a
/// `topic` selector for the client.
fn event_notification(topic_wire: &str, event: &RealtimeEvent) -> String {
    // Serialize the event to extract `event` (discriminator) and the
    // remaining fields as `data`. Two-step to avoid re-inventing the
    // enum's discriminator string here.
    let event_json = serde_json::to_value(event).expect("RealtimeEvent always serializes");
    let (event_name, data) = split_event_discriminator(event_json);

    let params = serde_json::json!({
        "topic": topic_wire,
        "event": event_name,
        "data":  data,
    });

    serde_json::to_string(&RpcNotification {
        jsonrpc: JSONRPC_V2,
        method: "rt.event",
        params,
    })
    .expect("RpcNotification always serializes")
}

/// Given a `RealtimeEvent` serialised as `{ "event": "file_created", ...rest }`,
/// split into `(event_name, rest)`. Falls back to `("unknown", full)` if
/// the shape doesn't match (defensive — shouldn't happen given the enum
/// derive, but a future untagged variant would land here).
fn split_event_discriminator(mut event_json: Value) -> (String, Value) {
    if let Some(obj) = event_json.as_object_mut()
        && let Some(Value::String(name)) = obj.remove("event")
    {
        return (name, Value::Object(obj.clone()));
    }
    ("unknown".to_owned(), event_json)
}

// ════════════════════════════════════════════════════════════════════════════
// Audit
// ════════════════════════════════════════════════════════════════════════════

fn audit_denied(caller_id: Uuid, topic: &str, reason: &'static str) {
    tracing::info!(
        target: "audit",
        event = "realtime.subscribe_denied",
        reason = reason,
        caller_id = %caller_id,
        topic = %topic,
        "👮🏻‍♂️ realtime subscribe rejected",
    );
}

// ════════════════════════════════════════════════════════════════════════════
// Tests
// ════════════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn success_response_shape() {
        let s = success_response(
            Value::Number(42.into()),
            serde_json::json!({ "subscribed": "folder:x" }),
        );
        let v: Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["jsonrpc"], "2.0");
        assert_eq!(v["id"], 42);
        assert_eq!(v["result"]["subscribed"], "folder:x");
        assert!(v.get("error").is_none());
    }

    #[test]
    fn error_response_shape() {
        let s = error_response(
            Value::Number(7.into()),
            error_code::NO_READ,
            "no_read",
            Some(serde_json::json!({ "topic": "folder:x" })),
        );
        let v: Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["jsonrpc"], "2.0");
        assert_eq!(v["id"], 7);
        assert_eq!(v["error"]["code"], error_code::NO_READ);
        assert_eq!(v["error"]["message"], "no_read");
        assert_eq!(v["error"]["data"]["topic"], "folder:x");
        assert!(v.get("result").is_none());
    }

    #[test]
    fn event_notification_shape() {
        let event = RealtimeEvent::FileCreated {
            file_id: Uuid::nil(),
            name: "notes.md".into(),
            parent_id: Uuid::nil(),
            actor: Uuid::nil(),
        };
        let s = event_notification("folder:abc", &event);
        let v: Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["jsonrpc"], "2.0");
        assert_eq!(v["method"], "rt.event");
        assert_eq!(v["params"]["topic"], "folder:abc");
        assert_eq!(v["params"]["event"], "file_created");
        assert_eq!(v["params"]["data"]["name"], "notes.md");
        // The discriminator field must have been lifted OUT of `data` —
        // otherwise the client sees `data.event` alongside the
        // top-level `event`, which is confusing and violates the plan's
        // wire shape.
        assert!(v["params"]["data"].get("event").is_none());
    }

    #[test]
    fn split_event_discriminator_extracts_and_removes() {
        let input = serde_json::json!({
            "event": "file_created",
            "file_id": "00000000-0000-0000-0000-000000000000",
            "name": "x",
        });
        let (name, rest) = split_event_discriminator(input);
        assert_eq!(name, "file_created");
        assert!(rest.get("event").is_none());
        assert_eq!(rest["name"], "x");
    }
}
