// SharedWorker feature detection — handshake-based, not `typeof`.
//
// Decides whether this browser/tab can run the shared bus. The three
// signals the plan calls out:
//
//   1. `typeof SharedWorker` — API presence. Safari < 16.4, some
//      enterprise policy variants.
//   2. `new SharedWorker(...)` — construction. CSP throws here when
//      `SharedWorkerAllowed=false` is set.
//   3. Live handshake — the worker ships a `hello-ack` within 1 s of
//      receiving a `hello`. Silence means the worker didn't actually
//      load (404, MIME mismatch, iOS PWA quirks) OR is on the wrong
//      version. Handshake success is the only proof shared mode
//      works; all three signals must agree.
//
// On any failure we return `null`; the caller (`client.svelte.ts`'s
// factory in Gate 2b) uses that to fall back to the per-tab client.
// The fallback is silent to consumers.
//
// Companion:
//   * `shared-worker-bridge.svelte.ts` — what we construct on success.
//   * `docs/plan/message-bus-via-shared-worker.md § Gate 2`

import log from 'loglevel';

import { SharedWorkerBridge } from './shared-worker-bridge.svelte';

const busLog = log.getLogger('oxi:message-bus');

const HANDSHAKE_TIMEOUT_MS = 1_000;

/**
 * Try to spin up a SharedWorker-backed bridge. Returns `null` if ANY
 * step fails — the caller falls back to the per-tab client.
 *
 * `workerUrl` is the Vite-produced URL for the compiled worker
 * entry. The caller passes it in rather than constructing a `new
 * URL(..., import.meta.url)` here so `detect.ts` stays pure and the
 * caller's Vite config is the single source of truth about the
 * worker module graph.
 */
export async function trySharedWorker(workerUrl: string): Promise<SharedWorkerBridge | null> {
	// (1) API presence.
	if (typeof SharedWorker === 'undefined') {
		busLog.info('SharedWorker not available — falling back to per-tab bus', {
			reason: 'typeof_undefined'
		});
		return null;
	}

	// (2) Construction. Browser-level CSP (`SharedWorkerAllowed=false`
	// via enterprise policy, sandboxed iframe without
	// `allow-same-origin`) throws synchronously here.
	let worker: SharedWorker;
	try {
		worker = new SharedWorker(workerUrl, { type: 'module', name: 'oxi-bus' });
	} catch (err) {
		busLog.info('SharedWorker construction threw — falling back to per-tab bus', {
			reason: 'construct_threw',
			err
		});
		return null;
	}

	// `onerror` on a SharedWorker fires if the worker script failed to
	// load (404, MIME mismatch, parse error) — browser-dependent, so
	// treat this as a signal, not a guarantee. Hook it so we can log
	// WHY the handshake timed out when it does.
	let scriptLoadErrored: unknown = undefined;
	worker.onerror = (e) => {
		scriptLoadErrored = e;
	};

	// (3) Handshake. Build the bridge, race its handshake against a 1-
	// second deadline. The bridge's constructor fires `hello`; the
	// worker's response is what resolves `whenHandshakeComplete()`.
	const bridge = new SharedWorkerBridge(worker.port);
	const handshake = bridge.whenHandshakeComplete();

	const timeout = new Promise<'timeout'>((resolve) =>
		setTimeout(() => resolve('timeout'), HANDSHAKE_TIMEOUT_MS)
	);
	const outcome = await Promise.race([handshake.then(() => 'ok' as const), timeout]);
	if (outcome === 'timeout') {
		busLog.info('SharedWorker handshake timed out — falling back to per-tab bus', {
			reason: 'handshake_timeout',
			timeoutMs: HANDSHAKE_TIMEOUT_MS,
			scriptLoadErrored
		});
		// Best-effort cleanup. The bridge's close() tears down pending
		// state so we don't leak the port.
		try {
			bridge.close();
		} catch {
			// Already dead; nothing to do.
		}
		return null;
	}

	busLog.info('SharedWorker handshake complete — shared bus is live', {
		workerVersion: bridge.workerVersion
	});
	return bridge;
}
