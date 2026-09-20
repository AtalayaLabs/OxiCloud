// JSON-RPC 2.0 frame builders + parsers for the message bus.
//
// Pure functions — no I/O, no state, no side effects. Sits between
// `client.svelte.ts` (owns the WebSocket + subscription refcounts) and
// the generated wire DTOs under `$lib/generated/message-bus/`. Keeping
// the framing logic isolated makes it directly unit-testable and keeps
// `client.svelte.ts` focused on lifecycle.
//
// One-way import direction: this file reads from `$lib/generated/…`;
// nothing under `generated/` imports from here.

import type RtSubscribeRequestBody from '$lib/generated/message-bus/RtSubscribeRequestBody';
import type RtUnsubscribeRequestBody from '$lib/generated/message-bus/RtUnsubscribeRequestBody';
import type RtPingRequestBody from '$lib/generated/message-bus/RtPingRequestBody';
import type RtEventParams from '$lib/generated/message-bus/RtEventParams';
import type RtRevokedParams from '$lib/generated/message-bus/RtRevokedParams';
import type RtErrorObject from '$lib/generated/message-bus/RtErrorObject';

/**
 * Discriminated result of parsing one text frame off the wire.
 *
 * A well-formed frame lands as `event`, `revoked`, `success`, or
 * `error`. Anything the client should silently drop (a malformed
 * payload, an unknown notification method, a frame with the wrong
 * `jsonrpc` version) collapses to `ignore` with a `reason` so the
 * logger can surface it at `warn` without the caller having to
 * distinguish.
 */
export type IncomingFrame =
	| { kind: 'event'; params: RtEventParams }
	| { kind: 'revoked'; params: RtRevokedParams }
	| { kind: 'write_denied'; params: RtWriteDeniedParams }
	| { kind: 'success'; id: number; result: unknown }
	| { kind: 'error'; id: number | null; error: RtErrorObject }
	| { kind: 'ignore'; reason: string; raw: unknown };

/** `rt.write_denied` notification — the server received a collab
 *  UPDATE frame the caller lacks Update for. Emitted per-frame, so a
 *  Viewer's local edit path can drop into read-only without the
 *  socket flapping (the sub itself stays valid — only the individual
 *  UPDATE is refused). `file_id` scopes to one collab session;
 *  `reason` is the stable machine key (today: `"no_edit"`). */
export interface RtWriteDeniedParams {
	file_id: string;
	reason: string;
}

/** JSON-RPC subscribe request. `id` correlates the eventual success/error. */
export function subscribeFrame(id: number, topic: string): RtSubscribeRequestBody {
	return {
		jsonrpc: '2.0',
		id,
		method: 'rt.subscribe',
		params: { topic }
	};
}

/** JSON-RPC unsubscribe request. */
export function unsubscribeFrame(id: number, topic: string): RtUnsubscribeRequestBody {
	return {
		jsonrpc: '2.0',
		id,
		method: 'rt.unsubscribe',
		params: { topic }
	};
}

/** JSON-RPC application-level ping. The server also issues protocol-level
 *  RFC 6455 Pings on its own timer (keepalive); this request is available
 *  for the client to probe round-trip latency on demand. */
export function pingFrame(id: number): RtPingRequestBody {
	return { jsonrpc: '2.0', id, method: 'rt.ping' };
}

/**
 * Parse one inbound text frame. Never throws — every unrecoverable
 * shape maps to `{kind: 'ignore', reason, raw}` so the caller can log
 * once and move on. The caller decides whether an ignored frame is
 * noise (double-ping) or a bug (unknown method).
 */
export function parseIncoming(raw: string): IncomingFrame {
	let parsed: unknown;
	try {
		parsed = JSON.parse(raw);
	} catch {
		return { kind: 'ignore', reason: 'not_json', raw };
	}
	if (!isJsonObject(parsed)) {
		return { kind: 'ignore', reason: 'not_object', raw };
	}
	if (parsed.jsonrpc !== '2.0') {
		return { kind: 'ignore', reason: 'wrong_jsonrpc_version', raw };
	}

	// Notification (server → client, no id).
	if (typeof parsed.method === 'string') {
		if (parsed.method === 'rt.event' && isJsonObject(parsed.params)) {
			return { kind: 'event', params: parsed.params as unknown as RtEventParams };
		}
		if (parsed.method === 'rt.revoked' && isJsonObject(parsed.params)) {
			return { kind: 'revoked', params: parsed.params as unknown as RtRevokedParams };
		}
		if (parsed.method === 'rt.write_denied' && isJsonObject(parsed.params)) {
			// Shape guard: keep parse strict — a missing file_id would
			// crash the dispatch. Type-narrowing before the cast.
			if (typeof parsed.params.file_id === 'string' && typeof parsed.params.reason === 'string') {
				return {
					kind: 'write_denied',
					params: {
						file_id: parsed.params.file_id,
						reason: parsed.params.reason
					}
				};
			}
			return { kind: 'ignore', reason: 'write_denied_bad_params', raw };
		}
		return { kind: 'ignore', reason: `unknown_method:${parsed.method}`, raw };
	}

	// Response to one of our requests.
	const id = typeof parsed.id === 'number' ? parsed.id : null;
	if (parsed.error !== undefined) {
		if (!isJsonObject(parsed.error)) {
			return { kind: 'ignore', reason: 'error_not_object', raw };
		}
		return { kind: 'error', id, error: parsed.error as unknown as RtErrorObject };
	}
	if (parsed.result !== undefined && id !== null) {
		return { kind: 'success', id, result: parsed.result };
	}
	return { kind: 'ignore', reason: 'malformed_response', raw };
}

function isJsonObject(v: unknown): v is Record<string, unknown> {
	return typeof v === 'object' && v !== null && !Array.isArray(v);
}
