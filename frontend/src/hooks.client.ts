/**
 * Client startup hook. Runs once before the first route renders:
 *  - wires the API client's session-expired behaviour (clear store + redirect),
 *  - loads translations for the resolved locale.
 */
import log from 'loglevel';
import { setSessionExpiredHandler } from '$lib/api/client';
import { initI18n } from '$lib/i18n/index.svelte';
import { session } from '$lib/stores/session.svelte';
import { seedNonceFromCookie } from '$lib/auth/dpop-proof';

// DevTools shortcut: expose a small `oxi.*` helper on window so users
// can toggle log levels and knobs from the browser console without
// needing to import anything.
//
// Log levels — namespaces used today: `oxi:upload` (delta + direct
// upload pipeline). Levels: 'trace' | 'debug' | 'info' | 'warn' | 'error' | 'silent'.
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
		};
	}
}

export async function init(): Promise<void> {
	if (typeof window !== 'undefined') {
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
			window.location.href = '/login?source=session_expired';
		}
	});

	// Consume the one-shot `oxicloud_dpop_nonce` cookie the backend
	// stamps on every login-success response — critical for redirect-
	// flow logins (OIDC callback, magic-link finish) where the browser
	// lands here BEFORE any client-side login handler has run. Without
	// this, the layout's `session.load()` fetchMe would be the first
	// bound request and eat a `use_dpop_nonce` 401 → retry cycle.
	seedNonceFromCookie();

	await initI18n();
}
