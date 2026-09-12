// Named constants for JSON-RPC 2.0 error codes emitted on the message-bus
// WebSocket wire. Mirrors `application/ports/message_bus_ports.rs::error_code`
// on the server — the Rust module is the source of truth.
//
// Hand-written, deliberately not generated: Modelina projects a JSON-Schema
// `enum` of numeric values into a TS enum with mangled member names
// (`MINUS_32001 = -32001`), which reads worse than no enum at all. The wire
// type is just `number`; readable name-to-code lookup lives here.
//
// Values are frozen across releases — a new denial cause gets a new value,
// never repurposes an existing one. Adding a code: bump the Rust module and
// this file in the same commit; the wire spec's description text is a
// derivative of the Rust constants (see `generate-asyncapi.rs`).

/**
 * Application-defined codes live in the JSON-RPC 2.0 server-defined range
 * `-32099..-32000`; standard envelope codes live in `-32700..-32600`.
 */
export const RtErrorCode = {
	// ── Application-defined (subscribe / edit path denials) ─────────────
	/** Resource-scoped topic, caller lacks Read on the resource (or the
	 *  resource does not exist — the two outcomes are indistinguishable to
	 *  the caller by design, to preserve anti-enumeration). */
	NO_READ: -32001,
	/** Resource-scoped topic requires Share, caller has Read but not Share.
	 *  Applies to `file:{id}:shares` (Phase B). */
	NO_SHARE: -32002,
	/** Resource-scoped topic requires Comment (`file:{id}:comments`, Phase B). */
	NO_COMMENT: -32003,
	/** Identity-scoped mismatch, OR unknown/malformed topic. Same wire code
	 *  regardless of whether the target exists — anti-enum. */
	TOPIC_FORBIDDEN: -32004,
	/** Per-connection subscription cap hit. */
	SUB_LIMIT: -32005,
	/** Subscribe-frame token bucket exhausted. */
	RATE_LIMITED: -32006,
	/** CRDT edit frame from a caller without Edit on the doc. Emitted as an
	 *  `rt.write_denied` notification (not tied to a request id). */
	NO_EDIT: -32007,

	// ── JSON-RPC 2.0 standard envelope codes ────────────────────────────
	/** Server-side failure the client should retry. */
	INTERNAL_ERROR: -32603,
	/** Malformed JSON-RPC envelope (missing `method`, wrong `jsonrpc` version). */
	INVALID_REQUEST: -32600,
	/** Method outside the `rt.*` allow-list. */
	METHOD_NOT_FOUND: -32601,
	/** Method known but `params` shape wrong (missing `topic`, unparseable). */
	INVALID_PARAMS: -32602
} as const satisfies Record<string, number>;

/** Union of every named code's numeric value. Narrows a bare `number` on
 *  `RtErrorObject.code` to the eleven known literals for exhaustive
 *  `switch` blocks. */
export type RtErrorCodeValue = (typeof RtErrorCode)[keyof typeof RtErrorCode];
