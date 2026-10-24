// Port-protocol envelopes for the Gate 2 SharedWorker bridge.
//
// Every message that crosses a `MessagePort` between a tab's bridge
// and the SharedWorker holding the single `/api/rt/ws` connection is
// one of the envelopes below. The two sides never touch
// `JSON.parse` / `JSON.stringify` — `postMessage` applies the
// structured-clone algorithm automatically — but every inbound
// envelope passes through `parsePortMessage` first so a malformed
// shape is dropped with a visible log line, never crashed on.
//
// Companion doc:
//   `docs/plan/message-bus-via-shared-worker.md § Port protocol`
//
// Gate 1 ships this module but no consumer; Gate 2 wires it into the
// worker + bridge. Keeping the type surface stable here lets the
// Vitest roundtrip tests pin wire-shape regressions before any
// runtime touches it.
//
// Related:
//   * `client-interface.ts`   — the public API the bridge implements.
//   * `frames.ts`             — JSON-RPC frames the worker speaks to the server.

import type { ConnectionState, MessageBusError, RtSubscribeAck } from './client-interface';
import type { RtWriteDeniedParams } from './frames';
import type RtEventParams from '$lib/api/generated/asyncapi/RtEventParams';
import type RtRevokedParams from '$lib/api/generated/asyncapi/RtRevokedParams';

/** `loglevel` log levels (string form). Mirrors the subset of
 *  `log.LogLevelDesc` the bus emits — explicit numeric levels and
 *  'silent' never appear in log envelopes. */
export type LogLevel = 'trace' | 'debug' | 'info' | 'warn' | 'error';

// ============================================================
// Tab → Worker envelopes
// ============================================================

/** Tab announces itself to the worker on connect. The worker replies
 *  with `hello-ack` carrying the current connection state so a
 *  late-joining tab doesn't have to wait for the next state
 *  transition to render the right badge. `wantedLogLevel` lets the
 *  worker's filter push-down (Gate 2 optimisation) skip emitting
 *  debug envelopes to tabs whose user doesn't want them. */
export interface HelloMessage {
	kind: 'hello';
	reqId: number;
	wantedLogLevel: LogLevel;
}

/** Tab asks the worker to subscribe to a topic. If the worker is
 *  already subscribed upstream, it serves the cached ack from its
 *  routing table and adds this port to the fan-out set. */
export interface SubscribeMessage {
	kind: 'subscribe';
	reqId: number;
	topic: string;
}

/** Tab asks the worker to drop its subscription. Idempotent;
 *  removing a port that was never added is a no-op. */
export interface UnsubscribeMessage {
	kind: 'unsubscribe';
	topic: string;
}

/** Tab registers for binary frames on a `file_id`. Multiple tabs
 *  can register on the same file. */
export interface RegisterBinaryHandlerMessage {
	kind: 'register-binary-handler';
	fileId: string;
}

export interface UnregisterBinaryHandlerMessage {
	kind: 'unregister-binary-handler';
	fileId: string;
}

export interface RegisterWriteDeniedHandlerMessage {
	kind: 'register-write-denied-handler';
	fileId: string;
}

export interface UnregisterWriteDeniedHandlerMessage {
	kind: 'unregister-write-denied-handler';
	fileId: string;
}

/** Tab sends a binary frame upstream. The `ArrayBuffer` is passed as
 *  a `postMessage` transferable so the one-hop tab → worker → server
 *  chain stays zero-copy. */
export interface SendBinaryMessage {
	kind: 'send-binary';
	bytes: ArrayBuffer;
}

/** Tab calls an upstream JSON-RPC method (today: `rt.collab_flush`,
 *  `rt.ping` goes through its own envelope for latency bookkeeping). */
export interface CallMessage {
	kind: 'call';
	reqId: number;
	method: string;
	params?: Record<string, unknown>;
}

/** Tab asks for a round-trip ping. The worker measures the WS
 *  round-trip latency and replies with `ping-pong` carrying it. */
export interface PingMessage {
	kind: 'ping';
	reqId: number;
}

/** Tab informs the worker of a `loglevel` change so the worker can
 *  push down the log-filter and skip emitting envelopes below the
 *  union of what any connected tab actually wants. */
export interface SetLogLevelMessage {
	kind: 'set-log-level';
	namespace: string;
	level: LogLevel;
}

/** Tab hands the worker a ready-to-use `Sec-WebSocket-Protocol` value
 *  for the next WS upgrade. The tab fetched the ticket itself via
 *  `apiFetch` — DPoP, CSRF, session cookie, refresh-on-401 all handled
 *  on the tab side where the browser's auth machinery lives. The
 *  worker never speaks HTTP.
 *
 *  `nonce === 0` indicates the initial ticket sent at handshake time.
 *  Non-zero values echo a `need-ticket` nonce: the worker uses them
 *  to ignore a late reply from a port after another port already
 *  provided a ticket for the same reconnect round. */
export interface ProvideTicketMessage {
	kind: 'provide-ticket';
	subprotocol: string;
	nonce: number;
}

/** Tab asks the worker to force a fresh reconnect of its single WS.
 *  Carries no body — the request is the signal. Routes through to
 *  `BusTransport.reconnect()`, which zeroes the circuit-breaker
 *  counter and schedules an immediate attempt.
 *
 *  NOT a server-side JSON-RPC call (an earlier revision confusingly
 *  used `{ kind: 'call', method: 'rt.reconnect' }`, which the server
 *  rejects as an unknown method — the worker never ran its own
 *  reconnect). This envelope is worker-local plumbing. */
export interface RequestReconnectMessage {
	kind: 'request-reconnect';
}

export type PortTabToWorkerMessage =
	| HelloMessage
	| SubscribeMessage
	| UnsubscribeMessage
	| RegisterBinaryHandlerMessage
	| UnregisterBinaryHandlerMessage
	| RegisterWriteDeniedHandlerMessage
	| UnregisterWriteDeniedHandlerMessage
	| SendBinaryMessage
	| CallMessage
	| PingMessage
	| SetLogLevelMessage
	| ProvideTicketMessage
	| RequestReconnectMessage;

// ============================================================
// Worker → Tab envelopes
// ============================================================

/** Reply to `hello` — tells the newly-connected tab the current
 *  connection state so it can paint the right badge immediately. */
export interface HelloAckMessage {
	kind: 'hello-ack';
	reqId: number;
	state: ConnectionState;
	/** The worker's build identity. Lets a tab whose expected version
	 *  doesn't match render a "reload all tabs" invite — in Gate 2 we
	 *  need to decide between fall-back-to-per-tab and user-reload. */
	workerVersion: string;
}

/** Broadcast on every connection state transition. */
export interface StateMessage {
	kind: 'state';
	state: ConnectionState;
}

/** Successful ack of a `subscribe` / `call` / `ping` reqId. */
export interface AckMessage {
	kind: 'ack';
	reqId: number;
	result: unknown;
}

/** Failed response to a reqId. `error` is the server's
 *  `RtErrorObject` for upstream failures, or a synthetic
 *  `{code, message}` for client-side failures (`ws_closed`,
 *  `send_failed`, `not_connected`). */
export interface NackMessage {
	kind: 'nack';
	reqId: number;
	error: MessageBusError;
}

/** Server `rt.event` fanned out to this port. */
export interface EventMessage {
	kind: 'event';
	topic: string;
	params: RtEventParams;
}

/** Server `rt.revoked` fanned out to this port. */
export interface RevokedMessage {
	kind: 'revoked';
	topic: string;
	params: RtRevokedParams;
}

/** Server binary frame fanned out to this port. Single-port fan-out
 *  uses `postMessage(msg, [bytes])` for zero-copy transfer; N-port
 *  fan-out structured-clones (one copy per port) because the
 *  transferable list is consumed by the first `postMessage`. */
export interface BinaryMessage {
	kind: 'binary';
	fileId: string;
	bytes: ArrayBuffer;
}

/** Server `rt.write_denied` fanned out to this port. */
export interface WriteDeniedMessage {
	kind: 'write-denied';
	fileId: string;
	params: RtWriteDeniedParams;
}

/** Pong carrying the measured latency for a prior `ping`. */
export interface PingPongMessage {
	kind: 'ping-pong';
	reqId: number;
	latencyMs: number;
}

/** Worker asks a connected port to fetch a fresh `/api/rt/ticket` and
 *  reply with a `provide-ticket` envelope. Sent on every reconnect
 *  attempt — the previous ticket is already consumed by the dropped
 *  WS. `nonce` must round-trip in the reply so the worker can ignore
 *  late replies after another port already served this round. */
export interface NeedTicketMessage {
	kind: 'need-ticket';
	nonce: number;
}

/** Worker-side `busLog` fan-out. The worker replaces its `loglevel`
 *  handler with an emitter that packages every log call as this
 *  envelope and sends it to every connected port. Each tab's bridge
 *  replays it into tab-local `loglevel` so DevTools shows the stream
 *  under the tab's own filter — the per-tab log tail parity
 *  requirement in `docs/plan/message-bus-via-shared-worker.md`. */
export interface LogMessage {
	kind: 'log';
	level: LogLevel;
	namespace: string;
	/** Must be structured-clone-safe. The worker-side emitter is
	 *  responsible for stripping non-cloneable args (Response, Error,
	 *  AbortSignal, functions) before packaging — bridges on the tab
	 *  side must not have to defend against them. */
	args: unknown[];
	/** `Date.now()` at emit time, in ms. Lets the receiving tab render
	 *  the DevTools line with the actual emit time rather than the
	 *  delivery time, which can be milliseconds-to-seconds later on a
	 *  backgrounded tab. */
	ts: number;
}

export type PortWorkerToTabMessage =
	| HelloAckMessage
	| StateMessage
	| AckMessage
	| NackMessage
	| EventMessage
	| RevokedMessage
	| BinaryMessage
	| WriteDeniedMessage
	| PingPongMessage
	| LogMessage
	| NeedTicketMessage;

/** Full union of every port envelope — tab-to-worker and
 *  worker-to-tab. The runtime typeguard `parsePortMessage` returns
 *  one of these on success, `null` otherwise. */
export type PortMessage = PortTabToWorkerMessage | PortWorkerToTabMessage;

/** Serialisable (structured-clone-safe) subset of the `RtSubscribeAck`
 *  shape. Re-exported here so the port-protocol stays the single
 *  source of truth for what crosses a port. */
export type { RtSubscribeAck };

// ============================================================
// Runtime typeguard
// ============================================================

const TAB_TO_WORKER_KINDS = new Set<string>([
	'hello',
	'subscribe',
	'unsubscribe',
	'register-binary-handler',
	'unregister-binary-handler',
	'register-write-denied-handler',
	'unregister-write-denied-handler',
	'send-binary',
	'call',
	'ping',
	'set-log-level',
	'provide-ticket',
	'request-reconnect'
]);

const WORKER_TO_TAB_KINDS = new Set<string>([
	'hello-ack',
	'state',
	'ack',
	'nack',
	'event',
	'revoked',
	'binary',
	'write-denied',
	'ping-pong',
	'log',
	'need-ticket'
]);

const LOG_LEVELS = new Set<string>(['trace', 'debug', 'info', 'warn', 'error']);

const CONNECTION_STATES = new Set<string>([
	'idle',
	'connecting',
	'connected',
	'disconnected',
	'unavailable'
]);

/** Narrow `unknown` to a known-shaped port envelope. Returns `null`
 *  for anything that doesn't match — the caller logs the drop so a
 *  wire-shape bug on the far side surfaces instead of silently
 *  routing garbage into the fan-out tables.
 *
 *  Each `kind` branch checks the fields that THIS code actually
 *  dereferences — enough to prevent runtime crashes, not a full
 *  schema validator. For example, we check `reqId` is a number on
 *  `hello` (it indexes `pending`) but don't drill into
 *  `RtEventParams` (the receiving handler owns that shape; the
 *  typeguard would just forward whatever came in). */
export function parsePortMessage(raw: unknown): PortMessage | null {
	if (typeof raw !== 'object' || raw === null) return null;
	const obj = raw as Record<string, unknown>;
	const kind = obj.kind;
	if (typeof kind !== 'string') return null;

	// Fast reject envelopes with an unknown kind — covers typos AND
	// future versions whose kinds we don't recognise yet.
	if (!TAB_TO_WORKER_KINDS.has(kind) && !WORKER_TO_TAB_KINDS.has(kind)) return null;

	switch (kind) {
		case 'hello': {
			if (typeof obj.reqId !== 'number') return null;
			if (typeof obj.wantedLogLevel !== 'string') return null;
			if (!LOG_LEVELS.has(obj.wantedLogLevel)) return null;
			return raw as HelloMessage;
		}
		case 'subscribe': {
			if (typeof obj.reqId !== 'number') return null;
			if (typeof obj.topic !== 'string') return null;
			return raw as SubscribeMessage;
		}
		case 'unsubscribe': {
			if (typeof obj.topic !== 'string') return null;
			return raw as UnsubscribeMessage;
		}
		case 'register-binary-handler':
		case 'unregister-binary-handler':
		case 'register-write-denied-handler':
		case 'unregister-write-denied-handler': {
			if (typeof obj.fileId !== 'string') return null;
			return raw as PortMessage;
		}
		case 'send-binary': {
			if (!(obj.bytes instanceof ArrayBuffer)) return null;
			return raw as SendBinaryMessage;
		}
		case 'call': {
			if (typeof obj.reqId !== 'number') return null;
			if (typeof obj.method !== 'string') return null;
			// `params` is optional. If present it must be an object; a
			// non-object `params` is a wire-shape bug worth dropping.
			if (obj.params !== undefined && (typeof obj.params !== 'object' || obj.params === null))
				return null;
			return raw as CallMessage;
		}
		case 'ping': {
			if (typeof obj.reqId !== 'number') return null;
			return raw as PingMessage;
		}
		case 'set-log-level': {
			if (typeof obj.namespace !== 'string') return null;
			if (typeof obj.level !== 'string') return null;
			if (!LOG_LEVELS.has(obj.level)) return null;
			return raw as SetLogLevelMessage;
		}
		case 'provide-ticket': {
			if (typeof obj.subprotocol !== 'string') return null;
			if (typeof obj.nonce !== 'number') return null;
			return raw as ProvideTicketMessage;
		}
		case 'request-reconnect': {
			// Body-less envelope; the kind IS the signal.
			return raw as RequestReconnectMessage;
		}
		case 'hello-ack': {
			if (typeof obj.reqId !== 'number') return null;
			if (typeof obj.state !== 'string') return null;
			if (!CONNECTION_STATES.has(obj.state)) return null;
			if (typeof obj.workerVersion !== 'string') return null;
			return raw as HelloAckMessage;
		}
		case 'state': {
			if (typeof obj.state !== 'string') return null;
			if (!CONNECTION_STATES.has(obj.state)) return null;
			return raw as StateMessage;
		}
		case 'ack': {
			if (typeof obj.reqId !== 'number') return null;
			// `result` is `unknown` — the caller's method-specific code
			// casts it. Anything that got here is at least a number-keyed
			// response to a call we initiated.
			return raw as AckMessage;
		}
		case 'nack': {
			if (typeof obj.reqId !== 'number') return null;
			if (typeof obj.error !== 'object' || obj.error === null) return null;
			const err = obj.error as Record<string, unknown>;
			if (typeof err.code !== 'number') return null;
			if (typeof err.message !== 'string') return null;
			return raw as NackMessage;
		}
		case 'event':
		case 'revoked': {
			if (typeof obj.topic !== 'string') return null;
			if (typeof obj.params !== 'object' || obj.params === null) return null;
			return raw as EventMessage | RevokedMessage;
		}
		case 'binary': {
			if (typeof obj.fileId !== 'string') return null;
			if (!(obj.bytes instanceof ArrayBuffer)) return null;
			return raw as BinaryMessage;
		}
		case 'write-denied': {
			if (typeof obj.fileId !== 'string') return null;
			if (typeof obj.params !== 'object' || obj.params === null) return null;
			return raw as WriteDeniedMessage;
		}
		case 'ping-pong': {
			if (typeof obj.reqId !== 'number') return null;
			if (typeof obj.latencyMs !== 'number') return null;
			return raw as PingPongMessage;
		}
		case 'log': {
			if (typeof obj.level !== 'string') return null;
			if (!LOG_LEVELS.has(obj.level)) return null;
			if (typeof obj.namespace !== 'string') return null;
			if (!Array.isArray(obj.args)) return null;
			if (typeof obj.ts !== 'number') return null;
			return raw as LogMessage;
		}
		case 'need-ticket': {
			if (typeof obj.nonce !== 'number') return null;
			return raw as NeedTicketMessage;
		}
	}
	// The `TAB_TO_WORKER_KINDS.has(kind) || WORKER_TO_TAB_KINDS.has(kind)` fast-reject above
	// covers every case label we have — but satisfy the compiler for the
	// theoretical future kind we add to the sets without a case.
	return null;
}

/**
 * Compile-time assertion helper.
 *
 * `Assert<T extends true>` is `never` on anything but `true` — a
 * downstream `type _ = Assert<Cond>` turns a failed condition into a
 * TypeScript error at the declaration site, visible in `npm run
 * check` and in the editor. Used by the Gate 2 SharedWorker bridge
 * conformance test to catch drift when a method is added to
 * `MessageBusClient` but not to the bridge (or vice versa).
 */
export type Assert<T extends true> = T;

/**
 * Structural `extends` check. `Equal<A, B>` is `true` iff A and B
 * are mutually assignable — safer than `A extends B` alone, which
 * only catches one direction of drift.
 */
export type Equal<A, B> =
	(<T>() => T extends A ? 1 : 2) extends <T>() => T extends B ? 1 : 2 ? true : false;
