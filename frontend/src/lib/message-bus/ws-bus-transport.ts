// `BusTransport` implementation that drives one real `/api/rt/ws`
// connection, with ticket exchange + reconnect/backoff/circuit
// breaker. Lives inside the SharedWorker (`worker.ts` instantiates
// it and passes it to `WorkerCore`).
//
// Mirrors the per-tab client's connection logic in
// `client.svelte.ts`, minus:
//
//   * Topic refcount — the WorkerCore owns the N-tab refcount; the
//     transport sees one `subscribe(topic)` per topic and tears it
//     down on `unsubscribe(topic)`.
//   * Svelte 5 runes — state transitions emit via `onStateChange`
//     callbacks; the bridge projects into runes on the tab side.
//   * Page visibility — a shared worker has no `document`; a
//     backgrounded tab shouldn't drop the shared WS for the OTHER
//     tabs on the browser.
//   * `apiJson` / `getCsrfHeaders` / `session.load()` — the worker
//     has no document.cookie / Svelte stores. The tab bridge
//     forwards the CSRF token on `hello` (Gate 2b2's wiring);
//     this transport takes it in the constructor.
//
// Companions:
//   * `worker-core.ts`     — the layer above (port routing).
//   * `client-interface.ts` — the public contract that eventually
//                             surfaces on the tab side.
//   * `frames.ts`          — JSON-RPC wire framing (reused as-is).

import log from 'loglevel';

import type { BusTransport } from './worker-core';
import type { ConnectionState, MessageBusError, RtSubscribeAck } from './client-interface';
import { RtErrorCode } from './error-codes';
import {
	parseIncoming,
	pingFrame,
	subscribeFrame,
	unsubscribeFrame,
	type IncomingFrame,
	type RtWriteDeniedParams
} from './frames';
import type RtEventParams from '$lib/api/generated/asyncapi/RtEventParams';
import type RtRevokedParams from '$lib/api/generated/asyncapi/RtRevokedParams';

const busLog = log.getLogger('oxi:message-bus:worker');

/** Response body from `POST /api/rt/ticket`. */
interface RtTicketResponse {
	ticket: string;
	expires_in_seconds: number;
	subprotocol: string;
}

/** Reconnect backoff — 250 ms doubling with full jitter, cap 30 s.
 *  Lifted verbatim from the per-tab client so the two paths' retry
 *  cadence stays identical. */
const RECONNECT_MIN_MS = 250;
const RECONNECT_MAX_MS = 30_000;

/** Circuit breaker — after N consecutive failed attempts give up
 *  and flip to `unavailable`. */
const MAX_CONSECUTIVE_FAILURES = 10;

/** Socket must stay open this long before we reset the backoff /
 *  circuit-breaker counters. */
const MIN_STABLE_MS = 3_000;

interface PendingCall {
	resolve: (result: unknown) => void;
	reject: (error: MessageBusError) => void;
}

/** Factory options for the transport. Everything is injectable so
 *  Vitest can drive the implementation against a mocked WS + fetch. */
export interface WsBusTransportOptions {
	/** `wss://host/api/rt/ws`. */
	wsUrl: string;
	/** `https://host/api/rt/ticket`. */
	ticketUrl: string;
	/** CSRF token the tab forwarded via `hello`. May be the empty
	 *  string if the tab is unauthenticated; the ticket POST then
	 *  takes a 403 and the circuit breaker trips on its own. */
	csrfToken: string;
	/** Injectable WebSocket constructor; tests pass a stub. */
	WebSocketCtor?: typeof WebSocket;
	/** Injectable fetch; tests pass a stub. */
	fetchFn?: typeof fetch;
}

/**
 * Single `/api/rt/ws` connection with ticket-based upgrade and the
 * same reconnect contract as the per-tab client.
 */
export class WsBusTransport implements BusTransport {
	#state: ConnectionState = 'idle';
	#latencyMs: number | null = null;

	readonly #opts: Required<WsBusTransportOptions>;

	#ws: WebSocket | null = null;
	/** Monotonic connect-attempt id; a superseded attempt bails. */
	#connectGen = 0;
	#backoffMs = RECONNECT_MIN_MS;
	#reconnectTimer: ReturnType<typeof setTimeout> | null = null;
	#consecutiveFailures = 0;
	#stableTimer: ReturnType<typeof setTimeout> | null = null;

	// JSON-RPC pending — a flat Map keyed by upstream reqId. The
	// transport owns its own reqId space (not shared with the tab
	// side), so no port disambiguation is needed.
	readonly #pending = new Map<number, PendingCall>();
	#nextId = 1;

	// Known-active topics. Tracked so a reconnect replays them.
	// Not refcounted — the WorkerCore above handles that; this
	// is just "what are we subscribed to upstream right now".
	readonly #activeTopics = new Set<string>();

	// reqId → topic lookup for in-flight subscribe frames. On a
	// `success` response the dispatcher consults this map first;
	// a hit resolves the topic waiter below, a miss falls through
	// to the generic `#pending` dispatch.
	readonly #pendingSubscribeFrames = new Map<number, string>();

	// topic → resolver for `.subscribe(topic)`. One in-flight
	// waiter per topic; `.subscribe()` returns the existing promise
	// if a prior caller is already waiting on the same topic.
	readonly #topicWaiters = new Map<
		string,
		{
			promise: Promise<RtSubscribeAck>;
			resolve: (a: RtSubscribeAck) => void;
			reject: (e: MessageBusError) => void;
		}
	>();

	// Listener registries. Multiple WorkerCore wirings is unusual
	// (we expect exactly one), but the interface promises a disposer
	// so support N for free.
	readonly #eventListeners = new Set<(topic: string, params: RtEventParams) => void>();
	readonly #revokedListeners = new Set<(topic: string, params: RtRevokedParams) => void>();
	readonly #binaryListeners = new Set<(fileId: string, bytes: ArrayBuffer) => void>();
	readonly #writeDeniedListeners = new Set<(fileId: string, params: RtWriteDeniedParams) => void>();
	readonly #stateListeners = new Set<(s: ConnectionState) => void>();

	constructor(options: WsBusTransportOptions) {
		this.#opts = {
			WebSocketCtor: WebSocket,
			// The generated API client can't live in a SharedWorker —
			// it depends on Svelte stores (session, serverStatus) that
			// have no counterpart here. The worker makes exactly one
			// authenticated POST (`/api/rt/ticket`); the DPoP service
			// worker intercepts and signs it, and the cookie is
			// auto-included on same-origin. Nothing else goes through
			// `fetch` from this file.
			// eslint-disable-next-line no-restricted-globals
			fetchFn: fetch.bind(globalThis),
			...options
		};
	}

	// ========================================================
	// BusTransport interface
	// ========================================================

	get state(): ConnectionState {
		return this.#state;
	}

	get latencyMs(): number | null {
		return this.#latencyMs;
	}

	subscribe(topic: string): Promise<RtSubscribeAck> {
		this.#activeTopics.add(topic);
		// Collapse concurrent subscribe(topic) calls into one in-flight
		// promise. The second caller gets the same ack as the first —
		// matches the per-tab client's `#subs` entry reuse.
		const existing = this.#topicWaiters.get(topic);
		if (existing) return existing.promise;

		let resolve!: (a: RtSubscribeAck) => void;
		let reject!: (e: MessageBusError) => void;
		const promise = new Promise<RtSubscribeAck>((res, rej) => {
			resolve = res;
			reject = rej;
		});
		this.#topicWaiters.set(topic, { promise, resolve, reject });

		if (this.#state === 'connected') {
			this.#sendSubscribeFrame(topic);
		} else if (this.#state === 'idle' || this.#state === 'disconnected') {
			this.#connect();
			// #onOpen will send the subscribe frame when it fires.
		} else if (this.#state === 'unavailable') {
			// Circuit breaker is tripped; reject synchronously (next tick).
			queueMicrotask(() => {
				const w = this.#topicWaiters.get(topic);
				if (!w) return;
				this.#topicWaiters.delete(topic);
				w.reject(synthError('not_connected'));
			});
		}
		// state === 'connecting' — the subscribe frame will fire on
		// #onOpen; nothing more to do here.
		return promise;
	}

	async unsubscribe(topic: string): Promise<void> {
		this.#activeTopics.delete(topic);
		if (this.#state !== 'connected') return;
		await this.#callUpstream(this.#nextId++, (id) => unsubscribeFrame(id, topic)).catch((err) => {
			busLog.debug('unsubscribe upstream failed — tolerable', { topic, err });
		});
	}

	async call(method: string, params: Record<string, unknown> | undefined): Promise<unknown> {
		if (this.#state !== 'connected') {
			throw synthError('not_connected');
		}
		return this.#callUpstream(this.#nextId++, (id) => ({
			jsonrpc: '2.0',
			id,
			method,
			params: params ?? {}
		}));
	}

	async ping(): Promise<number> {
		if (this.#state !== 'connected') throw synthError('not_connected');
		const started = Date.now();
		await this.#callUpstream(this.#nextId++, (id) => pingFrame(id));
		const latency = Date.now() - started;
		this.#latencyMs = latency;
		return latency;
	}

	sendBinary(bytes: ArrayBuffer): boolean {
		if (!this.#ws || this.#ws.readyState !== 1 /* OPEN */) return false;
		try {
			this.#ws.send(bytes);
			return true;
		} catch (err) {
			busLog.warn('ws.send(bytes) threw', { err });
			return false;
		}
	}

	reconnect(): void {
		if (this.#ws) {
			try {
				this.#ws.close();
			} catch {
				/* already closing */
			}
			this.#ws = null;
		}
		this.#backoffMs = RECONNECT_MIN_MS;
		this.#consecutiveFailures = 0;
		if (this.#state === 'unavailable') this.#setState('disconnected');
		this.#scheduleReconnect(0);
	}

	onEvent(cb: (topic: string, params: RtEventParams) => void): () => void {
		this.#eventListeners.add(cb);
		return () => this.#eventListeners.delete(cb);
	}

	onRevoked(cb: (topic: string, params: RtRevokedParams) => void): () => void {
		this.#revokedListeners.add(cb);
		return () => this.#revokedListeners.delete(cb);
	}

	onBinary(cb: (fileId: string, bytes: ArrayBuffer) => void): () => void {
		this.#binaryListeners.add(cb);
		return () => this.#binaryListeners.delete(cb);
	}

	onWriteDenied(cb: (fileId: string, params: RtWriteDeniedParams) => void): () => void {
		this.#writeDeniedListeners.add(cb);
		return () => this.#writeDeniedListeners.delete(cb);
	}

	onStateChange(cb: (state: ConnectionState) => void): () => void {
		this.#stateListeners.add(cb);
		cb(this.#state);
		return () => this.#stateListeners.delete(cb);
	}

	// ========================================================
	// Connection management
	// ========================================================

	#connect(): void {
		if (this.#state === 'connecting' || this.#state === 'connected') return;
		if (this.#state === 'unavailable') return;
		this.#setState('connecting');
		const gen = ++this.#connectGen;
		void this.#exchangeAndOpen(gen);
	}

	async #exchangeAndOpen(gen: number): Promise<void> {
		let subprotocol: string;
		try {
			const res = await this.#opts.fetchFn(this.#opts.ticketUrl, {
				method: 'POST',
				credentials: 'include',
				headers: {
					'Content-Type': 'application/json',
					...(this.#opts.csrfToken ? { 'X-CSRF-Token': this.#opts.csrfToken } : {})
				}
			});
			if (!res.ok) {
				const err = { status: res.status, message: `ticket_${res.status}` };
				if (gen === this.#connectGen) this.#onTicketFailure(err);
				return;
			}
			const body = (await res.json()) as RtTicketResponse;
			subprotocol = body.subprotocol;
			busLog.debug('ticket issued', { expires_in_seconds: body.expires_in_seconds });
		} catch (err) {
			if (gen === this.#connectGen) this.#onTicketFailure(err);
			return;
		}
		if (this.#state !== 'connecting' || gen !== this.#connectGen) {
			busLog.debug('ticket exchange superseded — discarding', {
				state: this.#state,
				gen,
				current: this.#connectGen
			});
			return;
		}

		let ws: WebSocket;
		try {
			ws = new this.#opts.WebSocketCtor(this.#opts.wsUrl, [subprotocol]);
		} catch (err) {
			busLog.warn('WebSocket ctor threw — reconnect scheduled', { err });
			this.#setState('disconnected');
			this.#scheduleReconnect();
			return;
		}
		this.#ws = ws;
		ws.binaryType = 'arraybuffer';
		const isLive = () => this.#ws === ws;
		ws.onopen = () => {
			if (!isLive()) {
				busLog.debug('onopen from superseded socket — closing it');
				try {
					ws.close();
				} catch {
					/* already closing */
				}
				return;
			}
			this.#onOpen();
		};
		ws.onmessage = (ev) => {
			if (isLive()) this.#onMessage(ev);
		};
		ws.onerror = (ev) => busLog.debug('ws error event', { ev });
		ws.onclose = (ev) => {
			if (isLive()) this.#onClose(ev);
		};
	}

	#onTicketFailure(err: unknown): void {
		// 403 = terminal, same as per-tab client. 401 falls through to
		// the reconnect path — a session may be restored in another
		// tab and we recover.
		if ((err as { status?: number } | null)?.status === 403) {
			busLog.warn('ticket refused — not retrying', { err });
			this.#setState('unavailable');
			return;
		}
		busLog.warn('ticket exchange failed — reconnect scheduled', { err });
		this.#setState('disconnected');
		this.#consecutiveFailures += 1;
		if (this.#consecutiveFailures >= MAX_CONSECUTIVE_FAILURES) {
			this.#setState('unavailable');
			return;
		}
		if (this.#activeTopics.size > 0) this.#scheduleReconnect();
	}

	#onOpen(): void {
		busLog.debug('connected');
		this.#setState('connected');
		// Stable-connection timer: reset counters only after
		// MIN_STABLE_MS to keep the circuit breaker meaningful.
		if (this.#stableTimer !== null) clearTimeout(this.#stableTimer);
		this.#stableTimer = setTimeout(() => {
			this.#stableTimer = null;
			if (this.#state === 'connected') {
				this.#backoffMs = RECONNECT_MIN_MS;
				this.#consecutiveFailures = 0;
				busLog.debug('stable connection — reconnect counters reset');
			}
		}, MIN_STABLE_MS);
		// Send subscribe for every active topic. First-connect: the
		// topic waiter in `#topicWaiters` resolves when the ack
		// arrives. Reconnect: no waiter (previous subscribe already
		// resolved before the drop), the ack is logged.
		for (const topic of this.#activeTopics) {
			this.#sendSubscribeFrame(topic);
		}
	}

	#sendSubscribeFrame(topic: string): void {
		if (!this.#ws || this.#ws.readyState !== 1 /* OPEN */) return;
		const id = this.#nextId++;
		this.#pendingSubscribeFrames.set(id, topic);
		try {
			this.#ws.send(JSON.stringify(subscribeFrame(id, topic)));
		} catch (err) {
			this.#pendingSubscribeFrames.delete(id);
			const w = this.#topicWaiters.get(topic);
			if (w) {
				this.#topicWaiters.delete(topic);
				w.reject({ code: RtErrorCode.INTERNAL_ERROR, message: 'send_failed', data: err });
			}
		}
	}

	#onClose(ev: CloseEvent): void {
		busLog.debug('ws closed', { code: ev.code, reason: ev.reason });
		this.#ws = null;
		if (this.#stableTimer !== null) {
			clearTimeout(this.#stableTimer);
			this.#stableTimer = null;
		}
		// Reject every pending call AND any topic waiter still
		// awaiting its ack.
		const closed = synthError('ws_closed');
		for (const { reject } of this.#pending.values()) reject(closed);
		this.#pending.clear();
		for (const w of this.#topicWaiters.values()) w.reject(closed);
		this.#topicWaiters.clear();
		this.#pendingSubscribeFrames.clear();
		this.#consecutiveFailures += 1;
		if (this.#consecutiveFailures >= MAX_CONSECUTIVE_FAILURES) {
			this.#setState('unavailable');
			return;
		}
		this.#setState('disconnected');
		if (this.#activeTopics.size > 0) this.#scheduleReconnect();
	}

	#scheduleReconnect(delayMs?: number): void {
		if (this.#reconnectTimer !== null) clearTimeout(this.#reconnectTimer);
		const d = delayMs ?? this.#backoffWithJitter();
		this.#reconnectTimer = setTimeout(() => {
			this.#reconnectTimer = null;
			if (this.#state === 'unavailable') return;
			this.#connect();
		}, d);
	}

	#backoffWithJitter(): number {
		const base = Math.min(this.#backoffMs, RECONNECT_MAX_MS);
		this.#backoffMs = Math.min(base * 2, RECONNECT_MAX_MS);
		// Full jitter: pick in [0, base].
		return Math.floor(Math.random() * base);
	}

	// ========================================================
	// Message handling
	// ========================================================

	#onMessage(ev: MessageEvent): void {
		if (typeof ev.data === 'string') {
			let parsed: IncomingFrame;
			try {
				parsed = parseIncoming(ev.data);
			} catch (err) {
				busLog.warn('incoming parse failed', { err, raw: ev.data });
				return;
			}
			this.#dispatchJson(parsed);
			return;
		}
		if (ev.data instanceof ArrayBuffer) {
			this.#dispatchBinary(ev.data);
			return;
		}
		// `Blob` fallback — shouldn't happen (we set binaryType), but
		// handle defensively.
		busLog.debug('unexpected message type', { data: ev.data });
	}

	#dispatchJson(frame: IncomingFrame): void {
		switch (frame.kind) {
			case 'success': {
				// Subscribe acks route to the per-topic waiter first.
				const topic = this.#pendingSubscribeFrames.get(frame.id);
				if (topic !== undefined) {
					this.#pendingSubscribeFrames.delete(frame.id);
					const w = this.#topicWaiters.get(topic);
					if (w) {
						this.#topicWaiters.delete(topic);
						w.resolve(frame.result as RtSubscribeAck);
					} else {
						// Replay ack after an earlier successful subscribe
						// — nobody waiting; log and drop.
						busLog.debug('subscribe replay acked', { topic });
					}
					return;
				}
				const pending = this.#pending.get(frame.id);
				if (!pending) return;
				this.#pending.delete(frame.id);
				pending.resolve(frame.result);
				return;
			}
			case 'error': {
				if (frame.id === null) {
					busLog.warn('server-sent error with null id', { err: frame.error });
					return;
				}
				// Subscribe errors also route to the per-topic waiter.
				const topic = this.#pendingSubscribeFrames.get(frame.id);
				if (topic !== undefined) {
					this.#pendingSubscribeFrames.delete(frame.id);
					const w = this.#topicWaiters.get(topic);
					if (w) {
						this.#topicWaiters.delete(topic);
						w.reject({
							code: frame.error.code,
							message: frame.error.message,
							data: frame.error.data
						});
					} else {
						busLog.warn('subscribe replay nacked', { topic, err: frame.error });
					}
					return;
				}
				const pending = this.#pending.get(frame.id);
				if (!pending) return;
				this.#pending.delete(frame.id);
				pending.reject({
					code: frame.error.code,
					message: frame.error.message,
					data: frame.error.data
				});
				return;
			}
			case 'event': {
				// Topic lives inside params — the frame envelope carries
				// no top-level topic field. Same shape the per-tab client
				// dispatches against.
				const topic = (frame.params as unknown as { topic?: string }).topic;
				if (typeof topic !== 'string') {
					busLog.debug('event missing params.topic', { frame });
					return;
				}
				for (const l of this.#eventListeners) l(topic, frame.params);
				return;
			}
			case 'revoked': {
				const topic = (frame.params as unknown as { topic?: string }).topic;
				if (typeof topic !== 'string') {
					busLog.debug('revoked missing params.topic', { frame });
					return;
				}
				// Mirror the server's side effect — the sub is gone upstream.
				this.#activeTopics.delete(topic);
				for (const l of this.#revokedListeners) l(topic, frame.params);
				return;
			}
			case 'write_denied':
				for (const l of this.#writeDeniedListeners) l(frame.params.file_id, frame.params);
				return;
			case 'ignore':
				busLog.debug('ignored incoming frame', { reason: frame.reason });
				return;
		}
	}

	/** Binary wire format is 1 byte `kind` + 16 bytes UUID + payload.
	 *  Pull the file-id out of bytes [1..17] without pulling in the
	 *  whole `wireCodec` module — the worker bundle stays dep-free
	 *  of the collab layer the per-tab client already keeps isolated
	 *  the same way (see `#formatUuidFromBytes` there). */
	#dispatchBinary(bytes: ArrayBuffer): void {
		const u8 = new Uint8Array(bytes);
		if (u8.length < 17) {
			busLog.warn('binary frame too short', { len: u8.length });
			return;
		}
		const fileId = formatUuidFromBytes(u8.subarray(1, 17));
		for (const l of this.#binaryListeners) l(fileId, bytes);
	}

	// ========================================================
	// Helpers
	// ========================================================

	#setState(next: ConnectionState): void {
		if (this.#state === next) return;
		this.#state = next;
		for (const l of this.#stateListeners) l(next);
	}

	async #callUpstream(id: number, buildFrame: (id: number) => object): Promise<unknown> {
		return new Promise<unknown>((resolve, reject) => {
			this.#pending.set(id, { resolve, reject });
			const frame = buildFrame(id);
			if (!this.#ws || this.#ws.readyState !== 1 /* OPEN */) {
				this.#pending.delete(id);
				reject(synthError('not_connected'));
				return;
			}
			try {
				this.#ws.send(JSON.stringify(frame));
			} catch (err) {
				this.#pending.delete(id);
				reject({
					code: RtErrorCode.INTERNAL_ERROR,
					message: 'send_failed',
					data: err
				});
			}
		});
	}
}

function synthError(message: string): MessageBusError {
	return { code: RtErrorCode.INTERNAL_ERROR, message };
}

/** Format 16 raw UUID bytes as canonical dashed hex. Mirrors
 *  `wireCodec.bytesToUuid` without importing from the collab
 *  layer — the worker bundle stays dep-free. */
function formatUuidFromBytes(b: Uint8Array): string {
	let out = '';
	for (let i = 0; i < 16; i++) {
		if (i === 4 || i === 6 || i === 8 || i === 10) out += '-';
		out += b[i].toString(16).padStart(2, '0');
	}
	return out;
}
