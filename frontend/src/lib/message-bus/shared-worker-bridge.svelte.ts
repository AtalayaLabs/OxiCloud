// Tab-side bridge: speaks the `MessageBusClient` interface to its
// consumers and proxies every call to the SharedWorker over a
// `MessagePort`.
//
// One `SharedWorkerBridge` per tab. The SharedWorker lives for the
// browser; every tab that opens one gets its own port into the
// worker's one WS. See `docs/plan/message-bus-via-shared-worker.md`
// for the shared-worker lifecycle.
//
// Companions:
//   * `client-interface.ts`  — the interface this file implements.
//   * `port-protocol.ts`     — the envelope language on the port.
//   * `worker-core.ts`       — the other side of this bridge, running
//                               in the SharedWorker.

import log from 'loglevel';

import type {
	ConnectionState,
	EventHandler,
	MessageBusClient,
	ReconnectHandler,
	RevokedHandler,
	RtCollabCapabilities,
	RtSubscribeAck,
	SubscribeAckHandler,
	UnsubscribeHandle
} from './client-interface';
import { RtErrorCode } from './error-codes';
import type { RtWriteDeniedParams } from './frames';
import type RtEventParams from '$lib/api/generated/asyncapi/RtEventParams';
import type RtRevokedParams from '$lib/api/generated/asyncapi/RtRevokedParams';
import {
	parsePortMessage,
	type LogLevel,
	type LogMessage,
	type PortTabToWorkerMessage,
	type PortWorkerToTabMessage
} from './port-protocol';

/** Logger — mirrors the per-tab client's `oxi:message-bus` namespace
 *  so a tab with debug on sees the bridge-side log stream in the
 *  same channel as the worker-side one. */
const busLog = log.getLogger('oxi:message-bus');

/** Topic → local handlers. Refcounted the same way `PerTabMessageBusClient`
 *  refcounts — multiple consumers on the same topic share one upstream
 *  subscribe (the worker tracks that side), but each consumer's
 *  callback runs independently on every inbound `event` envelope. */
interface SubEntry {
	count: number;
	handlers: Set<EventHandler>;
	revokedHandlers: Set<RevokedHandler>;
	ackedHandlers: Set<SubscribeAckHandler>;
	/** The ack result (if cached) so a late `onAcked` consumer still
	 *  fires exactly like the per-tab client's semantics. */
	cachedAck: RtSubscribeAck | undefined;
}

/** Pending request on the port: `postMessage` → `ack`/`nack` round-trip. */
interface PendingCall {
	resolve: (result: unknown) => void;
	reject: (error: { code: number; message: string; data?: unknown }) => void;
}

/**
 * Implements `MessageBusClient` over a `SharedWorker`.
 *
 * The runes `state` and `latencyMs` are tab-local reactive mirrors of
 * the worker's values — updated from inbound `state` / `ping-pong`
 * envelopes. Consumers' `$effect`s subscribe to them normally.
 *
 * Why no runes inside the SharedWorker core: a worker has no Svelte
 * component context and no DOM-side reactive graph consumer, so
 * $state there would be dead weight (and would drag `svelte/internal`
 * into the worker bundle). The core broadcasts plain `state`
 * envelopes; THIS file is where the rune lives.
 */
export class SharedWorkerBridge implements MessageBusClient {
	state = $state<ConnectionState>('idle');
	latencyMs = $state<number | null>(null);

	readonly #port: MessagePort;
	/** Monotonic reqId for request/response correlation. Independent
	 *  of the worker's upstream reqId space — the worker owns its
	 *  WS id-space; this bridge owns its port id-space. */
	#nextReqId = 1;

	/** Topic → local SubEntry. Local refcount on top of the worker's
	 *  cross-port refcount. The worker only sees one `subscribe` on
	 *  behalf of this tab (as long as the local refcount stays > 0);
	 *  that keeps the worker's own `topicSubs` honest. Plain Map —
	 *  internal plumbing, no reactive consumer should watch it. */
	// eslint-disable-next-line svelte/prefer-svelte-reactivity
	readonly #subs = new Map<string, SubEntry>();

	/** Pending `postMessage` request → resolver. Keyed by reqId. */
	// eslint-disable-next-line svelte/prefer-svelte-reactivity
	readonly #pending = new Map<number, PendingCall>();

	/** fileId → binary handler. Mirrors `PerTabMessageBusClient` —
	 *  one handler per file is the shape collab consumers rely on. */
	// eslint-disable-next-line svelte/prefer-svelte-reactivity
	readonly #binaryHandlers = new Map<string, (bytes: Uint8Array) => void>();

	/** fileId → write-denied handler. Same shape. */
	// eslint-disable-next-line svelte/prefer-svelte-reactivity
	readonly #writeDeniedHandlers = new Map<string, (params: RtWriteDeniedParams) => void>();

	/** Reconnect listeners. Fires on the SECOND-onwards `connected`
	 *  state transition — never on the first. Same contract as
	 *  per-tab mode so consumers do not special-case. */
	// eslint-disable-next-line svelte/prefer-svelte-reactivity
	readonly #reconnectHandlers = new Set<ReconnectHandler>();
	#hasConnectedBefore = false;

	/** Resolvers waiting on `whenConnected()`. Flushed on the next
	 *  `state === 'connected'` transition. */
	#connectedWaiters: Array<() => void> = [];

	/** Worker-reported version, captured from `hello-ack`. The caller
	 *  can read it via a future diagnostics hook; a mismatch across
	 *  reloads tells the operator a stale worker is holding the port. */
	workerVersion: string | null = null;

	/** True once we've received a `hello-ack` — guards against calls
	 *  racing the handshake. Lifecycle: set to true in `#onHelloAck`;
	 *  stays true forever (even if the worker dies, the bridge is
	 *  dead at that point too). */
	#handshakeComplete = false;
	#handshakeWaiters: Array<() => void> = [];

	constructor(port: MessagePort, opts?: { wantedLogLevel?: LogLevel }) {
		this.#port = port;
		port.onmessage = (event) => this.#onPortMessage(event.data);
		port.start();

		// Fire `hello` — tells the worker about this port's wanted log
		// level up-front so the push-down filter is correct from the
		// first log line.
		const reqId = this.#allocReqId();
		this.#post({
			kind: 'hello',
			reqId,
			wantedLogLevel: opts?.wantedLogLevel ?? 'info'
		});
	}

	// ============================================================
	// Public interface
	// ============================================================

	subscribe(
		topic: string,
		onEvent: EventHandler,
		onRevoked?: RevokedHandler,
		onAcked?: SubscribeAckHandler
	): UnsubscribeHandle {
		let entry = this.#subs.get(topic);
		const isFirstLocalRef = !entry;
		if (!entry) {
			entry = {
				count: 0,
				// Plain Sets — internal callback registries, not reactive.
				// eslint-disable-next-line svelte/prefer-svelte-reactivity
				handlers: new Set(),
				// eslint-disable-next-line svelte/prefer-svelte-reactivity
				revokedHandlers: new Set(),
				// eslint-disable-next-line svelte/prefer-svelte-reactivity
				ackedHandlers: new Set(),
				cachedAck: undefined
			};
			this.#subs.set(topic, entry);
		}
		entry.count += 1;
		entry.handlers.add(onEvent);
		if (onRevoked) entry.revokedHandlers.add(onRevoked);
		if (onAcked) {
			entry.ackedHandlers.add(onAcked);
			// Fire the ack immediately for a late joiner if it's cached.
			if (entry.cachedAck !== undefined) {
				try {
					onAcked(entry.cachedAck);
				} catch (err) {
					busLog.warn('onAcked handler threw', { topic, error: err });
				}
			}
		}

		if (isFirstLocalRef) {
			// First local ref on this topic — ask the worker to
			// subscribe. Later refs are purely local; the worker's
			// own refcount already covers them for this port.
			const reqId = this.#allocReqId();
			this.#callOverPort(reqId, { kind: 'subscribe', reqId, topic })
				.then((result) => {
					const ack = result as RtSubscribeAck;
					const live = this.#subs.get(topic);
					if (!live) return;
					live.cachedAck = ack;
					for (const h of live.ackedHandlers) {
						try {
							h(ack);
						} catch (err) {
							busLog.warn('onAcked handler threw', { topic, error: err });
						}
					}
				})
				.catch((err) => {
					busLog.warn('subscribe failed', { topic, error: err });
					const live = this.#subs.get(topic);
					if (!live) return;
					// Match the per-tab client's revoked-on-subscribe-denied
					// semantic — consumers rely on it to drop their state.
					const synthetic = {
						topic,
						reason: 'subscribe_denied'
					} as unknown as RtRevokedParams;
					for (const h of live.revokedHandlers) {
						try {
							h(synthetic);
						} catch (handlerErr) {
							busLog.warn('subscribe-denied handler threw', { topic, error: handlerErr });
						}
					}
					this.#subs.delete(topic);
				});
		}

		let released = false;
		return () => {
			if (released) return;
			released = true;
			this.#releaseOne(topic, onEvent, onRevoked, onAcked);
		};
	}

	whenConnected(): Promise<void> {
		if (this.state === 'connected') return Promise.resolve();
		return new Promise((resolve) => this.#connectedWaiters.push(resolve));
	}

	onReconnect(cb: ReconnectHandler): () => void {
		this.#reconnectHandlers.add(cb);
		let released = false;
		return () => {
			if (released) return;
			released = true;
			this.#reconnectHandlers.delete(cb);
		};
	}

	reconnect(): void {
		// Forwarded through a `call` so the worker side can drive its
		// own reconnect path (and so every tab's state rune picks up
		// the subsequent transitions through the normal broadcast).
		const reqId = this.#allocReqId();
		this.#post({ kind: 'call', reqId, method: 'rt.reconnect' });
	}

	close(): void {
		this.#port.close();
		this.#subs.clear();
		this.#binaryHandlers.clear();
		this.#writeDeniedHandlers.clear();
		for (const { reject } of this.#pending.values()) {
			reject({ code: RtErrorCode.INTERNAL_ERROR, message: 'bridge_closed' });
		}
		this.#pending.clear();
		this.#reconnectHandlers.clear();
		this.#connectedWaiters = [];
	}

	registerBinaryHandler(fileId: string, handler: (bytes: Uint8Array) => void): () => void {
		this.#binaryHandlers.set(fileId, handler);
		this.#post({ kind: 'register-binary-handler', fileId });
		let released = false;
		return () => {
			if (released) return;
			released = true;
			this.#binaryHandlers.delete(fileId);
			this.#post({ kind: 'unregister-binary-handler', fileId });
		};
	}

	registerWriteDeniedHandler(
		fileId: string,
		handler: (params: RtWriteDeniedParams) => void
	): () => void {
		this.#writeDeniedHandlers.set(fileId, handler);
		this.#post({ kind: 'register-write-denied-handler', fileId });
		let released = false;
		return () => {
			if (released) return;
			released = true;
			this.#writeDeniedHandlers.delete(fileId);
			this.#post({ kind: 'unregister-write-denied-handler', fileId });
		};
	}

	sendBinary(bytes: Uint8Array): boolean {
		// Copy into a dedicated ArrayBuffer so the transferable list is
		// safe to transfer: ownership on the sender ends on the next
		// line, which matches the per-tab client's "sendBinary is
		// fire-and-forget" semantics.
		const buf = bytes.buffer.slice(
			bytes.byteOffset,
			bytes.byteOffset + bytes.byteLength
		) as ArrayBuffer;
		try {
			this.#port.postMessage({ kind: 'send-binary', bytes: buf }, [buf]);
			return true;
		} catch (err) {
			busLog.warn('send-binary postMessage threw', { err });
			return false;
		}
	}

	async ping(): Promise<number> {
		const reqId = this.#allocReqId();
		return new Promise<number>((resolve, reject) => {
			this.#pending.set(reqId, {
				resolve: (r) => resolve((r as { latencyMs: number }).latencyMs),
				reject
			});
			this.#post({ kind: 'ping', reqId });
		});
	}

	async collabFlush(fileId: string): Promise<{ flushed: boolean } | null> {
		const reqId = this.#allocReqId();
		return new Promise<{ flushed: boolean } | null>((resolve, reject) => {
			this.#pending.set(reqId, {
				resolve: (r) => resolve(r as { flushed: boolean }),
				reject: (e) => {
					// Match per-tab semantics — `not_connected` returns
					// null so the caller refetches; everything else
					// bubbles.
					if (e.message === 'not_connected' || e.message === 'ws_closed') {
						resolve(null);
					} else {
						reject(e);
					}
				}
			});
			this.#post({ kind: 'call', reqId, method: 'rt.collab_flush', params: { file_id: fileId } });
		});
	}

	// ============================================================
	// Diagnostics hook — read-only accessor callers can wire into
	// `window.oxi.showBusDiagnostics()`. Low-cost; nothing changes
	// when it is read.
	// ============================================================

	diagnostics(): {
		state: ConnectionState;
		latencyMs: number | null;
		workerVersion: string | null;
		pendingCount: number;
		topicCount: number;
		binaryHandlerCount: number;
	} {
		return {
			state: this.state,
			latencyMs: this.latencyMs,
			workerVersion: this.workerVersion,
			pendingCount: this.#pending.size,
			topicCount: this.#subs.size,
			binaryHandlerCount: this.#binaryHandlers.size
		};
	}

	/** Promise resolved after the worker's `hello-ack` lands. Not
	 *  part of `MessageBusClient` — only `detect.ts`'s probe calls
	 *  it. See `trySharedWorker()`. */
	whenHandshakeComplete(): Promise<void> {
		if (this.#handshakeComplete) return Promise.resolve();
		return new Promise<void>((resolve) => this.#handshakeWaiters.push(resolve));
	}

	// ============================================================
	// Internals
	// ============================================================

	#releaseOne(
		topic: string,
		onEvent: EventHandler,
		onRevoked: RevokedHandler | undefined,
		onAcked: SubscribeAckHandler | undefined
	): void {
		const entry = this.#subs.get(topic);
		if (!entry) return;
		entry.count -= 1;
		entry.handlers.delete(onEvent);
		if (onRevoked) entry.revokedHandlers.delete(onRevoked);
		if (onAcked) entry.ackedHandlers.delete(onAcked);
		if (entry.count <= 0) {
			this.#subs.delete(topic);
			this.#post({ kind: 'unsubscribe', topic });
		}
	}

	#allocReqId(): number {
		return this.#nextReqId++;
	}

	#post(msg: PortTabToWorkerMessage, transfer?: Transferable[]): void {
		try {
			if (transfer && transfer.length > 0) {
				this.#port.postMessage(msg, transfer);
			} else {
				this.#port.postMessage(msg);
			}
		} catch (err) {
			busLog.warn('postMessage threw', { kind: msg.kind, err });
		}
	}

	#callOverPort(reqId: number, msg: PortTabToWorkerMessage): Promise<unknown> {
		return new Promise<unknown>((resolve, reject) => {
			this.#pending.set(reqId, { resolve, reject });
			this.#post(msg);
		});
	}

	#onPortMessage(raw: unknown): void {
		const msg = parsePortMessage(raw);
		if (!msg) {
			busLog.warn('bridge dropped unparseable port envelope', { raw });
			return;
		}
		if (!this.#isWorkerToTab(msg)) {
			busLog.warn('bridge dropped wrong-direction envelope', { kind: msg.kind });
			return;
		}
		switch (msg.kind) {
			case 'hello-ack':
				this.#onHelloAck(msg);
				return;
			case 'state':
				this.#onStateChange(msg.state);
				return;
			case 'ack': {
				const pending = this.#pending.get(msg.reqId);
				if (!pending) return;
				this.#pending.delete(msg.reqId);
				pending.resolve(msg.result);
				return;
			}
			case 'nack': {
				const pending = this.#pending.get(msg.reqId);
				if (!pending) return;
				this.#pending.delete(msg.reqId);
				pending.reject(msg.error);
				return;
			}
			case 'event':
				this.#onEvent(msg.topic, msg.params);
				return;
			case 'revoked':
				this.#onRevoked(msg.topic, msg.params);
				return;
			case 'binary':
				this.#onBinary(msg.fileId, msg.bytes);
				return;
			case 'write-denied':
				this.#onWriteDenied(msg.fileId, msg.params);
				return;
			case 'ping-pong': {
				const pending = this.#pending.get(msg.reqId);
				this.latencyMs = msg.latencyMs;
				if (!pending) return;
				this.#pending.delete(msg.reqId);
				pending.resolve({ latencyMs: msg.latencyMs });
				return;
			}
			case 'log':
				this.#onLog(msg);
				return;
		}
	}

	#onHelloAck(msg: { kind: 'hello-ack'; state: ConnectionState; workerVersion: string }): void {
		this.workerVersion = msg.workerVersion;
		this.#onStateChange(msg.state);
		this.#handshakeComplete = true;
		const waiters = this.#handshakeWaiters;
		this.#handshakeWaiters = [];
		for (const resolve of waiters) resolve();
	}

	#onStateChange(next: ConnectionState): void {
		const prev = this.state;
		if (prev === next) return;
		this.state = next;

		if (next === 'connected') {
			// Flush `whenConnected` waiters.
			const waiters = this.#connectedWaiters;
			this.#connectedWaiters = [];
			for (const r of waiters) r();

			// Reconnect contract — fire only on the SECOND-onwards
			// `connected` transition.
			if (this.#hasConnectedBefore) {
				for (const cb of this.#reconnectHandlers) {
					try {
						cb();
					} catch (err) {
						busLog.warn('reconnect handler threw', { err });
					}
				}
			} else {
				this.#hasConnectedBefore = true;
			}
		}
	}

	#onEvent(topic: string, params: RtEventParams): void {
		const entry = this.#subs.get(topic);
		if (!entry) return;
		for (const h of entry.handlers) {
			try {
				h(params);
			} catch (err) {
				busLog.warn('event handler threw', { topic, err });
			}
		}
	}

	#onRevoked(topic: string, params: RtRevokedParams): void {
		const entry = this.#subs.get(topic);
		if (!entry) return;
		// Worker already dropped the sub; mirror it here so a later
		// subscribe starts fresh.
		this.#subs.delete(topic);
		for (const h of entry.revokedHandlers) {
			try {
				h(params);
			} catch (err) {
				busLog.warn('revoked handler threw', { topic, err });
			}
		}
	}

	#onBinary(fileId: string, bytes: ArrayBuffer): void {
		const handler = this.#binaryHandlers.get(fileId);
		if (!handler) return;
		try {
			handler(new Uint8Array(bytes));
		} catch (err) {
			busLog.warn('binary handler threw', { fileId, err });
		}
	}

	#onWriteDenied(fileId: string, params: RtWriteDeniedParams): void {
		const handler = this.#writeDeniedHandlers.get(fileId);
		if (!handler) return;
		try {
			handler(params);
		} catch (err) {
			busLog.warn('write-denied handler threw', { fileId, err });
		}
	}

	/** Replay a worker-side log line into tab-local `loglevel`, so
	 *  DevTools in THIS tab paints it under the tab's own filter —
	 *  the per-tab log tail parity requirement from the plan. */
	#onLog(msg: LogMessage): void {
		const tabLog = log.getLogger(msg.namespace);
		switch (msg.level) {
			case 'trace':
				tabLog.trace(...msg.args);
				return;
			case 'debug':
				tabLog.debug(...msg.args);
				return;
			case 'info':
				tabLog.info(...msg.args);
				return;
			case 'warn':
				tabLog.warn(...msg.args);
				return;
			case 'error':
				tabLog.error(...msg.args);
				return;
		}
	}

	#isWorkerToTab(
		msg: PortTabToWorkerMessage | PortWorkerToTabMessage
	): msg is PortWorkerToTabMessage {
		switch (msg.kind) {
			case 'hello-ack':
			case 'state':
			case 'ack':
			case 'nack':
			case 'event':
			case 'revoked':
			case 'binary':
			case 'write-denied':
			case 'ping-pong':
			case 'log':
				return true;
			default:
				return false;
		}
	}
}

// `RtCollabCapabilities` is re-exported from `./client-interface` by
// consumers that want it; included here as a type-level anchor so the
// file compiles against the full extracted surface.
export type { RtCollabCapabilities };
