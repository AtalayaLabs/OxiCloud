/**
 * Auth endpoints. The 401-refresh/dedup behaviour lives in apiFetch; the auth
 * primitives here intentionally bypass it (see client.ts) so a 401 surfaces as
 * a genuine failure to the caller.
 */
import { ApiError, apiFetch } from '$lib/api/client';
import { getCsrfHeaders } from '$lib/api/csrf';
import type { AuthResponse, User } from '$lib/api/types';

/**
 * Best-effort parse of the backend `ErrorResponse` shape
 * (`{ status, error, message, error_type }`). Returns whatever it could
 * extract; never throws — a malformed body just yields undefineds.
 */
async function parseErrorBody(res: Response): Promise<{ errorType?: string; message?: string }> {
	try {
		const body = (await res.clone().json()) as {
			error_type?: unknown;
			message?: unknown;
			error?: unknown;
		};
		const errorType = typeof body.error_type === 'string' ? body.error_type : undefined;
		const rawMessage =
			(typeof body.message === 'string' ? body.message : undefined) ??
			(typeof body.error === 'string' ? body.error : undefined);
		return { errorType, message: rawMessage };
	} catch {
		return {};
	}
}

const JSON_HEADERS = { 'Content-Type': 'application/json' };

/**
 * Probe the current session. Uses the raw `fetch` (NOT apiFetch) on purpose:
 * a 401 here just means "not logged in" and must not trigger the global
 * refresh-and-redirect (which would bounce the app in a refresh loop on the
 * unauthenticated initial load). Returns null when unauthenticated.
 */
export async function fetchMe(): Promise<User | null> {
	const res = await fetch('/api/auth/me', { credentials: 'same-origin' });
	if (res.status === 401) return null;
	if (!res.ok) throw new Error(`/api/auth/me failed: ${res.status}`);
	return (await res.json()) as User;
}

/**
 * Attempt a single token refresh (raw fetch, no interceptor). Returns whether
 * it succeeded. Used by the startup probe; mid-session refresh is handled
 * transparently by apiFetch for all other endpoints.
 */
export async function tryRefresh(): Promise<boolean> {
	try {
		const res = await fetch('/api/auth/refresh', {
			method: 'POST',
			credentials: 'same-origin',
			headers: { ...JSON_HEADERS, ...getCsrfHeaders() },
			body: '{}'
		});
		return res.ok;
	} catch {
		return false;
	}
}

export async function login(emailOrUsername: string, password: string): Promise<AuthResponse> {
	// ── OPAQUE lookup (Phase 3) ────────────────────────────────────────
	// Ask the server whether this identifier already has an OPAQUE
	// envelope on file. If yes → use OPAQUE login (KE1/KE3). If no →
	// fall back to legacy `POST /api/auth/login`, which then silently
	// mints the envelope via the Phase 2 hook.
	//
	// Both paths return the SAME `AuthResponse` shape, so downstream
	// callers don't need to know which branch fired. Dynamic import
	// keeps the ~200 KiB `@serenity-kit/opaque` WASM bundle out of
	// pre-login bundles — the module loads only when a login is
	// actually attempted.
	//
	// `checkOpaqueAvailable` and `opaqueLogin` both fall back
	// gracefully: any wire failure inside the OPAQUE branch (503,
	// timeout, malformed response) either short-circuits to `false`
	// (lookup) or throws with an `InvalidCredentials` shape (login).
	// The lookup fallback lands here as `false` → legacy branch
	// takes over; a mid-login OPAQUE failure surfaces as a login
	// error to the user, same shape as a legacy failure — no silent
	// legacy fallback there because it would mask a wrong passphrase
	// as a network hiccup.
	const { checkOpaqueAvailable, opaqueLogin, syncOpaqueEnvelope } =
		await import('$lib/api/endpoints/opaque');
	const lookup = await checkOpaqueAvailable(emailOrUsername);
	if (lookup.has) {
		// Prefer the envelope's OWN KSF (returned by lookup) over the
		// server's current /params values: after a KSF config change,
		// existing envelopes need their historical KSF for the OPRF
		// to derive the right value; using current /params would fail
		// the AKE integrity check and return `InvalidCredentials`.
		// Fallback to /params only when the envelope predates
		// per-envelope KSF storage (`ksf === null`), which preserves
		// the pre-migration behaviour.
		const ksf = lookup.ksf ?? (await opaqueKsfForClient());
		const auth = await opaqueLogin(emailOrUsername, password, ksf);

		// Phase C: silent KSF rotation. If this envelope's KSF drifted
		// from what the server currently publishes (operator retuned
		// OXICLOUD_AUTH_OPAQUE_KSF_*), re-register the envelope under
		// the current params so the NEXT login benefits from the new
		// values (faster / stronger / whatever the tuning direction).
		// Envelopes that predate per-envelope storage (`lookup.ksf ===
		// null`) always trigger rotation — that's how they migrate
		// into the new storage schema organically.
		//
		// `syncOpaqueEnvelope` is the same helper the Phase 2 hook
		// uses after a legacy login; it swallows errors, so a
		// rotation failure is non-fatal (this login already succeeded)
		// and the NEXT login retries the same check. Fires only when
		// there's a real difference — no wasted crypto on the common
		// same-params case.
		const current = await opaqueKsfForClient();
		const needsRotation =
			!lookup.ksf ||
			lookup.ksf.memoryKib !== current.memoryKib ||
			lookup.ksf.iterations !== current.iterations ||
			lookup.ksf.parallelism !== current.parallelism;
		if (needsRotation) await syncOpaqueEnvelope(password);

		return auth;
	}

	// ── Legacy login (fallback for users without an envelope yet) ─────
	const res = await apiFetch('/api/auth/login', {
		method: 'POST',
		credentials: 'same-origin',
		headers: { ...JSON_HEADERS, ...getCsrfHeaders() },
		body: JSON.stringify({ username: emailOrUsername, password })
	});
	if (!res.ok) {
		// Surface the backend `error_type` so the login page can offer
		// specific UX: `EmailNotVerified` → "resend verification link",
		// `PasswordLoginDisabled` → nudge toward magic-link / SSO, etc.
		const { errorType, message } = await parseErrorBody(res);
		throw new ApiError(res.status, res.statusText, '/api/auth/login', errorType, message);
	}
	const auth = (await res.json()) as AuthResponse;

	// ── OPAQUE silent migration (Phase 2) ──────────────────────────────
	// After a successful legacy password login, transparently mint an
	// OPAQUE envelope for the same passphrase. The NEXT login for this
	// account will take the OPAQUE branch above.
	//
	// The freshly-issued session cookie is already active in the
	// browser at this point (Set-Cookie from the POST response), so
	// the session-authenticated register endpoints are reachable
	// straight away. `syncOpaqueEnvelope` no-ops when OPAQUE is
	// disabled server-side (mode=off) and swallows any error — a
	// failure here just leaves the envelope stale, and the NEXT
	// legacy login retries the same hook.
	await syncOpaqueEnvelope(password);

	return auth;
}

/**
 * Fetch the server's OPAQUE KSF config to pass to `opaqueLogin`.
 * Extracted so `login()` reads more linearly and the module keeps
 * one `fetchOpaqueParams` call site regardless of which branch
 * (lookup / login / silent-migration) reaches it first.
 */
async function opaqueKsfForClient(): Promise<{
	memoryKib: number;
	iterations: number;
	parallelism: number;
}> {
	const { fetchOpaqueParams } = await import('$lib/api/endpoints/opaque');
	const params = await fetchOpaqueParams();
	return params.ksf;
}

export interface OidcProviders {
	enabled: boolean;
	provider_name?: string;
	password_login_enabled?: boolean;
	/**
	 * True when the server accepts magic-link login requests. The backend
	 * composes three factors: SMTP wired, `OXICLOUD_AUTH_METHODS` allowlist
	 * includes `magic_link`, and OIDC is NOT enabled at the deployment
	 * (OIDC-enabled deployments must not offer magic-link — it would bypass
	 * any 2FA / step-up the IdP enforces).
	 */
	magic_link_login_enabled?: boolean;
	/**
	 * True when `OXICLOUD_REQUIRE_VERIFIED_EMAIL` is set. The login page
	 * uses this to explain the `EmailNotVerified` login response and
	 * surface a "resend verification link" affordance.
	 */
	require_verified_email?: boolean;
	authorize_endpoint?: string;
	/**
	 * Server-computed: true when the `auto_redirect_if_standalone_oidc`
	 * policy is on AND OIDC is the only working method (see
	 * `AuthApplicationService::auto_redirect_to_oidc`). When true the root
	 * layout guard `window.location.replace`s to `authorize_endpoint`
	 * instead of routing through `/login` — the server-side `/login`
	 * middleware only fires on full HTTP loads, so SPA client-side
	 * navigation to `/login` (root guard, dev via Vite) would otherwise
	 * stall on the login page. Because the flag is gated by the admin's
	 * policy on the SERVER, using it on the client does NOT override the
	 * policy toggle — we're just enacting the same decision on paths the
	 * middleware can't reach.
	 */
	auto_redirect_to_oidc?: boolean;
}

/** Public OIDC provider info for the login page. */
export async function getOidcProviders(): Promise<OidcProviders> {
	try {
		const res = await fetch('/api/auth/oidc/providers');
		if (!res.ok) return { enabled: false };
		return (await res.json()) as OidcProviders;
	} catch {
		return { enabled: false };
	}
}

export interface AuthStatus {
	initialized: boolean;
	admin_count: number;
	registration_allowed: boolean;
}

/**
 * System bootstrap probe. When `initialized === false` no admin exists yet and
 * the login page must offer the first-run admin-setup flow. Raw `fetch` (NOT
 * apiFetch): this is unauthenticated and a non-2xx must not bounce through the
 * refresh interceptor. Defaults to "initialized" on any failure so a transient
 * error never strands operators on the setup wizard.
 */
export async function getAuthStatus(): Promise<AuthStatus> {
	try {
		const res = await fetch('/api/auth/status', { credentials: 'same-origin' });
		if (!res.ok) return { initialized: true, admin_count: 1, registration_allowed: true };
		return (await res.json()) as AuthStatus;
	} catch {
		return { initialized: true, admin_count: 1, registration_allowed: true };
	}
}

/**
 * First-run admin bootstrap. POSTs to `/api/setup`, which creates the admin
 * user and marks the system initialized. Raw `fetch` (NOT apiFetch) so a 401
 * surfaces as a genuine failure instead of triggering the refresh-and-redirect.
 */
export async function setupAdmin(email: string, password: string): Promise<void> {
	const res = await fetch('/api/setup', {
		method: 'POST',
		credentials: 'same-origin',
		headers: { ...JSON_HEADERS, ...getCsrfHeaders() },
		body: JSON.stringify({ username: 'admin', email, password })
	});
	if (!res.ok) {
		const e = (await res.json().catch(() => ({}))) as { error?: string; message?: string };
		throw new Error(e.error || e.message || `setup failed: ${res.status}`);
	}
}

/**
 * OIDC code-exchange fallback. When the IdP round-trip lands back on the login
 * page with `?oidc_code=`, exchange it for a session (cookies are set
 * server-side). Raw `fetch` (NOT apiFetch) — a 401 here is a genuine exchange
 * failure, not an expired access token. Returns the user on success, null on
 * any failure so the caller can fall through to the normal login UI.
 */
export async function exchangeOidcCode(code: string): Promise<User | null> {
	try {
		const res = await fetch('/api/auth/oidc/exchange', {
			method: 'POST',
			credentials: 'same-origin',
			headers: { ...JSON_HEADERS, ...getCsrfHeaders() },
			body: JSON.stringify({ code })
		});
		if (!res.ok) return null;
		const data = (await res.json()) as { user?: User };
		return data.user ?? null;
	} catch {
		return null;
	}
}

/**
 * Register a new user. Since PR 18 both `username` and `password` are optional
 * on the backend: an email-only signup is valid and mints a welcome magic-link.
 * Raw `fetch` (NOT apiFetch) so a 401/validation failure surfaces to the caller
 * instead of tripping the global refresh-and-redirect interceptor — mirrors
 * the login primitive.
 */
export async function register(email: string, password?: string, username?: string): Promise<void> {
	const body: Record<string, unknown> = { email, role: 'user' };
	if (password) body.password = password;
	if (username) body.username = username;
	const res = await fetch('/api/auth/register', {
		method: 'POST',
		credentials: 'same-origin',
		headers: { ...JSON_HEADERS, ...getCsrfHeaders() },
		body: JSON.stringify(body)
	});
	if (!res.ok) {
		const e = (await res.json().catch(() => ({}))) as { error?: string; message?: string };
		throw new Error(e.error || e.message || `register failed: ${res.status}`);
	}
}

/**
 * Convert the authenticated external user into a full internal account.
 * Server flips `is_external` to false, provisions a personal drive via
 * the lifecycle hook, and returns the updated `User`.
 *
 * Password is optional — see backend `UpgradeToInternalDto`:
 *   * If the deployment offers magic-link login, blank password is
 *     accepted (user remains magic-link-only after upgrade).
 *   * Otherwise a password is required — the backend refuses with 400
 *     `error_type = "PasswordRequired"` and the SPA surfaces the
 *     server message.
 *
 * Uses `apiFetch` (unlike register/login) because the caller IS
 * authenticated; a 401 here IS a genuine "session expired" and the
 * refresh interceptor is the right response.
 */
export async function upgradeToInternal(password?: string): Promise<User> {
	const body: Record<string, unknown> = {};
	if (password) body.password = password;
	const res = await apiFetch('/api/auth/upgrade-to-internal', {
		method: 'POST',
		credentials: 'same-origin',
		headers: { ...JSON_HEADERS, ...getCsrfHeaders() },
		body: JSON.stringify(body)
	});
	if (!res.ok) {
		const { errorType, message } = await parseErrorBody(res);
		throw new ApiError(
			res.status,
			res.statusText,
			'/api/auth/upgrade-to-internal',
			errorType,
			message
		);
	}
	return (await res.json()) as User;
}

export type MagicLinkResult = 'sent' | 'unavailable';

/**
 * Anti-enumeration sign-in by email. Any 2xx resolves to `sent` with a uniform
 * message regardless of whether the email maps to an account. 503 means SMTP
 * isn't configured (`unavailable`) — operators need to see that. Other non-2xx
 * throw so the caller can show a generic error. Raw `fetch` (NOT apiFetch):
 * unauthenticated, must not enter the refresh interceptor.
 */
export async function sendMagicLink(email: string): Promise<MagicLinkResult> {
	const res = await fetch('/api/auth/magic-link/send', {
		method: 'POST',
		credentials: 'same-origin',
		headers: { ...JSON_HEADERS, ...getCsrfHeaders() },
		body: JSON.stringify({ email })
	});
	if (res.status === 503) return 'unavailable';
	if (!res.ok) throw new Error(`magic-link failed: ${res.status}`);
	return 'sent';
}

export interface LogoutResult {
	/**
	 * RP-initiated OIDC logout URL, present only when the session was minted
	 * through OIDC AND the IdP advertises an `end_session_endpoint`. The
	 * caller MUST navigate there via `window.location` (not `goto()`) so the
	 * browser leaves the SPA and hits the IdP; the IdP kills its SSO cookie
	 * and redirects back to `/login`. Without this hop the IdP session stays
	 * alive and the next `/login` visit would silently re-authenticate.
	 */
	postLogoutUrl?: string;
}

export async function logout(): Promise<LogoutResult> {
	const res = await apiFetch('/api/auth/logout', {
		method: 'POST',
		credentials: 'same-origin',
		headers: { ...JSON_HEADERS, ...getCsrfHeaders() },
		body: '{}'
	});
	if (!res.ok) return {};
	try {
		const body = (await res.json()) as { post_logout_url?: unknown };
		return typeof body?.post_logout_url === 'string' ? { postLogoutUrl: body.post_logout_url } : {};
	} catch {
		return {};
	}
}
