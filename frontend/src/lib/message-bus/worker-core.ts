// SharedWorker core — port routing + fan-out, no DOM / Svelte / WS.
//
// This is the plumbing the SharedWorker entry (`worker.ts`, Gate 2b)
// wraps. It is deliberately factored out so it is unit-testable in
// Vitest without `new SharedWorker(...)`: the test instantiates
// `WorkerCore`, hands it mocked ports and a mocked `BusTransport`,
// and asserts routing behaviour (per-port fan-out, cleanup on
// close, upstream subscribe fires only on first ref) without any
// browser lifecycle at all.
//
// Companion:
//   `docs/plan/message-bus-via-shared-worker.md § Routing state inside the worker`
//
// NOT in scope here:
//   * The actual `/api/rt/ws` WebSocket — abstracted behind `BusTransport`.
//     The worker entry wires a real transport that handles the ticket
//     exchange + reconnect; tests wire a fake.
//   * Svelte 5 runes — this file is plain TS so the worker bundle
//     doesn't drag `svelte/internal/client` in just to no-op reactivity
//     against a dead reactive graph. State broadcasts go out as
//     port envelopes.

import { RtErrorCode } from './error-codes';
import type { ConnectionState, MessageBusError, RtSubscribeAck } from './client-interface';
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

/** Priority order for log levels — higher wins. Used by the filter
 *  push-down to compute `maxLevelWantedByAnyTab` across all ports. */
const LOG_LEVEL_ORDER: Record<LogLevel, number> = {
	error: 50,
	warn: 40,
	info: 30,
	debug: 20,
	trace: 10
};

/**
 * Minimum contract the per-worker singleton transport must provide.
 * The real implementation (Gate 2b) wraps a single `/api/rt/ws` with
 * ticket exchange + reconnect + backoff; the Vitest harness supplies
 * a fake that just captures outbound calls and lets the test fire
 * inbound events.
 *
 * Not called `MessageBusClient` on purpose — that name is reserved
 * for the public interface consumers depend on. This is the
 * worker's internal abstraction over the raw WS, one level below.
 */
export interface BusTransport {
	readonly state: ConnectionState;
	/** Current connection round-trip in ms, if known. */
	readonly latencyMs: number | null;
	/** Subscribe upstream (first ref only — the WorkerCore de-dupes).
	 *  Resolves with the server's ack `result`, rejects with the
	 *  server's error or a synthetic `MessageBusError`. */
	subscribe(topic: string): Promise<RtSubscribeAck>;
	/** Unsubscribe upstream. Called by the WorkerCore when the last
	 *  port leaves a topic. Idempotent on the transport. */
	unsubscribe(topic: string): Promise<void>;
	/** Call a JSON-RPC method upstream and return the result (or
	 *  reject with the server's error). Today: `rt.collab_flush`. */
	call(method: string, params: Record<string, unknown> | undefined): Promise<unknown>;
	/** Round-trip ping. Resolves with the measured latency in ms. */
	ping(): Promise<number>;
	/** Fire-and-forget binary send. Returns false if the WS is not
	 *  open, true on best-effort send. */
	sendBinary(bytes: ArrayBuffer): boolean;
	/** Force a fresh reconnect. */
	reconnect(): void;
	/** Register a listener for every inbound `rt.event`. Returns a
	 *  disposer. */
	onEvent(cb: (topic: string, params: RtEventParams) => void): () => void;
	/** Register a listener for every inbound `rt.revoked`. Returns
	 *  a disposer. */
	onRevoked(cb: (topic: string, params: RtRevokedParams) => void): () => void;
	/** Register a listener for every inbound binary frame. Returns
	 *  a disposer. */
	onBinary(cb: (fileId: string, bytes: ArrayBuffer) => void): () => void;
	/** Register a listener for every inbound `rt.write_denied`.
	 *  Returns a disposer. */
	onWriteDenied(cb: (fileId: string, params: RtWriteDeniedParams) => void): () => void;
	/** Register a listener for `state` changes. Fires immediately
	 *  with the current value then on every change. Returns a disposer. */
	onStateChange(cb: (state: ConnectionState) => void): () => void;
	/** Register a listener for ticket requests. The callback's nonce
	 *  MUST be echoed back in `provideTicket(subprotocol, nonce)`. */
	onNeedTicket(cb: (nonce: number) => void): () => void;
	/** Hand the transport a `Sec-WebSocket-Protocol` value for the
	 *  next WS upgrade. nonce === 0 is the initial hello-time ticket;
	 *  other values must echo the nonce from `onNeedTicket`. */
	provideTicket(subprotocol: string, nonce: number): void;
	/** Tell the transport that the ticket fetch for `nonce` failed
	 *  synchronously (apiJson throw). Lets the transport count the
	 *  failure immediately rather than waiting the 5 s need-ticket
	 *  budget. Stale nonces (bridge replies late) are ignored. */
	ticketFetchFailed(nonce: number): void;
}

/** Internal routing entry for one topic subscription. */
interface TopicEntry {
	/** Ports fanning out this topic's events + revoked. */
	ports: Set<MessagePort>;
	/** `rt.subscribe` reply cached so a port joining AFTER the ack
	 *  gets the same response the first port saw. `undefined` while
	 *  the upstream subscribe is still in flight. */
	cachedAck: RtSubscribeAck | undefined;
	/** Error cached if the upstream subscribe failed. Replayed to any
	 *  late joiner so a Tab B that opens the same folder after a
	 *  denied subscribe gets the same denial, not a hang. */
	cachedError: MessageBusError | undefined;
}

/** Reverse-index entry on a port — tells cleanup what to walk. */
interface PortIndex {
	topics: Set<string>;
	binaryFileIds: Set<string>;
	writeDeniedFileIds: Set<string>;
	wantedLogLevel: LogLevel;
}

/**
 * Routing core for the SharedWorker. One per worker instance.
 *
 * Owns:
 *   * The port registry — every MessagePort currently connected.
 *   * Four routing tables (topic → ports, fileId → binary port set,
 *     fileId → write-denied port set, reqId → single port) keyed to
 *     survive per-port cleanup via a reverse-index WeakMap.
 *   * A single `BusTransport` (one WS for the whole browser).
 *
 * Does NOT own:
 *   * WebSocket lifecycle (transport does).
 *   * Reconnect backoff / ticket exchange (transport does).
 *   * Any Svelte-reactive state (plain fields; broadcasts go out as
 *     port envelopes).
 */
export class WorkerCore {
	readonly #transport: BusTransport;
	readonly #workerVersion: string;

	/** Topic → routing entry. */
	readonly #topics = new Map<string, TopicEntry>();
	/** fileId → ports to fan binary frames out to. One port per file is
	 *  the common case (one tab reading one file); a shared-link opened
	 *  in two tabs is where N>1 shows up. */
	readonly #binaryByFile = new Map<string, Set<MessagePort>>();
	/** fileId → ports to fan rt.write_denied to. */
	readonly #writeDeniedByFile = new Map<string, Set<MessagePort>>();
	// No shared pending-call map. The originating port is captured in
	// each `#handleCall` closure; `#ports.has(port)` on response arrival
	// is the "is this port still alive" gate. Tab A reqId=1 and tab B
	// reqId=1 can both be in flight simultaneously without collision —
	// they live in separate closures.

	/** Reverse index: port → everything it registered, so a port close
	 *  cleans up in O(what-this-port-did) rather than O(total). WeakMap
	 *  because the port is the identity; the GC releases the entry when
	 *  the port is unreferenced. */
	readonly #perPort = new WeakMap<MessagePort, PortIndex>();

	/** Mirror of `#perPort` keys so we can iterate connected ports —
	 *  WeakMap by definition is not iterable. */
	readonly #ports = new Set<MessagePort>();

	/** Highest log level any connected tab currently wants. Used by
	 *  the push-down filter: if no tab wants debug, the worker skips
	 *  the entire log-envelope fan-out for debug lines instead of
	 *  firing N postMessages to tabs that would discard every one. */
	#maxWantedLogLevel: LogLevel = 'info';

	constructor(transport: BusTransport, workerVersion: string) {
		this.#transport = transport;
		this.#workerVersion = workerVersion;

		// Wire the transport's event streams to this worker's fan-out
		// helpers. Each listener survives for the lifetime of the
		// WorkerCore; the transport is a worker-scoped singleton.
		this.#transport.onEvent((topic, params) => this.#fanoutEvent(topic, params));
		this.#transport.onRevoked((topic, params) => this.#fanoutRevoked(topic, params));
		this.#transport.onBinary((fileId, bytes) => this.#fanoutBinary(fileId, bytes));
		this.#transport.onWriteDenied((fileId, params) => this.#fanoutWriteDenied(fileId, params));
		this.#transport.onStateChange((state) => this.#broadcastState(state));
		this.#transport.onNeedTicket((nonce) => this.#askPortForTicket(nonce));
	}

	/** Broadcast `need-ticket` to ALL connected ports. Any live
	 *  bridge replies with `provide-ticket` carrying the matching
	 *  nonce; the transport's `provideTicket()` consumes the FIRST
	 *  reply and ignores later ones (`#ticketConsumed` guard).
	 *
	 *  Why broadcast, not first-port-wins: Chrome's `MessagePort`
	 *  `close` event doesn't fire reliably when a tab closes
	 *  — stale ports accumulate in `#ports`. If `#askPortForTicket`
	 *  picked the FIRST port and that port was a zombie, the
	 *  envelope would silently drop (postMessage on a closed port
	 *  is a spec no-op, not a throw), and the transport's 5 s timer
	 *  would be the only backstop. Observed 2026-10-24: a worker
	 *  with 4 ports but 2 real tabs couldn't recover from server
	 *  restart because the stale port kept being selected.
	 *
	 *  Broadcast costs N extra `postMessage` calls per cycle; cheap
	 *  (envelope is 24 bytes). The one live bridge still wins the
	 *  race because the transport consumes the first provide-ticket
	 *  reply. */
	#askPortForTicket(nonce: number): void {
		if (this.#ports.size === 0) {
			this.#warn('need-ticket emitted but no ports attached', { nonce });
			return;
		}
		for (const port of this.#ports) {
			this.#send(port, { kind: 'need-ticket', nonce });
		}
	}

	/** Called by the SharedWorker entry on each `connect` event. The
	 *  entry has already called `port.start()` and attached message
	 *  listeners by the time it reaches here. */
	registerPort(port: MessagePort): void {
		this.#ports.add(port);
		this.#perPort.set(port, {
			topics: new Set(),
			binaryFileIds: new Set(),
			writeDeniedFileIds: new Set(),
			// Default to `info` so a tab that never asks for a level
			// still sees warn/error without opting in.
			wantedLogLevel: 'info'
		});

		// Auto-recover from a tripped circuit when a new port joins.
		// The per-tab client is constructed fresh on every page load,
		// so a server-restart + page-refresh naturally re-arms the
		// backoff machinery there. In shared mode the worker outlives
		// the page refresh, so without this hook the user would see
		// `state === 'unavailable'` → "Server unreachable" banner
		// stuck on across every refresh until all tabs close and the
		// worker dies.
		//
		// A port joining IS the signal that the user re-engaged with
		// the app — they opened a tab, refreshed, or clicked the
		// banner's Refresh button. Treat it as if they explicitly
		// asked for `reconnect()`: clear the circuit-breaker counter,
		// schedule an immediate attempt.
		if (this.#transport.state === 'unavailable') {
			this.#transport.reconnect();
		}
	}

	/** Called by the SharedWorker entry when a port's `close` event
	 *  fires (tab closed) OR when the entry detects a dead port from a
	 *  failed `postMessage`. Idempotent — a port removed twice is a
	 *  no-op on the second call. */
	deregisterPort(port: MessagePort): void {
		const index = this.#perPort.get(port);
		if (!index) return;
		this.#perPort.delete(port);
		this.#ports.delete(port);

		// Walk topics: drop this port from every topic's port set, and
		// if that leaves the port set empty, unsubscribe upstream.
		for (const topic of index.topics) {
			const entry = this.#topics.get(topic);
			if (!entry) continue;
			entry.ports.delete(port);
			if (entry.ports.size === 0) {
				this.#topics.delete(topic);
				// Fire-and-forget — the transport logs upstream failures.
				void this.#transport.unsubscribe(topic);
			}
		}

		for (const fileId of index.binaryFileIds) {
			const set = this.#binaryByFile.get(fileId);
			if (!set) continue;
			set.delete(port);
			if (set.size === 0) this.#binaryByFile.delete(fileId);
		}

		for (const fileId of index.writeDeniedFileIds) {
			const set = this.#writeDeniedByFile.get(fileId);
			if (!set) continue;
			set.delete(port);
			if (set.size === 0) this.#writeDeniedByFile.delete(fileId);
		}

		// Pending calls initiated by this port are gated at response
		// time: `#handleCall` and `#handlePing` check `this.#ports.has(port)`
		// before firing their ack / nack. A port removed from `#ports`
		// here therefore drops its pending responses implicitly — the
		// transport may still return a result for the in-flight call,
		// but nothing posts to the dead port.

		// Recompute the push-down filter in case this port was the
		// one holding the max log level up.
		this.#recomputeMaxWantedLogLevel();
	}

	/** Called by the SharedWorker entry on every port `message`. */
	async handlePortMessage(port: MessagePort, raw: unknown): Promise<void> {
		const msg = parsePortMessage(raw);
		if (!msg) {
			// Visible drop. The sender (bridge) should fix its envelope;
			// log so the next deploy's QA notices.
			this.#warn('port message dropped — unparseable envelope', { raw });
			return;
		}

		// Only tab-to-worker envelopes can arrive on a port the worker
		// owns. If the far side ever sends a worker-to-tab envelope
		// (indicating confusion / an attack), drop it.
		if (!this.#isTabToWorker(msg)) {
			this.#warn('port message dropped — wrong direction', { kind: msg.kind });
			return;
		}

		switch (msg.kind) {
			case 'hello':
				this.#handleHello(port, msg.reqId, msg.wantedLogLevel);
				return;
			case 'subscribe':
				await this.#handleSubscribe(port, msg.reqId, msg.topic);
				return;
			case 'unsubscribe':
				this.#handleUnsubscribe(port, msg.topic);
				return;
			case 'register-binary-handler':
				this.#handleRegisterBinary(port, msg.fileId);
				return;
			case 'unregister-binary-handler':
				this.#handleUnregisterBinary(port, msg.fileId);
				return;
			case 'register-write-denied-handler':
				this.#handleRegisterWriteDenied(port, msg.fileId);
				return;
			case 'unregister-write-denied-handler':
				this.#handleUnregisterWriteDenied(port, msg.fileId);
				return;
			case 'send-binary':
				this.#transport.sendBinary(msg.bytes);
				return;
			case 'call':
				await this.#handleCall(port, msg.reqId, msg.method, msg.params);
				return;
			case 'ping':
				await this.#handlePing(port, msg.reqId);
				return;
			case 'set-log-level':
				this.#handleSetLogLevel(port, msg.level);
				return;
			case 'provide-ticket':
				this.#transport.provideTicket(msg.subprotocol, msg.nonce);
				return;
			case 'request-reconnect':
				this.#transport.reconnect();
				return;
			case 'ticket-fetch-failed':
				this.#transport.ticketFetchFailed(msg.nonce);
				return;
		}
	}

	// Expose the push-down filter so the worker entry's `busLog`
	// handler can skip emitting log envelopes under the threshold.
	// Separate getter rather than a public field so the test can
	// observe recomputation after `set-log-level` without touching
	// internal state.
	get maxWantedLogLevel(): LogLevel {
		return this.#maxWantedLogLevel;
	}

	/** Debug variant of #warn — same fan-out path, debug level. For
	 *  diagnostic lines that help in-the-wild debugging but are
	 *  noise during normal operation (fires on every port join,
	 *  state transition, etc.). */
	#debug(message: string, context: Record<string, unknown>): void {
		this.emitLog('debug', 'oxi:message-bus:worker', [message, context]);
	}

	/** Fire a worker log line out to every connected port whose
	 *  wantedLogLevel includes `level`. Non-cloneable arg values are
	 *  stripped — `postMessage` would throw otherwise and we'd lose
	 *  the whole fan-out. */
	emitLog(level: LogLevel, namespace: string, args: unknown[]): void {
		if (LOG_LEVEL_ORDER[level] < LOG_LEVEL_ORDER[this.#maxWantedLogLevel]) {
			// Below the max — no tab wants this. Skip entirely.
			return;
		}

		const safeArgs = args.map(sanitiseLogArg);
		const envelope: LogMessage = {
			kind: 'log',
			level,
			namespace,
			args: safeArgs,
			ts: Date.now()
		};
		for (const port of this.#ports) {
			const index = this.#perPort.get(port);
			if (!index) continue;
			if (LOG_LEVEL_ORDER[level] < LOG_LEVEL_ORDER[index.wantedLogLevel]) continue;
			this.#send(port, envelope);
		}
	}

	// ============================================================
	// Message handlers
	// ============================================================

	#handleHello(port: MessagePort, reqId: number, wantedLogLevel: LogLevel): void {
		const index = this.#perPort.get(port);
		if (index) index.wantedLogLevel = wantedLogLevel;
		this.#recomputeMaxWantedLogLevel();
		// Diagnostic debug — pins the "worker→port→bridge→tab-console"
		// fan-out chain. Fires on every new port join (page refresh,
		// new tab). Debug level so it's quiet by default; operators
		// enabling `oxi.setLogLevel('oxi:message-bus:worker', 'debug')`
		// see it + can verify the chain. Was warn during initial
		// debugging — downgraded because every page refresh produced
		// one warn, which was noise for anyone not actively
		// diagnosing.
		this.#debug('hello received — port registered', {
			wantedLogLevel,
			portCount: this.#ports.size,
			state: this.#transport.state
		});
		this.#send(port, {
			kind: 'hello-ack',
			reqId,
			state: this.#transport.state,
			workerVersion: this.#workerVersion
		});
	}

	async #handleSubscribe(port: MessagePort, reqId: number, topic: string): Promise<void> {
		let entry = this.#topics.get(topic);
		const isFirstPort = !entry;
		if (!entry) {
			entry = { ports: new Set(), cachedAck: undefined, cachedError: undefined };
			this.#topics.set(topic, entry);
		}
		entry.ports.add(port);
		this.#perPort.get(port)?.topics.add(topic);

		if (isFirstPort) {
			try {
				const ack = await this.#transport.subscribe(topic);
				// Second port may have joined while we awaited; serve
				// `cachedAck` to late joiners.
				entry.cachedAck = ack;
				entry.cachedError = undefined;
				// Reply to THIS port that initiated the upstream call.
				this.#send(port, { kind: 'ack', reqId, result: ack });
			} catch (err) {
				const error = toMessageBusError(err);
				entry.cachedError = error;
				entry.cachedAck = undefined;
				this.#send(port, { kind: 'nack', reqId, error });
				// The upstream subscribe failed — drop the entry so a
				// retry from a different port starts fresh. Clear the
				// port set first; the one caller already got its nack.
				entry.ports.clear();
				this.#topics.delete(topic);
			}
			return;
		}

		// Late joiner — the upstream subscribe already happened.
		if (entry.cachedAck !== undefined) {
			this.#send(port, { kind: 'ack', reqId, result: entry.cachedAck });
			return;
		}
		if (entry.cachedError !== undefined) {
			this.#send(port, { kind: 'nack', reqId, error: entry.cachedError });
			return;
		}
		// Still in flight — queue by racing the first port's response.
		// The simplest implementation reuses the first port's cached
		// ack / error once it lands by registering a one-shot waiter
		// on the entry. For Gate 2 scope we accept a short wait: the
		// transport resolves within one RTT, and the typical
		// late-join window is sub-ms (two tabs opening the same URL
		// at once).
		await this.#waitForCachedResult(topic);
		const resolved = this.#topics.get(topic);
		if (!resolved) {
			this.#send(port, {
				kind: 'nack',
				reqId,
				error: { code: RtErrorCode.INTERNAL_ERROR, message: 'subscribe_lost' }
			});
			return;
		}
		if (resolved.cachedAck !== undefined) {
			this.#send(port, { kind: 'ack', reqId, result: resolved.cachedAck });
		} else if (resolved.cachedError !== undefined) {
			this.#send(port, { kind: 'nack', reqId, error: resolved.cachedError });
		}
	}

	#handleUnsubscribe(port: MessagePort, topic: string): void {
		const entry = this.#topics.get(topic);
		if (!entry) return;
		entry.ports.delete(port);
		this.#perPort.get(port)?.topics.delete(topic);
		if (entry.ports.size === 0) {
			this.#topics.delete(topic);
			void this.#transport.unsubscribe(topic);
		}
	}

	#handleRegisterBinary(port: MessagePort, fileId: string): void {
		let set = this.#binaryByFile.get(fileId);
		if (!set) {
			set = new Set();
			this.#binaryByFile.set(fileId, set);
		}
		set.add(port);
		this.#perPort.get(port)?.binaryFileIds.add(fileId);
	}

	#handleUnregisterBinary(port: MessagePort, fileId: string): void {
		const set = this.#binaryByFile.get(fileId);
		if (!set) return;
		set.delete(port);
		this.#perPort.get(port)?.binaryFileIds.delete(fileId);
		if (set.size === 0) this.#binaryByFile.delete(fileId);
	}

	#handleRegisterWriteDenied(port: MessagePort, fileId: string): void {
		let set = this.#writeDeniedByFile.get(fileId);
		if (!set) {
			set = new Set();
			this.#writeDeniedByFile.set(fileId, set);
		}
		set.add(port);
		this.#perPort.get(port)?.writeDeniedFileIds.add(fileId);
	}

	#handleUnregisterWriteDenied(port: MessagePort, fileId: string): void {
		const set = this.#writeDeniedByFile.get(fileId);
		if (!set) return;
		set.delete(port);
		this.#perPort.get(port)?.writeDeniedFileIds.delete(fileId);
		if (set.size === 0) this.#writeDeniedByFile.delete(fileId);
	}

	async #handleCall(
		port: MessagePort,
		reqId: number,
		method: string,
		params: Record<string, unknown> | undefined
	): Promise<void> {
		try {
			const result = await this.#transport.call(method, params);
			// Port may have closed while we awaited; drop silently if so.
			if (!this.#ports.has(port)) return;
			this.#send(port, { kind: 'ack', reqId, result });
		} catch (err) {
			if (!this.#ports.has(port)) return;
			this.#send(port, { kind: 'nack', reqId, error: toMessageBusError(err) });
		}
	}

	async #handlePing(port: MessagePort, reqId: number): Promise<void> {
		try {
			const latencyMs = await this.#transport.ping();
			if (!this.#ports.has(port)) return;
			this.#send(port, { kind: 'ping-pong', reqId, latencyMs });
		} catch (err) {
			if (!this.#ports.has(port)) return;
			this.#send(port, { kind: 'nack', reqId, error: toMessageBusError(err) });
		}
	}

	#handleSetLogLevel(port: MessagePort, level: LogLevel): void {
		const index = this.#perPort.get(port);
		if (index) index.wantedLogLevel = level;
		this.#recomputeMaxWantedLogLevel();
	}

	// ============================================================
	// Fan-out helpers
	// ============================================================

	#fanoutEvent(topic: string, params: RtEventParams): void {
		const entry = this.#topics.get(topic);
		if (!entry) return;
		for (const port of entry.ports) {
			this.#send(port, { kind: 'event', topic, params });
		}
	}

	#fanoutRevoked(topic: string, params: RtRevokedParams): void {
		const entry = this.#topics.get(topic);
		if (!entry) return;
		// Server already dropped the sub; mirror its side effect in the
		// routing table so a later subscribe starts a fresh upstream.
		this.#topics.delete(topic);
		for (const port of entry.ports) {
			this.#send(port, { kind: 'revoked', topic, params });
			this.#perPort.get(port)?.topics.delete(topic);
		}
	}

	#fanoutBinary(fileId: string, bytes: ArrayBuffer): void {
		const set = this.#binaryByFile.get(fileId);
		if (!set || set.size === 0) return;
		if (set.size === 1) {
			// Single-port fast path: transferable, zero-copy to the one
			// handler that cares. The vast majority of binary frames
			// land here (one tab per file is the common case).
			const [only] = set;
			this.#send(only, { kind: 'binary', fileId, bytes }, [bytes]);
			return;
		}
		// Multi-port: structured clone per port. `postMessage` consumes
		// the transferable list on the first send, so N-port fan-out
		// has to copy. OxiCloud's Yjs frames are 20–200 B — the cost
		// is negligible.
		for (const port of set) {
			this.#send(port, { kind: 'binary', fileId, bytes: bytes.slice(0) });
		}
	}

	#fanoutWriteDenied(fileId: string, params: RtWriteDeniedParams): void {
		const set = this.#writeDeniedByFile.get(fileId);
		if (!set) return;
		for (const port of set) {
			this.#send(port, { kind: 'write-denied', fileId, params });
		}
	}

	#broadcastState(state: ConnectionState): void {
		for (const port of this.#ports) {
			this.#send(port, { kind: 'state', state });
		}
	}

	// ============================================================
	// Helpers
	// ============================================================

	#recomputeMaxWantedLogLevel(): void {
		let max: LogLevel = 'info';
		for (const port of this.#ports) {
			const index = this.#perPort.get(port);
			if (!index) continue;
			if (LOG_LEVEL_ORDER[index.wantedLogLevel] < LOG_LEVEL_ORDER[max]) {
				max = index.wantedLogLevel;
			}
		}
		this.#maxWantedLogLevel = max;
	}

	/** Poll loop for late-join subscribe: wait up to one tick for the
	 *  cached result to appear. The transport's subscribe resolves on
	 *  the next await point, so this is effectively "yield once". */
	async #waitForCachedResult(topic: string): Promise<void> {
		for (let i = 0; i < 100; i++) {
			const entry = this.#topics.get(topic);
			if (!entry) return;
			if (entry.cachedAck !== undefined || entry.cachedError !== undefined) return;
			await Promise.resolve();
		}
	}

	#isTabToWorker(
		msg: PortTabToWorkerMessage | PortWorkerToTabMessage
	): msg is PortTabToWorkerMessage {
		switch (msg.kind) {
			case 'hello':
			case 'subscribe':
			case 'unsubscribe':
			case 'register-binary-handler':
			case 'unregister-binary-handler':
			case 'register-write-denied-handler':
			case 'unregister-write-denied-handler':
			case 'send-binary':
			case 'call':
			case 'ping':
			case 'set-log-level':
			case 'provide-ticket':
			case 'request-reconnect':
				return true;
			default:
				return false;
		}
	}

	/** Send on a port. Catches the structured-clone / closed-port
	 *  throw so one dead port doesn't break fan-out. The transferable
	 *  list is optional. */
	#send(port: MessagePort, envelope: PortWorkerToTabMessage, transfer?: Transferable[]): void {
		try {
			if (transfer && transfer.length > 0) {
				port.postMessage(envelope, transfer);
			} else {
				port.postMessage(envelope);
			}
		} catch (err) {
			// Common cases: port is closed, or the envelope contains an
			// uncloneable value we missed. The caller's control flow
			// shouldn't crash on either.
			this.#warn('port send failed — deregistering port', { kind: envelope.kind, err });
			this.deregisterPort(port);
		}
	}

	/** Worker-internal warn, routed through the same `emitLog` path
	 *  so visible from any tab with debug on. Separate from the
	 *  broader `oxi:message-bus` namespace so operators can filter. */
	#warn(message: string, context: Record<string, unknown>): void {
		this.emitLog('warn', 'oxi:message-bus:worker', [message, context]);
	}
}

/** Trim non-structured-clone-safe values out of log args. `postMessage`
 *  would throw on an `Error`, a `Response`, a function, an AbortSignal —
 *  the fan-out would die for every port if we didn't scrub.
 *
 *  Preserves: strings, numbers, booleans, null, plain objects, arrays,
 *  typed arrays, Date, Map, Set. Reduces anything else to a stable
 *  descriptor so DevTools still shows SOMETHING, not nothing. */
function sanitiseLogArg(arg: unknown): unknown {
	if (arg === null || arg === undefined) return arg;
	const t = typeof arg;
	if (t === 'string' || t === 'number' || t === 'boolean' || t === 'bigint') return arg;
	if (arg instanceof Error) {
		return { __logKind: 'Error', name: arg.name, message: arg.message, stack: arg.stack };
	}
	if (arg instanceof Date) return arg;
	if (arg instanceof Map) return arg;
	if (arg instanceof Set) return arg;
	if (ArrayBuffer.isView(arg)) return arg;
	if (arg instanceof ArrayBuffer) return arg;
	if (Array.isArray(arg)) return arg.map(sanitiseLogArg);
	if (t === 'function') return { __logKind: 'Function', name: (arg as { name?: string }).name };
	if (t === 'object') {
		// Plain object — recurse one level. Deeper structures survive
		// structured clone as long as they stay JSON-like; nested
		// pathological shapes self-sanitise via the recursion.
		const out: Record<string, unknown> = {};
		for (const [k, v] of Object.entries(arg as Record<string, unknown>)) {
			try {
				out[k] = sanitiseLogArg(v);
			} catch {
				out[k] = { __logKind: 'Unloggable' };
			}
		}
		return out;
	}
	return { __logKind: 'Unknown' };
}

/** Normalise a thrown value into the wire's `MessageBusError`. The
 *  transport layer already throws these shapes; this is defence-in-
 *  depth for a stray `Error` or string. */
function toMessageBusError(err: unknown): MessageBusError {
	if (err && typeof err === 'object' && 'code' in err && 'message' in err) {
		const e = err as { code: unknown; message: unknown; data?: unknown };
		if (typeof e.code === 'number' && typeof e.message === 'string') {
			return { code: e.code, message: e.message, data: e.data };
		}
	}
	if (err instanceof Error) {
		return { code: RtErrorCode.INTERNAL_ERROR, message: err.message };
	}
	return { code: RtErrorCode.INTERNAL_ERROR, message: String(err) };
}
