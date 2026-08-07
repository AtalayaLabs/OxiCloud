/**
 * Typed API client with transparent 401 → token-refresh → retry.
 *
 * Ported from static/js/core/fetchWrapper.js. Unlike that wrapper, this does
 * NOT monkeypatch `window.fetch`; every endpoint module calls `apiFetch`
 * explicitly. The behavioural invariants are preserved exactly:
 *
 *  - A captured raw `fetch` is used for the real network calls so the refresh
 *    request and the retry never re-enter the interceptor (no recursion).
 *  - Concurrent 401s collapse into a single in-flight `/api/auth/refresh`.
 *  - Cross-origin responses are passed through untouched.
 *  - Auth primitives (login/logout/refresh/register/setup/oidc/device) and
 *    public-share endpoints (/api/s/) bypass the refresh-and-retry path:
 *    a 401 there is genuine ("bad credentials" / "password required"), not an
 *    expired access token.
 *  - When refresh fails, the session-expired handler fires (clear + redirect)
 *    and the call rejects.
 */

import { getCsrfHeaders } from './csrf';
import { updateFromHeader } from '$lib/stores/serverStatus.svelte';

/**
 * Name of the response header the server stamps while a
 * maintenance event is live. Case-insensitive on the wire — the
 * Fetch API's `Headers.get` matches irrespective of case, so this
 * constant matches whatever axum emits.
 */
const SERVER_STATUS_HEADER = 'x-server-status';

const REFRESH_ENDPOINT = '/api/auth/refresh';

/** Auth primitives — a 401 here is genuine, never an expired access token. */
const AUTH_PRIMITIVES = [
	'/api/auth/login',
	'/api/auth/logout',
	'/api/auth/refresh',
	'/api/auth/register',
	'/api/auth/setup',
	'/api/auth/oidc/',
	'/api/auth/device/'
];

export type FetchFn = typeof fetch;

export interface ApiClientDeps {
	/** Underlying fetch used for the real network call (bypasses the interceptor). */
	rawFetch: FetchFn;
	/** Invoked once when a refresh definitively fails (clear session + redirect). */
	onSessionExpired: () => void;
	/**
	 * Invoked when the server returns `403 { error_type: "PasswordChangeRequired" }`.
	 * Typically routes the SPA to `/profile?forcePasswordChange=1` — the same
	 * destination the root layout's nav-guard uses for a fresh navigation. Default
	 * is a no-op; the app wires the real handler at startup.
	 */
	onPasswordChangeRequired?: () => void;
	/** Test seam for `window.location.origin`. */
	origin?: string;
}

function urlString(input: RequestInfo | URL): string {
	if (typeof input === 'string') return input;
	if (input instanceof URL) return input.href;
	return input.url ?? '';
}

function isCrossOrigin(urlStr: string, origin: string): boolean {
	try {
		return new URL(urlStr, origin).origin !== origin;
	} catch {
		// Unparseable URL — treat as cross-origin so we pass it through untouched.
		return true;
	}
}

function bypassesRetry(urlStr: string): boolean {
	return AUTH_PRIMITIVES.some((p) => urlStr.includes(p)) || urlStr.includes('/api/s/');
}

/**
 * Build an isolated apiFetch with its own refresh-dedup state. Used directly in
 * tests; the app uses the default singleton below.
 */
export function createApiFetch(deps: ApiClientDeps): FetchFn {
	const { rawFetch, onSessionExpired } = deps;
	// Default no-op keeps existing test callers that don't wire this
	// dep from crashing on a 403 PasswordChangeRequired — they'd just
	// see the raw 403 flow through, which is what they already assert.
	const onPasswordChangeRequired = deps.onPasswordChangeRequired ?? (() => {});
	let refreshInFlight: Promise<boolean> | null = null;

	async function refresh(): Promise<boolean> {
		if (refreshInFlight) return refreshInFlight;
		refreshInFlight = (async () => {
			try {
				const r = await rawFetch(REFRESH_ENDPOINT, {
					method: 'POST',
					credentials: 'same-origin',
					headers: { 'Content-Type': 'application/json', ...getCsrfHeaders() },
					body: '{}'
				});
				return r.ok;
			} catch {
				return false;
			} finally {
				refreshInFlight = null;
			}
		})();
		return refreshInFlight;
	}

	const apiFetch: FetchFn = async (input, init) => {
		const origin = deps.origin ?? globalThis.location?.origin ?? 'http://localhost';
		const response = await rawFetch(input, init);
		// Server-status header piggyback — the server stamps
		// `x-server-status` on every response while a maintenance
		// event is in progress (see middleware::server_status). Read
		// it and update the reactive store; the AppShell banner
		// subscribes and shows/hides itself. Absent header = nothing
		// happening; the update fn resets the store to default in
		// that case so a lingering banner disappears.
		//
		// Runs on EVERY response including a 401 (below) so a session
		// refresh doesn't accidentally clear a live banner.
		updateFromHeader(response.headers.get(SERVER_STATUS_HEADER));

		// Backend `require_no_password_change_pending_layer` returns 403
		// `PasswordChangeRequired` on every non-allowlisted endpoint
		// while the caller's `force_password_change_at_next_login` flag
		// is set (admin picked a temporary password). Intercepting here
		// short-circuits any stale-tab request that outran the SPA's
		// nav-guard — the user is bounced to `/profile` in mandatory
		// mode, matching what the guard would do on a fresh navigation.
		//
		// Clones the body so downstream callers can still consume the
		// response after we've peeked at the error_type. Skipped for
		// non-JSON responses (WebDAV, etc.) — the check silently
		// falls through and returns the original 403 to the caller,
		// which will surface its own error the usual way.
		if (response.status === 403) {
			const clone = response.clone();
			try {
				const body = (await clone.json()) as { error_type?: unknown };
				if (body?.error_type === 'PasswordChangeRequired') {
					onPasswordChangeRequired();
				}
			} catch {
				/* not JSON or parse failed — pass through as normal 403 */
			}
			return response;
		}

		if (response.status !== 401) return response;

		const urlStr = urlString(input as RequestInfo | URL);
		if (isCrossOrigin(urlStr, origin)) return response;
		if (bypassesRetry(urlStr)) return response;

		const refreshed = await refresh();
		if (!refreshed) {
			onSessionExpired();
			throw new Error('Session expired');
		}
		const retryResponse = await rawFetch(input, init);
		updateFromHeader(retryResponse.headers.get(SERVER_STATUS_HEADER));
		return retryResponse;
	};

	return apiFetch;
}

// ── Default singleton ──────────────────────────────────────────────────────

let sessionExpiredHandler: () => void = () => {
	if (typeof window !== 'undefined') {
		window.location.href = '/login?source=session_expired';
	}
};

/** Wire the real session-expired behaviour (clear store + redirect) at startup. */
export function setSessionExpiredHandler(fn: () => void): void {
	sessionExpiredHandler = fn;
}

// Same shape as `sessionExpiredHandler` — mutable so the app can install
// the real behaviour post-mount, and a fallback for the (rare) case
// where no handler is wired yet (bootstrap, tests). The fallback does
// a hard `window.location` navigation so a stale tab that outran the
// SPA's nav-guard still lands the user on the mandatory form.
let passwordChangeRequiredHandler: () => void = () => {
	if (typeof window !== 'undefined') {
		const here = encodeURIComponent(window.location.pathname + window.location.search);
		window.location.href = `/profile?forcePasswordChange=1&next=${here}`;
	}
};

/**
 * Wire the SPA's mandatory-mode handler. Called once from the root
 * layout: uses `goto()` for a soft nav so `next=` preserves the
 * intended destination without triggering a full page reload.
 */
export function setPasswordChangeRequiredHandler(fn: () => void): void {
	passwordChangeRequiredHandler = fn;
}

const rawFetch: FetchFn =
	typeof globalThis.fetch === 'function' ? globalThis.fetch.bind(globalThis) : (undefined as never);

/** App-wide fetch — route every API call through this. */
export const apiFetch: FetchFn = createApiFetch({
	rawFetch,
	onSessionExpired: () => sessionExpiredHandler(),
	onPasswordChangeRequired: () => passwordChangeRequiredHandler()
});

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
