/**
 * Client startup hook. Runs once before the first route renders:
 *  - wires the API client's session-expired behaviour (clear store + redirect),
 *  - loads translations for the resolved locale.
 */
import { base } from '$app/paths';
import log from 'loglevel';
import { setSessionExpiredHandler } from '$lib/api/client';
import { initI18n } from '$lib/i18n/index.svelte';
import { serverConfig } from '$lib/stores/serverConfig.svelte';
import { session } from '$lib/stores/session.svelte';
import { seedNonceFromCookie } from '$lib/auth/dpop-proof';

// DevTools shortcut: expose a small `oxi.*` helper on window so users
// can toggle log levels and knobs from the browser console without
// needing to import anything.
//
// Log levels — namespaces used today: `oxi:upload` (delta + direct
// upload pipeline), `oxi:message-bus` (WebSocket client + `useTopic`),
// `oxi:config` (server-config boot fetch), `oxi:auth` (login/logout +
// fetchMe/tryRefresh + DPoP binding), `oxi:session` (session store
// probe/refresh/dedup).
// Levels: 'trace' | 'debug' | 'info' | 'warn' | 'error' | 'silent'.
// Choices persist to `localStorage['loglevel:<namespace>']` via loglevel.
//
//   oxi.setLogLevel('oxi:upload', 'debug')    // deep dive
//   oxi.setLogLevel('oxi:upload', 'warn')     // quiet
//   oxi.log.setLevel('debug')                  // everything to debug
//
// Delta-upload batch size — bytes per PUT to `/api/files/delta/chunks`.
// Default 8 MiB. Behind proxies with tight per-request timeouts
// (Cloudflare Tunnel: 100 s absolute), lower this so each PUT completes
// within the window on a slow uplink:
//
//   oxi.UPLOAD_BATCH_BYTES = 1024 * 1024      // 1 MiB per PUT
//
// Persists to `localStorage['oxi:upload:batchBytes']`. Read on every
// upload — set once from the console, refresh not required.
const BATCH_BYTES_KEY = 'oxi:upload:batchBytes';
const BATCH_BYTES_DEFAULT = 8 * 1024 * 1024;

function readBatchBytes(): number {
	try {
		if (typeof localStorage === 'undefined') return BATCH_BYTES_DEFAULT;
		const raw = localStorage.getItem(BATCH_BYTES_KEY);
		if (!raw) return BATCH_BYTES_DEFAULT;
		const n = Number(raw);
		return Number.isFinite(n) && n > 0 ? n : BATCH_BYTES_DEFAULT;
	} catch {
		return BATCH_BYTES_DEFAULT;
	}
}

function writeBatchBytes(n: number): void {
	if (typeof localStorage === 'undefined') return;
	try {
		if (n === BATCH_BYTES_DEFAULT) localStorage.removeItem(BATCH_BYTES_KEY);
		else localStorage.setItem(BATCH_BYTES_KEY, String(n));
	} catch {
		/* quota / disabled — best-effort */
	}
}

declare global {
	interface Window {
		oxi?: {
			log: typeof log;
			setLogLevel: (namespace: string, level: log.LogLevelDesc) => string;
			listLogLevels: () => Record<string, string>;
			UPLOAD_BATCH_BYTES: number;
			/** End-to-end diagnostic for the message bus. Round-trips a
			 *  `rt.ping` across the full chain (tab → worker in shared
			 *  mode, OR tab-local in per-tab mode → server → back) and
			 *  returns a structured snapshot:
			 *
			 *    await oxi.checkBusChain()
			 *    // {
			 *    //   ok: true,
			 *    //   impl: 'SharedWorkerBridge',
			 *    //   state: 'connected',
			 *    //   workerVersion: 'dev-1792800537',
			 *    //   latencyMs: 7
			 *    // }
			 *
			 *  `ok: false` with `error: {code, message}` is returned
			 *  instead of throwing — easier to read in DevTools when
			 *  the WS is down. `message === 'not_connected'` is the
			 *  common signal that the worker lost its WS. */
			checkBusChain: () => Promise<{
				ok: boolean;
				impl: string;
				state: string;
				workerVersion: string | null;
				latencyMs: number | null;
				error?: { code: number; message: string };
			}>;
			/** Force the message bus to re-arm its circuit breaker and
			 *  schedule an immediate reconnect. Works whether the state
			 *  is `disconnected`, `unavailable`, or stuck in a
			 *  `connecting` cycle. No-op when already `connected`.
			 *  Routes through the `request-reconnect` port envelope in
			 *  shared mode. */
			reconnectBus: () => Promise<void>;
		};
	}
}

export async function init(): Promise<void> {
	if (typeof window !== 'undefined') {
		// Register the DPoP service worker as the very first thing the client
		// does. The root layout waits for `navigator.serviceWorker.ready` and
		// reloads once when the page landed uncontrolled — register any later
		// (from `onMount`, say) and that reload arrives after the login form is
		// already usable, wiping whatever the user has typed.
		//
		// `scope: base || '/'`: the app's home URL is the BARE base path
		// (`/oxicloud`, no trailing slash), which falls outside the scope
		// derived from the script URL (`/oxicloud/`). The server authorizes the
		// widened scope with `Service-Worker-Allowed` on the script response.
		if ('serviceWorker' in navigator) {
			void navigator.serviceWorker
				.register(`${base}/service-worker.js`, { type: 'module', scope: base || '/' })
				.catch((err) => log.debug('DPoP service worker registration failed', err));
		}

		const helpers = {
			log,
			// Return a confirmation string so the DevTools echo is a
			// useful "worked → new level" signal instead of `undefined`.
			setLogLevel(namespace: string, level: log.LogLevelDesc): string {
				log.getLogger(namespace).setLevel(level);
				return `${namespace} → ${level}`;
			},
			// Enumerate the levels loglevel has persisted so users can see
			// what's currently set without opening the Application tab.
			listLogLevels(): Record<string, string> {
				const out: Record<string, string> = {};
				if (typeof localStorage === 'undefined') return out;
				for (let i = 0; i < localStorage.length; i++) {
					const key = localStorage.key(i);
					if (key?.startsWith('loglevel:')) {
						out[key.slice('loglevel:'.length)] = localStorage.getItem(key) ?? '';
					}
				}
				return out;
			},
			// End-to-end bus diagnostic — see the Window.oxi JSDoc above
			// for the full return shape and failure modes. Dynamic
			// import so the main `window.oxi` setup stays sync and the
			// message-bus module graph isn't eagerly pulled in by
			// hooks.client.ts (it's already loaded elsewhere; the
			// dynamic import returns the cached singleton).
			async checkBusChain() {
				const { messageBus } = await import('$lib/message-bus/client.svelte');
				const impl = messageBus.constructor.name;
				// `workerVersion` only exists on the SharedWorker bridge;
				// null in per-tab mode. The optional chain + the ?? null
				// keep both impls' return shape identical.
				const workerVersion =
					(messageBus as unknown as { workerVersion?: string | null }).workerVersion ?? null;
				try {
					const latencyMs = await messageBus.ping();
					return {
						ok: true,
						impl,
						state: messageBus.state,
						workerVersion,
						latencyMs
					};
				} catch (err) {
					const e = err as { code?: number; message?: string };
					return {
						ok: false,
						impl,
						state: messageBus.state,
						workerVersion,
						latencyMs: null,
						error: {
							code: typeof e.code === 'number' ? e.code : -1,
							message: typeof e.message === 'string' ? e.message : String(err)
						}
					};
				}
			},
			// Manual reconnect — forces the bus to re-arm its circuit
			// breaker and schedule an immediate attempt. Easiest way
			// out of a `state === 'unavailable'` without reloading the
			// page, and shortens the long-backoff tail during
			// `disconnected` cycles.
			async reconnectBus() {
				const { messageBus } = await import('$lib/message-bus/client.svelte');
				messageBus.reconnect();
			}
		};
		// UPLOAD_BATCH_BYTES: getter reads live from localStorage so any
		// tab / component pulling `window.oxi.UPLOAD_BATCH_BYTES` sees the
		// current value; setter persists so the choice survives reload
		// (mirrors loglevel's persistence pattern).
		Object.defineProperty(helpers, 'UPLOAD_BATCH_BYTES', {
			get: readBatchBytes,
			set: writeBatchBytes,
			enumerable: true,
			configurable: true
		});
		window.oxi = helpers as Window['oxi'];
	}

	setSessionExpiredHandler(() => {
		session.reset();
		if (typeof window !== 'undefined') {
			window.location.href = `${base}/login?source=session_expired`;
		}
	});

	// Consume the one-shot `oxicloud_dpop_nonce` cookie the backend
	// stamps on every login-success response — critical for redirect-
	// flow logins (OIDC callback, magic-link finish) where the browser
	// lands here BEFORE any client-side login handler has run. Without
	// this, the layout's `session.load()` fetchMe would be the first
	// bound request and eat a `use_dpop_nonce` 401 → retry cycle.
	seedNonceFromCookie();

	// Boot in parallel: translations and server-config discovery are
	// independent of each other, and both must resolve before any route
	// mounts. `serverConfig.load()` primes the reactive feature-flag
	// store; `useTopic` / `useFolderTopic` / `useReconnect` read from
	// it to decide whether to open a WebSocket at all. See
	// `stores/serverConfig.svelte.ts` for the failure semantics
	// (defaults preserved on fetch error).
	await Promise.all([initI18n(), serverConfig.load()]);
}
