/**
 * OPAQUE aPAKE (RFC 9807) client wrapper. Phase 0 substrate — endpoints
 * are wired in Phase 1; this module ships the primitives so the login form
 * can adopt them in a single small change once the handlers land.
 *
 * The passphrase never leaves this file. All `password` inputs flow into
 * the WASM handshake exclusively; nothing serialises them to the network
 * or logs them. Callers are responsible for clearing their own copy from
 * component state as soon as the returned promise settles.
 *
 * ## Wire shape
 *
 * Two HTTP round-trips per operation, matching what the backend expects:
 *
 * Registration (only reachable while already authenticated — Phase 1 flow):
 * ```
 *   POST /api/auth/opaque/register/start   { registrationRequest }
 *     → { registrationResponse }
 *   POST /api/auth/opaque/register/finish  { registrationRecord, ciphersuiteVersion }
 *     → 204 No Content
 * ```
 *
 * Login (unauthenticated):
 * ```
 *   POST /api/auth/opaque/login/ke1  { userIdentifier, startLoginRequest }
 *     → { exchangeId, loginResponse }
 *   POST /api/auth/opaque/login/ke3  { exchangeId, finishLoginRequest }
 *     → { user, access_token, refresh_token, ... }  // same AuthResponse shape
 * ```
 *
 * The KSF params are frozen at first-time registration into
 * `auth.users.opaque_ciphersuite_version` server-side; the client must
 * always send matching params on login — that's what [`OpaqueKsfConfig`]
 * carries. In Phase 1 the config is fetched from `/api/health` (or a
 * dedicated `/api/auth/opaque/params` endpoint) at page load and cached.
 */
import { client, ready } from '@serenity-kit/opaque';
import { ApiError, apiFetch } from '$lib/api/client';
import { getCsrfHeaders } from '$lib/api/csrf';
import type { AuthResponse } from '$lib/api/types';

/**
 * Client-side Argon2id parameters — must match the server's config
 * ([`OpaqueConfig::ksf_*`] in Rust). Fetched from the server at page load
 * so a bump in either direction stays in lock-step; hardcoded fallbacks
 * mirror the Rust defaults for offline dev.
 */
export interface OpaqueKsfConfig {
	/** Memory cost in KiB. Server default: 262144 (256 MiB). */
	memoryKib: number;
	/** Iterations. Server default: 3. */
	iterations: number;
	/** Parallelism / lanes. Server default: 4. */
	parallelism: number;
}

/**
 * Wire shape of `GET /api/auth/opaque/params`. `enabled: false` means
 * the server's OPAQUE substrate is off — the SPA should short-circuit
 * all `syncOpaqueEnvelope` / `opaqueLogin` calls and stay on the legacy
 * password path. Numeric fields carry safe defaults regardless so a
 * client that ignored the flag wouldn't nil-deref.
 */
export interface OpaqueServerParams {
	enabled: boolean;
	ciphersuiteVersion: number;
	ksf: OpaqueKsfConfig;
}

/**
 * In-memory cache of the params response. Fetched once per page load
 * (per SPA runtime), invalidated only by a hard refresh — this matches
 * the operator contract that changing OPAQUE env vars requires a server
 * restart, and the SPA reload that follows picks up the new values.
 *
 * Unresolved `null` = we haven't tried yet. A settled promise (or a
 * thrown one) is what subsequent callers await, so concurrent first
 * touches collapse into ONE `GET /params` round-trip.
 */
let opaqueParamsInflight: Promise<OpaqueServerParams> | null = null;

/**
 * Test-only: drop the params cache so the next call re-fetches.
 * Exposed as `__resetOpaqueParamsCache` to signal "internal — call
 * from tests only." Runtime code MUST NOT use this; the operator
 * contract is that params change requires a page reload.
 */
export function __resetOpaqueParamsCache(): void {
	opaqueParamsInflight = null;
}

/** Fetch (and cache) the server's OPAQUE params. See [`OpaqueServerParams`]. */
export function fetchOpaqueParams(): Promise<OpaqueServerParams> {
	if (opaqueParamsInflight) return opaqueParamsInflight;
	opaqueParamsInflight = (async () => {
		const res = await apiFetch('/api/auth/opaque/params', {
			credentials: 'same-origin'
		});
		if (!res.ok) {
			// Treat a broken /params as "OPAQUE not available" rather
			// than propagating an error — the SPA should degrade to
			// legacy password auth, not crash. Cache the negative
			// result so we don't hammer a broken endpoint.
			return {
				enabled: false,
				ciphersuiteVersion: 0,
				ksf: { memoryKib: 0, iterations: 0, parallelism: 0 }
			};
		}
		return (await res.json()) as OpaqueServerParams;
	})();
	return opaqueParamsInflight;
}

/**
 * Ask the server whether `userIdentifier` resolves to a user with an
 * OPAQUE envelope on file. The SPA login form calls this before
 * submit to decide between OPAQUE (KE1/KE3) and legacy password
 * login. The two paths converge to the same session shape, so the
 * user never notices the branch.
 *
 * Returns `false` on any error — network hiccup, disabled substrate,
 * malformed response — so callers fall back to legacy login rather
 * than blocking on the OPAQUE probe. The Phase 2 silent-migration
 * hook will still run after the legacy login and mint the envelope,
 * so this transient "false" just delays adoption by one login cycle.
 *
 * The server-side shape is anti-enum: same `hasOpaque: false` for
 * both "unknown user" and "user without envelope." Callers must
 * never assume `hasOpaque: false` implies the user exists.
 */
export async function checkOpaqueAvailable(userIdentifier: string): Promise<boolean> {
	// Cheap short-circuit: if the substrate isn't enabled server-side,
	// the endpoint would return 503 anyway. `syncOpaqueEnvelope`
	// primed the cache after any prior login in this session; this
	// call reuses it.
	const params = await fetchOpaqueParams();
	if (!params.enabled) return false;
	try {
		const res = await apiFetch('/api/auth/opaque/login/lookup', {
			method: 'POST',
			credentials: 'same-origin',
			headers: { ...JSON_HEADERS, ...getCsrfHeaders() },
			body: JSON.stringify({ userIdentifier })
		});
		if (!res.ok) return false;
		const body = (await res.json()) as { hasOpaque?: boolean };
		return body.hasOpaque === true;
	} catch {
		return false;
	}
}

/**
 * Silent OPAQUE registration after a passphrase-touching action
 * (signup completion, change-password, silent migration on legacy
 * login). Fetches params on demand, runs the two-round OPAQUE
 * register handshake with `password`, and swallows errors — a
 * failure here leaves the envelope stale, but a subsequent legacy
 * login will retry via the silent-migration hook. Callers should
 * clear their local copy of `password` from memory as soon as this
 * settles (either await or catch — the promise resolves in both
 * paths so `.finally(() => clearPw())` is the idiomatic wire).
 *
 * Callers MUST hold a valid session — the register endpoints are
 * session-authenticated (they bind the envelope to the current
 * user_id). Post-signup / post-change-password sessions qualify.
 */
export async function syncOpaqueEnvelope(password: string): Promise<void> {
	const params = await fetchOpaqueParams();
	if (!params.enabled) return; // Substrate off — no-op.
	try {
		await opaqueRegister(password, params.ksf, params.ciphersuiteVersion);
	} catch (err) {
		// Non-fatal: legacy login still works, silent migration will
		// retry on next legacy /api/auth/login. Log to console so a
		// developer poking at DevTools sees the failure but the user
		// doesn't get a confusing toast for something they didn't ask
		// for. Reset the cache so the next call re-probes /params —
		// the failure might have been a transient outage.
		console.warn('OPAQUE envelope sync failed (silent migration will retry):', err);
	}
}

const JSON_HEADERS = { 'Content-Type': 'application/json' };

/** Build the shape `@serenity-kit/opaque` expects for its `keyStretching` opt. */
function ksfOption(cfg: OpaqueKsfConfig) {
	return {
		'argon2id-custom': {
			iterations: cfg.iterations,
			memory: cfg.memoryKib,
			parallelism: cfg.parallelism
		}
	} as const;
}

/**
 * Best-effort parse of the backend `ErrorResponse` shape into a stable
 * pair. Never throws; mirrors the same shape `auth.ts` uses.
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

/**
 * Register an OPAQUE envelope for the currently-authenticated caller.
 * Two HTTP round-trips; the WASM handshake runs entirely client-side.
 *
 * Called by the migration hook after any successful legacy-password
 * login (Phase 2) and by the change-password / password-reset flows
 * (Phase 1+) so the envelope stays in lock-step with the passphrase.
 *
 * Throws [`ApiError`] with the parsed `error_type` on server rejection;
 * throws a plain [`Error`] on protocol failures.
 */
export async function opaqueRegister(
	password: string,
	ksf: OpaqueKsfConfig,
	ciphersuiteVersion: number
): Promise<void> {
	await ready;

	// ── Round 1 ─────────────────────────────────────────────────────────
	const { clientRegistrationState, registrationRequest } = client.startRegistration({ password });
	const startRes = await apiFetch('/api/auth/opaque/register/start', {
		method: 'POST',
		credentials: 'same-origin',
		headers: { ...JSON_HEADERS, ...getCsrfHeaders() },
		body: JSON.stringify({ registrationRequest })
	});
	if (!startRes.ok) {
		const { errorType, message } = await parseErrorBody(startRes);
		throw new ApiError(
			startRes.status,
			startRes.statusText,
			'/api/auth/opaque/register/start',
			errorType,
			message ?? 'opaque register start failed'
		);
	}
	const { registrationResponse } = (await startRes.json()) as { registrationResponse: string };

	// ── Round 2 ─────────────────────────────────────────────────────────
	const { registrationRecord } = client.finishRegistration({
		password,
		registrationResponse,
		clientRegistrationState,
		keyStretching: ksfOption(ksf)
	});
	const finishRes = await apiFetch('/api/auth/opaque/register/finish', {
		method: 'POST',
		credentials: 'same-origin',
		headers: { ...JSON_HEADERS, ...getCsrfHeaders() },
		body: JSON.stringify({ registrationRecord, ciphersuiteVersion })
	});
	if (!finishRes.ok) {
		const { errorType, message } = await parseErrorBody(finishRes);
		throw new ApiError(
			finishRes.status,
			finishRes.statusText,
			'/api/auth/opaque/register/finish',
			errorType,
			message ?? 'opaque register finish failed'
		);
	}
}

/**
 * Log in via OPAQUE. Two HTTP round-trips; server issues the session
 * cookies + refresh token on successful `ke3`.
 *
 * Returns the [`AuthResponse`] identical in shape to the legacy login
 * path, so callers (`LoginForm.svelte`) can flow both branches through a
 * single downstream handler.
 *
 * Throws [`ApiError`] with the parsed `error_type` on server rejection
 * — including the anti-enumeration case where the account has no
 * envelope (server returns the same `InvalidCredentials` code as a
 * wrong-passphrase failure to avoid leaking which one it was).
 */
export async function opaqueLogin(
	userIdentifier: string,
	password: string,
	ksf: OpaqueKsfConfig
): Promise<AuthResponse> {
	await ready;

	// ── KE1 ─────────────────────────────────────────────────────────────
	const { clientLoginState, startLoginRequest } = client.startLogin({ password });
	const ke1Res = await apiFetch('/api/auth/opaque/login/ke1', {
		method: 'POST',
		credentials: 'same-origin',
		headers: { ...JSON_HEADERS, ...getCsrfHeaders() },
		body: JSON.stringify({ userIdentifier, startLoginRequest })
	});
	if (!ke1Res.ok) {
		const { errorType, message } = await parseErrorBody(ke1Res);
		throw new ApiError(
			ke1Res.status,
			ke1Res.statusText,
			'/api/auth/opaque/login/ke1',
			errorType,
			message ?? 'opaque login ke1 failed'
		);
	}
	const { exchangeId, loginResponse } = (await ke1Res.json()) as {
		exchangeId: string;
		loginResponse: string;
	};

	// ── KE3 ─────────────────────────────────────────────────────────────
	// `finishLogin` returns undefined when the server response is
	// well-formed but the passphrase is wrong — surface that as a
	// terminal client-side failure (never reaches the server) rather
	// than sending garbage to KE3.
	const finished = client.finishLogin({
		clientLoginState,
		loginResponse,
		password,
		keyStretching: ksfOption(ksf)
	});
	if (!finished) {
		throw new ApiError(
			401,
			'Unauthorized',
			'/api/auth/opaque/login/ke1',
			'InvalidCredentials',
			'invalid credentials'
		);
	}
	const { finishLoginRequest } = finished;

	const ke3Res = await apiFetch('/api/auth/opaque/login/ke3', {
		method: 'POST',
		credentials: 'same-origin',
		headers: { ...JSON_HEADERS, ...getCsrfHeaders() },
		body: JSON.stringify({ exchangeId, finishLoginRequest })
	});
	if (!ke3Res.ok) {
		const { errorType, message } = await parseErrorBody(ke3Res);
		throw new ApiError(
			ke3Res.status,
			ke3Res.statusText,
			'/api/auth/opaque/login/ke3',
			errorType,
			message ?? 'opaque login ke3 failed'
		);
	}
	return (await ke3Res.json()) as AuthResponse;
}
