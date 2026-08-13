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
// can toggle log levels from the browser console without needing to
// import anything. Namespaces used today: `oxi:upload` (delta + direct
// upload pipeline). Levels: 'trace' | 'debug' | 'info' | 'warn' | 'error' | 'silent'.
// Choices persist to `localStorage['loglevel:<namespace>']` via loglevel.
//
// Usage:
//   oxi.setLogLevel('oxi:upload', 'debug')    // deep dive
//   oxi.setLogLevel('oxi:upload', 'warn')     // quiet
//   oxi.log.setLevel('debug')                  // everything to debug
declare global {
	interface Window {
		oxi?: {
			log: typeof log;
			setLogLevel: (namespace: string, level: log.LogLevelDesc) => string;
			listLogLevels: () => Record<string, string>;
		};
	}
}

export async function init(): Promise<void> {
	if (typeof window !== 'undefined') {
		window.oxi = {
			log,
			// Return a confirmation string so the DevTools echo is a
			// useful "worked → new level" signal instead of `undefined`.
			setLogLevel(namespace, level) {
				log.getLogger(namespace).setLevel(level);
				return `${namespace} → ${level}`;
			},
			// Enumerate the levels loglevel has persisted so users can see
			// what's currently set without opening the Application tab.
			listLogLevels() {
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
