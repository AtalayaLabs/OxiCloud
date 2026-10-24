// `BusTransport` implementation that drives one real `/api/rt/ws`
// connection with reconnect/backoff/circuit-breaker. Lives inside
// the SharedWorker (`worker.ts` instantiates it and passes it to
// `WorkerCore`).
//
// The transport makes NO HTTP calls. The ticket for the next WS
// upgrade arrives from the tab side via `provideTicket(subprotocol)`
// — the tab fetched it through `apiFetch` (DPoP + CSRF + session
// cookie + refresh-on-401 all handled on the tab side where the
// auth machinery lives). On reconnect the transport fires
// `onNeedTicket(nonce)` to ask any attached port for a fresh one.
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
//   * HTTP / cookies / CSRF / DPoP — all handled on the tab side.
//     The worker speaks WS only, with a tab-forwarded ticket.
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

/** How long we wait for a tab to answer `need-ticket` before giving
 *  up and scheduling another reconnect round. The tab's ticket POST
 *  itself has its own timeout; this is the port round-trip budget. */
const NEED_TICKET_TIMEOUT_MS = 5_000;

interface PendingCall {
	resolve: (result: unknown) => void;
	reject: (error: MessageBusError) => void;
}

/** Factory options. The WS URL is the only mandatory config; the
 *  constructor + listener registration are both injectable so Vitest
 *  can drive the implementation against a fake. */
export interface WsBusTransportOptions {
	/** `wss://host/api/rt/ws`. */
	wsUrl: string;
	/** Injectable WebSocket constructor; tests pass a stub. */
	WebSocketCtor?: typeof WebSocket;
}

/**
 * Single `/api/rt/ws` connection with ticket-via-port upgrade and the
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

	// Nonce space for `need-ticket` round-trips. Each `#askForTicket`
	// bumps it; the matching `provideTicket(nonce)` call must echo
	// the current nonce or the transport ignores the late reply.
	// Marked -1 after consumption so a duplicate `provideTicket` for
	// the same nonce (two tabs both answering a `need-ticket` fan-out)
	// doesn't open a second WS.
	#ticketNonce = 0;
	#ticketConsumed = false;
	/** Timer that fires if no tab replies to `need-ticket` within
	 *  `NEED_TICKET_TIMEOUT_MS`. Cleared on reply or on close. */
	#needTicketTimer: ReturnType<typeof setTimeout> | null = null;

	readonly #pending = new Map<number, PendingCall>();
	#nextId = 1;

	readonly #activeTopics = new Set<string>();

	readonly #pendingSubscribeFrames = new Map<number, string>();

	readonly #topicWaiters = new Map<
		string,
		{
			promise: Promise<RtSubscribeAck>;
			resolve: (a: RtSubscribeAck) => void;
			reject: (e: MessageBusError) => void;
		}
	>();

	readonly #eventListeners = new Set<(topic: string, params: RtEventParams) => void>();
	readonly #revokedListeners = new Set<(topic: string, params: RtRevokedParams) => void>();
	readonly #binaryListeners = new Set<(fileId: string, bytes: ArrayBuffer) => void>();
	readonly #writeDeniedListeners = new Set<(fileId: string, params: RtWriteDeniedParams) => void>();
	readonly #stateListeners = new Set<(s: ConnectionState) => void>();
	/** `onNeedTicket` listeners — fired when the transport needs a tab
	 *  to fetch and provide a fresh ticket. The nonce the callback
	 *  receives MUST be echoed back in `provideTicket(sp, nonce)`. */
	readonly #needTicketListeners = new Set<(nonce: number) => void>();

	constructor(options: WsBusTransportOptions) {
		this.#opts = {
			WebSocketCtor: WebSocket,
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
			this.#askForTicket();
		} else if (this.#state === 'unavailable') {
			queueMicrotask(() => {
				const w = this.#topicWaiters.get(topic);
				if (!w) return;
				this.#topicWaiters.delete(topic);
				w.reject(synthError('not_connected'));
			});
		}
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
		// Aggressive reset — explicit user intent, zero every piece of
		// pending state. Without this, a user calling reconnect() while
		// state === 'connecting' (ticket POST in flight) would hit
		// #askForTicket's early-return guard and the schedule
		// would silently no-op; the stuck cycle persists.
		if (this.#ws) {
			try {
				this.#ws.close();
			} catch {
				/* already closing */
			}
			this.#ws = null;
		}
		if (this.#reconnectTimer !== null) {
			clearTimeout(this.#reconnectTimer);
			this.#reconnectTimer = null;
		}
		if (this.#needTicketTimer !== null) {
			clearTimeout(this.#needTicketTimer);
			this.#needTicketTimer = null;
		}
		if (this.#stableTimer !== null) {
			clearTimeout(this.#stableTimer);
			this.#stableTimer = null;
		}
		this.#backoffMs = RECONNECT_MIN_MS;
		this.#consecutiveFailures = 0;
		this.#ticketConsumed = false;
		// Reject any pending topic waiters so their callers unblock —
		// subscribe() will create a fresh waiter on the next cycle.
		const closed = synthError('reconnect');
		for (const w of this.#topicWaiters.values()) w.reject(closed);
		this.#topicWaiters.clear();
		this.#pendingSubscribeFrames.clear();
		// Drop pending calls too — their replies would reference the
		// now-dead WS id-space.
		for (const { reject } of this.#pending.values()) reject(closed);
		this.#pending.clear();
		// Force state out of 'connecting'/'connected' so #askForTicket
		// doesn't early-return. The subsequent scheduleReconnect(0)
		// kicks a fresh attempt.
		if (this.#state !== 'disconnected') {
			this.#setState('disconnected');
		}
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
	// Ticket flow — the tab provides, the worker consumes.
	// ========================================================

	/** Register a listener for `need-ticket` events. The listener's
	 *  caller (`WorkerCore`) picks a port and sends a `need-ticket`
	 *  envelope carrying the nonce; the port replies via
	 *  `provideTicket(subprotocol, nonce)`. */
	onNeedTicket(cb: (nonce: number) => void): () => void {
		this.#needTicketListeners.add(cb);
		return () => this.#needTicketListeners.delete(cb);
	}

	/** Hand the transport a ready-to-use `Sec-WebSocket-Protocol`
	 *  value. `nonce` must echo the current ticket round — late
	 *  replies after another port already served are ignored.
	 *  nonce === 0 is the initial hello-time ticket. */
	provideTicket(subprotocol: string, nonce: number): void {
		if (nonce !== 0 && nonce !== this.#ticketNonce) {
			busLog.debug('provideTicket ignored — superseded nonce', {
				supplied: nonce,
				current: this.#ticketNonce
			});
			return;
		}
		if (this.#ticketConsumed) {
			busLog.debug('provideTicket ignored — ticket already consumed this round', {
				supplied: nonce,
				current: this.#ticketNonce
			});
			return;
		}
		if (this.#state !== 'connecting' && this.#state !== 'idle' && this.#state !== 'disconnected') {
			busLog.debug('provideTicket ignored — not in a connect window', { state: this.#state });
			return;
		}
		this.#ticketConsumed = true;
		// Clear the pending need-ticket timeout; a port answered.
		if (this.#needTicketTimer !== null) {
			clearTimeout(this.#needTicketTimer);
			this.#needTicketTimer = null;
		}
		this.#openWithSubprotocol(subprotocol);
	}

	// ========================================================
	// Connection management
	// ========================================================

	/** Ask the above layer (WorkerCore → one port) for a ticket. */
	#askForTicket(): void {
		if (this.#state === 'connecting' || this.#state === 'connected') return;
		if (this.#state === 'unavailable') return;
		this.#setState('connecting');
		this.#ticketNonce += 1;
		this.#ticketConsumed = false;
		const n = this.#ticketNonce;
		for (const cb of this.#needTicketListeners) {
			try {
				cb(n);
			} catch (err) {
				busLog.warn('need-ticket listener threw', { err });
			}
		}
		if (this.#needTicketTimer !== null) clearTimeout(this.#needTicketTimer);
		this.#needTicketTimer = setTimeout(() => {
			this.#needTicketTimer = null;
			if (this.#state !== 'connecting') return;
			// No port answered in time. Count as a failure and retry
			// through the normal backoff path — a port might attach
			// in the meantime.
			this.#consecutiveFailures += 1;
			busLog.warn('need-ticket timed out', { nonce: n });
			if (this.#consecutiveFailures >= MAX_CONSECUTIVE_FAILURES) {
				this.#setState('unavailable');
				return;
			}
			this.#setState('disconnected');
			if (this.#activeTopics.size > 0) this.#scheduleReconnect();
		}, NEED_TICKET_TIMEOUT_MS);
	}

	#openWithSubprotocol(subprotocol: string): void {
		const gen = ++this.#connectGen;
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
		const isLive = () => this.#ws === ws && gen === this.#connectGen;
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

	#onOpen(): void {
		busLog.debug('connected');
		this.#setState('connected');
		if (this.#stableTimer !== null) clearTimeout(this.#stableTimer);
		this.#stableTimer = setTimeout(() => {
			this.#stableTimer = null;
			if (this.#state === 'connected') {
				this.#backoffMs = RECONNECT_MIN_MS;
				this.#consecutiveFailures = 0;
				busLog.debug('stable connection — reconnect counters reset');
			}
		}, MIN_STABLE_MS);
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
			this.#askForTicket();
		}, d);
	}

	#backoffWithJitter(): number {
		const base = Math.min(this.#backoffMs, RECONNECT_MAX_MS);
		this.#backoffMs = Math.min(base * 2, RECONNECT_MAX_MS);
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
		busLog.debug('unexpected message type', { data: ev.data });
	}

	#dispatchJson(frame: IncomingFrame): void {
		switch (frame.kind) {
			case 'success': {
				const topic = this.#pendingSubscribeFrames.get(frame.id);
				if (topic !== undefined) {
					this.#pendingSubscribeFrames.delete(frame.id);
					const w = this.#topicWaiters.get(topic);
					if (w) {
						this.#topicWaiters.delete(topic);
						w.resolve(frame.result as RtSubscribeAck);
					} else {
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
