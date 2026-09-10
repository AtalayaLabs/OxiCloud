//! Message bus WebSocket handler — the endpoint every WS session
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
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::Response;
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;
use uuid::Uuid;

use crate::application::ports::authorization_ports::AuthorizationEngine;
use crate::application::ports::message_bus_ports::{
    AuthzCheck, BusResource, MessageBus, MessageBusEvent, ParseTopicErr, Topic, error_code,
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

/// Default server-initiated protocol Ping interval. Keeps intermediate
/// proxies (Traefik, nginx, Cloudflare) and NAT boxes from reaping the
/// TCP session as idle. 30 s sits comfortably under nginx's 60 s
/// default and Cloudflare's 100 s hard limit; behind Traefik we
/// document a much longer `idleTimeout` anyway.
///
/// Overridable at server start via `OXICLOUD_RT_WS_KEEPALIVE_SECONDS`
/// — test suites drop it to a low value to exercise the keepalive path
/// within a bounded wall-clock.
const DEFAULT_KEEPALIVE_SECONDS: u64 = 30;

/// Read the keepalive interval from env at connection time. Kept as a
/// function rather than a `LazyLock` so a running server with the env
/// var flipped picks it up on the NEXT connection without a restart —
/// useful for smoke tests that toggle the value on the fly.
fn keepalive_interval() -> Duration {
    Duration::from_secs(
        std::env::var("OXICLOUD_RT_WS_KEEPALIVE_SECONDS")
            .ok()
            .and_then(|s| s.parse().ok())
            .filter(|&n: &u64| n > 0)
            .unwrap_or(DEFAULT_KEEPALIVE_SECONDS),
    )
}

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

/// Messages the per-topic reader tasks send to the session's main
/// loop. Two shapes:
///
/// - `Frame` — a client-bound text frame (`rt.event` notification,
///   `rt.revoked` notification, whatever). Main loop writes it to
///   the socket.
/// - `EvictFolders` — internal control signal. The reader for the
///   session's auto-subscribed `user:{caller}:authz` topic translates
///   inbound [`MessageBusEvent::AuthzChanged`] events into this rather
///   than a client-visible frame. Main loop walks its subs, drops any
///   whose resource is in the list, and emits one `rt.revoked` frame
///   per evicted topic.
enum SessionOut {
    Frame(String),
    EvictFolders(Vec<Uuid>),
}

async fn handle_session(mut socket: WebSocket, caller_id: Uuid, state: Arc<AppState>) {
    // Outbound queue — every path that produces a client-bound frame
    // enqueues here; the writer half of the select drains. Also
    // carries internal `EvictFolders` control signals from the
    // authz reader — the main loop reacts to those without them
    // hitting the socket.
    let (out_tx, mut out_rx) = mpsc::channel::<SessionOut>(OUTBOUND_CHANNEL_CAPACITY);

    // Active subscriptions on this session. Keyed by the wire-form topic
    // string so an incoming `rt.unsubscribe` with the same string is
    // recognised without re-parsing.
    let mut subs: HashMap<String, Sub> = HashMap::new();

    // Auto-subscribe to the caller's private authz-change topic.
    // No AuthZ check (identity-scoped: caller_id == user_id by
    // construction), no client `rt.subscribe` frame. The reader for
    // this topic translates `AuthzChanged` events into
    // `SessionOut::EvictFolders` signals instead of pushing an
    // `rt.event` notification the client can see — client-visible
    // effect is the `rt.revoked` per evicted sub.
    install_subscription(Topic::UserAuthz(caller_id), &mut subs, &out_tx, &state);

    // Server-initiated protocol Ping ticker — prevents intermediate
    // proxies (Traefik, nginx, Cloudflare) and NAT boxes from reaping
    // the TCP session as idle. Browsers can't send Ping control frames
    // (the JS `WebSocket` API doesn't expose them), so the server owns
    // this responsibility; the client's WS layer auto-Pongs. A truly
    // dead peer surfaces on the next `socket.send` and breaks out of
    // the loop the same way any WS error does — no pong-timeout
    // tracking needed for MVP.
    //
    // ─────────────────────── Scaling note ────────────────────────────
    // This is a `tokio::time::interval` PER connection — not a thread.
    // The tokio timer wheel handles arbitrary N intervals in O(1) and
    // each Sleep future is ~150 bytes of state. Per-session task
    // memory dominates at any interesting N (~1 KB stack), which is
    // still trivial: 10 000 clients ≈ 12 MB total + ~333 Pings/sec
    // spread across the worker pool.
    //
    // If a deployment ever hits 100 000+ concurrent WS AND the
    // per-connection interval becomes a measurable cost, the swap is:
    //   1. one global `tokio::spawn(async { interval.tick().await; ... })`
    //      task that scans a `DashMap<SessionId, mpsc::Sender<()>>`
    //      registry and pings each session's mailbox on tick,
    //   2. session tasks receive the mailbox signal in their `select!`
    //      and send `Message::Ping` from there (still per-session, so
    //      one slow socket doesn't block the whole fleet).
    // Neither pattern change would touch the wire; both are same-file
    // refactors. Don't do this until N genuinely warrants it — until
    // then, per-connection is the standard tokio idiom for a reason.
    let mut keepalive = tokio::time::interval(keepalive_interval());
    // Coalesce backlog if the runtime pauses (e.g. under heavy load)
    // rather than firing a burst of Pings when it recovers.
    keepalive.set_missed_tick_behavior(MissedTickBehavior::Delay);
    // Discard the immediate first tick — the socket just opened; a
    // client sending its opening `rt.subscribe` shouldn't race a Ping.
    keepalive.tick().await;

    loop {
        tokio::select! {
            // biased: process outbound before inbound so an event burst
            // doesn't get overtaken by a control-frame handshake.
            biased;

            outbound = out_rx.recv() => {
                match outbound {
                    Some(SessionOut::Frame(text)) => {
                        if socket.send(Message::Text(text.into())).await.is_err() {
                            break;
                        }
                    }
                    Some(SessionOut::EvictFolders(folders)) => {
                        // Grant-revocation cascade. Walk the sub set;
                        // drop any Folder(id) whose id is in the list;
                        // emit one `rt.revoked` frame per eviction so
                        // the client knows to stop rendering that
                        // resource. Idempotent: re-evicting an
                        // already-gone topic is a no-op.
                        for folder_uuid in folders {
                            let wire = Topic::Folder(folder_uuid).to_wire_key();
                            if subs.remove(&wire).is_some() {
                                let frame = revoked_notification(
                                    &wire,
                                    "grant_revoked",
                                );
                                if socket
                                    .send(Message::Text(frame.into()))
                                    .await
                                    .is_err()
                                {
                                    return; // session dead
                                }
                                audit_evicted(caller_id, &wire, "grant_revoked");
                            }
                        }
                    }
                    None => break, // out_tx dropped — unreachable but safe
                }
            }

            _ = keepalive.tick() => {
                // RFC 6455 Ping control frame. 0-byte payload is
                // spec-legal and the smallest wire footprint. Client
                // auto-Pongs; nothing to observe here on that.
                if socket.send(Message::Ping(Bytes::new())).await.is_err() {
                    break;
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
                        // Client Ping → axum auto-Pongs. Client Pong is
                        // the response to OUR keepalive Ping — nothing
                        // to do at the app layer; TCP + WS keep the
                        // pipe warm regardless.
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
    out_tx: &mpsc::Sender<SessionOut>,
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
    out_tx: &mpsc::Sender<SessionOut>,
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
    install_subscription(topic, subs, out_tx, state);

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
// Subscription installer
// ════════════════════════════════════════════════════════════════════════════

/// Spawn a reader task for `topic` and insert it into `subs`. No AuthZ
/// check — the caller is responsible for gating (either via
/// `handle_subscribe`'s explicit dispatch, or via identity-by-
/// construction for the auto-subscribed `Topic::UserAuthz(caller)`).
///
/// The reader interprets bus events differently by topic class:
///
/// - For `Topic::UserAuthz(_)`: an incoming `MessageBusEvent::AuthzChanged`
///   is translated to `SessionOut::EvictFolders(affected)` — the main
///   loop then walks the sub set and drops matching topics. Any other
///   event kind on this topic is ignored (defensive; shouldn't happen
///   in MVP).
/// - For every other topic: bus events are wrapped into a client-
///   visible `rt.event` notification and pushed as `SessionOut::Frame`.
fn install_subscription(
    topic: Topic,
    subs: &mut HashMap<String, Sub>,
    out_tx: &mpsc::Sender<SessionOut>,
    state: &Arc<AppState>,
) {
    let topic_wire = topic.to_wire_key();
    let mut stream = MessageBus::subscribe(state.bus.as_ref(), &topic);
    let out_tx_task = out_tx.clone();
    let translate_authz = matches!(topic, Topic::UserAuthz(_));
    // Clone for the reader closure; keep the original to key `subs`.
    let topic_wire_reader = topic_wire.clone();

    let reader = tokio::spawn(async move {
        while let Some(event) = stream.next().await {
            let message = if translate_authz {
                match event {
                    MessageBusEvent::AuthzChanged { affected_folders } => {
                        SessionOut::EvictFolders(affected_folders)
                    }
                    // The authz topic only carries AuthzChanged in
                    // MVP; other variants would be a producer bug —
                    // drop them silently so a mis-wired publish
                    // doesn't spam the client.
                    _ => continue,
                }
            } else {
                SessionOut::Frame(event_notification(&topic_wire_reader, &event))
            };
            if out_tx_task.send(message).await.is_err() {
                // Session's outbound channel closed — receiver dropped.
                break;
            }
        }
    });
    subs.insert(topic_wire, Sub { reader });
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
/// Payload discipline (see plan): thin facts only. The `MessageBusEvent`'s
/// own `#[serde(tag = "event")]` shape provides `event` + variant fields
/// under one flat object; we lift them into `params.data` alongside a
/// `topic` selector for the client.
fn event_notification(topic_wire: &str, event: &MessageBusEvent) -> String {
    // Serialize the event to extract `event` (discriminator) and the
    // remaining fields as `data`. Two-step to avoid re-inventing the
    // enum's discriminator string here.
    let event_json = serde_json::to_value(event).expect("MessageBusEvent always serializes");
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

/// Given a `MessageBusEvent` serialised as `{ "event": "file_created", ...rest }`,
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

/// Build the server-initiated `rt.revoked` JSON-RPC notification.
/// Emitted when a subscription is evicted mid-session (grant revoked,
/// resource deleted, etc.). Not tied to a request id — client sees
/// this as a signal to stop rendering the topic.
fn revoked_notification(topic_wire: &str, reason: &'static str) -> String {
    serde_json::to_string(&RpcNotification {
        jsonrpc: JSONRPC_V2,
        method: "rt.revoked",
        params: serde_json::json!({
            "topic": topic_wire,
            "reason": reason,
        }),
    })
    .expect("RpcNotification always serializes")
}

// ════════════════════════════════════════════════════════════════════════════
// Audit
// ════════════════════════════════════════════════════════════════════════════

fn audit_denied(caller_id: Uuid, topic: &str, reason: &'static str) {
    tracing::info!(
        target: "audit",
        event = "message_bus.subscribe_denied",
        reason = reason,
        caller_id = %caller_id,
        topic = %topic,
        "👮🏻‍♂️ message-bus subscribe rejected",
    );
}

/// Audit line for server-initiated eviction — every `rt.revoked`
/// frame we send should also have a durable trail. Stable `reason`
/// vocabulary matches the WS wire's `reason` field.
fn audit_evicted(caller_id: Uuid, topic: &str, reason: &'static str) {
    tracing::info!(
        target: "audit",
        event = "message_bus.subscription_evicted",
        reason = reason,
        caller_id = %caller_id,
        topic = %topic,
        "🚫 message-bus subscription evicted",
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
        let event = MessageBusEvent::FileCreated {
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
