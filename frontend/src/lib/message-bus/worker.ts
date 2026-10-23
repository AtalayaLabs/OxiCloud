/// <reference lib="webworker" />
// SharedWorker entry — the one file in `message-bus/` that
// `new SharedWorker(...)` resolves to. Pure glue: wire up one
// `WsBusTransport`, one `WorkerCore`, and the port lifecycle. All
// real behaviour lives in those two.
//
// The worker makes ZERO HTTP calls. Tickets for the WS upgrade
// arrive from the tab side via `provide-ticket` port envelopes —
// the tab fetched them through `apiFetch`, so DPoP, CSRF, session
// cookies, and refresh-on-401 are all handled where the browser's
// auth machinery lives. On reconnect the worker asks any attached
// port for a fresh ticket via `need-ticket`.
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

/** `wss://host/api/rt/ws` relative to the worker's origin — the
 *  SharedWorker security model enforces same-origin anyway. */
function wsUrl(): string {
	const proto = self.location.protocol === 'https:' ? 'wss:' : 'ws:';
	return `${proto}//${self.location.host}/api/rt/ws`;
}

/** Lazily-constructed on first `connect`. One per worker instance. */
let core: WorkerCore | null = null;

function ensureCore(): WorkerCore {
	if (core) return core;
	const transport = new WsBusTransport({ wsUrl: wsUrl() });
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
			rawMethod(...args);
			try {
				c.emitLog(methodName as LogLevel, String(loggerName ?? 'oxi:message-bus'), args);
			} catch {
				// Never crash the log path from a fan-out failure.
			}
		};
	};
	log.setLevel(log.getLevel());
}

// ============================================================
// Port lifecycle — the actual SharedWorker API
// ============================================================

const scope = self as unknown as SharedWorkerGlobalScope;

scope.addEventListener('connect', (event: Event) => {
	const port = (event as MessageEvent).ports[0];
	if (!port) return;

	// Build the core on the first connect so every subsequent
	// register/handlePortMessage goes through the same instance.
	const c = ensureCore();
	c.registerPort(port);

	port.onmessage = (msg) => {
		const parsed = parsePortMessage(msg.data);
		if (!parsed) {
			console.warn('[worker] dropped unparseable envelope', msg.data);
			return;
		}
		void c.handlePortMessage(port, parsed);
	};

	port.addEventListener('close', () => c.deregisterPort(port));

	port.start();
});
