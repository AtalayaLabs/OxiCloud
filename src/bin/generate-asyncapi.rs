//! AsyncAPI 3.0 spec generator for the realtime message bus.
//!
//! Mirrors `generate-openapi.rs`: constructs the spec from the same
//! Rust enums the server uses (`Topic`, `RealtimeEvent`, JSON-RPC
//! error codes) and writes `resources/gen/asyncapi.json`.
//!
//! This is the first-PR MVP surface — the two topics and two events
//! that Phase A ships (see `docs/plan/message-bus.md § First PR`).
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

use oxicloud::application::ports::realtime_ports::error_code;
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
        "asyncapi": "3.0.0",
        "info": {
            "title":   "OxiCloud realtime message bus",
            "version": env!("CARGO_PKG_VERSION"),
            "description": r#"
JSON-RPC 2.0 over WebSocket for control + events, Yjs sync protocol for
CRDT binary frames. The wire is described here for the first-PR MVP
surface (folder-live updates); Phase B (comments, presence) and
Phase C (sync-client push, album live) extend the same channels — see
`docs/plan/message-bus.md`.
"#.trim(),
            "license": { "name": "AGPL-3.0-or-later" },
        },
        // Applied to every message that doesn't set its own — the JSON-RPC
        // control frames are all `application/json`. Binary Yjs frames
        // stay out of AsyncAPI (see the Server description for pointers).
        "defaultContentType": "application/json",
        "servers": {
            "default": {
                "host": "{host}",
                "pathname": "/api/rt/ws",
                "protocol": "wss",
                "description": "OxiCloud realtime bus WebSocket endpoint. Text frames are JSON-RPC 2.0. Binary frames (out of AsyncAPI scope) are Yjs sync protocol for the collab editor — see `docs/plan/markdown-collab.md`.",
                "variables": {
                    "host": {
                        "description": "Server host — replace with the deployment domain",
                        "default": "cloud.example.com",
                    },
                },
                "protocolVersion": "13",
                // Subprotocol advertised in the WS handshake. Handler
                // accepts `oxi.rt.v1` and the optional bearer element
                // `authorization.bearer.<jwt>` alongside it.
                "bindings": {
                    "ws": { "subProtocol": "oxi.rt.v1" }
                },
                // Every request MUST be authenticated. Programmatic
                // clients set `Authorization: Bearer <jwt>` on the WS
                // upgrade (same header the REST API uses); browser
                // clients — which can't set headers on `new WebSocket()`
                // — will use the deferred ticket flow (a plain HTTP
                // POST issues a short-lived one-shot ticket bound to
                // the WS URL, see the plan's DPoP-gap section).
                "security": [
                    { "$ref": "#/components/securitySchemes/bearerAuth" }
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
        }
    })
}

fn operations() -> Value {
    json!({
        "subscribeFolder": {
            "action": "send",
            "channel": { "$ref": "#/channels/Folder" },
            "summary": "Subscribe to a folder's mutation stream",
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
            "summary": "Unsubscribe from a folder's mutation stream",
            "messages": [
                { "$ref": "#/channels/Folder/messages/UnsubscribeRequest" }
            ]
        },
        "receiveFolderEvent": {
            "action": "receive",
            "channel": { "$ref": "#/channels/Folder" },
            "summary": "Server-pushed `rt.event` notification for a folder mutation",
            "messages": [
                { "$ref": "#/channels/Folder/messages/FolderEvent" }
            ]
        },
        // Application-layer keepalive. Separate from the RFC 6455 Ping
        // control frame the server sends on `OXICLOUD_RT_WS_KEEPALIVE_SECONDS`
        // (which is transport-level and not modelled in AsyncAPI). This
        // operation lets a client actively confirm the socket is
        // end-to-end alive when transport-level Pings alone can't rule
        // out a proxy black-hole.
        "ping": {
            "action": "send",
            "channel": { "$ref": "#/channels/Folder" },
            "summary": "Application-level keepalive; `rt.pong` reply confirms end-to-end liveness",
            "messages": [
                { "$ref": "#/channels/Folder/messages/PingRequest" }
            ],
            "reply": {
                "channel": { "$ref": "#/channels/Folder" },
                "messages": [
                    { "$ref": "#/channels/Folder/messages/PongResponse" }
                ]
            }
        }
    })
}

fn components() -> Value {
    json!({
        "messages": {
            // ── Requests ────────────────────────────────────────────
            "RtSubscribeRequest": {
                "name": "rt.subscribe",
                "title": "Subscribe to a topic",
                "contentType": "application/json",
                "payload": { "$ref": "#/components/schemas/RtSubscribeRequestBody" },
            },
            "RtUnsubscribeRequest": {
                "name": "rt.unsubscribe",
                "title": "Unsubscribe from a topic",
                "contentType": "application/json",
                "payload": { "$ref": "#/components/schemas/RtUnsubscribeRequestBody" },
            },
            "RtPingRequest": {
                "name": "rt.ping",
                "title": "Keepalive ping",
                "contentType": "application/json",
                "payload": { "$ref": "#/components/schemas/RtPingRequestBody" },
            },
            // ── Responses ───────────────────────────────────────────
            "RtSubscribedResponse": {
                "name": "rt.subscribed",
                "title": "Subscribe / unsubscribe ack",
                "contentType": "application/json",
                "payload": { "$ref": "#/components/schemas/RtSuccessResponseBody" },
            },
            "RtErrorResponse": {
                "name": "rt.error",
                "title": "JSON-RPC error object",
                "contentType": "application/json",
                "payload": { "$ref": "#/components/schemas/RtErrorResponseBody" },
            },
            "RtPongResponse": {
                "name": "rt.pong",
                "title": "Reply to rt.ping — `result.pong == true`",
                "contentType": "application/json",
                "payload": { "$ref": "#/components/schemas/RtPongResponseBody" },
            },
            // ── Notifications (server → client) ─────────────────────
            "RtFolderEventNotification": {
                "name": "rt.event",
                "title": "Folder mutation event",
                "contentType": "application/json",
                "payload": { "$ref": "#/components/schemas/RtFolderEventBody" },
            }
        },
        "schemas": {
            "RtSubscribeRequestBody": rpc_request_schema("rt.subscribe", topic_params_schema()),
            "RtUnsubscribeRequestBody": rpc_request_schema("rt.unsubscribe", topic_params_schema()),
            "RtPingRequestBody": rpc_request_schema("rt.ping", json!({ "type": "null" })),
            "RtSuccessResponseBody": rpc_success_response_schema(),
            "RtPongResponseBody": rpc_pong_response_schema(),
            "RtErrorResponseBody": rpc_error_response_schema(),
            "RtFolderEventBody": folder_event_notification_schema(),
            "FileCreatedData": file_created_schema(),
            "FileRenamedData": file_renamed_schema(),
            "FileMovedData": file_moved_schema(),
            "FileDeletedData": file_deleted_schema(),
            "FolderCreatedData": folder_created_schema(),
            "FolderRenamedData": folder_renamed_schema(),
            "FolderMovedData": folder_moved_schema(),
            "FolderDeletedData": folder_deleted_schema(),
        },
        // How the client authenticates. Handler side is `auth_middleware`
        // — the same middleware every `/api/*` request goes through, so
        // any JWT valid for REST is valid for WS.
        "securitySchemes": {
            "bearerAuth": {
                "type": "http",
                "scheme": "bearer",
                "bearerFormat": "JWT",
                "description": "OxiCloud JWT — same access_token minted by `POST /api/auth/login` (or the OPAQUE handshake). Programmatic clients set `Authorization: Bearer <jwt>` on the WS upgrade request. Browsers, which cannot set headers on `new WebSocket()`, will use the deferred ticket flow (`POST /api/rt/ticket` → short-lived one-shot ticket in the WS URL); see the plan's DPoP-gap section.",
            }
        }
    })
}

// ─── Schema builders ────────────────────────────────────────────────────────

fn rpc_request_schema(method: &str, params_schema: Value) -> Value {
    json!({
        "type": "object",
        "required": ["jsonrpc", "id", "method"],
        "properties": {
            "jsonrpc": { "type": "string", "const": "2.0" },
            "id":      { "type": ["integer", "string", "null"] },
            "method":  { "type": "string", "const": method },
            "params":  params_schema,
        }
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
            "result":  { "type": "object" },
        }
    })
}

/// Reply to `rt.ping` — the shape pins `result.pong == true` so
/// contract tests can assert on it directly.
fn rpc_pong_response_schema() -> Value {
    json!({
        "type": "object",
        "required": ["jsonrpc", "id", "result"],
        "properties": {
            "jsonrpc": { "type": "string", "const": "2.0" },
            "id":      { "type": ["integer", "string", "null"] },
            "result": {
                "type": "object",
                "required": ["pong"],
                "properties": {
                    "pong": { "type": "boolean", "const": true }
                }
            },
        }
    })
}

fn rpc_error_response_schema() -> Value {
    // The `code`/`message` catalog is the stable public vocabulary —
    // any change here IS a wire break. Every entry mirrors
    // `application/ports/realtime_ports.rs::error_code`.
    json!({
        "type": "object",
        "required": ["jsonrpc", "id", "error"],
        "properties": {
            "jsonrpc": { "type": "string", "const": "2.0" },
            "id":      { "type": ["integer", "string", "null"] },
            "error": {
                "type": "object",
                "required": ["code", "message"],
                "properties": {
                    "code": {
                        "type": "integer",
                        "enum": [
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
                        ],
                    },
                    "message": {
                        "type": "string",
                        "description": "Stable wire vocabulary; matches the `code`.",
                        "enum": [
                            "no_read", "no_share", "no_comment", "topic_forbidden",
                            "sub_limit", "rate_limited", "no_edit",
                            "internal_error", "invalid_request",
                            "method_not_found", "invalid_params",
                        ],
                    },
                    "data": {
                        "type": "object",
                        "description": "Optional caller-facing context (e.g. offending topic).",
                    }
                }
            }
        }
    })
}

fn folder_event_notification_schema() -> Value {
    json!({
        "type": "object",
        "description": "JSON-RPC notification (no `id`). `method = \"rt.event\"`.",
        "required": ["jsonrpc", "method", "params"],
        "properties": {
            "jsonrpc": { "type": "string", "const": "2.0" },
            "method":  { "type": "string", "const": "rt.event" },
            "params": {
                "type": "object",
                "required": ["topic", "event", "data"],
                "properties": {
                    "topic": { "type": "string" },
                    "event": {
                        "type": "string",
                        "enum": [
                            "file_created", "file_renamed", "file_moved", "file_deleted",
                            "folder_created", "folder_renamed", "folder_moved", "folder_deleted",
                        ],
                    },
                    "data": {
                        "oneOf": [
                            { "$ref": "#/components/schemas/FileCreatedData" },
                            { "$ref": "#/components/schemas/FileRenamedData" },
                            { "$ref": "#/components/schemas/FileMovedData" },
                            { "$ref": "#/components/schemas/FileDeletedData" },
                            { "$ref": "#/components/schemas/FolderCreatedData" },
                            { "$ref": "#/components/schemas/FolderRenamedData" },
                            { "$ref": "#/components/schemas/FolderMovedData" },
                            { "$ref": "#/components/schemas/FolderDeletedData" },
                        ]
                    }
                }
            }
        }
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
