/**
 * Auth endpoints. The 401-refresh/dedup behaviour lives in apiFetch; the auth
 * primitives here intentionally bypass it (see client.ts) so a 401 surfaces as
 * a genuine failure to the caller.
 */
import { ApiError, apiFetch } from '$lib/api/client';
import {
	dpopBind,
	getSystemStatus,
	login as sdkLogin,
	logout as sdkLogout,
	oidcExchange,
	oidcLinkStart,
	oidcProviders,
	oidcUnlink,
	register as sdkRegister,
	sendMagicLink as sdkSendMagicLink,
	setupAdmin as sdkSetupAdmin,
	upgradeToInternal as sdkUpgradeToInternal
} from '../generated/openapi/sdk.gen';
import type { SystemInitStatusDto } from '../generated/openapi/types.gen';
import type { AuthResponse, SelfUser } from '$lib/api/types';

/**
 * Map an SDK-thrown error (`throwOnError: true`) into the same `ApiError`
 * shape the hand-written wrappers produced from `apiFetch` + `parseErrorBody`.
 *
 * Hey API's thrown object carries `response` (the raw Fetch Response — status,
 * statusText) alongside `data`, which is the ALREADY-PARSED error body — no
 * second JSON parse is needed, unlike the apiFetch path. See `transport.ts`
 * for the matching contract: both paths share the same `ErrorResponseDto`
 * shape on non-2xx.
 */
function apiErrorFromSdkError(e: unknown, path: string): ApiError {
	const err = e as { response?: Response; data?: unknown };
	const status = err.response?.status ?? 0;
	const statusText = err.response?.statusText ?? '';
	const body = (err.data ?? {}) as {
		error_type?: unknown;
		message?: unknown;
		error?: unknown;
	};
	const errorType = typeof body.error_type === 'string' ? body.error_type : undefined;
	const message =
		(typeof body.message === 'string' ? body.message : undefined) ??
		(typeof body.error === 'string' ? body.error : undefined);
	return new ApiError(status, statusText, path, errorType, message);
}

const JSON_HEADERS = { 'Content-Type': 'application/json' };

/** Probe the session without refreshing or redirecting on an ordinary 401. */
export async function fetchMe(): Promise<SelfUser | null> {
	const res = await apiFetch('/api/auth/me', {
		credentials: 'same-origin',
		retryUnauthorized: false
	});
	if (res.status === 401) return null;
	if (!res.ok) throw new Error(`/api/auth/me failed: ${res.status}`);
	return (await res.json()) as SelfUser;
}

/** Startup refresh through the generated client and shared DPoP nonce handling. */
export async function tryRefresh(): Promise<boolean> {
	try {
		const res = await apiFetch('/api/auth/refresh', {
			method: 'POST',
			credentials: 'same-origin',
			retryUnauthorized: false,
			headers: JSON_HEADERS,
			body: '{}'
		});
		return res.ok;
	} catch {
		return false;
	}
}

/**
 * Compute a DPoP JWK thumbprint for the current browser keypair, if
 * WebCrypto + IndexedDB are available. Returned to callers as an
 * optional string; a `null` return means the browser can't support
 * DPoP, and login proceeds unbound (see `docs/plan/dpop.md` — fail-
 * open per-session, immutable so bound sessions can't be downgraded).
 *
 * Any failure is swallowed to `null` — a DPoP hiccup MUST NOT block
 * authentication. The unbound session is still functional, just not
 * DPoP-protected against info-stealer replay.
 */
async function tryBindingThumbprint(): Promise<string | null> {
	try {
		const { ensureKeypair, computeJkt } = await import('$lib/auth/dpop');
		const kp = await ensureKeypair();
		return await computeJkt(kp.publicKey);
	} catch (err) {
		console.debug('dpop: keypair unavailable, logging in unbound', err);
		return null;
	}
}

/**
 * Post-redirect DPoP bind — for OIDC callback and magic-link
 * redemption pages, where the session was created before the SPA had
 * a chance to send its thumbprint in the login body. Call once,
 * fire-and-forget style: any failure (409 already bound, 400
 * malformed, network error) is swallowed to `false` — the session
 * either was already bound (no harm) or can't be bound now (fail-
 * open per plan).
 *
 * Returns `true` when the bind succeeded, `false` otherwise. Callers
 * typically ignore the return value; they might log it in dev.
 */
export async function bindDpopIfPossible(): Promise<boolean> {
	// Phase 4 — SDK call. `/api/auth/dpop/` is in AUTH_PRIMITIVES, so no
	// refresh dance. Returns false on any failure (swallowed) — a browser
	// that fails to bind is still usable with an unbound session, which
	// is why the OIDC-callback / magic-link-redemption paths fall through
	// to this best-effort call rather than aborting.
	const jkt = await tryBindingThumbprint();
	if (!jkt) return false;
	try {
		await dpopBind({ body: { dpop_jkt: jkt }, throwOnError: true });
		// Bind attached the thumbprint to the SESSION row — but the
		// browser is still carrying the JWT issued at OIDC callback
		// / magic-link redemption BEFORE that bind, so its `cnf.jkt`
		// claim is empty. Force a refresh cycle: the `rotate_session`
		// path preserves the DPoP binding on the new row and mints a
		// fresh JWT whose `cnf.jkt` reflects it. Without this,
		// downstream code that keys off `CurrentUser::dpop_jkt`
		// (admin sessions "is_current" highlight, DPoP verifier's
		// expected_jkt lookup) sees None and treats the caller as
		// unbound.
		await tryRefresh();
		return true;
	} catch (err) {
		console.debug('dpop: bind endpoint call failed', err);
		return false;
	}
}

export async function login(emailOrUsername: string, password: string): Promise<AuthResponse> {
	// Compute DPoP JKT ONCE per login attempt so both branches
	// (OPAQUE and legacy) send the same value. Null = browser can't
	// support DPoP (missing SubtleCrypto / IndexedDB / secure context)
	// or a transient failure; the server accepts absent → session
	// created unbound.
	const dpopJkt = await tryBindingThumbprint();

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
		const auth = await opaqueLogin(emailOrUsername, password, ksf, dpopJkt);

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
	// Phase 4 — SDK call. `/api/auth/login` is in AUTH_PRIMITIVES so a
	// 401 here surfaces as a genuine bad-credentials error, not an
	// expired-token refresh dance. The backend's `error_type` surfaces
	// via `apiErrorFromSdkError` so the login page can branch on
	// `EmailNotVerified` → "resend verification link",
	// `PasswordLoginDisabled` → nudge to magic-link / SSO, etc.
	let auth: AuthResponse;
	try {
		auth = (await sdkLogin({
			body: {
				username: emailOrUsername,
				password,
				...(dpopJkt ? { dpop_jkt: dpopJkt } : {})
			},
			throwOnError: true
		})) as AuthResponse;
	} catch (e) {
		throw apiErrorFromSdkError(e, '/api/auth/login');
	}

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
	// Phase 4 — SDK call via generated client. Target `/api/auth/oidc/providers`
	// is in `transport.ts::AUTH_PRIMITIVES` so the refresh-dance isn't engaged
	// here. The swallow-everything fallback is preserved: the login page polls
	// this on first render and MUST NOT surface an error if the IdP config
	// endpoint is unreachable — it just hides the OIDC-sign-in button.
	try {
		const data = await oidcProviders({ throwOnError: true });
		return data as OidcProviders;
	} catch {
		return { enabled: false };
	}
}

export type AuthStatus = SystemInitStatusDto;

/**
 * System bootstrap probe. When `initialized === false` no admin exists yet and
 * the login page must offer the first-run admin-setup flow. This auth primitive
 * bypasses unauthorized retry in the shared transport. Defaults to "initialized" on any failure so a transient
 * error never strands operators on the setup wizard.
 */
export async function getAuthStatus(): Promise<AuthStatus> {
	// Phase 4 — SDK call. The fallback object is NOT a plausible default;
	// it is specifically the "already-initialized, nothing-to-set-up" state
	// so a boot probe on a transiently-unreachable `/api/auth/status`
	// doesn't push the user into the first-run setup wizard. Preserve it.
	const fallback: AuthStatus = {
		initialized: true,
		admin_count: 1,
		registration_allowed: true,
		faces_enabled: false
	};
	try {
		return await getSystemStatus({ throwOnError: true });
	} catch {
		return fallback;
	}
}

/**
 * First-run admin bootstrap. POSTs to `/api/setup`, which creates the admin
 * user and marks the system initialized. An ordinary 401 surfaces to the caller.
 */
export async function setupAdmin(email: string, password: string): Promise<void> {
	// Phase 4 — SDK call. `/api/setup` is in AUTH_PRIMITIVES, so no refresh
	// dance to opt out of. Error shape stays plain `Error` (not `ApiError`)
	// because the setup wizard renders `.message` directly and never
	// branches on `error_type` — a semantic chosen pre-Phase-4.
	try {
		await sdkSetupAdmin({
			body: { username: 'admin', email, password },
			throwOnError: true
		});
	} catch (e) {
		const err = e as { response?: { status?: number }; data?: unknown };
		const body = (err.data ?? {}) as { error?: unknown; message?: unknown };
		const msg =
			(typeof body.message === 'string' && body.message) ||
			(typeof body.error === 'string' && body.error) ||
			`setup failed: ${err.response?.status ?? 'unknown'}`;
		throw new Error(msg, { cause: e });
	}
}

/**
 * OIDC code-exchange fallback. When the IdP round-trip lands back on the login
 * page with `?oidc_code=`, exchange it for a session (cookies are set
 * server-side). A 401 here is an exchange failure; the transport skips refresh. Returns the user on success, null on
 * any failure so the caller can fall through to the normal login UI.
 */
export async function exchangeOidcCode(code: string): Promise<SelfUser | null> {
	// Phase 4 — SDK call. CSRF + Content-Type are attached by the shared
	// transport (`transport.ts:162`), so the per-call `getCsrfHeaders()`
	// spread + `JSON_HEADERS` disappear. Null-on-any-error preserved: the
	// OIDC redirect-back page needs to handle an invalid/expired code
	// gracefully (user sees "sign-in failed, try again") without a toast
	// cascade.
	try {
		const data = await oidcExchange({ body: { code }, throwOnError: true });
		return (data as { user?: SelfUser | null }).user ?? null;
	} catch {
		return null;
	}
}

/**
 * Register a new user. Since PR 18 both `username` and `password` are optional
 * on the backend: an email-only signup is valid and mints a welcome magic-link.
 * Like login, registration bypasses unauthorized retry in the shared transport.
 */
export async function register(email: string, password?: string, username?: string): Promise<void> {
	// Phase 4 — SDK call. Target `/api/auth/register` is in AUTH_PRIMITIVES.
	// Plain `Error` throw preserved (no `ApiError`) because the signup form
	// renders `.message` directly, same as `setupAdmin`.
	const body: Record<string, unknown> = { email, role: 'user' };
	if (password) body.password = password;
	if (username) body.username = username;
	try {
		await sdkRegister({ body: body as never, throwOnError: true });
	} catch (e) {
		const err = e as { response?: { status?: number }; data?: unknown };
		const errBody = (err.data ?? {}) as { error?: unknown; message?: unknown };
		const msg =
			(typeof errBody.message === 'string' && errBody.message) ||
			(typeof errBody.error === 'string' && errBody.error) ||
			`register failed: ${err.response?.status ?? 'unknown'}`;
		throw new Error(msg, { cause: e });
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
export async function upgradeToInternal(password?: string): Promise<SelfUser> {
	// Phase 4 — SDK call. Caller IS authenticated; a 401 here is a genuine
	// session expiry and the shared transport's refresh+retry is the right
	// response (so no `fetch: apiProbeTransport` override). On non-2xx the
	// SDK throws; `apiErrorFromSdkError` reshapes to the same `ApiError`
	// surface the SPA already surfaces via its error-mapping modal.
	const body: Record<string, unknown> = {};
	if (password) body.password = password;
	try {
		const data = await sdkUpgradeToInternal({
			body: body as never,
			throwOnError: true
		});
		return data as SelfUser;
	} catch (e) {
		throw apiErrorFromSdkError(e, '/api/auth/upgrade-to-internal');
	}
}

export type MagicLinkResult = 'sent' | 'unavailable';

/**
 * Anti-enumeration sign-in by email. Any 2xx resolves to `sent` with a uniform
 * message regardless of whether the email maps to an account. 503 means SMTP
 * isn't configured (`unavailable`) — operators need to see that. Other non-2xx
 * throw so the caller can show a generic error. This auth primitive skips refresh.
 */
export async function sendMagicLink(email: string): Promise<MagicLinkResult> {
	// Phase 4 — SDK call. The 503 → 'unavailable' branch stays: a server
	// without SMTP configured returns 503 by design, and the login page
	// treats that as a soft signal ("magic-link is off, use password").
	// All other non-2xx errors throw so the caller surfaces a toast.
	try {
		await sdkSendMagicLink({ body: { email }, throwOnError: true });
		return 'sent';
	} catch (e) {
		const status = (e as { response?: { status?: number } })?.response?.status;
		if (status === 503) return 'unavailable';
		throw new Error(`magic-link failed: ${status ?? 'unknown'}`, { cause: e });
	}
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

/**
 * Start the self-service OIDC linking flow. Returns the authorize URL
 * the caller should full-page navigate to (`window.location.assign`).
 * The IdP round-trip lands on `/api/auth/oidc/callback` which
 * dispatches to the link branch and redirects to
 * `/profile?linked=1` (success) or `/profile?link_error=<reason>`
 * (safety-check refusal). See docs/plan/oidc-account-linking.md.
 */
export async function startOidcLink(): Promise<string> {
	// Phase 4 — SDK call. Two distinct failure modes stay separate:
	//   * HTTP non-2xx → ApiError (SPA renders the IdP error toast)
	//   * HTTP 200 but no `authorize_url` → generic Error (programming
	//     bug on the server; shouldn't be surfaced as "the IdP said no")
	// Split as two try blocks so the second malformed-response throw
	// isn't caught by apiErrorFromSdkError and remapped.
	let body: unknown;
	try {
		body = await oidcLinkStart({ throwOnError: true });
	} catch (e) {
		throw apiErrorFromSdkError(e, '/api/auth/oidc/link/start');
	}
	const authorizeUrl = (body as { authorize_url?: unknown }).authorize_url;
	if (typeof authorizeUrl !== 'string' || authorizeUrl.length === 0) {
		throw new Error('malformed link/start response: missing authorize_url');
	}
	return authorizeUrl;
}

/**
 * Detach the currently-authenticated user's OIDC identity. Refuses
 * (403 with `error_type: "AccessDenied"`) when the user has no other
 * credential (password / OPAQUE) and would be locked out. Callers
 * should offer the "set a password first" affordance in that case.
 */
export async function unlinkOidc(): Promise<void> {
	// Phase 4 — SDK call. Pattern-identical to startOidcLink minus the
	// authorize_url validation (there is no success-shape to check).
	try {
		await oidcUnlink({ throwOnError: true });
	} catch (e) {
		throw apiErrorFromSdkError(e, '/api/auth/oidc/unlink');
	}
}

export async function logout(): Promise<LogoutResult> {
	// The session-teardown gate (`setLogoutInProgress(true)`) is flipped
	// by the CALLER (`AppShell::onLogout`) BEFORE this function runs, and
	// left ON across the goto to /login. See `client.ts::logoutInProgress`
	// for what the gate suppresses (short-circuits ambient fetches with
	// AbortError + blocks the session-expired divert).
	//
	// Three phases to preserve under the SDK swap:
	//   1. HTTP call (may fail — server down, 500, aborted mid-logout).
	//   2. DPoP browser-state teardown — UNCONDITIONAL, runs even if
	//      step 1 failed, so a user's logout intent never leaves a
	//      stale keypair/nonce around.
	//   3. post_logout_url extraction — only on success.
	let body: { post_logout_url?: unknown } | undefined;
	try {
		// Empty body: the server-side handler takes `axum::body::Bytes`,
		// not a typed Json extractor, and its own docstring states
		// "Optional — omit when using the HttpOnly cookie". The SDK types
		// require `refresh_token` only because the utoipa annotation on
		// `logout_handler` declares `request_body = inline(RefreshTokenDto)`
		// without marking it optional — a backend-side annotation fix is
		// tracked separately. Cast through `as never` so TypeScript accepts
		// the correct empty-body call without introducing a bogus token.
		body = (await sdkLogout({ body: {} as never, throwOnError: true })) as {
			post_logout_url?: unknown;
		};
	} catch {
		// Server rejected logout (500, network down, aborted). The DPoP
		// teardown below still fires — leaving a stale keypair would
		// confuse the next login's bind step. `{}` returned at the end.
	}

	// Wipe DPoP browser state so the next login mints a fresh keypair —
	// no correlation across the logout boundary is desirable (a new
	// session is a new identity from the per-request-signature
	// standpoint). Runs UNCONDITIONALLY of the server response: the
	// user's intent was to log out, period.
	try {
		const { clearKeypair } = await import('$lib/auth/dpop');
		const { clearNonce } = await import('$lib/auth/dpop-proof');
		const { broadcastSessionCleared } = await import('$lib/auth/session-broadcast');
		await clearKeypair();
		clearNonce();
		// Notify every OTHER tab of this origin that the session is
		// gone — Gate 8 cross-tab UX. Tabs that were sitting idle
		// don't have to wait for their next 401 to notice.
		broadcastSessionCleared();
	} catch (err) {
		console.debug('dpop: cleanup failed during logout', err);
	}

	if (!body) return {};
	return typeof body.post_logout_url === 'string' ? { postLogoutUrl: body.post_logout_url } : {};
}
