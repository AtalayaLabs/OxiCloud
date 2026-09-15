/** Static assets are outside the API client's session/refresh lifecycle. */
export function fetchAsset(input: string, init?: RequestInit): Promise<Response> {
	const url = new URL(input, globalThis.location?.origin ?? 'http://localhost');
	if (
		url.pathname === '/api' ||
		url.pathname.startsWith('/api/') ||
		url.pathname.startsWith('/wopi/')
	) {
		throw new Error('API requests must use the generated API client');
	}
	return fetch(input, init);
}
