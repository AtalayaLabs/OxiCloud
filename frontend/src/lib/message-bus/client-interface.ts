// Public interface for the message-bus client.
//
// Extracted from `client.svelte.ts` so a second implementation (the
// Gate 2 SharedWorker bridge) can wrap the same wire-protocol
// contract without copy-pasting the surface and letting the two
// drift. See `docs/plan/message-bus-via-shared-worker.md` — the
// doc's "One core, two harnesses" invariant lives here.
//
// This file carries NO behaviour. Every runtime detail (reconnect
// backoff, visibility handling, DPoP-ticket exchange) stays in the
// per-tab client; the SharedWorker variant will host its own copy
// of the client, and expose this same interface via a port bridge.
//
// Related:
//   * `client.svelte.ts`      — per-tab implementation.
//   * `port-protocol.ts`      — port envelope types (Gate 2 wiring).
//   * `frames.ts`             — JSON-RPC wire framing (pure).
//   * `docs/plan/message-bus-via-shared-worker.md` § "One core, two harnesses"

import type { RtWriteDeniedParams } from './frames';
import type RtEventParams from '$lib/api/generated/asyncapi/RtEventParams';
import type RtRevokedParams from '$lib/api/generated/asyncapi/RtRevokedParams';

/** Reactive connection state. `idle` before the first `subscribe`;
 *  `connecting` while the handshake is in flight; `connected` once
 *  the server has accepted the upgrade; `disconnected` after any
 *  close (reconnect fires from the client); `unavailable` after the
 *  reconnect circuit breaker trips — the client has stopped
 *  auto-retrying, and the UI should invite the user to refresh. */
export type ConnectionState = 'idle' | 'connecting' | 'connected' | 'disconnected' | 'unavailable';

/** Callback invoked for every `rt.event` notification on a topic. */
export type EventHandler = (params: RtEventParams) => void;

/** Shape of the `result` object in a successful `rt.subscribe` reply.
 *  `subscribed` echoes the topic wire form; `capabilities` is present
 *  today only on `collab:{fileId}` topics and carries the caller's
 *  per-file editing capabilities (see `RtCollabCapabilities`). Absent
 *  on every other topic. */
export interface RtSubscribeAck {
	subscribed: string;
	capabilities?: RtCollabCapabilities;
}

/** Per-file editing capabilities the server surfaces on a
 *  `collab:{fileId}` subscribe ack. `can_write` reflects the caller's
 *  `Permission::Update` on the file at subscribe time — the FE gates
 *  CodeMirror between edit and read-only modes on this flag. */
export interface RtCollabCapabilities {
	can_write: boolean;
}

/** Called with the `rt.subscribe` reply's `result` when the server
 *  acks the topic. Fired once per full subscribe cycle — the initial
 *  ack AND every replay after reconnect. Consumers that only care
 *  about the initial ack should self-latch after the first fire. */
export type SubscribeAckHandler = (result: RtSubscribeAck) => void;

/** Callback invoked when the server sends `rt.revoked` for a topic —
 *  the subscription is already gone server-side by the time the frame
 *  arrives; the client removes it from the local refcount map and
 *  fires this so the consumer can toast / redirect / whatever. */
export type RevokedHandler = (params: RtRevokedParams) => void;

/** Callback invoked when the WS reconnects AFTER a prior disconnect —
 *  never on the first connect. Fires after client-side sub replay has
 *  been kicked off (`#sendSubscribe` for every known topic), so the
 *  handler can safely call `reload()`-style refetches knowing the
 *  post-reconnect event stream is armed. Bridges the "events published
 *  during the disconnect window are lost" gap — see
 *  `project_message_bus_reconnect_gap` memory. */
export type ReconnectHandler = () => void;

/** Handle returned by `subscribe`. Call to release one refcount on the
 *  topic; the client unsubscribes over the wire only when the last
 *  refcount drops. Idempotent — calling twice from the same subscriber
 *  is safe (second call is a no-op). */
export type UnsubscribeHandle = () => void;

/**
 * Shape returned by a rejected JSON-RPC call. Structurally a superset
 * of `RtErrorObject` — every server-side error slots in, and this
 * type also lets the client raise synthetic errors (`ws_closed`,
 * `send_failed`, `not_connected`) whose `message` is a plain string
 * outside the wire's `RtErrorMessage` enum.
 */
export interface MessageBusError {
	code: number;
	message: string;
	data?: unknown;
}

/**
 * The public contract every message-bus implementation must satisfy.
 *
 * Both the current per-tab client (`PerTabMessageBusClient` in
 * `client.svelte.ts`) and the Gate 2 SharedWorker bridge
 * (`SharedWorkerBridge`, not yet written) implement this. Consumers
 * import the `messageBus` singleton typed against this interface —
 * so a switch from per-tab to shared mode is invisible at every call
 * site.
 *
 * The two reactive fields (`state`, `latencyMs`) are declared with
 * `readonly` here; implementations back them with Svelte 5 `$state`
 * runes so UIs reactively re-render on changes. The `readonly` makes
 * the compiler reject consumer-side writes without preventing the
 * class from mutating its own rune.
 */
export interface MessageBusClient {
	/** Reactive connection state. Driven by the implementation; consumers
	 *  read it to render a connection badge or gate collab editing. */
	readonly state: ConnectionState;

	/** Last observed round-trip in ms, updated on each `rt.pong`.
	 *  `null` until the first ping completes. */
	readonly latencyMs: number | null;

	/**
	 * Refcounted subscribe. First refcount on a topic sends `rt.subscribe`
	 * on the wire; the returned handle drops that ref — last ref out
	 * sends `rt.unsubscribe`.
	 */
	subscribe(
		topic: string,
		onEvent: EventHandler,
		onRevoked?: RevokedHandler,
		onAcked?: SubscribeAckHandler
	): UnsubscribeHandle;

	/** Resolves once the socket is open. Never rejects — a connection that
	 *  never arrives leaves the promise pending, and the implementation's
	 *  own `state` surface is what tells the UI something is wrong. */
	whenConnected(): Promise<void>;

	/** Fires on the SECOND-onwards successful connect — not the first. The
	 *  returned handle removes the callback. */
	onReconnect(cb: ReconnectHandler): () => void;

	/** Force a fresh reconnect. Also the escape hatch after the circuit
	 *  breaker trips. */
	reconnect(): void;

	/** Tear down. Mostly a test hook — the singleton lives for the lifetime
	 *  of the tab in production. */
	close(): void;

	/** Register a binary-frame handler for one `file_id`. One handler per
	 *  file; the returned handle unregisters. */
	registerBinaryHandler(fileId: string, handler: (bytes: Uint8Array) => void): () => void;

	/** Register an `rt.write_denied` handler for one `file_id`. Fires when
	 *  the server refused a collab UPDATE frame (Read granted, Update
	 *  not). The returned handle unregisters. */
	registerWriteDeniedHandler(
		fileId: string,
		handler: (params: RtWriteDeniedParams) => void
	): () => void;

	/** Send a binary frame upstream. Returns `false` if the WS is not open
	 *  (frame dropped), `true` on best-effort send. Frames sent while
	 *  `state !== 'connected'` are intentionally lost — see the per-tab
	 *  client's `sendBinary` docstring for the rationale. */
	sendBinary(bytes: Uint8Array): boolean;

	/** Round-trip ping. Resolves with the observed latency in ms (also
	 *  cached into `latencyMs`). */
	ping(): Promise<number>;

	/** Flush pending collab writes for a file through the server — asks
	 *  the server to drain its coalescing buffer immediately. Returns
	 *  `null` if the WS is not connected (caller should refetch). */
	collabFlush(fileId: string): Promise<{ flushed: boolean } | null>;
}
