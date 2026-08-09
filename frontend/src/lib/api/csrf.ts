/**
 * CSRF double-submit cookie utility — ported from static/js/core/csrf.js.
 *
 * Reads the `oxicloud_csrf` cookie (NOT HttpOnly) and exposes its value as the
 * `X-CSRF-Token` header. The server's `csrf_middleware` validates that the
 * header matches the cookie for every mutating (POST/PUT/DELETE/PATCH) request
 * authenticated via the HttpOnly session cookie.
 */

export function getCsrfToken(): string {
	const match = document.cookie.split('; ').find((row) => row.startsWith('oxicloud_csrf='));
	return match ? (match.split('=')[1] ?? '') : '';
}

/** Headers to merge into a mutating request; empty when no token is present. */
export function getCsrfHeaders(): Record<string, string> {
	const token = getCsrfToken();
	return token ? { 'X-CSRF-Token': token } : {};
}

/**
 * Best-effort "does the browser think it has a session?" hint. The server
 * sets `oxicloud_csrf` (non-HttpOnly, JS-visible) alongside the session
 * cookies on every login and clears it on logout, so its ABSENCE is a
 * reliable proof of "no session" — cheaper than a network probe that
 * would 401 → refresh 401 → 401 on first landing with no cookies.
 *
 * Its PRESENCE is only a hint: the session cookies (HttpOnly) may have
 * been revoked server-side while the CSRF cookie lingers. Callers that
 * see `true` must still probe /api/auth/me — this helper just lets a
 * fresh no-cookie bootstrap skip the doomed 2× /me + /refresh burst.
 */
export function hasSessionHint(): boolean {
	if (typeof document === 'undefined') return false;
	return document.cookie.split('; ').some((row) => row.startsWith('oxicloud_csrf='));
}
