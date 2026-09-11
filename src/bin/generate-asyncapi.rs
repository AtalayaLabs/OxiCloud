//! AsyncAPI 3.0 spec generator for the message bus.
//!
//! Mirrors `generate-openapi.rs`: constructs the spec from the same
//! Rust enums the server uses (`Topic`, `MessageBusEvent`, JSON-RPC
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
        "asyncapi": "3.0.0",
        "info": {
            "title":   "OxiCloud message bus",
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
                "description": "OxiCloud message bus WebSocket endpoint. Text frames are JSON-RPC 2.0. Binary frames (out of AsyncAPI scope) are Yjs sync protocol for the collab editor — see `docs/plan/markdown-collab.md`.",
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
                //     `new WebSocket()`). See `docs/plan/message-bus.md § F`.
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
        "receiveRevoked": {
            "action": "receive",
            "channel": { "$ref": "#/channels/Folder" },
            "summary": "Server-initiated eviction of a subscription (grant revoked, resource deleted, etc.). Client stops rendering the topic.",
            "messages": [
                { "$ref": "#/channels/Folder/messages/RevokedNotification" }
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
    let mut components = json!({
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
            },
            "RtRevokedNotification": {
                "name": "rt.revoked",
                "title": "Subscription evicted",
                "contentType": "application/json",
                "payload": { "$ref": "#/components/schemas/RtRevokedBody" },
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
                "description": "Browser path — the FE first calls `POST /api/rt/ticket` under the full REST middleware stack (auth + DPoP-proofed request), receives an opaque one-shot UUID with a 30 s TTL, then sets `Sec-WebSocket-Protocol: oxi.ticket.<uuid>` on the WS upgrade. The server redeems the ticket (single-use — a second attempt fails) and treats the WS session as authenticated for the caller who issued it. See `docs/plan/message-bus.md § F` and `handlers/rt_ticket_handler.rs`.",
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
