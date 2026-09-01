/** Runtime configuration for the generated Hey API client. */
import type { CreateClientConfig } from './generated/client.gen';
import { apiFetch } from './client';
import { getCsrfHeaders } from './csrf';

const SAFE_METHODS = new Set(['GET', 'HEAD', 'OPTIONS', 'TRACE']);

/**
 * Keep generated calls on the same transport as the hand-written endpoint
 * modules. This preserves DPoP proofs, refresh de-duplication, maintenance
 * status handling, and the forced-password-change redirect.
 *
 * Mutating generated calls also receive the current CSRF token automatically;
 * the legacy wrappers add that header themselves at each call site.
 */
const generatedApiFetch: typeof fetch = (input, init) => {
	const requestMethod = input instanceof Request ? input.method : 'GET';
	const method = (init?.method ?? requestMethod).toUpperCase();
	const headers = new Headers(input instanceof Request ? input.headers : undefined);

	new Headers(init?.headers).forEach((value, name) => headers.set(name, value));
	if (!SAFE_METHODS.has(method)) {
		for (const [name, value] of Object.entries(getCsrfHeaders())) {
			headers.set(name, value);
		}
	}

	return apiFetch(input, {
		...init,
		credentials: init?.credentials ?? 'same-origin',
		headers
	});
};

export const createClientConfig: CreateClientConfig = (config) => ({
	...config,
	baseUrl: '',
	credentials: 'same-origin',
	fetch: generatedApiFetch
});
