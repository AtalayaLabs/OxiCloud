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

import { base } from '$app/paths';
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

/** Reconnect backoff — 250 ms doubling with full jitter, capped at 30 s.
 *  Same shape as the HTTP retry we use in the fetch interceptor. */
const RECONNECT_MIN_MS = 250;
const RECONNECT_MAX_MS = 30_000;

/** Circuit breaker — after N consecutive failed attempts (either a
 *  ticket-exchange rejection or a WS close before `onopen` fires),
 *  give up and flip state to `unavailable`. The UI is expected to
 *  render a "please refresh" invite; `reconnect()` from an explicit
 *  user action zeroes the counters and re-arms. Prevents an
 *  unrecoverable auth state (revoked session, wrong CSRF cookie,
 *  missing DPoP nonce), OR a stopped server, from flooding both the
 *  server (once it comes back) and the client's own log. */
const MAX_CONSECUTIVE_FAILURES = 10;

/** How long the socket must stay open after `onopen` before we
 *  consider the connection "stable" and reset the backoff /
 *  circuit-breaker counters. A server mid-shutdown can accept a
 *  TCP connection and close it milliseconds later; resetting on
 *  the momentary open would let the client hammer the server
 *  forever (and reset the circuit breaker every cycle). 3 s
 *  covers "server dying mid-handshake" without being noticeable
 *  on a healthy reconnect. */
const MIN_STABLE_MS = 3_000;

/** How long a tab must stay hidden before the client proactively
 *  closes its WebSocket. Balances two costs:
 *
 *  - Aggressive close (0 grace) churns on every alt-tab: users
 *    switch tabs dozens of times a day for quick lookups; a full
 *    ticket exchange + reconnect on every switch is wasteful.
 *  - No close leaves the WS holding an fd, a broadcast receiver
 *    slot, and the session's outbound `mpsc::Sender` server-side
 *    for as long as the tab is open — even if the user hasn't
 *    looked at it in hours.
 *
 *  60 s comfortably absorbs "alt-tab, check something, come back"
 *  and starts saving real state on tabs left in the background for
 *  real work. On return we run the same `onReconnect` handlers the
 *  server-restart path uses — no new code needed for state resync.
 *
 *  The Page Visibility API (`document.visibilityState`) fires the
 *  same event whether the user switched tabs, minimised the window,
 *  or the screen locked. All three want the same treatment. */
const HIDDEN_GRACE_MS = 60_000;

interface SubEntry {
	count: number;
	handlers: Set<EventHandler>;
	revokedHandlers: Set<RevokedHandler>;
	/** Ack callbacks — fired with the server's `rt.subscribe` reply
	 *  every time the subscribe succeeds (initial + every reconnect
	 *  replay). Consumers that only care about the initial ack
	 *  self-latch inside the handler. */
	ackedHandlers: Set<SubscribeAckHandler>;
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
	/** True once we've observed at least one successful `#onOpen`.
	 *  Used to distinguish "initial connect" (don't fire onReconnect
	 *  handlers — the initial load path is doing the fetch already)
	 *  from "reconnect" (do fire — events during the outage window
	 *  were lost, consumers must refetch). */
	#hasConnectedBefore = false;
	/** Reconnect handlers, invoked from `#onOpen` on the SECOND-onwards
	 *  successful connect. Plain Set — internal registry, not
	 *  reactive. Same rationale as `#subs` / `#pending`. */
	// eslint-disable-next-line svelte/prefer-svelte-reactivity
	#reconnectHandlers = new Set<ReconnectHandler>();
	/** setTimeout handle for the "close on hidden after grace" timer.
	 *  `null` when the tab is visible OR the timer already fired. See
	 *  `HIDDEN_GRACE_MS` for the design tradeoff. */
	#hiddenTimer: ReturnType<typeof setTimeout> | null = null;
	/** setTimeout handle for the "connection has been stable for
	 *  `MIN_STABLE_MS`" callback. Fires from `#onOpen` and, on
	 *  fire, clears the backoff / consecutiveFailures counters.
	 *  Cancelled on close so a socket that dies before the
	 *  threshold doesn't get credit for a stable open. */
	#stableTimer: ReturnType<typeof setTimeout> | null = null;
	/** Bound `visibilitychange` listener kept so `close()` can
	 *  detach it. Not attached in SSR (`typeof document ===
	 *  "undefined"`); the client is lazy so this is just belt-and-
	 *  braces against a caller doing something unusual. */
	#onVisibilityChange: (() => void) | null = null;

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

	/** Binary-frame handlers keyed by `file_id` (dashed UUID). One
	 *  handler per file — the collab layer owns a doc singleton per
	 *  file, so multiple handlers would be a bug. Plain Map for the
	 *  same reason as `#subs` (internal plumbing, not reactive). */
	// eslint-disable-next-line svelte/prefer-svelte-reactivity
	#binaryHandlers = new Map<string, (bytes: Uint8Array) => void>();

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
			return `${proto}//${window.location.host}${base}/api/rt/ws`;
		};
		this.#url = opts?.url ?? (typeof window !== 'undefined' ? defaultUrl() : '');
		this.#WebSocketCtor = opts?.WebSocketCtor ?? WebSocket;

		// Wire the Page Visibility hook — closes the WS after
		// `HIDDEN_GRACE_MS` when the tab goes hidden, reconnects on
		// return. See the constant's doc for the tradeoff. Guarded by
		// `typeof document !== 'undefined'` so SSR / non-browser
		// harnesses (Vitest with a stubbed WebSocket) don't crash.
		if (typeof document !== 'undefined') {
			this.#onVisibilityChange = () => this.#handleVisibilityChange();
			document.addEventListener('visibilitychange', this.#onVisibilityChange);
		}
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
	subscribe(
		topic: string,
		onEvent: EventHandler,
		onRevoked?: RevokedHandler,
		onAcked?: SubscribeAckHandler
	): UnsubscribeHandle {
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
					// eslint-disable-next-line svelte/prefer-svelte-reactivity
					ackedHandlers: new Set(),
					acked: false
				};
				this.#subs.set(topic, entry);
			}
			entry.count += 1;
			entry.handlers.add(onEvent);
			if (onRevoked) entry.revokedHandlers.add(onRevoked);
			if (onAcked) entry.ackedHandlers.add(onAcked);

			// Kick the connection if nothing is holding it yet, otherwise
			// send `rt.subscribe` if this is the first ref on this topic.
			// `unavailable` intentionally does NOT kick — the circuit
			// breaker has tripped and the user needs to `reconnect()`
			// or refresh the page. A new subscribe from that state is
			// registered in `#subs` (so it replays if reconnect happens)
			// but no wire attempt fires.
			if (this.state === 'idle' || this.state === 'disconnected') {
				this.#connect();
			} else if (entry.count === 1 && this.state === 'connected') {
				this.#sendSubscribe(topic).catch((err) => {
					busLog.warn('subscribe failed', { topic, error: err });
					this.#failSubscribe(topic, err);
				});
			}

			let released = false;
			return () => {
				if (released) return;
				released = true;
				// Cleanup path — Svelte `$effect` cleanup doesn't track
				// anyway, but stay defensive: untrack around the
				// internal state reads inside #releaseOne.
				untrack(() => this.#releaseOne(topic, onEvent, onRevoked, onAcked));
			};
		});
	}

	/**
	 * Register a handler that fires when the WS reconnects AFTER a
	 * prior disconnect (server restart, network blip, sleep/wake).
	 * NOT called on the initial connect — that path is already
	 * handled by the consumer's own load logic. Returns an
	 * unsubscribe fn.
	 *
	 * Wrapped in `untrack` for the same reason `subscribe` is —
	 * reading `#hasConnectedBefore` etc. inside a caller's `$effect`
	 * would leak a reactive dep. Callers reach for this via the
	 * `useReconnect` composable, which manages the lifecycle.
	 *
	 * Bridges the "events lost during outage window" gap: consumers
	 * refetch on reconnect to bring their view back in line with the
	 * server, since bus publishes during the disconnect never reached
	 * this session. See `project_message_bus_reconnect_gap` memory.
	 */
	onReconnect(cb: ReconnectHandler): () => void {
		return untrack(() => {
			this.#reconnectHandlers.add(cb);
			let released = false;
			return () => {
				if (released) return;
				released = true;
				this.#reconnectHandlers.delete(cb);
			};
		});
	}

	/** Force a fresh reconnect — for a live-updates toggle or a manual
	 *  "reconnect" button. Rare; not part of the normal flow. Also the
	 *  escape hatch after the circuit breaker trips: zeroes the
	 *  consecutive-failure counter, clears `unavailable` state, and
	 *  schedules an immediate attempt. */
	reconnect(): void {
		if (this.#ws) this.#ws.close();
		this.#backoffMs = RECONNECT_MIN_MS;
		this.#consecutiveFailures = 0;
		// Explicitly clear the circuit-tripped state; without this,
		// `#connect`'s early-return on `unavailable` would swallow
		// the reconnect attempt.
		if (this.state === 'unavailable') this.state = 'disconnected';
		this.#scheduleReconnect(0);
	}

	/** Tear down. Currently only meaningful in tests — the singleton
	 *  lives for the lifetime of the tab. */
	close(): void {
		if (this.#reconnectTimer !== null) {
			clearTimeout(this.#reconnectTimer);
			this.#reconnectTimer = null;
		}
		if (this.#hiddenTimer !== null) {
			clearTimeout(this.#hiddenTimer);
			this.#hiddenTimer = null;
		}
		if (this.#stableTimer !== null) {
			clearTimeout(this.#stableTimer);
			this.#stableTimer = null;
		}
		if (this.#onVisibilityChange && typeof document !== 'undefined') {
			document.removeEventListener('visibilitychange', this.#onVisibilityChange);
			this.#onVisibilityChange = null;
		}
		if (this.#ws) {
			this.#ws.close();
			this.#ws = null;
		}
		this.state = 'idle';
		this.#subs.clear();
		this.#pending.clear();
	}

	// ─────────────────────── page visibility ─────────────────────────

	/** `visibilitychange` handler. Two transitions:
	 *
	 *  - visible → hidden: start the grace timer (or reset it, if
	 *    the timer was already running from a previous hide → visible
	 *    → hide flip that didn't fire yet — clearing first is safe).
	 *  - hidden → visible: cancel the timer if it hasn't fired; if
	 *    the WS was already closed AND we still hold subscriptions,
	 *    trigger a reconnect so the `onReconnect` handlers refetch
	 *    and the state catches up.
	 *
	 *  Wrapped in `untrack` because this method reads `this.state`
	 *  (a `$state`); the caller is a DOM event listener, but
	 *  defensively we don't want a future refactor that puts this
	 *  behind an `$effect` to inherit a dep on `state`. Same
	 *  pattern applied to every other class-method state read —
	 *  see the `subscribe()` docstring for the general rule. */
	#handleVisibilityChange(): void {
		untrack(() => {
			if (typeof document === 'undefined') return;
			if (document.visibilityState === 'hidden') {
				if (this.#hiddenTimer !== null) clearTimeout(this.#hiddenTimer);
				this.#hiddenTimer = setTimeout(() => this.#closeForHidden(), HIDDEN_GRACE_MS);
				busLog.debug('tab hidden — WS close scheduled', { graceMs: HIDDEN_GRACE_MS });
			} else {
				if (this.#hiddenTimer !== null) {
					clearTimeout(this.#hiddenTimer);
					this.#hiddenTimer = null;
					busLog.debug('tab visible again — hidden-close cancelled (WS still open)');
				}
				// If the WS was closed by the previous grace-timer fire,
				// pop back up. `reconnect()` zeroes the circuit breaker
				// and schedules an immediate attempt; `#onOpen` will
				// then fire every registered `onReconnect` handler and
				// consumers refetch to catch up on missed events. Skip
				// if there are no live subscribers — no point opening
				// a connection nobody's listening on.
				if (this.state === 'disconnected' && this.#subs.size > 0) {
					busLog.debug('tab visible again — reconnecting after grace close');
					this.reconnect();
				}
			}
		});
	}

	/** Grace timer fired — the tab has been hidden for `HIDDEN_GRACE_MS`.
	 *  Close the WS, preserving the local `#subs` map so a return to
	 *  visible can re-subscribe every topic through the normal
	 *  `#onOpen` replay path. Nothing to do if we're already
	 *  disconnected (server-restart flow, etc.). */
	#closeForHidden(): void {
		untrack(() => {
			this.#hiddenTimer = null;
			if (this.state === 'idle' || this.state === 'disconnected') return;
			busLog.warn('closing WS — tab hidden past grace window', {
				subsPreserved: this.#subs.size
			});
			if (this.#ws) {
				this.#ws.close();
				this.#ws = null;
			}
			// Cancel any in-flight reconnect timer — the tab is asleep,
			// no point scheduling more attempts until it's visible again.
			if (this.#reconnectTimer !== null) {
				clearTimeout(this.#reconnectTimer);
				this.#reconnectTimer = null;
			}
			this.state = 'disconnected';
			// Reject pending calls with a synthetic "hidden" error so
			// callers don't hang. Matches the `#onClose` shape.
			const closed: MessageBusError = {
				code: RtErrorCode.INTERNAL_ERROR,
				message: 'ws_closed_tab_hidden'
			};
			for (const pending of this.#pending.values()) pending.reject(closed);
			this.#pending.clear();
		});
	}

	// ─────────────────────── connection lifecycle ────────────────────

	#connect(): void {
		if (this.state === 'connecting' || this.state === 'connected' || this.state === 'unavailable') {
			return;
		}
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
		// Binary frames (collab wire) arrive on the same socket as
		// JSON-RPC text frames — `arraybuffer` is easier to work with
		// than `Blob` (sync `Uint8Array` access, no async read step
		// in `#dispatchBinary`). Default is `Blob`; the switch has no
		// cost when nobody's sending binary.
		ws.binaryType = 'arraybuffer';
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
		// DO NOT reset backoff / consecutiveFailures here. A server
		// mid-shutdown can accept a TCP connection and close it
		// milliseconds later — if we reset on the momentary `open`,
		// the counter never climbs and the circuit breaker never
		// trips. Instead, arm a "stable connection" timer: only
		// reset if the socket stays open for MIN_STABLE_MS. If it
		// dies before that, the counter keeps its value from the
		// pre-open increment and the exponential backoff keeps
		// growing on the way to the circuit breaker.
		if (this.#stableTimer !== null) clearTimeout(this.#stableTimer);
		this.#stableTimer = setTimeout(() => {
			this.#stableTimer = null;
			if (this.state === 'connected') {
				this.#backoffMs = RECONNECT_MIN_MS;
				this.#consecutiveFailures = 0;
				busLog.debug('stable connection — reconnect counters reset');
			}
		}, MIN_STABLE_MS);
		// Snapshot whether this is a reconnect BEFORE we flip the
		// `hasConnectedBefore` bit, so handlers only fire on 2nd+ open.
		const isReconnect = this.#hasConnectedBefore;
		this.#hasConnectedBefore = true;
		// Replay every already-known topic. `entry.acked` is reset here
		// because the fresh connection has no server-side memory of
		// prior subscriptions.
		for (const [topic, entry] of this.#subs) {
			entry.acked = false;
			this.#sendSubscribe(topic).catch((err) => {
				busLog.warn('resubscribe failed', { topic, error: err });
				this.#failSubscribe(topic, err);
			});
		}
		// Fire reconnect handlers AFTER sub replay is kicked (the
		// `rt.subscribe` frames are on the socket; ack may be
		// in-flight). Handlers refetching state via REST will see a
		// consistent post-reconnect view; any events published between
		// resubscribe and the handler's refetch race safely — a stale
		// event just means one extra `reload()` on the next tick.
		if (isReconnect && this.#reconnectHandlers.size > 0) {
			busLog.debug('firing reconnect handlers', {
				count: this.#reconnectHandlers.size
			});
			for (const cb of this.#reconnectHandlers) {
				try {
					cb();
				} catch (err) {
					busLog.warn('reconnect handler threw', { error: err });
				}
			}
		}
	}

	#onMessage(ev: MessageEvent): void {
		if (typeof ev.data !== 'string') {
			// Binary frame — Yjs collab wire (`[kind][file_id][payload]`).
			// The full codec lives in `$lib/collab/wireCodec.ts`; here we
			// only peel the first 17 bytes to route to the right handler,
			// so this file doesn't take a hard dep on the collab module
			// (message-bus is used by consumers who don't care about
			// collab). Handler is per-file, keyed by the dashed UUID.
			this.#dispatchBinary(ev.data as ArrayBuffer | Blob);
			return;
		}
		const frame = parseIncoming(ev.data);
		this.#dispatch(frame);
	}

	#dispatchBinary(data: ArrayBuffer | Blob): void {
		// `data` is `ArrayBuffer` when `binaryType='arraybuffer'` (default
		// for our client — see the connect path); we defensively handle
		// `Blob` too since some transports downgrade under load.
		const emit = (bytes: Uint8Array) => {
			if (bytes.length < 17) {
				busLog.warn('binary frame too short', { len: bytes.length });
				return;
			}
			const fileId = this.#formatUuidFromBytes(bytes.subarray(1, 17));
			const handler = this.#binaryHandlers.get(fileId);
			if (!handler) {
				busLog.debug('binary frame for unknown file_id', { fileId, kind: bytes[0] });
				return;
			}
			try {
				handler(bytes);
			} catch (err) {
				busLog.warn('binary handler threw', { fileId, error: err });
			}
		};
		if (data instanceof ArrayBuffer) {
			emit(new Uint8Array(data));
		} else {
			// Blob path — async read. Rare but supported.
			data.arrayBuffer().then((buf) => emit(new Uint8Array(buf)));
		}
	}

	/** Inline dashed-UUID formatter — same shape as
	 *  `$lib/collab/wireCodec.bytesToUuid`, duplicated here so the
	 *  message-bus module stays dep-free of the collab module. Keep
	 *  in sync if the codec changes format. */
	#formatUuidFromBytes(b: Uint8Array): string {
		let out = '';
		for (let i = 0; i < 16; i++) {
			if (i === 4 || i === 6 || i === 8 || i === 10) out += '-';
			out += b[i].toString(16).padStart(2, '0');
		}
		return out;
	}

	/** Register a handler for binary frames whose header carries the
	 *  given dashed-UUID `fileId`. Returns a dispose fn. Only ONE
	 *  handler per file_id at a time — a second `register` for the
	 *  same id replaces the first (the collab layer owns the doc
	 *  singleton per file, so multiple handlers would be a bug).
	 *
	 *  Untracked for the same reason `subscribe` is: consumers call
	 *  this from Svelte reactive scopes and internal state reads must
	 *  not leak deps. */
	registerBinaryHandler(fileId: string, handler: (bytes: Uint8Array) => void): () => void {
		return untrack(() => {
			this.#binaryHandlers.set(fileId, handler);
			let released = false;
			return () => {
				if (released) return;
				released = true;
				untrack(() => {
					const current = this.#binaryHandlers.get(fileId);
					if (current === handler) this.#binaryHandlers.delete(fileId);
				});
			};
		});
	}

	/** Send a binary frame on the WS. Fire-and-forget — errors log
	 *  but don't reject a promise (there's no id-correlated ack for
	 *  binary frames; the app-level protocol handles retries via
	 *  Yjs's sync-step-1 on reconnect). Returns `false` when the
	 *  socket isn't open so callers can decide whether to buffer. */
	sendBinary(bytes: Uint8Array): boolean {
		if (this.state !== 'connected' || !this.#ws) {
			busLog.debug('sendBinary while not connected — drop', { len: bytes.length });
			return false;
		}
		try {
			// `WebSocket.send` accepts `BufferSource`; TS post-4.9
			// narrows `Uint8Array<ArrayBufferLike>` in a way that
			// doesn't slot in cleanly (the underlying buffer could
			// theoretically be shared). Materialise a fresh, non-
			// shared `ArrayBuffer` copy — one small copy per outbound
			// frame is a fine cost for a keystroke-scale write path.
			const buf = new ArrayBuffer(bytes.byteLength);
			new Uint8Array(buf).set(bytes);
			this.#ws.send(buf);
			return true;
		} catch (err) {
			busLog.warn('sendBinary failed', { error: err });
			return false;
		}
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
		// Cancel any pending "connection stable" callback — if the
		// socket died before MIN_STABLE_MS, the reconnect counter
		// MUST NOT be reset. This is what prevents a server dying
		// mid-shutdown (open → close within ms) from flooding.
		if (this.#stableTimer !== null) {
			clearTimeout(this.#stableTimer);
			this.#stableTimer = null;
		}
		this.state = 'disconnected';
		// Reject every pending call — the caller sees a synthetic
		// error rather than hanging. Reconnect will re-issue the
		// subscribe via `#onOpen`, not by resolving these.
		const closed: MessageBusError = { code: RtErrorCode.INTERNAL_ERROR, message: 'ws_closed' };
		for (const pending of this.#pending.values()) pending.reject(closed);
		this.#pending.clear();
		// Reconnect only when we still have subscribers AND the tab is
		// currently visible. When hidden, `#closeForHidden` closes the
		// WS on purpose to save resources — auto-reconnecting here
		// would defeat the whole grace-close mechanism. The
		// `visibilitychange` handler's hidden→visible transition takes
		// care of the recovery via `this.reconnect()`.
		if (this.#subs.size > 0 && !this.#tabIsHidden()) {
			this.#scheduleReconnect();
		}
	}

	/** Small helper — `true` if the Page Visibility API says the tab
	 *  is hidden right now. Guards non-browser harnesses (SSR,
	 *  Vitest without jsdom overrides) that lack `document`. */
	#tabIsHidden(): boolean {
		return typeof document !== 'undefined' && document.visibilityState === 'hidden';
	}

	#scheduleReconnect(overrideMs?: number): void {
		if (this.#reconnectTimer !== null) return;
		this.#consecutiveFailures += 1;
		// Circuit breaker: after too many failures in a row, stop
		// retrying and flip to `unavailable`. UI is expected to
		// invite the user to refresh; `reconnect()` from an explicit
		// action re-arms. Prevents a bad auth state (session revoked,
		// CSRF cookie stripped, DPoP nonce mismatch) OR a stopped
		// server from flooding both the server (once it comes back)
		// AND the client's own log.
		if (this.#consecutiveFailures >= MAX_CONSECUTIVE_FAILURES) {
			busLog.error('circuit breaker tripped — reconnect suspended after too many failures', {
				consecutiveFailures: this.#consecutiveFailures,
				max: MAX_CONSECUTIVE_FAILURES,
				remedy: 'call messageBus.reconnect() to retry, or refresh the page'
			});
			this.state = 'unavailable';
			return;
		}
		const delay = overrideMs ?? this.#backoffMs;
		// Half-jitter — sleep is at least `delay / 2`, at most `delay`.
		// Full jitter (`0..delay`) allowed rapid-fire retries during a
		// server outage: three attempts could land within a single
		// backoff window on unlucky RNG. Half-jitter keeps the "no
		// faster than delay/2" invariant while still spreading the
		// thundering-herd load. AWS Architecture Blog "Exponential
		// backoff and jitter" writeup calls this out — decorrelated
		// or half is safer than full for a server-side flood.
		const halfDelay = Math.floor(delay / 2);
		const jittered = halfDelay + Math.floor(Math.random() * (halfDelay + 1));
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
			if (entry) {
				entry.acked = true;
				// Fan-out the ack payload to every registered ack
				// handler. Cast is defensive: the server's contract for
				// `rt.subscribe` returns an object with `subscribed`
				// echoing the topic (see `handle_subscribe` in
				// `rt_ws.rs`); if the wire drifts, handlers see the
				// object as-is and are free to narrow / ignore fields
				// they didn't expect.
				const ack = result as RtSubscribeAck;
				for (const handler of entry.ackedHandlers) {
					try {
						handler(ack);
					} catch (err) {
						busLog.warn('subscribe ack handler threw', { topic, error: err });
					}
				}
			}
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

	#releaseOne(
		topic: string,
		onEvent: EventHandler,
		onRevoked?: RevokedHandler,
		onAcked?: SubscribeAckHandler
	): void {
		const entry = this.#subs.get(topic);
		if (!entry) return;
		entry.handlers.delete(onEvent);
		if (onRevoked) entry.revokedHandlers.delete(onRevoked);
		if (onAcked) entry.ackedHandlers.delete(onAcked);
		entry.count -= 1;
		if (entry.count > 0) return;
		this.#subs.delete(topic);
		if (this.state === 'connected' && entry.acked) {
			void this.#sendUnsubscribe(topic).catch(() => {
				// Server drops idempotently; nothing to do if it errors.
			});
		}
	}

	/**
	 * Permanently mark a topic as unusable when `rt.subscribe` comes
	 * back with a server-side denial (JSON-RPC `error`) — the caller
	 * lacks Read, or the topic is unknown (collapsed to the same wire
	 * code for anti-enumeration), etc. A real denial is NOT retryable
	 * — it won't spontaneously succeed on the next reconnect, and
	 * leaving the entry in `#subs` guarantees an infinite retry loop
	 * as `#onOpen` replays every stored sub after every reconnect.
	 *
	 * **Transient transport errors are NOT denials.** The
	 * `#call`/`#onClose` paths reject pending calls with a synthetic
	 * `{code: INTERNAL_ERROR, message: 'ws_closed' | 'not_connected'
	 * | 'send_failed'}` when the WS drops mid-subscribe (typical
	 * during a DPoP nonce refresh + WS teardown / reopen). Treating
	 * those as denials would spuriously flip consumers to `denied`
	 * for a transport hiccup — the caller actually still has Read;
	 * the server just didn't get to answer. Leave the sub in `#subs`
	 * so `#onOpen`'s replay retries after reconnect.
	 *
	 * On real denial, fires the sub's `revokedHandlers` with a
	 * synthetic `subscribe_denied` reason so consumers can tear down
	 * UI the same way they do for a server-initiated eviction.
	 */
	#failSubscribe(topic: string, err: MessageBusError): void {
		// Transient transport errors → don't permanently kill the
		// sub. `message` is the discriminator because they all
		// share `code: INTERNAL_ERROR` (see the `#call` / `#onClose`
		// synthesis sites).
		if (
			err.message === 'ws_closed' ||
			err.message === 'not_connected' ||
			err.message === 'send_failed'
		) {
			busLog.debug('subscribe interrupted by transport — will replay on reconnect', {
				topic,
				error: err
			});
			return;
		}
		const entry = this.#subs.get(topic);
		if (!entry) return;
		this.#subs.delete(topic);
		const handlers = [...entry.revokedHandlers];
		for (const h of handlers) {
			try {
				h({ topic, reason: 'subscribe_denied' } as unknown as RtRevokedParams);
			} catch (handlerErr) {
				busLog.warn('subscribe-denied handler threw', { topic, error: handlerErr });
			}
		}
		busLog.warn('subscribe permanently failed — sub dropped, no retry', {
			topic,
			error: err
		});
	}
}

/**
 * Process-wide singleton — one WebSocket per tab. Lazy: nothing opens
 * until the first `subscribe`. Exported for `useTopic` to consume;
 * app code should reach for the composables instead.
 */
export const messageBus = new MessageBusClient();
