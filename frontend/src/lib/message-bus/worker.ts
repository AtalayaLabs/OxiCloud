/// <reference lib="webworker" />
// SharedWorker entry — the one file in `message-bus/` that
// `new SharedWorker(...)` resolves to. Pure glue: wire up one
// `WsBusTransport`, one `WorkerCore`, and the port lifecycle. All
// real behaviour lives in those two.
//
// Workflow:
//
//   1. First `connect` event (first tab opens the worker): we have
//      no transport yet — the tab's `hello` envelope will carry
//      the CSRF token we need for the ticket POST. Defer transport
//      construction until that hello arrives.
//   2. Each subsequent `connect` event registers the port against
//      the existing `WorkerCore`. The transport continues to serve
//      one WS for the whole browser.
//   3. Worker-side `busLog` fan-out: `loglevel`'s handler is
//      replaced with a thin wrapper that calls `WorkerCore.emitLog`,
//      so every log line goes out as a port envelope to any tab
//      that asked for it. Per-tab log tail parity.
//
// Companion:
//   `docs/plan/message-bus-via-shared-worker.md § Routing state inside the worker`

import log from 'loglevel';

import { WorkerCore } from './worker-core';
import { WsBusTransport } from './ws-bus-transport';
import { parsePortMessage, type LogLevel } from './port-protocol';

/** Stamped at build time by the Vite config. Lets a tab with a
 *  mismatched expected version render a "reload all tabs" invite
 *  via `hello-ack.workerVersion`. */
declare const __OXI_WORKER_VERSION__: string;
const WORKER_VERSION = typeof __OXI_WORKER_VERSION__ === 'string' ? __OXI_WORKER_VERSION__ : 'dev';

/** URLs the worker POSTs / opens. Relative to `self.location.origin`
 *  so the worker always talks to the same host that served it —
 *  SharedWorker's security model enforces same-origin anyway. */
function wsUrl(): string {
	const proto = self.location.protocol === 'https:' ? 'wss:' : 'ws:';
	return `${proto}//${self.location.host}/api/rt/ws`;
}
function ticketUrl(): string {
	return `${self.location.origin}/api/rt/ticket`;
}

/** The one `WorkerCore` for this worker instance. Built lazily on
 *  first `hello` so we have the CSRF token for the transport. */
let core: WorkerCore | null = null;

/** Build the WorkerCore + transport on the first `hello` carrying a
 *  usable CSRF token. Subsequent hellos just update log level + reply.
 *  Returns the ready core. */
function ensureCore(csrfToken: string): WorkerCore {
	if (core) return core;
	const transport = new WsBusTransport({
		wsUrl: wsUrl(),
		ticketUrl: ticketUrl(),
		csrfToken
	});
	core = new WorkerCore(transport, WORKER_VERSION);

	// Replace `loglevel`'s factory so every log call fans out to
	// connected tabs via `WorkerCore.emitLog`. The push-down filter
	// inside the core drops anything no tab asked for.
	installLoglevelFanout(core);

	return core;
}

function installLoglevelFanout(c: WorkerCore): void {
	const originalFactory = log.methodFactory;
	log.methodFactory = function (methodName, logLevel, loggerName) {
		const rawMethod = originalFactory.call(this, methodName, logLevel, loggerName);
		return function (...args: unknown[]) {
			// Call the raw method so the worker's own console still gets
			// the line (DevTools "Shared workers" panel). Then fan out.
			rawMethod(...args);
			try {
				c.emitLog(methodName as LogLevel, String(loggerName ?? 'oxi:message-bus'), args);
			} catch {
				// Never crash the log path from a fan-out failure.
			}
		};
	};
	// loglevel caches the method factory per-logger — rebuild handlers.
	log.setLevel(log.getLevel());
}

// ============================================================
// Port lifecycle — the actual SharedWorker API
// ============================================================

// `self` typed as a SharedWorkerGlobalScope. In a non-worker environment
// (e.g. accidentally importing `worker.ts` into a page bundle), this is
// a no-op — `connect` just never fires.
const scope = self as unknown as SharedWorkerGlobalScope;

scope.addEventListener('connect', (event: Event) => {
	// `connect` carries the newly-created MessagePort in `ports[0]`.
	const port = (event as MessageEvent).ports[0];
	if (!port) return;

	port.onmessage = (msg) => {
		const parsed = parsePortMessage(msg.data);
		if (!parsed) {
			// Pre-core-ready drop — happens if the port sends garbage
			// before `hello`. Log via console.warn directly; the
			// fan-out isn't installed yet.
			console.warn('[worker] dropped unparseable envelope', msg.data);
			return;
		}
		// First `hello` boots the core (we need the CSRF token the tab
		// forwarded in the envelope).
		if (parsed.kind === 'hello' && !core) {
			// `hello` doesn't carry CSRF today — the port-protocol envelope
			// stops at `wantedLogLevel`. The tab bridge CAN forward one via
			// a dedicated envelope (future-work). For now the worker boots
			// without a CSRF; the ticket POST will fail with a 403 on
			// CSRF-strict backends and the circuit breaker trips. See the
			// plan § "CSRF forwarding" note for the follow-up.
			const c = ensureCore('');
			c.registerPort(port);
			void c.handlePortMessage(port, parsed);
			return;
		}
		if (!core) {
			console.warn('[worker] message before hello', { kind: parsed.kind });
			return;
		}
		void core.handlePortMessage(port, parsed);
	};

	// Node's MessagePort fires `close`; the browser spec removed `close`
	// but Chrome fires it anyway for a tab close. If present, deregister.
	// If not, cleanup is best-effort — a dead port will emit nothing and
	// its routing entries stay allocated until its topic set is drained
	// by another tab's `unsubscribe` or similar.
	port.addEventListener('close', () => {
		if (core) core.deregisterPort(port);
	});

	// Browsers deliver `message` events only after `start()` on ports
	// obtained from `MessageChannel`; `connect` ports are already
	// started, but a defensive `start()` is a cheap no-op.
	port.start();

	// Register the port immediately once the core exists. If the first
	// message is `hello`, the handler above calls `registerPort` first.
	if (core) core.registerPort(port);
});
