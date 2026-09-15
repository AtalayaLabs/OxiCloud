/** Public API entry point. Every request is built and executed by Hey API. */
import { client } from './generated/client.gen';
import type { RequestOptions } from './generated/client';
import { generatedApiFetch } from './hey-api';
import { apiProbeTransport, createApiTransport, urlString } from './transport';
export {
	createApiFetch,
	setSessionExpiredHandler,
	setPasswordChangeRequiredHandler,
	setLogoutInProgress,
	isLogoutInProgress
} from './transport';
export type { FetchFn, ApiClientDeps } from './transport';

export interface ApiRequestInit extends RequestInit {
	/** A bootstrap 401 means unauthenticated; do not refresh or redirect. */
	retryUnauthorized?: boolean;
	/** Alternate wire transport (e.g. XHR upload progress), still built by Hey API. */
	fetch?: typeof fetch;
}

/**
 * Response-preserving facade for endpoint adapters, downloads and multipart
 * uploads. The generated SDK uses the same configured transport directly.
 * Keep error bodies readable by the existing endpoint-specific error handlers.
 */
export async function apiFetch(
	input: RequestInfo | URL,
	init: ApiRequestInit = {}
): Promise<Response> {
	const { retryUnauthorized, fetch: wireFetch, ...requestInit } = init;
	const request = input instanceof Request ? input : undefined;
	const headers = new Headers(request?.headers);
	new Headers(requestInit.headers).forEach((value, name) => headers.set(name, value));
	const url = new URL(urlString(input), globalThis.location?.origin ?? 'http://localhost');
	let response: Response | undefined;
	try {
		await client.request({
			...requestInit,
			url: url.pathname + url.search,
			baseUrl: url.origin,
			method: (requestInit.method ?? request?.method ?? 'GET').toUpperCase() as NonNullable<
				RequestOptions['method']
			>,
			body: requestInit.body ?? request?.body ?? undefined,
			// Existing adapters already serialize JSON; preserve multipart/binary bodies.
			bodySerializer: (body) => body,
			headers: { 'Content-Type': null, ...Object.fromEntries(headers) },
			credentials: requestInit.credentials ?? request?.credentials ?? 'same-origin',
			signal: requestInit.signal ?? request?.signal,
			...(request?.body ? { duplex: 'half' } : {}),
			parseAs: 'stream',
			responseStyle: 'fields',
			throwOnError: true,
			fetch: async (input, init) => {
				response = wireFetch
					? await generatedApiFetch(input, init, createApiTransport(wireFetch))
					: retryUnauthorized === false
						? await generatedApiFetch(input, init, apiProbeTransport)
						: await generatedApiFetch(input, init);
				return response.ok ? response : response.clone();
			}
		});
	} catch (error) {
		if (!response) throw error;
	}
	if (!response) throw new Error('API request completed without a response');
	return response;
}

/** Convenience: fetch JSON, throwing on non-2xx. */
export async function apiJson<T>(input: RequestInfo | URL, init?: RequestInit): Promise<T> {
	const res = await apiFetch(input, init);
	if (!res.ok) {
		throw new ApiError(res.status, res.statusText, input);
	}
	return (await res.json()) as T;
}

export class ApiError extends Error {
	/**
	 * `error_type` field from the backend's `ErrorResponse` body, when
	 * present. Callers switch on this to render specific UX for
	 * distinguished failures (e.g. `EmailNotVerified` → "resend
	 * verification link" prompt). Falls back to `undefined` when the
	 * response body isn't parseable or the endpoint doesn't emit one.
	 */
	readonly errorType?: string;

	constructor(
		readonly status: number,
		readonly statusText: string,
		readonly resource: RequestInfo | URL,
		errorType?: string,
		serverMessage?: string
	) {
		super(
			serverMessage ?? `API ${status} ${statusText} for ${urlString(resource as RequestInfo | URL)}`
		);
		this.name = 'ApiError';
		this.errorType = errorType;
	}
}
