import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
// Partial mock: stub `apiFetch` + `apiJson` so test fixtures can drive
// the handful of legacy call sites still using them, while keeping
// everything else (`apiProbeTransport`, `ApiError`, `withBase`, …) as
// the real exports so Phase-4-migrated wrappers that need them (e.g.
// `fetchMe` passes `apiProbeTransport` as the SDK's `fetch` option)
// see the real values, not undefined.
vi.mock('$lib/api/client', async (importOriginal) => {
	const actual = await importOriginal<typeof import('$lib/api/client')>();
	return {
		...actual,
		apiFetch: vi.fn(),
		apiJson: vi.fn()
	};
});
vi.mock('$lib/api/csrf', () => ({ getCsrfHeaders: () => ({}) }));
// Several probes (`fetchMe`, `tryRefresh`) run through the DPoP shim.
// Default `proof = null` matches jsdom's missing WebCrypto keys — so
// pre-existing suite behaviour is unchanged. Individual tests flip the
// state to a non-null value to exercise the DPoP-attached path.
const dpopState = vi.hoisted(() => ({ proof: null as string | null }));
vi.mock('$lib/auth/dpop-proof', async () => {
	const actual =
		await vi.importActual<typeof import('$lib/auth/dpop-proof')>('$lib/auth/dpop-proof');
	return {
		...actual,
		buildDpopProof: vi.fn(async () => dpopState.proof)
	};
});
// Mock the OPAQUE WASM client so `login()`'s Phase 2 silent-migration
// hook and Phase 3 lookup-then-login flip can exercise the wire path
// (params + lookup + register or ke1/ke3 handshake) without touching
// real WASM in jsdom.
vi.mock('@serenity-kit/opaque', () => ({
	ready: Promise.resolve(),
	client: {
		startRegistration: vi.fn(() => ({
			clientRegistrationState: 'STATE-R',
			registrationRequest: 'REQ-R'
		})),
		finishRegistration: vi.fn(() => ({
			registrationRecord: 'RECORD-R',
			exportKey: 'EK',
			serverStaticPublicKey: 'SPK'
		})),
		startLogin: vi.fn(() => ({
			clientLoginState: 'STATE-L',
			startLoginRequest: 'REQ-L'
		})),
		finishLogin: vi.fn(() => ({
			finishLoginRequest: 'REQ-F',
			sessionKey: 'SK',
			exportKey: 'EK',
			serverStaticPublicKey: 'SPK'
		}))
	}
}));
// Mock the generated SDK: Phase-4 migration routes legacy /api/auth/login
// (and the other 10 wrappers in auth.ts) through this module instead of
// through `apiFetch`. All 11 symbols are mocked as vi.fn() so a test that
// doesn't customize a mock just gets an undefined-returning stub — the
// transitive callers in auth.ts wrap SDK calls in `try { ... } catch { ... }`
// with a safe fallback, so an unmocked call never crashes the test.
vi.mock('../generated/openapi/sdk.gen', () => ({
	dpopBind: vi.fn(),
	getCurrentUser: vi.fn(),
	getSystemStatus: vi.fn(),
	login: vi.fn(),
	logout: vi.fn(),
	oidcExchange: vi.fn(),
	oidcLinkStart: vi.fn(),
	oidcProviders: vi.fn(),
	oidcUnlink: vi.fn(),
	refreshToken: vi.fn(),
	register: vi.fn(),
	sendMagicLink: vi.fn(),
	setupAdmin: vi.fn(),
	upgradeToInternal: vi.fn()
}));
import { apiFetch, apiJson, apiProbeTransport } from '$lib/api/client';
import {
	getCurrentUser as sdkGetCurrentUserMock,
	getSystemStatus as sdkGetSystemStatusMock,
	login as sdkLoginMock,
	logout as sdkLogoutMock,
	oidcExchange as sdkOidcExchangeMock,
	oidcProviders as sdkOidcProvidersMock,
	refreshToken as sdkRefreshTokenMock,
	register as sdkRegisterMock,
	sendMagicLink as sdkSendMagicLinkMock,
	setupAdmin as sdkSetupAdminMock
} from '../generated/openapi/sdk.gen';
import * as auth from './auth';
import { __resetOpaqueParamsCache } from './opaque';
const f = apiFetch as unknown as ReturnType<typeof vi.fn>;
const j = apiJson as unknown as ReturnType<typeof vi.fn>;
// Expose the SDK stubs the migrated wrappers call. Each is `vi.fn()`
// from the `vi.mock('../generated/openapi/sdk.gen')` block above —
// cast here so individual tests can `.mockResolvedValueOnce(...)` or
// read `.mock.calls`.
const sdkGetCurrentUser = sdkGetCurrentUserMock as unknown as ReturnType<typeof vi.fn>;
const sdkLogin = sdkLoginMock as unknown as ReturnType<typeof vi.fn>;
const sdkLogout = sdkLogoutMock as unknown as ReturnType<typeof vi.fn>;
const sdkOidcProviders = sdkOidcProvidersMock as unknown as ReturnType<typeof vi.fn>;
const sdkGetSystemStatus = sdkGetSystemStatusMock as unknown as ReturnType<typeof vi.fn>;
const sdkOidcExchange = sdkOidcExchangeMock as unknown as ReturnType<typeof vi.fn>;
const sdkRefreshToken = sdkRefreshTokenMock as unknown as ReturnType<typeof vi.fn>;
const sdkRegister = sdkRegisterMock as unknown as ReturnType<typeof vi.fn>;
const sdkSendMagicLink = sdkSendMagicLinkMock as unknown as ReturnType<typeof vi.fn>;
const sdkSetupAdmin = sdkSetupAdminMock as unknown as ReturnType<typeof vi.fn>;
// Several auth probes use the raw global fetch (NOT apiFetch) on purpose.
// `headers: new Headers()` matches the real `fetch()` contract — several
// probes (fetchMe, tryRefresh) run through the DPoP nonce-update shim,
// which calls `response.headers.get('DPoP-Nonce')` on every reply.
const okRes = { ok: true, status: 200, headers: new Headers(), json: async () => ({}) };
beforeEach(() => {
	vi.clearAllMocks();
	// login() dynamically imports the OPAQUE client and calls
	// syncOpaqueEnvelope, which caches /params results in a
	// module-level singleton. Reset it so each test's mock
	// responses drive a fresh /params fetch.
	__resetOpaqueParamsCache();
	// Default: DPoP proof unavailable (matches jsdom's missing WebCrypto).
	// DPoP-aware describe blocks below opt in by setting a proof value.
	dpopState.proof = null;
	f.mockResolvedValue(okRes);
	j.mockResolvedValue({});
	vi.stubGlobal('fetch', vi.fn().mockResolvedValue(okRes));
});
afterEach(() => vi.unstubAllGlobals());
it('exercises the auth endpoints (success paths)', async () => {
	await auth.fetchMe().catch(() => {});
	await auth.tryRefresh().catch(() => {});
	await auth.login('u', 'p').catch(() => {});
	await auth.getOidcProviders().catch(() => {});
	await auth.getAuthStatus().catch(() => {});
	await auth.setupAdmin('e@x.test', 'p').catch(() => {});
	await auth.exchangeOidcCode('code').catch(() => {});
	await auth.register('e@x.test', 'p', 'u').catch(() => {});
	await auth.sendMagicLink('e@x.test').catch(() => {});
	await auth.logout().catch(() => {});
	// Smoke count across every wire surface this file routes through:
	//   * apiFetch (`f`) — still used by fetchMe + OPAQUE helpers in opaque.ts
	//   * raw global fetch (`fc`) — tryRefresh goes direct
	//   * SDK stubs — the 8 Phase-4-migrated wrappers call these; each stub
	//     records its invocation whether or not it resolved to a usable value
	// The spirit is "every function was called and ran its code path", not
	// a precise count — the exact number shifts as more wrappers migrate.
	const fc = (globalThis.fetch as unknown as ReturnType<typeof vi.fn>).mock.calls.length;
	const sdkMocks = [
		sdkLogin,
		sdkLogout,
		sdkOidcProviders,
		sdkGetSystemStatus,
		sdkSetupAdmin,
		sdkOidcExchange,
		sdkRegister,
		sdkSendMagicLink
	];
	const sdkCount = sdkMocks.reduce((n, m) => n + m.mock.calls.length, 0);
	expect(fc + f.mock.calls.length + sdkCount).toBeGreaterThan(5);
});
it('fetchMe returns null when the server responds 401', async () => {
	// Phase 4 — SDK call with `responseStyle: 'fields'` + no throwOnError.
	// On 401, Hey API resolves (not rejects) with `{response, error}` so
	// fetchMe can read `response.status` directly. The pre-migration test
	// mocked a REJECTED promise; after the "token not provided" fix the
	// SDK resolves with the response-shaped object instead. See the
	// comment in `fetchMe` for why — tl;dr: Hey API's throwOnError path
	// throws the parsed error BODY, not a Response wrapper.
	sdkGetCurrentUser.mockResolvedValue({
		response: new Response(null, { status: 401 }),
		error: { error: 'Token not provided' }
	});
	await expect(auth.fetchMe()).resolves.toBeNull();
});
it('fetchMe opts out of the refresh+retry interceptor on 401', async () => {
	// `fetchMe` is the boot-time "am I logged in?" probe. On an expired
	// cookie it MUST return null cleanly, not silently refresh+retry —
	// a refresh at boot defeats the "cleanly logged-out on boot" path.
	// Pinned pre-migration (d131ebc4) and preserved under the SDK swap:
	// fetchMe passes `fetch: apiProbeTransport` on the SDK call, which
	// has `retryUnauthorized: false` baked in and a no-op
	// `onSessionExpired` handler. See src/lib/api/transport.ts:362.
	sdkGetCurrentUser.mockResolvedValueOnce({
		response: new Response(null, { status: 401 }),
		error: { error: 'Token not provided' }
	});
	await expect(auth.fetchMe()).resolves.toBeNull();
	// One SDK call — no refresh dance, no second trip.
	expect(sdkGetCurrentUser).toHaveBeenCalledTimes(1);
	// Opt-out signal: the per-call `fetch` option must be
	// apiProbeTransport (identity check — the probe variant is the
	// contract, not just "any fetch override"). If the migration
	// drops the override, the default `apiTransport` would silently
	// refresh+retry and this assertion catches it.
	const call = sdkGetCurrentUser.mock.calls[0]?.[0];
	expect(call).toBeTruthy();
	expect(call.fetch).toBe(apiProbeTransport);
	// AND responseStyle must be 'fields' — guards against a future
	// swap back to 'data'+throwOnError that would reintroduce the
	// "token not provided" regression (the body-parse throws, which
	// catch here can't distinguish from a 401).
	expect(call.responseStyle).toBe('fields');
});
it('tryRefresh returns false when the refresh fails', async () => {
	// Phase 4 — SDK call. Any thrown error (401, 500, network) returns
	// `false` cleanly; the `apiProbeTransport` override guarantees no
	// `onSessionExpired` divert on the way out.
	sdkRefreshToken.mockRejectedValue({ response: { status: 401 } });
	await expect(auth.tryRefresh()).resolves.toBe(false);
});

// ── Phase 2 + 3: OPAQUE lookup, silent migration, and login flip ─────
//
// `login()` MUST first probe the server for an OPAQUE envelope via
// `POST /api/auth/opaque/login/lookup`:
//   - if the response says `hasOpaque: true` → dispatch to
//     `opaqueLogin` (KE1/KE3) and skip the legacy path entirely
//     (Phase 3 cutover);
//   - if `false` → fall through to `POST /api/auth/login` (legacy),
//     then run `syncOpaqueEnvelope(password)` to silently mint the
//     envelope for the NEXT login (Phase 2 silent migration).
//
// Regression risks these tests guard:
//   - The lookup POST goes missing (Phase 3 flip never fires — every
//     login stays on legacy forever, defeats the OPAQUE substrate).
//   - The silent-migration hook goes missing on the legacy branch
//     (envelopes never get minted, so the lookup always returns
//     false — same net effect as above).
//   - The OPAQUE branch silently falls back to legacy on any WASM
//     hiccup (would mask a wrong passphrase as a network error).
it('login flips to OPAQUE when the lookup reports hasOpaque: true (no rotation when KSF matches)', async () => {
	// Order on the wire, given the current implementation:
	//   1. GET  /api/auth/opaque/params            (via checkOpaqueAvailable → fetchOpaqueParams)
	//   2. POST /api/auth/opaque/login/lookup     → { hasOpaque: true, ksf: <matches /params> }
	//   3. POST /api/auth/opaque/login/ke1        → { exchangeId, loginResponse }
	//   4. POST /api/auth/opaque/login/ke3        → AuthResponse
	// The legacy /api/auth/login POST must NOT fire on this branch, AND
	// Phase C's rotation MUST NOT fire because the envelope's KSF matches
	// what /params publishes — nothing to rotate to.
	const paramsKsf = { memoryKib: 8, iterations: 1, parallelism: 1 };
	f.mockResolvedValueOnce({
		ok: true,
		status: 200,
		statusText: 'OK',
		json: async () => ({
			enabled: true,
			ciphersuiteVersion: 1,
			ksf: paramsKsf
		})
	})
		.mockResolvedValueOnce({
			ok: true,
			status: 200,
			statusText: 'OK',
			json: async () => ({ hasOpaque: true, ksf: paramsKsf })
		})
		.mockResolvedValueOnce({
			ok: true,
			status: 200,
			statusText: 'OK',
			json: async () => ({ exchangeId: 'ex-1', loginResponse: 'LR' })
		})
		.mockResolvedValueOnce({
			ok: true,
			status: 200,
			statusText: 'OK',
			json: async () => ({
				user: { id: 'u1', email: 'a@x.test' },
				access_token: 'at-opaque',
				refresh_token: 'rt-opaque',
				token_type: 'Bearer',
				expires_in: 3600
			})
		});

	const authResponse = await auth.login('alice@example.com', 'correct horse battery staple');
	expect(authResponse.access_token).toBe('at-opaque');

	const urls = f.mock.calls.map((c: unknown[]) => c[0] as string);
	expect(urls).toEqual([
		'/api/auth/opaque/params',
		'/api/auth/opaque/login/lookup',
		'/api/auth/opaque/login/ke1',
		'/api/auth/opaque/login/ke3'
	]);
	// Legacy MUST NOT run when we took the OPAQUE branch.
	expect(urls).not.toContain('/api/auth/login');
	// Phase C: no rotation → no register/* calls.
	expect(urls).not.toContain('/api/auth/opaque/register/start');
	expect(urls).not.toContain('/api/auth/opaque/register/finish');
});

// ── Phase C: silent KSF rotation on OPAQUE login ─────────────────────
//
// `login()` MUST fire `syncOpaqueEnvelope(password)` after a successful
// OPAQUE login when the envelope's stored KSF differs from the server's
// currently-published KSF (or when the envelope predates per-envelope
// KSF storage, signalled by `lookup.ksf === null / absent`). Regression
// this guards: silently ignoring the drift would freeze users on
// whatever KSF they registered under years ago, making the operator's
// tuning-defaults knob effectively write-only for existing accounts.

it('OPAQUE login triggers silent KSF rotation when envelope KSF drifted from /params', async () => {
	// Wire order:
	//   1. GET  /api/auth/opaque/params     (via checkOpaqueAvailable — server publishes NEW KSF)
	//   2. POST /api/auth/opaque/login/lookup → { hasOpaque: true, ksf: <OLD, drifted from /params> }
	//   3. POST /api/auth/opaque/login/ke1  → { exchangeId, loginResponse }  (uses OLD envelope KSF)
	//   4. POST /api/auth/opaque/login/ke3  → AuthResponse
	//   5. POST /api/auth/opaque/register/start  ← rotation fires
	//   6. POST /api/auth/opaque/register/finish
	// After (6), the envelope is re-minted under the CURRENT /params
	// KSF, so the user's next login uses the new values.
	const newParamsKsf = { memoryKib: 8, iterations: 1, parallelism: 1 };
	const oldEnvelopeKsf = { memoryKib: 32, iterations: 3, parallelism: 4 };
	f.mockResolvedValueOnce({
		ok: true,
		status: 200,
		statusText: 'OK',
		json: async () => ({ enabled: true, ciphersuiteVersion: 1, ksf: newParamsKsf })
	})
		.mockResolvedValueOnce({
			ok: true,
			status: 200,
			statusText: 'OK',
			json: async () => ({ hasOpaque: true, ksf: oldEnvelopeKsf })
		})
		.mockResolvedValueOnce({
			ok: true,
			status: 200,
			statusText: 'OK',
			json: async () => ({ exchangeId: 'ex-1', loginResponse: 'LR' })
		})
		.mockResolvedValueOnce({
			ok: true,
			status: 200,
			statusText: 'OK',
			json: async () => ({
				user: { id: 'u1', email: 'a@x.test' },
				access_token: 'at-opaque',
				refresh_token: 'rt-opaque',
				token_type: 'Bearer',
				expires_in: 3600
			})
		})
		.mockResolvedValueOnce({
			ok: true,
			status: 200,
			statusText: 'OK',
			json: async () => ({ registrationResponse: 'RESP-R' })
		})
		.mockResolvedValueOnce({ ok: true, status: 204, json: async () => ({}) });

	const authResponse = await auth.login('alice@example.com', 'pw');
	expect(authResponse.access_token).toBe('at-opaque');

	const urls = f.mock.calls.map((c: unknown[]) => c[0] as string);
	expect(urls).toEqual([
		'/api/auth/opaque/params',
		'/api/auth/opaque/login/lookup',
		'/api/auth/opaque/login/ke1',
		'/api/auth/opaque/login/ke3',
		'/api/auth/opaque/register/start',
		'/api/auth/opaque/register/finish'
	]);
});

it('OPAQUE login triggers silent rotation when envelope predates per-envelope KSF (ksf null)', async () => {
	// `lookup.ksf` is null/absent → envelope predates migration
	// 20261005000000 → rotation fires so the envelope gets stored under
	// the new per-envelope schema on the next go-round. Same wire
	// sequence as the drift case above.
	f.mockResolvedValueOnce({
		ok: true,
		status: 200,
		statusText: 'OK',
		json: async () => ({
			enabled: true,
			ciphersuiteVersion: 1,
			ksf: { memoryKib: 8, iterations: 1, parallelism: 1 }
		})
	})
		.mockResolvedValueOnce({
			ok: true,
			status: 200,
			statusText: 'OK',
			// No `ksf` field at all — pre-migration envelope.
			json: async () => ({ hasOpaque: true })
		})
		.mockResolvedValueOnce({
			ok: true,
			status: 200,
			statusText: 'OK',
			json: async () => ({ exchangeId: 'ex-1', loginResponse: 'LR' })
		})
		.mockResolvedValueOnce({
			ok: true,
			status: 200,
			statusText: 'OK',
			json: async () => ({
				user: { id: 'u1', email: 'a@x.test' },
				access_token: 'at',
				refresh_token: 'rt',
				token_type: 'Bearer',
				expires_in: 3600
			})
		})
		.mockResolvedValueOnce({
			ok: true,
			status: 200,
			statusText: 'OK',
			json: async () => ({ registrationResponse: 'RESP-R' })
		})
		.mockResolvedValueOnce({ ok: true, status: 204, json: async () => ({}) });

	await auth.login('alice@example.com', 'pw');

	const urls = f.mock.calls.map((c: unknown[]) => c[0] as string);
	expect(urls).toContain('/api/auth/opaque/register/start');
	expect(urls).toContain('/api/auth/opaque/register/finish');
});

it('login falls back to legacy + silent-migration when hasOpaque: false', async () => {
	// Order on the wire (Phase-4 split — see sdkLogin mock below):
	//   1. GET  /api/auth/opaque/params        (apiFetch)
	//   2. POST /api/auth/opaque/login/lookup  (apiFetch)  → { hasOpaque: false }
	//   3. POST /api/auth/login                (sdkLogin)  → AuthResponse
	//   4. POST /api/auth/opaque/register/start   (apiFetch)
	//   5. POST /api/auth/opaque/register/finish  (apiFetch)
	f.mockResolvedValueOnce({
		ok: true,
		status: 200,
		statusText: 'OK',
		json: async () => ({
			enabled: true,
			ciphersuiteVersion: 1,
			ksf: { memoryKib: 8, iterations: 1, parallelism: 1 }
		})
	})
		.mockResolvedValueOnce({
			ok: true,
			status: 200,
			statusText: 'OK',
			json: async () => ({ hasOpaque: false })
		})
		.mockResolvedValueOnce({
			ok: true,
			status: 200,
			statusText: 'OK',
			json: async () => ({ registrationResponse: 'RESP-R' })
		})
		.mockResolvedValueOnce({ ok: true, status: 204, json: async () => ({}) });
	// Phase 4: legacy POST /api/auth/login now goes through the SDK, not
	// apiFetch. The SDK call is mocked separately — it still returns the
	// same AuthResponse shape so downstream silent-migration + return
	// value are unchanged.
	sdkLogin.mockResolvedValueOnce({
		user: { id: 'u1', email: 'a@x.test' },
		access_token: 'at',
		refresh_token: 'rt',
		token_type: 'Bearer',
		expires_in: 3600
	});

	const authResponse = await auth.login('alice@example.com', 'correct horse battery staple');
	expect(authResponse.access_token).toBe('at');

	// URL ordering now covers the apiFetch surface only; the SDK leg is
	// asserted via `sdkLogin.mock.calls` below.
	const urls = f.mock.calls.map((c: unknown[]) => c[0] as string);
	expect(urls).toEqual([
		'/api/auth/opaque/params',
		'/api/auth/opaque/login/lookup',
		'/api/auth/opaque/register/start',
		'/api/auth/opaque/register/finish'
	]);
	expect(sdkLogin).toHaveBeenCalledOnce();
	expect(sdkLogin.mock.calls[0][0]).toMatchObject({
		body: expect.objectContaining({
			username: 'alice@example.com',
			password: 'correct horse battery staple'
		}),
		throwOnError: true
	});
});

it('login skips the OPAQUE branch entirely when the substrate is disabled', async () => {
	// /params replies `enabled: false` (mode=off or misconfig). The
	// lookup POST MUST NOT fire (cheap short-circuit inside
	// checkOpaqueAvailable), and the legacy silent-migration hook
	// MUST also short-circuit — no register/start calls either.
	// Only apiFetch call: GET /api/auth/opaque/params. The legacy
	// POST /api/auth/login runs through the SDK (mocked below).
	f.mockResolvedValueOnce({
		ok: true,
		status: 200,
		statusText: 'OK',
		json: async () => ({
			enabled: false,
			ciphersuiteVersion: 0,
			ksf: { memoryKib: 0, iterations: 0, parallelism: 0 }
		})
	});
	sdkLogin.mockResolvedValueOnce({
		user: { id: 'u1', email: 'a@x.test' },
		access_token: 'at',
		refresh_token: 'rt',
		token_type: 'Bearer',
		expires_in: 3600
	});

	const authResponse = await auth.login('alice@example.com', 'pw');
	expect(authResponse.access_token).toBe('at');

	const urls = f.mock.calls.map((c: unknown[]) => c[0] as string);
	expect(urls).toEqual(['/api/auth/opaque/params']);
	expect(sdkLogin).toHaveBeenCalledOnce();
});

it('login returns AuthResponse even when silent-migration fails (non-fatal)', async () => {
	// Wire order: /params (enabled=true) → lookup (false) → legacy
	// login (200) → /params is cached, skip → register/start fails
	// with 500 → syncOpaqueEnvelope logs a console.warn and returns,
	// login() still returns the AuthResponse to the caller.
	f.mockResolvedValueOnce({
		ok: true,
		status: 200,
		statusText: 'OK',
		json: async () => ({
			enabled: true,
			ciphersuiteVersion: 1,
			ksf: { memoryKib: 8, iterations: 1, parallelism: 1 }
		})
	})
		.mockResolvedValueOnce({
			ok: true,
			status: 200,
			statusText: 'OK',
			json: async () => ({ hasOpaque: false })
		})
		.mockResolvedValueOnce({
			ok: false,
			status: 500,
			statusText: 'Internal Server Error',
			json: async () => ({})
		});
	// Phase 4: legacy POST /api/auth/login routed through SDK — returns
	// the AuthResponse that must survive the subsequent syncOpaqueEnvelope
	// failure. The 500 above is the register/start response that fires
	// INSIDE syncOpaqueEnvelope; the AuthResponse from sdkLogin is what
	// the caller receives regardless.
	sdkLogin.mockResolvedValueOnce({
		user: { id: 'u1', email: 'a@x.test' },
		access_token: 'at',
		refresh_token: 'rt',
		token_type: 'Bearer',
		expires_in: 3600
	});

	const consoleSpy = vi.spyOn(console, 'warn').mockImplementation(() => {});
	const authResponse = await auth.login('alice@example.com', 'pw');
	expect(authResponse.access_token).toBe('at');
	consoleSpy.mockRestore();
});

// ── tryRefresh — DPoP wiring ─────────────────────────────────────────
//
// Startup probe (raw `fetch`, NOT apiFetch) that must still authenticate
// under `DPOP=required`: page reloads with a bound session + expired
// access token would otherwise 401. Mirrors `fetchMe`'s DPoP handling:
// dynamic-import proof module, attach `DPoP` header, harvest response
// nonce into the shared cache, retry ONCE on `use_dpop_nonce`.
//
// Regression risks these tests guard:
//   - Proof stops being attached → bound sessions can't refresh.
//   - Nonce not harvested → the next request 401s with `nonce_missing`.
//   - Second challenge loops (would burn cycles and mask a real server
//     bug behind an infinite retry).
describe('tryRefresh — DPoP wiring', () => {
	const okRefreshRes = () => ({
		ok: true,
		status: 200,
		headers: new Headers(),
		json: async () => ({})
	});
	const nonceChallenge = () => ({
		ok: false,
		status: 401,
		headers: new Headers({
			'WWW-Authenticate': 'DPoP error="use_dpop_nonce"',
			'DPoP-Nonce': 'srv-fresh'
		}),
		json: async () => ({})
	});

	beforeEach(async () => {
		// Phase 4 — tryRefresh now routes through the generated SDK's
		// `refreshToken`, which the top-level `vi.mock` has stubbed as
		// a plain `vi.fn()`. These wire-level tests need the REAL SDK
		// call so the request actually hits the DPoP transport and the
		// stubbed `globalThis.fetch` records the headers. Point the
		// mocked SDK symbol at the real implementation inside this
		// describe only; every other test still gets the plain stub.
		const sdk = await vi.importActual<typeof import('../generated/openapi/sdk.gen')>(
			'../generated/openapi/sdk.gen'
		);
		sdkRefreshToken.mockImplementation(sdk.refreshToken);
		// Opt into DPoP-attached behaviour for this block; individual
		// tests can still flip it back to null to check the fail-open.
		dpopState.proof = 'proof.abc';
	});

	it('attaches a DPoP header on the refresh POST', async () => {
		const spy = vi.fn().mockResolvedValue(okRefreshRes());
		vi.stubGlobal('fetch', spy);

		const ok = await auth.tryRefresh();
		expect(ok).toBe(true);

		const [url, init] = spy.mock.calls[0];
		expect((url as Request).url).toBe(`${location.origin}/api/auth/refresh`);
		const hdrs = new Headers((init as RequestInit).headers ?? {});
		expect(hdrs.get('DPoP')).toBe('proof.abc');
	});

	it('sends no DPoP header when the proof module has no keypair (fail-open)', async () => {
		dpopState.proof = null;
		const spy = vi.fn().mockResolvedValue(okRefreshRes());
		vi.stubGlobal('fetch', spy);

		await auth.tryRefresh();

		const [, init] = spy.mock.calls[0];
		const hdrs = new Headers((init as RequestInit).headers ?? {});
		expect(hdrs.get('DPoP')).toBeNull();
	});

	it('retries once on a use_dpop_nonce challenge, then succeeds', async () => {
		const spy = vi
			.fn()
			.mockResolvedValueOnce(nonceChallenge())
			.mockResolvedValueOnce(okRefreshRes());
		vi.stubGlobal('fetch', spy);

		const ok = await auth.tryRefresh();
		expect(ok).toBe(true);
		expect(spy).toHaveBeenCalledTimes(2);
	});

	it('does not loop when the retry ALSO returns use_dpop_nonce', async () => {
		// A second challenge would indicate a server-side nonce bug;
		// tryRefresh must surface it as a plain refresh failure rather
		// than looping forever.
		const spy = vi.fn().mockResolvedValue(nonceChallenge());
		vi.stubGlobal('fetch', spy);

		const ok = await auth.tryRefresh();
		expect(ok).toBe(false);
		expect(spy).toHaveBeenCalledTimes(2);
	});
});

// ── Error-message pins for the plain-Error throw paths ──────────────
//
// `register`, `setupAdmin`, and `sendMagicLink` all throw `new Error(...)`
// rather than the typed `ApiError`, because the signup form / setup
// wizard / magic-link page render `.message` directly without
// branching on `error_type`. These pins guard against a specific class
// of regression where the thrown message surfaces as the literal string
// "undefined" — the manual sandbox checklist watches for exactly that
// (`Row 7: "a 400/409 shows a clean error message, not undefined"`).
//
// Pre-migration shape: `apiFetch` returned a Response; the wrapper read
// `.json().message || .json().error || "foo failed: {status}"`.
// Post-migration shape: SDK throws the parsed body (`{error|message}`);
// the wrapper reads from `err.data`. A regression on either half can
// collapse the || chain to `undefined`, which only shows up visually
// in the SPA.
describe('plain-Error wrappers never surface "undefined" to the UI', () => {
	it('register extracts the server error message, not undefined', async () => {
		sdkRegister.mockRejectedValueOnce({
			response: { status: 409 },
			data: { message: 'email already registered' }
		});
		await expect(auth.register('x@y.z', 'p')).rejects.toThrow('email already registered');
	});

	it('register falls back to a status-formatted message when the body is empty', async () => {
		sdkRegister.mockRejectedValueOnce({ response: { status: 500 }, data: {} });
		await expect(auth.register('x@y.z', 'p')).rejects.toThrow(/^register failed: 500$/);
		// The whole point: NEVER the string "undefined".
		try {
			await auth.register('x@y.z', 'p');
			sdkRegister.mockRejectedValueOnce({ response: { status: 500 }, data: {} });
		} catch (e) {
			expect((e as Error).message).not.toContain('undefined');
		}
	});

	it('setupAdmin extracts the server error message, not undefined', async () => {
		sdkSetupAdmin.mockRejectedValueOnce({
			response: { status: 400 },
			data: { error: 'password too weak' }
		});
		await expect(auth.setupAdmin('e@x.test', 'short')).rejects.toThrow('password too weak');
	});

	it('setupAdmin falls back to a status message when the body is empty', async () => {
		sdkSetupAdmin.mockRejectedValueOnce({ response: { status: 500 }, data: null });
		await expect(auth.setupAdmin('e@x.test', 'p')).rejects.toThrow(/^setup failed: 500$/);
	});

	it('sendMagicLink preserves the caught error as `cause` on throw', async () => {
		// 503 branches to 'unavailable' (soft). Other failures throw an
		// Error — must carry `cause` so the UI can surface the real
		// transport failure in dev consoles. The ESLint rule
		// preserve-caught-error enforces this at the source side; this
		// test pins it at the behavioural side.
		const underlying = { response: { status: 500 }, data: {} };
		sdkSendMagicLink.mockRejectedValueOnce(underlying);
		try {
			await auth.sendMagicLink('x@y.z');
			throw new Error('expected magic-link to throw');
		} catch (e) {
			expect((e as Error).message).not.toContain('undefined');
			expect((e as Error).cause).toBe(underlying);
		}
	});
});
