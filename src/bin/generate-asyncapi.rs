//! AsyncAPI 3.0 spec generator for the message bus.
//!
//! Mirrors `generate-openapi.rs`: constructs the spec from the same
//! Rust enums the server uses (`Topic`, `MessageBusEvent`, JSON-RPC
//! error codes) and writes `resources/gen/asyncapi.json`.
//!
//! This is the first-PR MVP surface — the two topics and two events
//! that Phase A ships (see `docs/architecture/message-bus-and-notifications.md § First PR`).
//! Adding a topic/event later is a match arm + a new schema block in
//! this file; the CI dirty-tree check (same as OpenAPI's) prevents
//! spec/code drift.
//!
//! Format: JSON, not YAML — matches `openapi.json`. AsyncAPI's own
//! tooling reads either; JSON also keeps us dep-free.
//!
//! Invocation:
//!
//! ```bash
//! cargo run --bin generate-asyncapi
//! # or
//! just asyncapi
//! ```

use std::fs;
use std::path::PathBuf;

use oxicloud::application::ports::message_bus_ports::error_code;
use serde_json::{Value, json};

fn main() {
    let doc = build_asyncapi();
    let json =
        serde_json::to_string_pretty(&doc).expect("Failed to serialize AsyncAPI spec to JSON");

    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let resources_gen_dir = manifest_dir.join("resources").join("gen");
    fs::create_dir_all(&resources_gen_dir).expect("Failed to create resources/gen directory");
    let output_path = resources_gen_dir.join("asyncapi.json");
    fs::write(&output_path, json).expect("Failed to write AsyncAPI spec to file");

    println!(
        "AsyncAPI spec generated successfully at: {}",
        output_path.display()
    );
}

fn build_asyncapi() -> Value {
    json!({
        // AsyncAPI 3.0.0 — deliberately NOT 3.1.0.
        //
        // Under 3.1.0 `@asyncapi/modelina` interprets the schema tree
        // as one collapsed root and emits a single `Root.ts` with
        // everything nested inline. Under 3.0.0 it walks each named
        // component and emits 30 separate model files. Both are
        // "correct" outputs; the FE codebase is built on the split
        // shape (per-schema imports let `useTopic<...>` unions stay
        // compile-time exhaustive) so we pin to 3.0.0.
        //
        // Revisit if the FE ever moves to a single-blob type or if
        // Modelina's 3.1.0 traversal changes. Validator will flag it
        // as `information` severity ("The latest version is not used")
        // — that's expected, not a bug.
        "asyncapi": "3.0.0",
        "info": {
            "title":   "OxiCloud message bus",
            // OXICLOUD_TAG (bare tag, no `git describe` suffix), NOT
            // OXICLOUD_VERSION — this file is committed under
            // resources/gen/ and drift-checked by CI on every PR.
            // See build.rs → strip_describe_suffix() and the mirror
            // comment on the utoipa `info()` in src/interfaces/api/mod.rs.
            "version": env!("OXICLOUD_TAG"),
            "description": r#"
JSON-RPC 2.0 over WebSocket for control + events, Yjs sync protocol for
CRDT binary frames. The wire is described here for the first-PR MVP
surface (folder-live updates); Phase B (comments, presence) and
Phase C (sync-client push, album live) extend the same channels — see
`docs/architecture/message-bus-and-notifications.md`.
"#.trim(),
            // Must match the top-level LICENSE file (SPDX identifier).
            "license": {
                "name": "MIT",
                "url": "https://opensource.org/licenses/MIT",
            },
        },
        // Applied to every message that doesn't set its own — the JSON-RPC
        // control frames are all `application/json`. Binary Yjs frames on
        // the collab channel override to `application/octet-stream`.
        "defaultContentType": "application/json",
        "servers": {
            "default": {
                "host": "{host}",
                "pathname": "/api/rt/ws",
                "protocol": "wss",
                "description": "OxiCloud message bus WebSocket endpoint. Text frames are JSON-RPC 2.0 (control plane + `rt.event` notifications). Binary frames are the Yjs sync protocol for the collab editor — layout `[1 byte kind][16 bytes file_id BE][payload]`, see the `Collab` channel below and `docs/plan/markdown-collab.md`.",
                "variables": {
                    "host": {
                        "description": "Server host — replace with the deployment domain",
                        "default": "cloud.example.com",
                    },
                },
                "protocolVersion": "13",
                // Subprotocol advertised in the WS handshake. The handler
                // accepts one of two shapes:
                //   * `oxi.ticket.<uuid>` — the browser path. Redeems a
                //     one-shot 30 s ticket minted by
                //     `POST /api/rt/ticket` (that endpoint runs under the
                //     full auth + DPoP stack, so the ticket effectively
                //     inherits the proofed session).
                //   * (no subprotocol) — falls back to
                //     `Authorization: Bearer <jwt>`, used by programmatic
                //     clients that can set headers (e.g. rt-hurl-helper).
                "bindings": {
                    "ws": { "subProtocol": "oxi.ticket.{ticket}" }
                },
                // Every request MUST be authenticated. Two paths:
                //   * `bearerAuth` — programmatic clients set
                //     `Authorization: Bearer <jwt>` on the WS upgrade
                //     (same header the REST API uses).
                //   * `ticketAuth` — browser clients POST
                //     `/api/rt/ticket` with full auth + DPoP, receive
                //     an opaque one-shot token, and pass it via
                //     `Sec-WebSocket-Protocol: oxi.ticket.<uuid>`
                //     (browsers cannot set arbitrary headers on
                //     `new WebSocket()`). See `docs/architecture/message-bus-and-notifications.md § F`.
                "security": [
                    { "$ref": "#/components/securitySchemes/bearerAuth" },
                    { "$ref": "#/components/securitySchemes/ticketAuth" }
                ],
            }
        },
        "channels": channels(),
        "operations": operations(),
        "components": components(),
    })
}

fn channels() -> Value {
    json!({
        "Folder": {
            "address": "folder:{folderId}",
            "description": "A folder's mutation stream — file/subfolder created events fire here. AuthZ: caller must hold `Read` on the folder.",
            "parameters": {
                "folderId": { "description": "Folder UUID" }
            },
            "messages": {
                "SubscribeRequest":   { "$ref": "#/components/messages/RtSubscribeRequest" },
                "UnsubscribeRequest": { "$ref": "#/components/messages/RtUnsubscribeRequest" },
                "PingRequest":        { "$ref": "#/components/messages/RtPingRequest" },
                "PongResponse":       { "$ref": "#/components/messages/RtPongResponse" },
                "SubscribedResponse": { "$ref": "#/components/messages/RtSubscribedResponse" },
                "ErrorResponse":      { "$ref": "#/components/messages/RtErrorResponse" },
                "FolderEvent":        { "$ref": "#/components/messages/RtFolderEventNotification" },
                "RevokedNotification": { "$ref": "#/components/messages/RtRevokedNotification" },
            }
        },
        "UserAuthz": {
            "address": "user:{userId}:authz",
            "description": "A user's private AuthZ-change channel. Identity-scoped: caller_id must equal userId (no admin bypass).",
            "parameters": {
                "userId": { "description": "User UUID — must match the authenticated caller" }
            },
            "messages": {
                "SubscribeRequest":   { "$ref": "#/components/messages/RtSubscribeRequest" },
                "UnsubscribeRequest": { "$ref": "#/components/messages/RtUnsubscribeRequest" },
            }
        },
        "UserNotifications": {
            "address": "user:{userId}:notifications",
            "description": "A user's private notifications channel. Identity-scoped: caller_id must equal userId (no admin bypass). Auto-subscribed at session open; the FE bell refetches `GET /api/notifications` when a `notification_received` event fires. The DB row is authoritative — a missed push recovers on the next mount.",
            "parameters": {
                "userId": { "description": "User UUID — must match the authenticated caller" }
            },
            "messages": {
                "SubscribeRequest":   { "$ref": "#/components/messages/RtSubscribeRequest" },
                "UnsubscribeRequest": { "$ref": "#/components/messages/RtUnsubscribeRequest" },
            }
        },
        "Job": {
            "address": "job:{jobName}",
            "description": "A named background job's run lifecycle — Started / Progress / Ended. Consumed by the admin dashboard so operators who trigger a long-running job (backend migration, thumb import…) can navigate off the admin page and come back without losing progress. AuthZ: admin-only (Class 3 role-scoped) — non-admin gets `topic_forbidden`, indistinguishable on the wire from an unknown topic.",
            "parameters": {
                "jobName": { "description": "Scheduler-registered short slug (e.g. `backend_migration`); `[a-z0-9_-]` chars only" }
            },
            "messages": {
                "SubscribeRequest":   { "$ref": "#/components/messages/RtSubscribeRequest" },
                "UnsubscribeRequest": { "$ref": "#/components/messages/RtUnsubscribeRequest" },
                "SubscribedResponse": { "$ref": "#/components/messages/RtSubscribedResponse" },
                "ErrorResponse":      { "$ref": "#/components/messages/RtErrorResponse" },
                "JobEvent":           { "$ref": "#/components/messages/RtFolderEventNotification" },
                "RevokedNotification": { "$ref": "#/components/messages/RtRevokedNotification" },
            }
        },
        "Collab": {
            "address": "collab:{fileId}",
            "description": "Collaborative `.md` editing over Yjs. Two frame families ride the same channel:\n\n  * **JSON-RPC control plane** (`rt.subscribe` / `rt.unsubscribe` / `rt.collab_flush`) — same text-frame shape as every other channel.\n  * **Binary data plane** — Yjs sync protocol as raw bytes: `[1 byte kind][16 bytes file_id BE][payload]`. Kinds: `0x01` UPDATE (Yjs update blob), `0x02` AWARENESS (presence, not persisted), `0x03` SYNC (state vector c→s / diff s→c).\n\nAuthZ: `Read` on the file for subscribe (JSON) and for `0x03 SYNC` (defense-in-depth vs the subscribe gate). `Update` on the file for `0x01 UPDATE` and `rt.collab_flush`. AWARENESS is presence-only and rides the subscribe-time Read gate.\n\nSee `docs/plan/markdown-collab.md` and `src/application/services/collab_wire.rs`.",
            "parameters": {
                "fileId": { "description": "File UUID — the target `.md` file" }
            },
            "messages": {
                "SubscribeRequest":     { "$ref": "#/components/messages/RtSubscribeRequest" },
                "UnsubscribeRequest":   { "$ref": "#/components/messages/RtUnsubscribeRequest" },
                "CollabFlushRequest":   { "$ref": "#/components/messages/RtCollabFlushRequest" },
                "CollabFlushResponse":  { "$ref": "#/components/messages/RtCollabFlushResponse" },
                "SubscribedResponse":   { "$ref": "#/components/messages/RtSubscribedResponse" },
                "ErrorResponse":        { "$ref": "#/components/messages/RtErrorResponse" },
                "RevokedNotification":  { "$ref": "#/components/messages/RtRevokedNotification" },
                "CollabSyncFrame":      { "$ref": "#/components/messages/CollabSyncFrame" },
                "CollabUpdateFrame":    { "$ref": "#/components/messages/CollabUpdateFrame" },
                "CollabAwarenessFrame": { "$ref": "#/components/messages/CollabAwarenessFrame" },
            }
        }
    })
}

fn operations() -> Value {
    json!({
        "subscribeFolder": {
            "action": "send",
            "channel": { "$ref": "#/channels/Folder" },
            "title": "rt.subscribe — join a folder's mutation stream",
            "summary": "**`rt.subscribe`** → client joins a folder's mutation stream. Reply is `rt.subscribed` (ok) or `rt.error` (permission denied, unknown topic, etc.).",
            "messages": [
                { "$ref": "#/channels/Folder/messages/SubscribeRequest" }
            ],
            "reply": {
                "channel": { "$ref": "#/channels/Folder" },
                "messages": [
                    { "$ref": "#/channels/Folder/messages/SubscribedResponse" },
                    { "$ref": "#/channels/Folder/messages/ErrorResponse" },
                ]
            }
        },
        "unsubscribeFolder": {
            "action": "send",
            "channel": { "$ref": "#/channels/Folder" },
            "title": "rt.unsubscribe — leave a folder's mutation stream",
            "summary": "**`rt.unsubscribe`** → client leaves a previously-joined folder. Server acks with `rt.subscribed` (`result.ok = true`) — the ack message is shared with subscribe.",
            "messages": [
                { "$ref": "#/channels/Folder/messages/UnsubscribeRequest" }
            ],
            // The wire DOES send `rt.subscribed` back on unsubscribe too
            // (the ack message is shared with subscribe — see
            // `RtSubscribedResponse`'s title). Declaring the reply here
            // makes the AsyncAPI-HTML template render this as REQUEST
            // (matching `subscribeFolder`), not fire-and-forget SEND,
            // and surfaces the ack payload in the operation panel.
            "reply": {
                "channel": { "$ref": "#/channels/Folder" },
                "messages": [
                    { "$ref": "#/channels/Folder/messages/SubscribedResponse" },
                    { "$ref": "#/channels/Folder/messages/ErrorResponse" },
                ]
            }
        },
        "receiveFolderEvent": {
            "action": "receive",
            "channel": { "$ref": "#/channels/Folder" },
            "title": "rt.event — folder mutation notification",
            "summary": "**`rt.event`** → server pushes a mutation notice (file/subfolder created, renamed, deleted). Client updates its live view. Delivered only to subscribers holding `Read` on the folder.",
            "messages": [
                { "$ref": "#/channels/Folder/messages/FolderEvent" }
            ]
        },
        "receiveRevoked": {
            "action": "receive",
            "channel": { "$ref": "#/channels/Folder" },
            "title": "rt.revoked — subscription evicted",
            "summary": "**`rt.revoked`** → server evicts the client's subscription (grant revoked, resource deleted, session expired). Client stops rendering the topic and drops any local state.",
            "messages": [
                { "$ref": "#/channels/Folder/messages/RevokedNotification" }
            ]
        },
        // Application-layer keepalive. Separate from the RFC 6455 Ping
        // control frame the server sends on `OXICLOUD_MESSAGEBUS_KEEPALIVE_SECONDS`
        // (which is transport-level and not modelled in AsyncAPI). This
        // operation lets a client actively confirm the socket is
        // end-to-end alive when transport-level Pings alone can't rule
        // out a proxy black-hole.
        "ping": {
            "action": "send",
            "channel": { "$ref": "#/channels/Folder" },
            "title": "rt.ping — application-level keepalive",
            "summary": "**`rt.ping`** → application-level liveness probe. Reply is `rt.pong` with `result.pong = true`. Distinct from RFC 6455 transport-level Ping; useful when a proxy black-holes traffic without dropping the socket.",
            "messages": [
                { "$ref": "#/channels/Folder/messages/PingRequest" }
            ],
            "reply": {
                "channel": { "$ref": "#/channels/Folder" },
                "messages": [
                    { "$ref": "#/channels/Folder/messages/PongResponse" }
                ]
            }
        },
        // ── Collab operations ────────────────────────────────────
        "subscribeCollab": {
            "action": "send",
            "channel": { "$ref": "#/channels/Collab" },
            "title": "rt.subscribe — join a collab session (AuthZ gate: Read)",
            "summary": "Subscribe to a file's collab channel. Clears the server-side `Permission::Read` gate on the file so the binary data-plane frames start flowing. Without this, the server drops binary frames from this socket. Same JSON-RPC shape as the folder subscribe; only the topic prefix differs.",
            "messages": [
                { "$ref": "#/channels/Collab/messages/SubscribeRequest" }
            ],
            "reply": {
                "channel": { "$ref": "#/channels/Collab" },
                "messages": [
                    { "$ref": "#/channels/Collab/messages/SubscribedResponse" },
                    { "$ref": "#/channels/Collab/messages/ErrorResponse" },
                ]
            }
        },
        "collabFlush": {
            "action": "send",
            "channel": { "$ref": "#/channels/Collab" },
            "title": "rt.collab_flush — force flush CRDT text to blob (on tab close / save)",
            "summary": "**`rt.collab_flush`** → the FE editor's explicit-save path. Server checks `Permission::Update` on the file, calls `CollabSession::flush_to_blob` (idempotent, short-circuits on unchanged content hash), returns `{ flushed: bool }`. Denials use `no_edit`. See `src/interfaces/api/handlers/rt_ws.rs::handle_collab_flush`.",
            "messages": [
                { "$ref": "#/channels/Collab/messages/CollabFlushRequest" }
            ],
            "reply": {
                "channel": { "$ref": "#/channels/Collab" },
                "messages": [
                    { "$ref": "#/channels/Collab/messages/CollabFlushResponse" },
                    { "$ref": "#/channels/Collab/messages/ErrorResponse" },
                ]
            }
        },
        "sendCollabSync": {
            "action": "send",
            "channel": { "$ref": "#/channels/Collab" },
            "title": "0x03 SYNC (c→s, sync-step-1) — send client state vector",
            "summary": "Client sends its Yjs state vector so the server can compute the diff needed to catch it up. Payload is `Y.encodeStateVector(doc)` bytes; the empty state vector is a single `0x00` byte (varint 0 clients) — NOT zero-length. Server replies with a `0x03 SYNC` frame carrying the Yjs update bytes.",
            "messages": [
                { "$ref": "#/channels/Collab/messages/CollabSyncFrame" }
            ],
            "reply": {
                "channel": { "$ref": "#/channels/Collab" },
                "messages": [
                    { "$ref": "#/channels/Collab/messages/CollabSyncFrame" }
                ]
            }
        },
        "sendCollabUpdate": {
            "action": "send",
            "channel": { "$ref": "#/channels/Collab" },
            "title": "0x01 UPDATE (c→s) — apply Yjs update to the shared doc",
            "summary": "Client emits a Yjs update blob describing local edits. Server applies to the per-file actor's `yrs::Doc`, broadcasts to every other subscribed socket on the same file, and arms the debouncer for eventual flush-to-blob. AuthZ: server checks `Permission::Update` per frame — a Viewer's frame gets `collab.write_denied` audit and the socket closes.",
            "messages": [
                { "$ref": "#/channels/Collab/messages/CollabUpdateFrame" }
            ]
        },
        "receiveCollabUpdate": {
            "action": "receive",
            "channel": { "$ref": "#/channels/Collab" },
            "title": "0x01 UPDATE (s→c) — fan-out from another editor",
            "summary": "Server broadcasts a Yjs update it applied for another editor on the same file. The originating client also receives its own update back — Yjs's `applyUpdate` is idempotent so this is a no-op locally.",
            "messages": [
                { "$ref": "#/channels/Collab/messages/CollabUpdateFrame" }
            ]
        },
        "sendCollabAwareness": {
            "action": "send",
            "channel": { "$ref": "#/channels/Collab" },
            "title": "0x02 AWARENESS (c→s) — presence / cursor position",
            "summary": "Presence-only (cursor position, user handle, colour). Not applied to the CRDT, not persisted. AuthZ: the subscribe-time Read gate is sufficient — a Viewer's cursor is legitimate.",
            "messages": [
                { "$ref": "#/channels/Collab/messages/CollabAwarenessFrame" }
            ]
        },
        "receiveCollabAwareness": {
            "action": "receive",
            "channel": { "$ref": "#/channels/Collab" },
            "title": "0x02 AWARENESS (s→c) — peer presence",
            "summary": "Server broadcasts another editor's presence bytes. Fan-out policy is same as UPDATE — every subscriber including origin.",
            "messages": [
                { "$ref": "#/channels/Collab/messages/CollabAwarenessFrame" }
            ]
        },
        "receiveCollabRevoked": {
            "action": "receive",
            "channel": { "$ref": "#/channels/Collab" },
            "title": "rt.revoked — collab session evicted",
            "summary": "Grant revoked, file deleted, or admin kicked. Client tears down the editor UI.",
            "messages": [
                { "$ref": "#/channels/Collab/messages/RevokedNotification" }
            ]
        }
    })
}

fn components() -> Value {
    // Placeholder UUID used across every example — same shape as the
    // topic-parameter examples further down. Keeping it in one const
    // means all rendered examples refer to the same imaginary folder,
    // so a reader can trace a subscribe → event → unsubscribe flow
    // without having to reconcile changing IDs mid-page.
    let example_folder_id = "00000000-0000-0000-0000-000000000000";
    let example_folder_topic = format!("folder:{example_folder_id}");

    let mut components = json!({
        "messages": {
            // ── Requests ────────────────────────────────────────────
            "RtSubscribeRequest": {
                "name": "rt.subscribe",
                "title": "Subscribe to a topic",
                "contentType": "application/json",
                "payload": { "$ref": "#/components/schemas/RtSubscribeRequestBody" },
                "examples": [{
                    "name": "subscribe-folder",
                    "summary": "Client → server: subscribe to a folder's mutation stream",
                    "payload": {
                        "jsonrpc": "2.0",
                        "id": 1,
                        "method": "rt.subscribe",
                        "params": { "topic": example_folder_topic },
                    },
                }],
            },
            "RtUnsubscribeRequest": {
                "name": "rt.unsubscribe",
                "title": "Unsubscribe from a topic",
                "contentType": "application/json",
                "payload": { "$ref": "#/components/schemas/RtUnsubscribeRequestBody" },
                "examples": [{
                    "name": "unsubscribe-folder",
                    "summary": "Client → server: unsubscribe from a previously-joined folder",
                    "payload": {
                        "jsonrpc": "2.0",
                        "id": 2,
                        "method": "rt.unsubscribe",
                        "params": { "topic": example_folder_topic },
                    },
                }],
            },
            "RtPingRequest": {
                "name": "rt.ping",
                "title": "Keepalive ping",
                "contentType": "application/json",
                "payload": { "$ref": "#/components/schemas/RtPingRequestBody" },
                "examples": [{
                    "name": "ping",
                    "summary": "Client → server: liveness probe (reply is rt.pong)",
                    "payload": {
                        "jsonrpc": "2.0",
                        "id": 42,
                        "method": "rt.ping",
                    },
                }],
            },
            // ── Responses ───────────────────────────────────────────
            "RtSubscribedResponse": {
                "name": "rt.subscribed",
                "title": "Subscribe / unsubscribe ack",
                "contentType": "application/json",
                "payload": { "$ref": "#/components/schemas/RtSuccessResponseBody" },
                "examples": [{
                    "name": "subscribed-ok",
                    "summary": "Server → client: successful rt.subscribe / rt.unsubscribe ack",
                    "payload": {
                        "jsonrpc": "2.0",
                        "id": 1,
                        "result": { "ok": true },
                    },
                }],
            },
            "RtErrorResponse": {
                "name": "rt.error",
                "title": "JSON-RPC error object",
                "contentType": "application/json",
                "payload": { "$ref": "#/components/schemas/RtErrorResponseBody" },
                "examples": [{
                    "name": "error-no-read",
                    "summary": "Server → client: rt.subscribe refused because caller lacks Read on the folder",
                    "payload": {
                        "jsonrpc": "2.0",
                        "id": 1,
                        "error": {
                            "code": error_code::NO_READ,
                            "message": "no_read",
                            "data": { "topic": example_folder_topic },
                        },
                    },
                }],
            },
            "RtPongResponse": {
                "name": "rt.pong",
                "title": "Reply to rt.ping — `result.pong == true`",
                "contentType": "application/json",
                "payload": { "$ref": "#/components/schemas/RtPongResponseBody" },
                "examples": [{
                    "name": "pong",
                    "summary": "Server → client: reply to a client rt.ping",
                    "payload": {
                        "jsonrpc": "2.0",
                        "id": 42,
                        "result": { "pong": true },
                    },
                }],
            },
            // ── Notifications (server → client) ─────────────────────
            "RtFolderEventNotification": {
                "name": "rt.event",
                "title": "Folder mutation event",
                "contentType": "application/json",
                "payload": { "$ref": "#/components/schemas/RtFolderEventBody" },
                "examples": [{
                    "name": "event-file-created",
                    "summary": "Server → client: a file was created inside a subscribed folder",
                    "payload": {
                        "jsonrpc": "2.0",
                        "method": "rt.event",
                        "params": {
                            "topic": example_folder_topic,
                            "kind": "file.created",
                            "seq": 17,
                            "data": {
                                "id": "11111111-1111-1111-1111-111111111111",
                                "name": "report.pdf",
                                "size": 12345,
                                "content_hash": "b3-…",
                                "created_at": "2026-09-13T18:04:00Z",
                            },
                        },
                    },
                }],
            },
            "RtRevokedNotification": {
                "name": "rt.revoked",
                "title": "Subscription evicted",
                "contentType": "application/json",
                "payload": { "$ref": "#/components/schemas/RtRevokedBody" },
                "examples": [{
                    "name": "revoked-permission-lost",
                    "summary": "Server → client: subscription evicted because the caller lost Read on the folder",
                    "payload": {
                        "jsonrpc": "2.0",
                        "method": "rt.revoked",
                        "params": {
                            "topic": example_folder_topic,
                            "reason": "permission_lost",
                        },
                    },
                }],
            },
            // ── Collab: control-plane (JSON) ────────────────────────
            "RtCollabFlushRequest": {
                "name": "rt.collab_flush",
                "title": "Force flush of the CRDT text to the file's blob",
                "contentType": "application/json",
                "payload": { "$ref": "#/components/schemas/RtCollabFlushRequestBody" },
                "examples": [{
                    "name": "collab-flush",
                    "summary": "Client → server: on tab close / explicit save",
                    "payload": {
                        "jsonrpc": "2.0",
                        "id": 7,
                        "method": "rt.collab_flush",
                        "params": { "file_id": "00000000-0000-0000-0000-000000000000" },
                    },
                }],
            },
            "RtCollabFlushResponse": {
                "name": "rt.collab_flush.reply",
                "title": "Collab flush ack — `result.flushed`",
                "contentType": "application/json",
                "payload": { "$ref": "#/components/schemas/RtCollabFlushResponseBody" },
                "examples": [{
                    "name": "collab-flush-ok-wrote",
                    "summary": "Server → client: a blob write happened",
                    "payload": {
                        "jsonrpc": "2.0",
                        "id": 7,
                        "result": { "flushed": true },
                    },
                }, {
                    "name": "collab-flush-ok-noop",
                    "summary": "Server → client: unchanged content, short-circuited",
                    "payload": {
                        "jsonrpc": "2.0",
                        "id": 7,
                        "result": { "flushed": false },
                    },
                }],
            },
            // ── Collab: data-plane (binary) ─────────────────────────
            //
            // Payload schemas are `format: "binary"` — AsyncAPI's JSON
            // Schema can't describe byte-fielded layouts natively; the
            // full layout is in the `description`. All three messages
            // share the 17-byte header prefix.
            "CollabSyncFrame": {
                "name": "collab.sync",
                "title": "0x03 SYNC — Yjs state vector (c→s) / update diff (s→c)",
                "contentType": "application/octet-stream",
                "payload": { "$ref": "#/components/schemas/CollabBinaryFrame" },
            },
            "CollabUpdateFrame": {
                "name": "collab.update",
                "title": "0x01 UPDATE — Yjs update blob (bidirectional)",
                "contentType": "application/octet-stream",
                "payload": { "$ref": "#/components/schemas/CollabBinaryFrame" },
            },
            "CollabAwarenessFrame": {
                "name": "collab.awareness",
                "title": "0x02 AWARENESS — presence (bidirectional, not persisted)",
                "contentType": "application/octet-stream",
                "payload": { "$ref": "#/components/schemas/CollabBinaryFrame" },
            }
        },
        "schemas": {
            // Top-level JSON-RPC frame bodies.
            "RtSubscribeRequestBody": rpc_request_schema("rt.subscribe", Some(ref_schema("RtSubscribeParams"))),
            "RtUnsubscribeRequestBody": rpc_request_schema("rt.unsubscribe", Some(ref_schema("RtUnsubscribeParams"))),
            "RtPingRequestBody": rpc_request_schema("rt.ping", None),
            "RtSuccessResponseBody": rpc_success_response_schema(),
            "RtPongResponseBody": rpc_pong_response_schema(),
            "RtErrorResponseBody": rpc_error_response_schema(),
            "RtFolderEventBody": folder_event_notification_schema(),
            "RtRevokedBody": revoked_notification_schema(),

            // Hoisted nested schemas — pulled out from inline `params`,
            // inner `error`, `result`, and enum arrays so Modelina (and
            // any other spec-driven codegen) gets real names instead of
            // `AnonymousSchema_N`. Keep names in sync with the shape:
            // renaming here silently breaks the generated FE types, so
            // the CI dirty-tree check catches drift.
            "RtSubscribeParams":   topic_params_schema(),
            "RtUnsubscribeParams": topic_params_schema(),
            "RtEventParams":       event_params_schema(),
            "RtEventDataUnion":    event_data_union_schema(),
            "RtEventKind":         event_kind_schema(),
            "RtRevokedParams":     revoked_params_schema(),
            "RtRevokedReason":     revoked_reason_schema(),
            "RtErrorObject":       rpc_error_object_schema(),
            "RtErrorCode":         rpc_error_code_schema(),
            "RtErrorMessage":      rpc_error_message_schema(),
            "RtPongResult":        rpc_pong_result_schema(),

            // Per-event data payloads (one per `event` discriminator).
            "FileCreatedData": file_created_schema(),
            "FileRenamedData": file_renamed_schema(),
            "FileMovedData": file_moved_schema(),
            "FileDeletedData": file_deleted_schema(),
            "FolderCreatedData": folder_created_schema(),
            "FolderRenamedData": folder_renamed_schema(),
            "FolderMovedData": folder_moved_schema(),
            "FolderDeletedData": folder_deleted_schema(),
            "NotificationReceivedData": notification_received_schema(),
            "JobRunStartedData": job_run_started_schema(),
            "JobRunProgressData": job_run_progress_schema(),
            "JobRunEndedData": job_run_ended_schema(),

            // ── Collab ─────────────────────────────────────────────
            "RtCollabFlushRequestBody":  rpc_request_schema("rt.collab_flush", Some(ref_schema("RtCollabFlushParams"))),
            "RtCollabFlushResponseBody": rpc_typed_response_schema(ref_schema("RtCollabFlushResult")),
            "RtCollabFlushParams":       collab_flush_params_schema(),
            "RtCollabFlushResult":       collab_flush_result_schema(),
            "CollabBinaryFrame":         collab_binary_frame_schema(),
        },
        // How the client authenticates. Handler side is `auth_middleware`
        // — the same middleware every `/api/*` request goes through, so
        // any JWT valid for REST is valid for WS.
        "securitySchemes": {
            "bearerAuth": {
                "type": "http",
                "scheme": "bearer",
                "bearerFormat": "JWT",
                "description": "OxiCloud JWT — same access_token minted by `POST /api/auth/login` (or the OPAQUE handshake). Programmatic clients set `Authorization: Bearer <jwt>` on the WS upgrade request. DPoP-bound tokens are refused on this path (the WS handshake cannot carry a DPoP proof); browsers use `ticketAuth` instead.",
            },
            // `httpApiKey` (not bare `apiKey`) — AsyncAPI 3.0 reserves
            // `apiKey` for server-variable-based schemes; a header-
            // scoped key is `httpApiKey` with `in: header`.
            "ticketAuth": {
                "type": "httpApiKey",
                "in": "header",
                "name": "Sec-WebSocket-Protocol",
                "description": "Browser path — the FE first calls `POST /api/rt/ticket` under the full REST middleware stack (auth + DPoP-proofed request), receives an opaque one-shot UUID with a 30 s TTL, then sets `Sec-WebSocket-Protocol: oxi.ticket.<uuid>` on the WS upgrade. The server redeems the ticket (single-use — a second attempt fails) and treats the WS session as authenticated for the caller who issued it. See `docs/architecture/message-bus-and-notifications.md § F` and `handlers/rt_ticket_handler.rs`.",
            }
        }
    });

    // Close every top-level object schema in components.schemas —
    // the Rust wire (`serde` on named struct fields) never emits
    // extras, so `additionalProperties: false` is honest, and it
    // removes the `additionalProperties?: Record<string, unknown>`
    // escape-hatch field Modelina would otherwise generate on every
    // TS interface. One-shot post-process instead of 19 individual
    // `"additionalProperties": false` lines sprinkled through the
    // schema builders.
    //
    // Deliberately NOT recursive: we only close the named top-level
    // schemas. Recursing into `properties` closes anonymous inline
    // sub-objects, which then triggers Modelina to name them (and
    // fail our AnonymousSchema guard). If a nested object needs a
    // real name AND `additionalProperties: false`, hoist it explicitly
    // to `components.schemas` and reference via `$ref`.
    if let Some(schemas) = components.get_mut("schemas").and_then(Value::as_object_mut) {
        for schema in schemas.values_mut() {
            close_object_schema_shallow(schema);
        }
    }

    components
}

/// Add `additionalProperties: false` to a top-level object schema if
/// it declares `type: "object"` and doesn't already set the field.
/// Non-object schemas (`enum`, `oneOf`, `type: "integer"`, string
/// types, etc.) are untouched. Never descends — see `components()`.
fn close_object_schema_shallow(schema: &mut Value) {
    let Value::Object(map) = schema else { return };
    let is_object = matches!(map.get("type"), Some(Value::String(s)) if s == "object");
    if is_object && !map.contains_key("additionalProperties") {
        map.insert("additionalProperties".to_string(), Value::Bool(false));
    }
}

// ─── Schema builders ────────────────────────────────────────────────────────

/// `$ref` shorthand — every hoisted inline schema below is referenced
/// through this so consumers of the spec (Modelina, AsyncAPI Studio, any
/// SDK generator) see named types instead of `AnonymousSchema_N`.
fn ref_schema(name: &str) -> Value {
    json!({ "$ref": format!("#/components/schemas/{name}") })
}

/// JSON-RPC 2.0 request envelope. `params_schema` is `Some(...)` for
/// methods that take arguments (`rt.subscribe`, `rt.unsubscribe`) and
/// `None` for methods that don't (`rt.ping`). Omitting `params` from
/// the properties entirely — rather than declaring it as
/// `{"type": "null"}` — keeps Modelina from emitting `params?: any`
/// on the generated TS: no property in the schema → no property in
/// the interface, which is what JSON-RPC 2.0 allows anyway (`params`
/// is optional per spec).
fn rpc_request_schema(method: &str, params_schema: Option<Value>) -> Value {
    let mut properties = json!({
        "jsonrpc": { "type": "string", "const": "2.0" },
        "id":      { "type": ["integer", "string", "null"] },
        "method":  { "type": "string", "const": method },
    });
    if let Some(params) = params_schema {
        properties["params"] = params;
    }
    json!({
        "type": "object",
        "required": ["jsonrpc", "id", "method"],
        "properties": properties,
    })
}

fn topic_params_schema() -> Value {
    json!({
        "type": "object",
        "required": ["topic"],
        "properties": {
            "topic": {
                "type": "string",
                "description": "Wire form: `folder:<uuid>` or `user:<uuid>:authz`",
                "examples": ["folder:00000000-0000-0000-0000-000000000000"],
            }
        }
    })
}

fn rpc_success_response_schema() -> Value {
    json!({
        "type": "object",
        "required": ["jsonrpc", "id", "result"],
        "properties": {
            "jsonrpc": { "type": "string", "const": "2.0" },
            "id":      { "type": ["integer", "string", "null"] },
            // Generic base shape — every specific method has its own
            // typed result schema (RtPongResult, subscribed ack, etc.).
            // Declaring every JSON type explicitly nudges Modelina
            // toward a real union rather than the bare `any` it emits
            // for a purely descriptive schema — matches the JSON-RPC
            // spec's "any JSON value" phrasing while giving downstream
            // codegens something to project.
            "result": {
                "description": "Method-specific result payload. See the concrete response schema for each `method`.",
                "type": ["object", "array", "string", "number", "integer", "boolean", "null"],
            },
        }
    })
}

/// Reply to `rt.ping` — the shape pins `result.pong == true` so
/// contract tests can assert on it directly. `result` is hoisted to
/// [`RtPongResult`] so Modelina gets a named type.
fn rpc_pong_response_schema() -> Value {
    json!({
        "type": "object",
        "required": ["jsonrpc", "id", "result"],
        "properties": {
            "jsonrpc": { "type": "string", "const": "2.0" },
            "id":      { "type": ["integer", "string", "null"] },
            "result":  ref_schema("RtPongResult"),
        }
    })
}

fn rpc_pong_result_schema() -> Value {
    json!({
        "type": "object",
        "required": ["pong"],
        "properties": {
            "pong": { "type": "boolean", "const": true }
        }
    })
}

fn rpc_error_response_schema() -> Value {
    // The `code`/`message` catalog is the stable public vocabulary —
    // any change here IS a wire break. Every entry mirrors
    // `application/ports/message_bus_ports.rs::error_code`. The inner
    // error object is hoisted to `RtErrorObject` so Modelina emits a
    // named type instead of `AnonymousSchema_N`.
    json!({
        "type": "object",
        "required": ["jsonrpc", "id", "error"],
        "properties": {
            "jsonrpc": { "type": "string", "const": "2.0" },
            "id":      { "type": ["integer", "string", "null"] },
            "error":   ref_schema("RtErrorObject"),
        }
    })
}

fn rpc_error_object_schema() -> Value {
    json!({
        "type": "object",
        "description": "JSON-RPC 2.0 error object. `code` + `message` form a stable pair; `data` optionally carries caller-visible context (e.g. offending topic).",
        "required": ["code", "message"],
        "properties": {
            "code":    ref_schema("RtErrorCode"),
            "message": ref_schema("RtErrorMessage"),
            // Per JSON-RPC 2.0: "A Primitive or Structured value that
            // contains additional information about the error." The
            // union covers every JSON type so Modelina emits a real
            // TS union rather than a bare `any`. Client MUST check
            // `code` before assuming `data`'s shape.
            "data": {
                "description": "Optional caller-facing context; shape depends on the specific `code`.",
                "type": ["object", "array", "string", "number", "integer", "boolean", "null"],
            }
        }
    })
}

fn rpc_error_code_schema() -> Value {
    // Kept as plain `integer` — Modelina projects a JSON-Schema `enum` of
    // numeric values into a TS enum with mangled member names
    // (`MINUS_32001 = -32001`), which is worse than no enum at all. The
    // Rust `error_code` module is the source of truth for named
    // constants; the FE mirrors it in `frontend/src/lib/message-bus/
    // error-codes.ts` (hand-written, 11 lines, sits alongside the
    // generated DTOs). Description enumerates the full set inline so the
    // AsyncAPI spec is still self-documenting.
    let full_description = format!(
        "Stable integer error code. Values are frozen across releases — a \
         new denial cause gets a new value, never repurposes an existing \
         one. Application-defined codes ({}..={}):\n\
         · {} NO_READ — resource-scoped topic, caller lacks Read (or \
         resource doesn't exist — indistinguishable by design)\n\
         · {} NO_SHARE — resource requires Share, caller has Read but not Share\n\
         · {} NO_COMMENT — resource requires Comment\n\
         · {} TOPIC_FORBIDDEN — identity-scoped mismatch or unknown/malformed topic\n\
         · {} SUB_LIMIT — per-connection subscription cap hit\n\
         · {} RATE_LIMITED — subscribe-frame token bucket exhausted\n\
         · {} NO_EDIT — CRDT edit frame from a caller without Edit\n\
         Standard JSON-RPC 2.0 codes:\n\
         · {} INTERNAL_ERROR · {} INVALID_REQUEST · {} METHOD_NOT_FOUND · {} INVALID_PARAMS",
        -32099,
        -32000,
        error_code::NO_READ,
        error_code::NO_SHARE,
        error_code::NO_COMMENT,
        error_code::TOPIC_FORBIDDEN,
        error_code::SUB_LIMIT,
        error_code::RATE_LIMITED,
        error_code::NO_EDIT,
        error_code::INTERNAL_ERROR,
        error_code::INVALID_REQUEST,
        error_code::METHOD_NOT_FOUND,
        error_code::INVALID_PARAMS,
    );
    json!({
        "type": "integer",
        "description": full_description,
    })
}

fn rpc_error_message_schema() -> Value {
    json!({
        "type": "string",
        "description": "Stable wire vocabulary; matches the corresponding `code`.",
        "enum": [
            "no_read", "no_share", "no_comment", "topic_forbidden",
            "sub_limit", "rate_limited", "no_edit",
            "internal_error", "invalid_request",
            "method_not_found", "invalid_params",
        ],
    })
}

fn folder_event_notification_schema() -> Value {
    json!({
        "type": "object",
        "description": "JSON-RPC notification (no `id`). `method = \"rt.event\"`. `params` is hoisted to `RtEventParams`.",
        "required": ["jsonrpc", "method", "params"],
        "properties": {
            "jsonrpc": { "type": "string", "const": "2.0" },
            "method":  { "type": "string", "const": "rt.event" },
            "params":  ref_schema("RtEventParams"),
        }
    })
}

fn event_params_schema() -> Value {
    json!({
        "type": "object",
        "required": ["topic", "event", "data"],
        "properties": {
            "topic": { "type": "string" },
            "event": ref_schema("RtEventKind"),
            "data":  ref_schema("RtEventDataUnion"),
        }
    })
}

fn event_kind_schema() -> Value {
    json!({
        "type": "string",
        "description": "Discriminator for the `data` payload. Mirrors the `#[serde(tag = \"event\", rename_all = \"snake_case\")]` variants of the Rust `MessageBusEvent` enum — a new event kind is a new enum variant on both sides.",
        "enum": [
            "file_created", "file_renamed", "file_moved", "file_deleted",
            "folder_created", "folder_renamed", "folder_moved", "folder_deleted",
            "notification_received",
            "job_run_started", "job_run_progress", "job_run_ended",
        ],
    })
}

fn event_data_union_schema() -> Value {
    json!({
        "description": "Tagged union of every possible `rt.event` payload. Discriminated by the sibling `event` field (see `RtEventKind`).",
        "oneOf": [
            ref_schema("FileCreatedData"),
            ref_schema("FileRenamedData"),
            ref_schema("FileMovedData"),
            ref_schema("FileDeletedData"),
            ref_schema("FolderCreatedData"),
            ref_schema("FolderRenamedData"),
            ref_schema("FolderMovedData"),
            ref_schema("FolderDeletedData"),
            ref_schema("NotificationReceivedData"),
            ref_schema("JobRunStartedData"),
            ref_schema("JobRunProgressData"),
            ref_schema("JobRunEndedData"),
        ]
    })
}

fn file_created_schema() -> Value {
    json!({
        "type": "object",
        "required": ["file_id", "name", "parent_id", "actor"],
        "properties": {
            "file_id":   { "type": "string", "format": "uuid" },
            "name":      { "type": "string" },
            "parent_id": { "type": "string", "format": "uuid" },
            "actor":     { "type": "string", "format": "uuid" },
        }
    })
}

fn file_renamed_schema() -> Value {
    json!({
        "type": "object",
        "required": ["file_id", "old_name", "new_name", "parent_id", "actor"],
        "properties": {
            "file_id":   { "type": "string", "format": "uuid" },
            "old_name":  { "type": "string" },
            "new_name":  { "type": "string" },
            "parent_id": { "type": "string", "format": "uuid" },
            "actor":     { "type": "string", "format": "uuid" },
        }
    })
}

fn file_moved_schema() -> Value {
    json!({
        "type": "object",
        "description": "Emitted on BOTH the source (`from`) and destination (`to`) folder topics. Subscribers to either see the event exactly once because they're subscribed to only one of the two.",
        "required": ["file_id", "name", "from", "to", "actor"],
        "properties": {
            "file_id": { "type": "string", "format": "uuid" },
            "name":    { "type": "string" },
            "from":    { "type": "string", "format": "uuid" },
            "to":      { "type": "string", "format": "uuid" },
            "actor":   { "type": "string", "format": "uuid" },
        }
    })
}

fn file_deleted_schema() -> Value {
    json!({
        "type": "object",
        "description": "The wire doesn't distinguish soft (trash) vs. permanent delete — clients treat both as \"disappears from the folder view\". `parent_id` is the folder the file used to live in.",
        "required": ["file_id", "parent_id", "actor"],
        "properties": {
            "file_id":   { "type": "string", "format": "uuid" },
            "parent_id": { "type": "string", "format": "uuid" },
            "actor":     { "type": "string", "format": "uuid" },
        }
    })
}

fn folder_created_schema() -> Value {
    json!({
        "type": "object",
        "required": ["folder_id", "name", "parent_id", "actor"],
        "properties": {
            "folder_id": { "type": "string", "format": "uuid" },
            "name":      { "type": "string" },
            "parent_id": { "type": "string", "format": "uuid" },
            "actor":     { "type": "string", "format": "uuid" },
        }
    })
}

fn folder_renamed_schema() -> Value {
    json!({
        "type": "object",
        "required": ["folder_id", "old_name", "new_name", "parent_id", "actor"],
        "properties": {
            "folder_id": { "type": "string", "format": "uuid" },
            "old_name":  { "type": "string" },
            "new_name":  { "type": "string" },
            "parent_id": { "type": "string", "format": "uuid" },
            "actor":     { "type": "string", "format": "uuid" },
        }
    })
}

fn folder_moved_schema() -> Value {
    json!({
        "type": "object",
        "description": "Emitted on BOTH the source (`from`) and destination (`to`) folder topics — same shape as `FileMoved`.",
        "required": ["folder_id", "name", "from", "to", "actor"],
        "properties": {
            "folder_id": { "type": "string", "format": "uuid" },
            "name":      { "type": "string" },
            "from":      { "type": "string", "format": "uuid" },
            "to":        { "type": "string", "format": "uuid" },
            "actor":     { "type": "string", "format": "uuid" },
        }
    })
}

fn folder_deleted_schema() -> Value {
    json!({
        "type": "object",
        "description": "Soft vs. permanent delete are indistinguishable on the wire.",
        "required": ["folder_id", "parent_id", "actor"],
        "properties": {
            "folder_id": { "type": "string", "format": "uuid" },
            "parent_id": { "type": "string", "format": "uuid" },
            "actor":     { "type": "string", "format": "uuid" },
        }
    })
}

// ─────────────────── Notification event payload ──────────────────
// Published on `Topic::UserNotifications(user_id)`. Identity-scoped
// (Class 2) — caller must equal the topic's user_id, no admin
// bypass. Payload is a thin poke: `notification_id` + `kind` +
// `created_at`. The FE bell refetches `GET /api/notifications` on
// receipt for the row's full payload; the DB is the truth, the bus
// event is just an invalidation.

fn notification_received_schema() -> Value {
    // Pure cache-invalidation event — no fields on the wire.
    // The topic (`user:{u}:notifications`) signals the semantic;
    // the FE responds by refetching `GET /api/notifications`
    // (or a delta via `?after=<cursor>`). All payload data lives
    // in the REST DTO (OpenAPI), not here. See
    // `docs/plan/templated-messages.md § Schema ownership`.
    json!({
        "type": "object",
        "description": "A new notification was created for the caller. Pure cache-invalidation event — no fields on the wire. The FE refetches `GET /api/notifications` on receipt and reads the payload from the REST DTO (see `openapi.json`). Zero schema overlap between the bus wire (this file) and the REST wire — the strict form of the AsyncAPI-defines-envelope / OpenAPI-defines-payload split.",
        "additionalProperties": false,
        "properties": {}
    })
}

// ─────────────────── Job event data payloads ─────────────────────
// Published on `Topic::Job(name)`. AuthZ is Class-3 (admin-only) —
// non-admins get `topic_forbidden` on subscribe, so these payloads
// only ever reach admin subscribers. See `handlers/rt_ws.rs`.

fn job_run_started_schema() -> Value {
    json!({
        "type": "object",
        "description": "A background job's run started. `name` matches the scheduler-registered job name (e.g. `backend_migration`). `actor` is `00000000-0000-0000-0000-000000000000` today — the scheduler doesn't yet thread the trigger caller through.",
        "required": ["name", "started_at", "actor"],
        "properties": {
            "name":       { "type": "string" },
            "started_at": { "type": "string", "format": "date-time" },
            "actor":      { "type": "string", "format": "uuid" },
        }
    })
}

fn job_run_progress_schema() -> Value {
    json!({
        "type": "object",
        "description": "A background job made progress. Throttled at the publish site to at most one per 3 s per job (see scheduler engine). `step` / `total` populate a progress bar; all three fields are optional because different jobs report different granularities.",
        "required": ["name"],
        "properties": {
            "name":    { "type": "string" },
            "step":    { "type": ["integer", "null"], "minimum": 0 },
            "total":   { "type": ["integer", "null"], "minimum": 0 },
            "message": { "type": ["string", "null"] },
        }
    })
}

fn job_run_ended_schema() -> Value {
    json!({
        "type": "object",
        "description": "A background job's run ended. `success = true` for a normal completion; `false` for failure / timeout / cancelled / paused-with-unhandled-outcome. `reason` populates the toast text on the `false` branch and links to `/admin/jobs/<name>` for the full outcome. Consumer typically drops its subscription on receipt (job is done).",
        "required": ["name", "success", "ended_at"],
        "properties": {
            "name":     { "type": "string" },
            "success":  { "type": "boolean" },
            "reason":   { "type": ["string", "null"] },
            "ended_at": { "type": "string", "format": "date-time" },
        }
    })
}

/// `rt.revoked` notification body — server tells the client that a
/// specific subscription has been evicted. `topic` is the wire-form
/// string the client originally subscribed to. `reason` is the stable
/// eviction vocabulary — never repurpose an existing value (matches
/// the AuthZ audit-line convention).
fn revoked_notification_schema() -> Value {
    json!({
        "type": "object",
        "description": "JSON-RPC notification (no `id`). `method = \"rt.revoked\"`. `params` hoisted to `RtRevokedParams`.",
        "required": ["jsonrpc", "method", "params"],
        "properties": {
            "jsonrpc": { "type": "string", "const": "2.0" },
            "method":  { "type": "string", "const": "rt.revoked" },
            "params":  ref_schema("RtRevokedParams"),
        }
    })
}

fn revoked_params_schema() -> Value {
    json!({
        "type": "object",
        "required": ["topic", "reason"],
        "properties": {
            "topic":  { "type": "string" },
            "reason": ref_schema("RtRevokedReason"),
        }
    })
}

fn revoked_reason_schema() -> Value {
    json!({
        "type": "string",
        "description": "Server-side eviction cause. Stable vocabulary; a new eviction reason is a new enum value.",
        "enum": [
            "grant_revoked",
            "resource_deleted",
            "group_membership_lost",
            "admin_kick",
        ]
    })
}

// ─────────────────────────── Collab ───────────────────────────

/// Generic JSON-RPC success envelope with a typed `result` — same
/// shape as [`rpc_pong_response_schema`] but with the result ref
/// picked by the caller. Used by any method that has a real result
/// object (`rt.collab_flush` today; more to come).
fn rpc_typed_response_schema(result_schema: Value) -> Value {
    json!({
        "type": "object",
        "required": ["jsonrpc", "id", "result"],
        "properties": {
            "jsonrpc": { "type": "string", "const": "2.0" },
            "id":      { "type": ["integer", "string", "null"] },
            "result":  result_schema,
        }
    })
}

/// `params` object for `rt.collab_flush { file_id }`.
fn collab_flush_params_schema() -> Value {
    json!({
        "type": "object",
        "required": ["file_id"],
        "properties": {
            "file_id": {
                "type": "string",
                "format": "uuid",
                "description": "Target file — dashed UUID.",
            }
        }
    })
}

/// `result` object for the `rt.collab_flush` reply: `{ flushed: bool }`.
/// `true` = a blob write happened; `false` = the content hash matched
/// the last flush, so the call was a no-op.
fn collab_flush_result_schema() -> Value {
    json!({
        "type": "object",
        "required": ["flushed"],
        "properties": {
            "flushed": {
                "type": "boolean",
                "description": "`true` when a blob write happened; `false` when the CRDT text was unchanged since the last flush (short-circuit).",
            }
        }
    })
}

/// Payload schema for every collab data-plane binary message
/// (`CollabSyncFrame`, `CollabUpdateFrame`, `CollabAwarenessFrame`).
///
/// AsyncAPI's JSON Schema can't describe byte-fielded layouts natively
/// — `format: "binary"` is the standard escape hatch for "opaque bytes,
/// see the description." The 17-byte prefix layout is the same across
/// all three kinds; the payload semantics differ (state vector vs
/// Yjs update blob vs awareness bytes) and are named per-message via
/// the message's `title`.
fn collab_binary_frame_schema() -> Value {
    json!({
        "type": "string",
        "format": "binary",
        "description": "Raw bytes of a collab wire frame. Layout: \
                        `[1 byte kind][16 bytes file_id BE][payload...]`. \
                        `kind`: `0x01` UPDATE (Yjs update blob), `0x02` \
                        AWARENESS (opaque presence bytes, not persisted), \
                        `0x03` SYNC (Yjs state vector c→s / Yjs update \
                        s→c). `file_id` is a UUID serialised as its 16 \
                        raw bytes (matches `Uuid::as_bytes()` on the \
                        server; `y-codemirror.next` binary handling on \
                        the client). Empty payloads are legal only for \
                        the SYNC direction that carries a state vector \
                        of zero clients — Yjs encodes that as a single \
                        `0x00` byte, NOT zero bytes.",
    })
}
