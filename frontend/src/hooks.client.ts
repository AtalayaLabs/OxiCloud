/**
 * Client startup hook. Runs once before the first route renders:
 *  - wires the API client's session-expired behaviour (clear store + redirect),
 *  - loads translations for the resolved locale.
 */
import { setSessionExpiredHandler } from '$lib/api/client';
import { initI18n } from '$lib/i18n/index.svelte';
import { session } from '$lib/stores/session.svelte';
import { seedNonceFromCookie } from '$lib/auth/dpop-proof';

export async function init(): Promise<void> {
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
