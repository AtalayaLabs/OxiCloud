import { appPath, withBase } from '$lib/utils/appPath';
/** Static assets are outside the API client's session/refresh lifecycle. */
export function fetchAsset(input: string, init?: RequestInit): Promise<Response> {
	input = withBase(input);
	const url = new URL(input, globalThis.location?.origin ?? 'http://localhost');
	const pathname = appPath(url.pathname);
	if (pathname === '/api' || pathname.startsWith('/api/') || pathname.startsWith('/wopi/')) {
		throw new Error('API requests must use the generated API client');
	}
	return fetch(input, init);
}
