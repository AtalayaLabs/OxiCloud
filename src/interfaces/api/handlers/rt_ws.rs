//! Message bus WebSocket handler — the endpoint every WS session
//! multiplexes over. See `docs/architecture/message-bus-and-notifications.md § Wire protocol`.
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
//! Route is mounted at `/api/rt/ws` OUTSIDE the standard
//! `auth_middleware` + `require_dpop_layer` stack — a browser can't
//! attach a `DPoP:` header to `new WebSocket()` (RFC 6455 gives us
//! only `Sec-WebSocket-Protocol`), and the standard chain would 401
//! on every DPoP-bound session. This handler self-authenticates
//! from two accepted sources:
//!
//! 1. **Ticket subprotocol** (`Sec-WebSocket-Protocol:
//!    oxi.ticket.<uuid>`) — the primary path for browser clients.
//!    The FE first `POST /api/rt/ticket` under the full middleware
//!    chain (auth + DPoP proofed), receives an opaque one-shot
//!    token, and passes it here. Verified by redeeming through
//!    [`AppState::rt_ticket_store`]. See
//!    `docs/architecture/message-bus-and-notifications.md § F`.
//! 2. **Bearer token** (`Authorization: Bearer <jwt>`) — the
//!    programmatic-client path used by `rt-hurl-helper` in api-test.
//!    Verified against `AuthServices::token_service`. DPoP-bound
//!    tokens are rejected on this path to preserve the substrate's
//!    proof-of-possession invariant.
//!
//! Neither → 401. Order matters: ticket first (short-lived, tied to
//! a proofed HTTP round-trip), bearer second.
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

use crate::domain::entities::user::UserRole;

use axum::body::Bytes;
use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;
use uuid::Uuid;

use crate::application::ports::auth_ports::TokenServicePort;
use crate::application::ports::authorization_ports::AuthorizationEngine;
use crate::application::ports::message_bus_ports::{
    AuthzCheck, BusResource, MessageBus, MessageBusEvent, ParseTopicErr, Topic, error_code,
};
use crate::common::di::AppState;
use crate::domain::services::authorization::{Permission, Resource, Subject};
use crate::infrastructure::services::rt_ticket_store::SUBPROTOCOL_PREFIX;

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
/// Overridable at server start via `OXICLOUD_MESSAGEBUS_KEEPALIVE_SECONDS`
/// — test suites drop it to a low value to exercise the keepalive path
/// within a bounded wall-clock.
const DEFAULT_KEEPALIVE_SECONDS: u64 = 30;

/// Read the keepalive interval from env at connection time. Kept as a
/// function rather than a `LazyLock` so a running server with the env
/// var flipped picks it up on the NEXT connection without a restart —
/// useful for smoke tests that toggle the value on the fly.
fn keepalive_interval() -> Duration {
    Duration::from_secs(
        std::env::var("OXICLOUD_MESSAGEBUS_KEEPALIVE_SECONDS")
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

/// `GET /api/rt/ws` — WS upgrade handler. Mounted outside the standard
/// `/api/*` middleware stack; self-authenticates via ticket
/// subprotocol OR bearer token (see the module doc).
///
/// Returns 101 Switching Protocols on success; 401 with an audit
/// entry on any auth failure. The response is deliberately terse —
/// browsers surface the status code via the `close` event's code
/// field (1006 on a rejected upgrade), so a longer body wouldn't
/// reach the FE anyway.
pub async fn rt_ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Response {
    let auth = match authenticate_upgrade(&headers, &state).await {
        Ok(auth) => auth,
        Err(reason) => {
            tracing::info!(
                target: "audit",
                event = "message_bus.upgrade_rejected",
                reason = %reason,
                "👮🏻‍♂️ WS upgrade rejected",
            );
            return (StatusCode::UNAUTHORIZED, "ws_auth_failed").into_response();
        }
    };
    let caller_id = auth.caller_id;
    // If the caller reached us via the ticket path, echo the exact
    // subprotocol they sent back on the 101 response — RFC 6455 §4.2.2
    // requires this or the client fails the connection.
    let ws = match auth.accepted_subprotocol {
        Some(sub) => ws.protocols([sub]),
        None => ws,
    };
    ws.on_upgrade(move |socket| handle_session(socket, caller_id, state))
}

/// Successful upgrade credentials — the resolved caller and (when the
/// ticket path was used) the subprotocol to echo on the 101 response.
struct UpgradeAuth {
    caller_id: Uuid,
    accepted_subprotocol: Option<String>,
}

/// Extract `Sec-WebSocket-Protocol` and match a ticket subprotocol
/// first; fall back to `Authorization: Bearer`. Returns a stable
/// `reason` key on failure so the audit log stays filterable.
async fn authenticate_upgrade(
    headers: &HeaderMap,
    state: &Arc<AppState>,
) -> Result<UpgradeAuth, &'static str> {
    if let Some(ticket_sub) = extract_ticket_subprotocol(headers) {
        // Redeem parses the UUID; a malformed subprotocol is a
        // structural failure ("bad_ticket_format"), an unknown-or-
        // expired UUID is a redemption failure ("ticket_invalid").
        let Some(ticket_str) = ticket_sub.strip_prefix(SUBPROTOCOL_PREFIX) else {
            return Err("bad_ticket_format");
        };
        let Ok(ticket_uuid) = Uuid::parse_str(ticket_str) else {
            return Err("bad_ticket_uuid");
        };
        let Some(caller_id) = state.rt_ticket_store.redeem(ticket_uuid) else {
            return Err("ticket_invalid");
        };
        return Ok(UpgradeAuth {
            caller_id,
            accepted_subprotocol: Some(ticket_sub),
        });
    }
    if let Some(bearer) = extract_bearer(headers) {
        let Some(auth_service) = state.auth_service.as_ref() else {
            return Err("auth_service_unavailable");
        };
        let claims = auth_service
            .token_service
            .validate_token(bearer)
            .map_err(|_| "bearer_invalid")?;
        if claims.sub_id.is_nil() {
            return Err("bearer_bad_subject");
        }
        // This path validates the token itself and never reaches
        // `CurrentUserId`, so closing `POST /api/rt/ticket` to anonymous
        // sessions does NOT close this door. Without the check here, a
        // scripted client could mint a share session, read its own JWT and
        // hold a WebSocket per share visitor — the connection blast the
        // ticket gate was meant to prevent. A browser cannot set
        // `Authorization` on `new WebSocket()`, so only the adversarial case
        // is affected, which is precisely the one that matters.
        if bearer_role_forbidden(&claims.role) {
            return Err("anonymous_forbidden");
        }
        return Ok(UpgradeAuth {
            caller_id: claims.sub_id,
            accepted_subprotocol: None,
        });
    }
    Err("no_credentials")
}

/// Find the first subprotocol value that looks like a ticket. Browsers
/// send `Sec-WebSocket-Protocol` as a comma-separated list per RFC 6455.
fn extract_ticket_subprotocol(headers: &HeaderMap) -> Option<String> {
    let raw = headers.get("sec-websocket-protocol")?.to_str().ok()?;
    raw.split(',')
        .map(str::trim)
        .find(|s| s.starts_with(SUBPROTOCOL_PREFIX))
        .map(|s| s.to_string())
}

/// Extract `Authorization: Bearer <token>` if present. Returns the raw
/// token string (never empty).
/// Whether a bearer's role claim disqualifies it from opening a socket.
///
/// Split out of `authenticate_upgrade` so the decision is testable without an
/// `AppState` — the same reason `middleware/user.rs` separates
/// `decide_live_role` from `resolve_live_role`. The authorize path itself
/// needs a JWT service and a ticket store, so the rule would otherwise be
/// reachable only through an integration test.
///
/// Unknown roles are refused. Every other parse site in the tree historically
/// defaulted to `user` on an unrecognised value, which fails OPEN; here a
/// value we cannot interpret gets no socket.
fn bearer_role_forbidden(claim_role: &str) -> bool {
    crate::domain::entities::user::UserRole::from_session(claim_role)
        .is_none_or(|r| r.is_anonymous())
}

fn extract_bearer(headers: &HeaderMap) -> Option<&str> {
    let value = headers.get("authorization")?.to_str().ok()?;
    let token = value.strip_prefix("Bearer ")?.trim();
    (!token.is_empty()).then_some(token)
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
/// loop. Three shapes:
///
/// - `Frame` — a client-bound text frame (`rt.event` notification,
///   `rt.revoked` notification, whatever). Main loop writes it to
///   the socket as a WS Text frame.
/// - `Binary` — a client-bound binary frame, pre-encoded to the
///   collab wire format (`[1 byte kind][16 bytes file_id][payload]`).
///   The forwarder task for a `Topic::Collab(file_id)` subscription
///   pushes these when the per-file actor broadcasts an update. Main
///   loop writes them to the socket as a WS Binary frame.
/// - `EvictFolders` — internal control signal. The reader for the
///   session's auto-subscribed `user:{caller}:authz` topic translates
///   inbound [`MessageBusEvent::AuthzChanged`] events into this rather
///   than a client-visible frame. Main loop walks its subs, drops any
///   whose resource is in the list, and emits one `rt.revoked` frame
///   per evicted topic.
enum SessionOut {
    Frame(String),
    Binary(Vec<u8>),
    EvictFolders(Vec<Uuid>),
}

/// RAII guard that decrements the live-session counter on ANY exit
/// path from `handle_session` — clean close, protocol error, panic
/// unwind, tokio task cancellation. Keeping the decrement in `Drop`
/// (not scattered inline before every `break;` / `return;`) means we
/// physically cannot leak a live count when a new exit branch is
/// added. `Arc` so it stays valid even if the task is aborted from
/// outside.
struct SessionCountGuard(Arc<std::sync::atomic::AtomicUsize>);

impl Drop for SessionCountGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    }
}

async fn handle_session(mut socket: WebSocket, caller_id: Uuid, state: Arc<AppState>) {
    // Live-session counter — incremented here, decremented on ANY
    // exit path via the `Drop` guard below (clean close, error,
    // panic unwind, task abort). Feeds the admin dashboard's
    // "Live activity" section. `Relaxed` because the counter is
    // approximate-by-design — a slightly stale read on the
    // dashboard is fine, and the atomic hop stays sub-nanosecond
    // on the hot path (session open / close).
    state
        .active_ws_sessions
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let _session_count_guard = SessionCountGuard(Arc::clone(&state.active_ws_sessions));

    // Snapshot the caller's role ONCE per session, so the Class-3
    // (`RoleAdmin`) AuthZ dispatch inside `handle_subscribe` doesn't
    // pay a DB hop on every subscribe frame. `resolve_live_role`
    // honours the short-TTL flags cache, and a demotion mid-session
    // takes effect on the NEXT reconnect (bounded by
    // USER_FLAGS_CACHE_TTL for the flags read at that point). If
    // the auth service isn't wired (unusual test config) or the
    // account is revoked, treat as non-admin — fail-closed for
    // admin gates. Passing "user" as the claim role is fail-open
    // for `resolve_live_role`'s non-admin fallback path.
    let caller_role: String = match state.auth_service.as_ref() {
        Some(auth) => {
            match crate::interfaces::middleware::user::resolve_live_role(
                auth.auth_application_service.as_ref(),
                caller_id,
                "user",
            )
            .await
            {
                crate::interfaces::middleware::user::LiveRole::Active(role) => role.to_string(),
                crate::interfaces::middleware::user::LiveRole::Revoked => {
                    // Account revoked between ticket-issue and now.
                    // Terminate the session immediately — dropping
                    // `socket` at end of scope closes the WS cleanly
                    // (no explicit `.close()` needed; that would
                    // require pulling `SinkExt` into scope for one
                    // line).
                    tracing::info!(
                        target: "audit",
                        event = "message_bus.session_rejected",
                        reason = "account_revoked",
                        caller_id = %caller_id,
                        "👮🏻‍♂️ WS session rejected — account revoked",
                    );
                    drop(socket);
                    return;
                }
            }
        }
        None => "user".to_string(),
    };

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

    // Auto-subscribe to the caller's private notifications topic —
    // same identity-scoped invariant as `:authz`. Events on this
    // stream (`MessageBusEvent::NotificationReceived`) forward
    // through as an `rt.event` notification so the FE bell can flip
    // its unread badge without a poll. The DB row is the truth (see
    // `docs/architecture/message-bus-and-notifications.md § Slice E`); a missed push recovers
    // on the next `GET /api/notifications`.
    install_subscription(
        Topic::UserNotifications(caller_id),
        &mut subs,
        &out_tx,
        &state,
    );

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
                    Some(SessionOut::Binary(bytes)) => {
                        // Pre-encoded collab wire frame from a
                        // `Topic::Collab(file_id)` forwarder task.
                        // Ship it verbatim.
                        if socket.send(Message::Binary(bytes.into())).await.is_err() {
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
                            handle_text_frame(&txt, caller_id, &caller_role, &state, &mut subs, &out_tx).await
                            && socket.send(Message::Text(reply.into())).await.is_err() {
                                break;
                            }
                    }
                    Some(Ok(Message::Binary(bytes))) => {
                        // Yjs sync-protocol frames for the collab editor.
                        // Format: `[1 byte kind][16 bytes file_id][payload…]`.
                        // Parsed by `collab_wire::parse_binary_frame`,
                        // dispatched by `CollabSessionService::handle_binary_frame`.
                        //
                        // When the collab feature isn't wired (no
                        // `collab_session_service` in AppState), silently
                        // drop the frame — the client will time out its
                        // own sync attempt and fall back gracefully.
                        // When it IS wired but the parse fails, close
                        // the socket with a protocol-violation reason;
                        // audit line captures the truth.
                        let Some(collab) = state.collab_session_service.as_ref() else {
                            continue;
                        };
                        use crate::application::services::collab_wire::{
                            parse_binary_frame, encode_binary_frame, kind, FrameParseErr,
                        };
                        let frame = match parse_binary_frame(&bytes) {
                            Ok(f) => f,
                            Err(e) => {
                                tracing::info!(
                                    target: "audit",
                                    event = "collab.protocol_violation",
                                    reason = match &e {
                                        FrameParseErr::TooShort { .. } => "too_short",
                                        FrameParseErr::UnknownKind { .. } => "unknown_kind",
                                    },
                                    caller_id = %caller_id,
                                    "👮🏻‍♂️ collab binary frame rejected: {e}",
                                );
                                break;
                            }
                        };
                        // Preserve kind + file_id BEFORE moving frame
                        // into the async call — the encode-back path
                        // needs both to build the reply header, and a
                        // parsed frame is one-shot-consumed by the
                        // router.
                        let reply_kind = frame.kind;
                        let reply_file_id = frame.file_id;
                        match collab.handle_binary_frame(caller_id, frame).await {
                            Ok(None) => {
                                // UPDATE + AWARENESS have no per-socket
                                // reply. Fan-out to other subscribers on
                                // the topic is a bus-side concern wired
                                // in a follow-up.
                            }
                            Ok(Some(reply_payload)) => {
                                // SYNC replies come back as the sync-step-2
                                // payload — re-wrap in a 0x03 binary frame
                                // (same kind as the incoming sync-step-1
                                // request per the Yjs protocol) and send.
                                let out = encode_binary_frame(
                                    if reply_kind == kind::SYNC { kind::SYNC } else { reply_kind },
                                    reply_file_id,
                                    &reply_payload,
                                );
                                if socket.send(Message::Binary(out.into())).await.is_err() {
                                    break;
                                }
                            }
                            Err(crate::application::services::collab_session_service::CollabError::AuthzDenied {
                                permission,
                                file_id,
                            }) => {
                                // Per-frame AuthZ denial. Anti-enumeration:
                                // same "close socket, no wire reason" shape
                                // as a bad-update. The AuthorizationEngine's
                                // own `authz.denied` line already recorded
                                // the deep reason; this event captures the
                                // frame-class context (read vs write) that
                                // the engine can't infer.
                                //
                                // Naming: `write_denied` for UPDATE
                                // (Permission::Update), `read_denied` for
                                // SYNC (Permission::Read). Distinct events
                                // so operators can filter "someone tried
                                // to write while only having read" from
                                // "someone tried to read without a grant".
                                let event_name = match permission {
                                    "update" => "collab.write_denied",
                                    "read" => "collab.read_denied",
                                    _ => "collab.authz_denied",
                                };
                                tracing::info!(
                                    target: "audit",
                                    event = event_name,
                                    reason = permission,
                                    caller_id = %caller_id,
                                    file_id = %file_id,
                                    "👮🏻‍♂️ collab frame denied: {permission} on file",
                                );
                                break;
                            }
                            Err(e) => {
                                tracing::info!(
                                    target: "audit",
                                    event = "collab.protocol_violation",
                                    reason = "apply_failed",
                                    caller_id = %caller_id,
                                    "👮🏻‍♂️ collab frame apply failed: {e}",
                                );
                                break;
                            }
                        }
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
    caller_role: &str,
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
        "rt.subscribe" => Some(
            handle_subscribe(id, req.params, caller_id, caller_role, state, subs, out_tx).await,
        ),
        "rt.unsubscribe" => Some(handle_unsubscribe(id, req.params, subs)),
        "rt.ping" => Some(success_response(id, serde_json::json!({ "pong": true }))),
        "rt.collab_flush" => Some(handle_collab_flush(id, req.params, caller_id, state).await),
        _ => Some(error_response(
            id,
            error_code::METHOD_NOT_FOUND,
            "method_not_found",
            Some(serde_json::json!({ "method": method })),
        )),
    }
}

/// `rt.collab_flush { file_id }` — client-initiated flush of the
/// CRDT text to the file's blob.
///
/// **Purpose:** the debouncer covers the ambient case (15 s idle /
/// 60 s max), but the FE editor knows better than the timer when a
/// flush is actually wanted: on tab close, on explicit save, or
/// before a URL change. Sending an explicit flush is one WS message
/// on the same socket the client is already using for edits — no
/// second roundtrip, no wall-clock wait on the debouncer.
///
/// **AuthZ:** `Permission::Update` on the file. Same gate as `0x01`
/// UPDATE frames — a Viewer cannot force a flush any more than they
/// can push an update. Denials use `error_code::NO_EDIT` /
/// `no_edit`, matching the write-side vocabulary. A missing file OR
/// a caller without Read collapses to `topic_forbidden` — anti-enum
/// parity with subscribe.
///
/// **Semantics:** delegates to `CollabSession::flush_to_blob`, which
/// is idempotent (short-circuits on unchanged content hash). Reply
/// `{ flushed: bool }` — `true` = a blob write happened, `false` =
/// no-op (nothing to flush, or content hash unchanged since last
/// flush). Callable at any time during a session.
///
/// **No active session case:** if `attach_file` needs to spawn an
/// actor to serve the request (client called flush before any
/// `0x03`/`0x01` frame), we still honour it — the actor seeds from
/// the blob, sees no writes, and short-circuits with `flushed:
/// false`. Cheap; keeps the API's contract simple.
async fn handle_collab_flush(
    id: Value,
    params: Value,
    caller_id: Uuid,
    state: &Arc<AppState>,
) -> String {
    // Extract file_id (uuid string).
    let file_id_str = match params.get("file_id").and_then(Value::as_str) {
        Some(s) => s,
        None => {
            return error_response(
                id,
                error_code::INVALID_PARAMS,
                "invalid_params",
                Some(serde_json::json!({ "missing": "file_id" })),
            );
        }
    };
    let file_id = match Uuid::parse_str(file_id_str) {
        Ok(u) => u,
        Err(_) => {
            return error_response(
                id,
                error_code::INVALID_PARAMS,
                "invalid_params",
                Some(serde_json::json!({ "invalid": "file_id" })),
            );
        }
    };

    // AuthZ. Same shape as an UPDATE binary frame gate: `Update` on
    // the file; deny collapses to `no_edit`, hidden files collapse
    // to `topic_forbidden` (anti-enumeration parity with subscribe).
    if state
        .authorization
        .require(
            Subject::User(caller_id),
            Permission::Update,
            Resource::File(file_id),
        )
        .await
        .is_err()
    {
        tracing::info!(
            target: "audit",
            event = "collab.flush_denied",
            reason = "no_edit",
            caller_id = %caller_id,
            file_id = %file_id,
            "👮🏻‍♂️ rt.collab_flush denied — no Update on file",
        );
        return error_response(
            id,
            error_code::NO_EDIT,
            "no_edit",
            Some(serde_json::json!({ "file_id": file_id_str })),
        );
    }

    // Delegate. Feature-off state returns `flushed: false` — the API
    // is honest that nothing happened rather than 404'ing.
    let Some(collab) = state.collab_session_service.as_ref() else {
        return success_response(id, serde_json::json!({ "flushed": false }));
    };
    let session = match collab.attach_file(caller_id, file_id).await {
        Ok(s) => s,
        Err(e) => {
            return error_response(
                id,
                error_code::INTERNAL_ERROR,
                "internal_error",
                Some(serde_json::json!({ "detail": e.to_string() })),
            );
        }
    };
    match session.flush_to_blob().await {
        Ok(flushed) => success_response(id, serde_json::json!({ "flushed": flushed })),
        Err(e) => error_response(
            id,
            error_code::INTERNAL_ERROR,
            "internal_error",
            Some(serde_json::json!({ "detail": e.to_string() })),
        ),
    }
}

async fn handle_subscribe(
    id: Value,
    params: Value,
    caller_id: Uuid,
    caller_role: &str,
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

    // For a `Topic::Collab` subscribe ack, we surface the caller's
    // Update capability alongside the Read gate below — the FE gates
    // CodeMirror between edit and read-only mode on this flag. Any
    // other topic leaves this `None`, and the ack shape stays flat.
    let mut collab_capabilities: Option<serde_json::Value> = None;

    // AuthZ dispatch — one match arm per gate class. Adding a new topic
    // variant with a new gate shape is a compile error here.
    match topic.required_perm() {
        AuthzCheck::ResourceRead { resource } => {
            let domain_resource = match resource {
                BusResource::Folder(uuid) => Resource::Folder(uuid),
                BusResource::File(uuid) => Resource::File(uuid),
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
            // Second pass for collab topics only: check Update on the
            // same resource so the ack can carry `can_write`. The
            // authorization engine's decision cache turns this into a
            // no-op after warm-up. Failure here is NOT a denial —
            // Viewers legitimately get `can_write: false` alongside
            // a successful Read-gated subscribe.
            if matches!(topic, Topic::Collab(_))
                && let BusResource::File(file_uuid) = resource
            {
                let can_write = state
                    .authorization
                    .require(
                        Subject::User(caller_id),
                        Permission::Update,
                        Resource::File(file_uuid),
                    )
                    .await
                    .is_ok();
                collab_capabilities = Some(serde_json::json!({ "can_write": can_write }));
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
        AuthzCheck::RoleAdmin => {
            // Class 3 — role-scoped. Caller must be admin. `caller_role`
            // was snapshotted at session start (see `handle_session`),
            // so no per-subscribe DB hit. A demotion mid-session
            // takes effect on the caller's next reconnect.
            if !UserRole::str_at_least(caller_role, UserRole::Admin) {
                audit_denied(caller_id, &topic_str, "role_denied");
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

    // Collab subscribes carry an extra `capabilities` object so the FE
    // can render read-only affordances without a second round trip.
    // Other topics keep the flat `{ subscribed: <topic> }` shape.
    let result = match collab_capabilities {
        Some(caps) => serde_json::json!({
            "subscribed":   topic_str,
            "capabilities": caps,
        }),
        None => serde_json::json!({ "subscribed": topic_str }),
    };
    success_response(id, result)
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
/// - For `Topic::UserNotifications(_)`: an incoming
///   `MessageBusEvent::NotificationReceived` is forwarded through the
///   default path — the FE bell listens for `rt.event` on the
///   auto-subscribed identity topic and refetches `GET
///   /api/notifications` when it sees one. Same anti-enumeration
///   invariant as `:authz` (identity-scoped, no admin bypass).
/// - For every other topic: bus events are wrapped into a client-
///   visible `rt.event` notification and pushed as `SessionOut::Frame`.
fn install_subscription(
    topic: Topic,
    subs: &mut HashMap<String, Sub>,
    out_tx: &mpsc::Sender<SessionOut>,
    state: &Arc<AppState>,
) {
    let topic_wire = topic.to_wire_key();

    // `Topic::Collab(file_id)` doesn't ride the JSON bus — its data
    // plane is a per-file broadcast channel owned by the collab actor.
    // Spawn a forwarder task that receives raw update bytes and
    // pre-encodes them into `0x01` binary frames for the socket.
    // If the collab service isn't wired (feature off), the subscribe
    // still succeeds — we just install a no-op sub — matching the
    // `Read`-gate-passed shape of any other topic. Silent-drop is
    // acceptable here because the feature-off state is a boot-time
    // decision, not a runtime one; the operator sees it in the
    // `collab.service_enabled` audit line (or its absence).
    if let Topic::Collab(file_id) = topic {
        let reader = spawn_collab_forwarder(file_id, out_tx.clone(), state);
        subs.insert(topic_wire, Sub { reader });
        return;
    }

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

/// Spawn a forwarder task for a `Topic::Collab(file_id)` subscription.
///
/// The task attaches the caller's session to the per-file actor,
/// obtains a `broadcast::Receiver` on its update outbox, and pipes
/// every applied update as a `0x01` binary frame to `out_tx`. The
/// task terminates when:
///
/// - the session's `out_tx` is dropped (WS closed), OR
/// - the actor's outbox drops all senders (actor shut down / idle-GC'd), OR
/// - the receiver falls `broadcast_capacity` updates behind
///   (`RecvError::Lagged`) — logged and terminated; the client's WS
///   reconnect + sync-step-1 catches up cleanly.
///
/// A missing `collab_session_service` (feature off) or an attach
/// error (repo blip) collapses to "no-op forwarder": the task exits
/// immediately with an audit line so the operator sees why the
/// subscribe ack'd but delivered nothing.
fn spawn_collab_forwarder(
    file_id: Uuid,
    out_tx: mpsc::Sender<SessionOut>,
    state: &Arc<AppState>,
) -> JoinHandle<()> {
    let collab = state.collab_session_service.clone();
    tokio::spawn(async move {
        let Some(collab) = collab else {
            tracing::debug!(
                target: "oxicloud::collab",
                file_id = %file_id,
                "collab subscribe with feature off — no-op forwarder",
            );
            return;
        };
        // Attach the file (spawns or reuses the actor) and take a
        // broadcast receiver on its update outbox. The caller_id used
        // for the reader is the session's — attach_file uses it only
        // when seeding a fresh doc from the blob.
        //
        // Attach can fail (repo error, stale FK). Log + exit; the
        // subscribe was already ack'd so the client sees a healthy
        // topic that just never delivers — the same shape as the
        // "feature off" case, and the audit line explains which.
        let session = match collab.attach_file(Uuid::nil(), file_id).await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(
                    target: "oxicloud::collab",
                    file_id = %file_id,
                    error = %e,
                    "collab forwarder: attach_file failed",
                );
                return;
            }
        };
        let mut rx = match session.subscribe_updates().await {
            Ok(rx) => rx,
            Err(e) => {
                tracing::warn!(
                    target: "oxicloud::collab",
                    file_id = %file_id,
                    error = %e,
                    "collab forwarder: subscribe_updates failed",
                );
                return;
            }
        };
        use crate::application::services::collab_session_service::INTERNAL_KIND_EVICTED;
        use crate::application::services::collab_wire::encode_binary_frame;
        let topic_wire = Topic::Collab(file_id).to_wire_key();
        loop {
            match rx.recv().await {
                Ok((frame_kind, payload)) => {
                    if frame_kind == INTERNAL_KIND_EVICTED {
                        // Server-only control message from
                        // `CollabSessionService::evict_sessions_for_file`.
                        // Never a valid wire kind — translate into an
                        // `rt.revoked` text frame so the SPA transitions
                        // its collab UI, then unwind. Reason is
                        // UTF-8-decoded from the payload; a corrupt
                        // producer defaults to a generic marker so we
                        // still ship SOME signal to the client.
                        let reason = std::str::from_utf8(&payload).unwrap_or("evicted");
                        let frame = revoked_notification(&topic_wire, reason);
                        let _ = out_tx.send(SessionOut::Frame(frame)).await;
                        return;
                    }
                    // Broadcast carries `(kind, bytes)` so UPDATE and
                    // AWARENESS share the same channel without a
                    // second forwarder. Encode with the kind the actor
                    // stamped; wire layout is otherwise identical.
                    let frame = encode_binary_frame(frame_kind, file_id, &payload);
                    if out_tx.send(SessionOut::Binary(frame)).await.is_err() {
                        // Session dead; unwind the forwarder.
                        return;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    // Slow consumer: the actor sent updates faster than
                    // this forwarder drained them for
                    // `broadcast_capacity` frames. `rx` is still valid
                    // and would keep returning `Lagged` until it catches
                    // up, so grab a fresh receiver instead — that
                    // discards the backlog cleanly. The client's next
                    // sync-step-1 (issued on reconnect or explicitly)
                    // reconciles anything missed; Yjs's idempotent
                    // apply guarantees no double-application.
                    tracing::info!(
                        target: "audit",
                        event = "collab.forwarder_lagged",
                        file_id = %file_id,
                        dropped = n,
                        "👮🏻‍♂️ collab forwarder fell behind; \
                         resubscribing on a fresh receiver",
                    );
                    match session.subscribe_updates().await {
                        Ok(fresh) => rx = fresh,
                        Err(_) => return, // actor gone — unwind
                    }
                }
            }
        }
    })
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
fn revoked_notification(topic_wire: &str, reason: &str) -> String {
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

    /// The WS upgrade is registered OUTSIDE the protected router — a browser
    /// cannot set `Authorization` on `new WebSocket()`, so it self-
    /// authenticates and neither `auth_middleware` nor
    /// `anonymous_allowlist_layer` ever sees it. Closing
    /// `POST /api/rt/ticket` to anonymous sessions therefore does NOT close
    /// this door; this check is the door.
    ///
    /// Unreachable today — the share ring is a cookie with its own `typ` and
    /// never a bearer — but it is what stops a scripted client minting share
    /// sessions and holding one socket per visitor, which is the connection
    /// blast the ticket gate exists to prevent.
    #[test]
    fn anonymous_and_unknown_bearers_get_no_socket() {
        assert!(bearer_role_forbidden("anonymous"));

        // Fails closed on anything unrecognised — a corrupt or
        // future-versioned claim gets no socket rather than a user's.
        for role in ["", "superuser", "Admin", "ANONYMOUS", "user "] {
            assert!(
                bearer_role_forbidden(role),
                "unrecognised role {role:?} must be refused"
            );
        }

        assert!(!bearer_role_forbidden("user"));
        assert!(!bearer_role_forbidden("admin"));
    }

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
