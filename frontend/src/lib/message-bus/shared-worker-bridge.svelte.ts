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

import { apiJson } from '$lib/api/client';
import { getCsrfHeaders } from '$lib/api/csrf';
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

/** Response body from `POST /api/rt/ticket`. */
interface RtTicketResponse {
	ticket: string;
	expires_in_seconds: number;
	subprotocol: string;
}

/** Logger — mirrors the per-tab client's `oxi:message-bus` namespace
 *  so a tab with debug on sees the bridge-side log stream in the
 *  same channel as the worker-side one. */
const busLog = log.getLogger('oxi:message-bus');

/** Client-side deadline for the ticket POST. The browser's native
 *  TCP-connect timeout is ~75 s, which keeps `apiJson` hanging on
 *  a dead-but-not-refusing server for the whole window — the worker
 *  only hears about failure via its own `NEED_TICKET_TIMEOUT_MS`
 *  backstop (5 s), so EVERY cycle during an outage costs 5 s even
 *  if the real failure would surface instantly. A 2 s client-side
 *  deadline closes that gap: on expiry we abort the fetch, catch
 *  the AbortError, and post `ticket-fetch-failed` so the worker
 *  schedules the next cycle immediately.
 *
 *  2 s is well above the P99 for healthy requests (localhost
 *  5–20 ms, behind a reverse proxy 50–200 ms, slow mobile network
 *  500 ms–1 s) and well below the browser's native timeout. */
const TICKET_FETCH_TIMEOUT_MS = 2_000;

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
		//
		// Default is `trace` (the lowest level) so the worker's push-
		// down filter admits everything by default; each tab's local
		// `loglevel` setting is what actually decides what prints to
		// its console. Without this, a tab calling
		// `oxi.setLogLevel('oxi:message-bus:worker', 'debug')` would
		// still get no debug lines because the port-level filter
		// defaulted to `info` and would reject them before fan-out.
		//
		// Trade-off vs. the earlier `info` default: slightly more
		// port traffic when no tab wants debug. For OxiCloud's log
		// volume this is negligible; a proper `set-log-level`
		// propagation wire belongs in a follow-up if the trade ever
		// becomes measurable.
		const reqId = this.#allocReqId();
		this.#post({
			kind: 'hello',
			reqId,
			wantedLogLevel: opts?.wantedLogLevel ?? 'trace'
		});

		// NO eager ticket primer. The bridge is constructed on every
		// page load, including unauthenticated routes (/login,
		// /register, /s/<share-token>). An eager `POST /api/rt/ticket`
		// on an anonymous session would 401 → apiFetch's refresh
		// interceptor retries → 401 → terminal auth failure handler
		// redirects to `/login?source=session_expired` → page
		// reload → bridge reconstructs → ticket POST → redirect →
		// infinite loop (observed 2026-10-24 on a fresh Chrome profile).
		//
		// The ticket POST happens ONLY on-demand — the first consumer
		// `.subscribe()` call drives the worker's `need-ticket` fan-out
		// to this port, and only then do we hit `/api/rt/ticket`.
		// Consumers are gated on `useTopic` / `useReconnect` which
		// skip when `serverConfig.features.message_bus === false`,
		// and in practice only run after login (AppShell etc.), so
		// unauthenticated pages never generate the first subscribe.
		//
		// Trade-off vs. the earlier eager primer: the first subscribe
		// pays one `need-ticket` round-trip (~5 ms over the local
		// MessagePort) before the WS can open. On the same page, that
		// cost is dwarfed by the subscribe's own RTT; invisible to
		// users.
	}

	// ============================================================
	// Ticket flow — the bridge owns the HTTP side; the worker owns
	// the WS side. The two cooperate via port envelopes.
	// ============================================================

	/** Fetch a fresh ticket from `/api/rt/ticket` via `apiFetch`
	 *  (DPoP + CSRF + session cookie + refresh-on-401 all handled)
	 *  and forward the resulting subprotocol to the worker via
	 *  `provide-ticket`. On throw, signal the worker synchronously
	 *  via `ticket-fetch-failed` so the circuit breaker counts the
	 *  failure immediately instead of waiting the 5 s need-ticket
	 *  budget.
	 *
	 *  Client-side AbortController deadline — see TICKET_FETCH_TIMEOUT_MS. */
	async #fetchAndProvideTicket(nonce: number): Promise<void> {
		const controller = new AbortController();
		const timer = setTimeout(() => controller.abort(), TICKET_FETCH_TIMEOUT_MS);
		try {
			const res = await apiJson<RtTicketResponse>('/api/rt/ticket', {
				method: 'POST',
				headers: getCsrfHeaders(),
				signal: controller.signal
			});
			this.#post({
				kind: 'provide-ticket',
				subprotocol: res.subprotocol,
				nonce
			});
		} catch (err) {
			// Three paths land here:
			//   * Server actively refusing (`ERR_CONNECTION_REFUSED`) —
			//     throws fast, `aborted` is false, next cycle schedules
			//     immediately via `ticket-fetch-failed`.
			//   * Server hanging on TCP SYN with no response — the
			//     browser's native TCP-connect timeout is ~75 s, far
			//     too long. The AbortController trips at
			//     TICKET_FETCH_TIMEOUT_MS and the fetch rejects with
			//     `AbortError`. `aborted` is true.
			//   * A real auth failure (403 anonymous, 401 after
			//     refresh failed) — throws with an HTTP status. The
			//     worker's circuit breaker trips after
			//     MAX_CONSECUTIVE_FAILURES. `aborted` is false.
			//
			// Either way, tell the worker RIGHT NOW so it doesn't sit
			// on the 5 s need-ticket timeout.
			const aborted = (err as { name?: string })?.name === 'AbortError';
			busLog.warn('ticket fetch failed — signalling worker (fast path)', {
				nonce,
				err,
				aborted
			});
			this.#post({ kind: 'ticket-fetch-failed', nonce });
		} finally {
			clearTimeout(timer);
		}
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
				.catch((err: { code?: number; message?: string } | undefined) => {
					// Transient transport errors → the sub is interrupted
					// mid-flight, NOT permanently denied. On the next
					// successful WS the worker replays its `activeTopics`
					// set upstream and we get an ack via the normal
					// subscribe-ack dispatch path (the bridge-local
					// `#subs` entry is intact — we haven't deleted it).
					// Firing `revokedHandlers` on a transient would make
					// `useTopic` + `useFolderTopic` + `collabDoc` tear
					// down UI state that will come right back on
					// reconnect, causing folder reloads and collab
					// editor flapping.
					//
					// Mirrors the per-tab client's `#failSubscribe`
					// discriminator (`err.message === 'ws_closed' |
					// 'not_connected' | 'send_failed'`). Adds
					// `'reconnect'` and `'bridge_closed'` because the
					// transport's aggressive reset throws those on
					// pending topic waiters when manually re-armed.
					//
					// Plain Set — short-lived local lookup, not
					// reactive state.
					// eslint-disable-next-line svelte/prefer-svelte-reactivity
					const transientMsgs = new Set([
						'ws_closed',
						'not_connected',
						'send_failed',
						'reconnect',
						'bridge_closed'
					]);
					const msg = err?.message;
					if (typeof msg === 'string' && transientMsgs.has(msg)) {
						busLog.debug(
							'subscribe interrupted by transport — will replay on reconnect',
							{ topic, error: err }
						);
						return;
					}
					busLog.warn('subscribe failed', { topic, error: err });
					const live = this.#subs.get(topic);
					if (!live) return;
					// Permanent denial — consumers rely on
					// `revokedHandlers` to drop their state.
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
		// Dedicated port envelope — NOT a JSON-RPC `call`. An earlier
		// revision used `{ kind: 'call', method: 'rt.reconnect' }`,
		// which the server rejected as an unknown method and the
		// worker never actually ran its own reconnect. The subsequent
		// state transitions still fan out via the worker's existing
		// `onStateChange` broadcast, so every tab's state rune
		// updates on the next cycle.
		this.#post({ kind: 'request-reconnect' });
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
			case 'need-ticket':
				// Worker needs a fresh ticket for the next WS upgrade.
				// Fire-and-forget: the worker also runs its own
				// `need-ticket` timeout so a failed fetch here surfaces
				// as a reconnect-cycle delay, not a hang.
				void this.#fetchAndProvideTicket(msg.nonce);
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
			case 'need-ticket':
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
