// Message-bus WebSocket client — one connection per tab.
//
// Owns the single `/api/rt/ws` connection, refcounted per-topic
// subscriptions, JSON-RPC request/response correlation, and reconnect
// with jittered exponential backoff. Consumers reach for this through
// the `useTopic` / `useFolderTopic` composables and never see the
// connection directly.
//
// Related files:
//   * `frames.ts`                — JSON-RPC framing (pure functions).
//   * `error-codes.ts`           — named constants for `RtErrorObject.code`.
//   * `$lib/composables/useTopic.svelte.ts` — per-component lifecycle.
//   * `$lib/generated/message-bus/`         — wire DTOs (Modelina, auto).
//
// Auth: same-origin WS carries the session cookie automatically. DPoP-
// required deployments need the ticket flow (Phase F, deferred); the
// unauthenticated close is surfaced through `state = 'disconnected'`
// and the console logger so users can diagnose without a redeploy.

import log from 'loglevel';
import { untrack } from 'svelte';

import { apiJson } from '$lib/api/client';
import { getCsrfHeaders } from '$lib/api/csrf';
import { RtErrorCode } from './error-codes';
import {
	parseIncoming,
	pingFrame,
	subscribeFrame,
	unsubscribeFrame,
	type IncomingFrame
} from './frames';
import type RtEventParams from '$lib/generated/message-bus/RtEventParams';
import type RtRevokedParams from '$lib/generated/message-bus/RtRevokedParams';

/** Response body from `POST /api/rt/ticket`. Matches the Rust
 *  `RtTicketResponse` shape — see `handlers/rt_ticket_handler.rs`. */
interface RtTicketResponse {
	/** Opaque ticket UUID. Redeemed once server-side. */
	ticket: string;
	/** Seconds until server-side expiry (informational; the client
	 *  should open the WS immediately). */
	expires_in_seconds: number;
	/** Full `Sec-WebSocket-Protocol` value the client MUST pass on
	 *  the upgrade — assembled server-side so a FE bug can't emit
	 *  the wrong prefix. */
	subprotocol: string;
}

/** Logger namespace — matches `frontend/AGENTS.md § Logging`. Users
 *  tune with `oxi.setLogLevel('oxi:message-bus', 'debug')`. */
const busLog = log.getLogger('oxi:message-bus');

/** Reactive connection state. `idle` before the first `subscribe`;
 *  `connecting` while the handshake is in flight; `connected` once
 *  the server has accepted the upgrade; `disconnected` after any
 *  close (reconnect fires from the client). */
export type ConnectionState = 'idle' | 'connecting' | 'connected' | 'disconnected';

/** Callback invoked for every `rt.event` notification on a topic. */
export type EventHandler = (params: RtEventParams) => void;

/** Callback invoked when the server sends `rt.revoked` for a topic —
 *  the subscription is already gone server-side by the time the frame
 *  arrives; the client removes it from the local refcount map and
 *  fires this so the consumer can toast / redirect / whatever. */
export type RevokedHandler = (params: RtRevokedParams) => void;

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

/** Reconnect backoff — 250 ms doubling with full jitter, capped at 30 s.
 *  Same shape as the HTTP retry we use in the fetch interceptor. */
const RECONNECT_MIN_MS = 250;
const RECONNECT_MAX_MS = 30_000;

/** Circuit breaker — after N consecutive failed attempts (either a
 *  ticket-exchange rejection or a WS close before `onopen` fires),
 *  give up and stay `disconnected` until the caller explicitly asks
 *  to `reconnect()`. Prevents an unrecoverable auth state (revoked
 *  session, wrong CSRF cookie, missing DPoP nonce) from flooding
 *  logs. Ten attempts × exponential-backoff-with-jitter is roughly a
 *  minute of trying — long enough for a transient blip, short enough
 *  to stop before it's noise. */
const MAX_CONSECUTIVE_FAILURES = 10;

interface SubEntry {
	count: number;
	handlers: Set<EventHandler>;
	revokedHandlers: Set<RevokedHandler>;
	/** True once the server has ack'd `rt.subscribe`. Used by
	 *  reconnect: on wire-up we re-send every already-ack'd topic. */
	acked: boolean;
}

interface PendingCall {
	resolve: (result: unknown) => void;
	reject: (error: MessageBusError) => void;
}

export class MessageBusClient {
	/** Reactive connection state — exposed for a debug indicator or
	 *  Playwright test. Not consumed by the composables directly. */
	state = $state<ConnectionState>('idle');
	/** Last observed round-trip in ms, updated on each `rt.pong`.
	 *  `null` until the first ping completes. */
	latencyMs = $state<number | null>(null);

	#ws: WebSocket | null = null;
	/** Backoff for the NEXT reconnect attempt. Reset to
	 *  `RECONNECT_MIN_MS` on every successful open. */
	#backoffMs = RECONNECT_MIN_MS;
	/** setTimeout handle for a scheduled reconnect. Cleared on
	 *  explicit `close()` so we don't reconnect after teardown. */
	#reconnectTimer: ReturnType<typeof setTimeout> | null = null;
	/** Consecutive failures — incremented on every attempt that dies
	 *  before `#onOpen()` gets to reset it. Once it hits
	 *  `MAX_CONSECUTIVE_FAILURES` the client stops reconnecting and
	 *  requires an explicit `reconnect()` from the caller. */
	#consecutiveFailures = 0;

	/** `topic` → `{count, handlers, revokedHandlers, acked}`. Refcount
	 *  drives the wire: first refcount ⇒ send `rt.subscribe`; last drop
	 *  ⇒ send `rt.unsubscribe`. Plain `Map` (not `SvelteMap`) — this is
	 *  internal plumbing keyed by topic string; a reactive collection
	 *  would re-run every component's `$effect` on unrelated
	 *  subscribes. */
	// eslint-disable-next-line svelte/prefer-svelte-reactivity
	#subs = new Map<string, SubEntry>();
	/** Pending JSON-RPC requests keyed by id. Same rationale as
	 *  `#subs` — internal state, not reactive. */
	// eslint-disable-next-line svelte/prefer-svelte-reactivity
	#pending = new Map<number, PendingCall>();
	#nextId = 1;

	/** URL for the WebSocket. Injectable so tests can point at a mock. */
	#url: string;
	/** WebSocket constructor. Injectable for the same reason. */
	#WebSocketCtor: typeof WebSocket;

	constructor(opts?: { url?: string; WebSocketCtor?: typeof WebSocket }) {
		// Default to same-origin `/api/rt/ws`. `location` is unavailable
		// in SSR; the client is instantiated lazily on first `subscribe`
		// so this executes in the browser.
		const defaultUrl = () => {
			const proto = window.location.protocol === 'https:' ? 'wss:' : 'ws:';
			return `${proto}//${window.location.host}/api/rt/ws`;
		};
		this.#url = opts?.url ?? (typeof window !== 'undefined' ? defaultUrl() : '');
		this.#WebSocketCtor = opts?.WebSocketCtor ?? WebSocket;
	}

	/**
	 * Refcounted subscribe. Adds `onEvent` (and optional `onRevoked`)
	 * to the local handlers for `topic`, sends `rt.subscribe` on the
	 * wire only for the first ref, and returns an unsubscribe fn that
	 * drops that same ref (last ref out sends `rt.unsubscribe`).
	 *
	 * Wrapped in `untrack` because `this.state` is `$state`. Without
	 * this, a caller invoking `subscribe` from a Svelte `$effect`
	 * (which `useTopic` does) would take a reactive dep on `state`.
	 * Every `state` transition (idle → connecting → disconnected →
	 * connecting → …) would then re-fire the caller's `$effect`,
	 * which re-calls `subscribe`, which flips `state`, which re-fires
	 * the effect — a 1000+/s runaway loop, observed on server-down
	 * (2026-09-11). `subscribe` is a mutation entry point; its reads
	 * of internal state MUST NOT contaminate reactive callers.
	 */
	subscribe(topic: string, onEvent: EventHandler, onRevoked?: RevokedHandler): UnsubscribeHandle {
		return untrack(() => {
			let entry = this.#subs.get(topic);
			if (!entry) {
				// Plain Sets: internal callback registries, not reactive.
				// Same rationale as `#subs` / `#pending` — see the doc
				// there.
				entry = {
					count: 0,
					// eslint-disable-next-line svelte/prefer-svelte-reactivity
					handlers: new Set(),
					// eslint-disable-next-line svelte/prefer-svelte-reactivity
					revokedHandlers: new Set(),
					acked: false
				};
				this.#subs.set(topic, entry);
			}
			entry.count += 1;
			entry.handlers.add(onEvent);
			if (onRevoked) entry.revokedHandlers.add(onRevoked);

			// Kick the connection if nothing is holding it yet, otherwise
			// send `rt.subscribe` if this is the first ref on this topic.
			if (this.state === 'idle' || this.state === 'disconnected') {
				this.#connect();
			} else if (entry.count === 1 && this.state === 'connected') {
				this.#sendSubscribe(topic).catch((err) =>
					busLog.warn('subscribe failed', { topic, error: err })
				);
			}

			let released = false;
			return () => {
				if (released) return;
				released = true;
				// Cleanup path — Svelte `$effect` cleanup doesn't track
				// anyway, but stay defensive: untrack around the
				// internal state reads inside #releaseOne.
				untrack(() => this.#releaseOne(topic, onEvent, onRevoked));
			};
		});
	}

	/** Force a fresh reconnect — for a live-updates toggle or a manual
	 *  "reconnect" button. Rare; not part of the normal flow. Also the
	 *  escape hatch after the circuit breaker trips: zeroes the
	 *  consecutive-failure counter so the next attempt actually fires. */
	reconnect(): void {
		if (this.#ws) this.#ws.close();
		this.#backoffMs = RECONNECT_MIN_MS;
		this.#consecutiveFailures = 0;
		this.#scheduleReconnect(0);
	}

	/** Tear down. Currently only meaningful in tests — the singleton
	 *  lives for the lifetime of the tab. */
	close(): void {
		if (this.#reconnectTimer !== null) {
			clearTimeout(this.#reconnectTimer);
			this.#reconnectTimer = null;
		}
		if (this.#ws) {
			this.#ws.close();
			this.#ws = null;
		}
		this.state = 'idle';
		this.#subs.clear();
		this.#pending.clear();
	}

	// ─────────────────────── connection lifecycle ────────────────────

	#connect(): void {
		if (this.state === 'connecting' || this.state === 'connected') return;
		this.state = 'connecting';
		busLog.debug('connecting', { url: this.#url });
		// Ticket exchange runs off a Promise; the connection is
		// finalised inside its `.then`. Errors during exchange land in
		// `#onTicketFailure`, which mirrors the WS-close reconnect path
		// so a transient auth blip retries with backoff.
		void this.#exchangeAndOpen();
	}

	/** POST `/api/rt/ticket`, then open the WS with the returned
	 *  subprotocol. The POST runs through `apiFetch` — DPoP proof
	 *  and session cookie handled by the interceptor — and we attach
	 *  the CSRF header ourselves per every state-changing endpoint's
	 *  convention (see `endpoints/shares.ts` for the pattern). */
	async #exchangeAndOpen(): Promise<void> {
		let subprotocol: string;
		try {
			const res = await apiJson<RtTicketResponse>('/api/rt/ticket', {
				method: 'POST',
				headers: getCsrfHeaders()
			});
			subprotocol = res.subprotocol;
			busLog.debug('ticket issued', { expires_in_seconds: res.expires_in_seconds });
		} catch (err) {
			this.#onTicketFailure(err);
			return;
		}
		// A close/reconnect could have raced this in-flight exchange;
		// bail if we lost the "connecting" role in the meantime.
		if (this.state !== 'connecting') {
			busLog.debug('ticket exchange raced with close — discarding', { state: this.state });
			return;
		}
		let ws: WebSocket;
		try {
			ws = new this.#WebSocketCtor(this.#url, [subprotocol]);
		} catch (err) {
			busLog.warn('WebSocket ctor threw — reconnect scheduled', { error: err });
			this.state = 'disconnected';
			this.#scheduleReconnect();
			return;
		}
		this.#ws = ws;
		ws.onopen = () => this.#onOpen();
		ws.onmessage = (ev) => this.#onMessage(ev);
		ws.onerror = (ev) => busLog.debug('ws error event', { ev });
		ws.onclose = (ev) => this.#onClose(ev);
	}

	/** Handle a failed ticket exchange. Same shape as a WS close —
	 *  we're not going to retry inline (a bad auth state won't fix
	 *  itself in 250 ms), so schedule the next attempt through the
	 *  standard reconnect path. */
	#onTicketFailure(err: unknown): void {
		busLog.warn('ticket exchange failed — reconnect scheduled', { error: err });
		this.state = 'disconnected';
		if (this.#subs.size > 0) this.#scheduleReconnect();
	}

	#onOpen(): void {
		busLog.debug('connected');
		this.state = 'connected';
		this.#backoffMs = RECONNECT_MIN_MS;
		this.#consecutiveFailures = 0;
		// Replay every already-known topic. `entry.acked` is reset here
		// because the fresh connection has no server-side memory of
		// prior subscriptions.
		for (const [topic, entry] of this.#subs) {
			entry.acked = false;
			this.#sendSubscribe(topic).catch((err) =>
				busLog.warn('resubscribe failed', { topic, error: err })
			);
		}
	}

	#onMessage(ev: MessageEvent): void {
		if (typeof ev.data !== 'string') {
			// Binary frames are the Yjs sync protocol (Phase G) — not in
			// scope yet. Silently drop; a future collab store will
			// receive them via a separate handler.
			busLog.debug('binary frame dropped (Phase G)');
			return;
		}
		const frame = parseIncoming(ev.data);
		this.#dispatch(frame);
	}

	#dispatch(frame: IncomingFrame): void {
		switch (frame.kind) {
			case 'event': {
				const entry = this.#subs.get(frame.params.topic);
				if (!entry) {
					busLog.debug('event for unknown topic', { topic: frame.params.topic });
					return;
				}
				// Trace each delivered event so devs can watch the bus
				// live in the console. Level `debug` — silent under the
				// default `warn`. See `frontend/AGENTS.md § Logging`
				// for the tune knob (`oxi.setLogLevel('oxi:message-bus',
				// 'debug')`).
				busLog.debug('event received', {
					topic: frame.params.topic,
					kind: frame.params.event,
					actor: (frame.params.data as { actor?: string })?.actor
				});
				for (const handler of entry.handlers) {
					try {
						handler(frame.params);
					} catch (err) {
						busLog.warn('event handler threw', { topic: frame.params.topic, error: err });
					}
				}
				break;
			}
			case 'revoked': {
				const entry = this.#subs.get(frame.params.topic);
				if (!entry) {
					busLog.debug('revoked for unknown topic', { topic: frame.params.topic });
					return;
				}
				busLog.warn('subscription revoked', {
					topic: frame.params.topic,
					reason: frame.params.reason
				});
				// Server-side sub is already gone; drop local state
				// BEFORE firing consumer handlers so any handler that
				// re-subscribes gets a fresh entry with `count = 1`.
				const revokedHandlers = [...entry.revokedHandlers];
				this.#subs.delete(frame.params.topic);
				for (const handler of revokedHandlers) {
					try {
						handler(frame.params);
					} catch (err) {
						busLog.warn('revoked handler threw', { topic: frame.params.topic, error: err });
					}
				}
				break;
			}
			case 'success': {
				const pending = this.#pending.get(frame.id);
				if (!pending) return;
				this.#pending.delete(frame.id);
				pending.resolve(frame.result);
				break;
			}
			case 'error': {
				busLog.warn('rt.error', { id: frame.id, error: frame.error });
				if (frame.id === null) return;
				const pending = this.#pending.get(frame.id);
				if (!pending) return;
				this.#pending.delete(frame.id);
				pending.reject(frame.error);
				break;
			}
			case 'ignore': {
				busLog.warn('ignored frame', { reason: frame.reason, raw: frame.raw });
				break;
			}
		}
	}

	#onClose(ev: CloseEvent): void {
		busLog.debug('close', { code: ev.code, reason: ev.reason });
		this.#ws = null;
		this.state = 'disconnected';
		// Reject every pending call — the caller sees a synthetic
		// error rather than hanging. Reconnect will re-issue the
		// subscribe via `#onOpen`, not by resolving these.
		const closed: MessageBusError = { code: RtErrorCode.INTERNAL_ERROR, message: 'ws_closed' };
		for (const pending of this.#pending.values()) pending.reject(closed);
		this.#pending.clear();
		// Only reconnect if we still have subscribers waiting.
		if (this.#subs.size > 0) this.#scheduleReconnect();
	}

	#scheduleReconnect(overrideMs?: number): void {
		if (this.#reconnectTimer !== null) return;
		this.#consecutiveFailures += 1;
		// Circuit breaker: after too many failures in a row, stop
		// retrying and require an explicit `reconnect()` call from
		// the caller. Prevents a bad auth state (session revoked,
		// CSRF cookie stripped, DPoP nonce mismatch) from flooding
		// server logs with the same 401/403 forever. `reconnect()`
		// zeroes the counter and re-arms.
		if (this.#consecutiveFailures >= MAX_CONSECUTIVE_FAILURES) {
			busLog.error('circuit breaker tripped — reconnect suspended after too many failures', {
				consecutiveFailures: this.#consecutiveFailures,
				max: MAX_CONSECUTIVE_FAILURES,
				remedy: 'call messageBus.reconnect() to retry, or refresh the page'
			});
			return;
		}
		const delay = overrideMs ?? this.#backoffMs;
		// Full jitter — random in [0, backoff]. Prevents thundering
		// herd if the server was momentarily overloaded.
		const jittered = Math.floor(Math.random() * (delay + 1));
		busLog.warn('reconnect scheduled', {
			attemptBackoffMs: delay,
			jitteredMs: jittered,
			consecutiveFailures: this.#consecutiveFailures
		});
		this.#reconnectTimer = setTimeout(() => {
			this.#reconnectTimer = null;
			this.#backoffMs = Math.min(this.#backoffMs * 2, RECONNECT_MAX_MS);
			this.#connect();
		}, jittered);
	}

	// ─────────────────────── request/response ────────────────────────

	#sendSubscribe(topic: string): Promise<void> {
		return this.#call((id) => subscribeFrame(id, topic)).then((result) => {
			const entry = this.#subs.get(topic);
			if (entry) entry.acked = true;
			busLog.debug('subscribed', { topic, result });
		});
	}

	#sendUnsubscribe(topic: string): Promise<void> {
		// Fire-and-forget — the server accepts idempotently. Not chained
		// on the promise because by the time we send this the caller
		// has already cleaned up its local state.
		return this.#call((id) => unsubscribeFrame(id, topic)).then(() => {
			busLog.debug('unsubscribed', { topic });
		});
	}

	/** Public latency probe. Sends `rt.ping` and updates `latencyMs`. */
	async ping(): Promise<number> {
		const started = performance.now();
		await this.#call((id) => pingFrame(id));
		const elapsed = Math.round(performance.now() - started);
		this.latencyMs = elapsed;
		return elapsed;
	}

	#call(makeFrame: (id: number) => object): Promise<unknown> {
		if (this.state !== 'connected' || !this.#ws) {
			const err: MessageBusError = {
				code: RtErrorCode.INTERNAL_ERROR,
				message: 'not_connected'
			};
			return Promise.reject(err);
		}
		const id = this.#nextId++;
		const frame = makeFrame(id);
		return new Promise((resolve, reject) => {
			this.#pending.set(id, { resolve, reject });
			try {
				this.#ws!.send(JSON.stringify(frame));
			} catch (err) {
				this.#pending.delete(id);
				busLog.warn('send failed', { id, error: err });
				reject({ code: RtErrorCode.INTERNAL_ERROR, message: 'send_failed' });
			}
		});
	}

	// ─────────────────────── refcount teardown ────────────────────────

	#releaseOne(topic: string, onEvent: EventHandler, onRevoked?: RevokedHandler): void {
		const entry = this.#subs.get(topic);
		if (!entry) return;
		entry.handlers.delete(onEvent);
		if (onRevoked) entry.revokedHandlers.delete(onRevoked);
		entry.count -= 1;
		if (entry.count > 0) return;
		this.#subs.delete(topic);
		if (this.state === 'connected' && entry.acked) {
			void this.#sendUnsubscribe(topic).catch(() => {
				// Server drops idempotently; nothing to do if it errors.
			});
		}
	}
}

/**
 * Process-wide singleton — one WebSocket per tab. Lazy: nothing opens
 * until the first `subscribe`. Exported for `useTopic` to consume;
 * app code should reach for the composables instead.
 */
export const messageBus = new MessageBusClient();
