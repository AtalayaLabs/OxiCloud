import { describe, it, expect, vi, beforeEach, afterEach } from 'vitest';
vi.mock('$lib/api/client', () => ({ apiFetch: vi.fn(), apiJson: vi.fn() }));
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
import { apiFetch, apiJson } from '$lib/api/client';
import * as auth from './auth';
import { __resetOpaqueParamsCache } from './opaque';
const f = apiFetch as unknown as ReturnType<typeof vi.fn>;
const j = apiJson as unknown as ReturnType<typeof vi.fn>;
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
	const fc = (globalThis.fetch as unknown as ReturnType<typeof vi.fn>).mock.calls.length;
	expect(fc + f.mock.calls.length).toBeGreaterThan(3);
});
it('fetchMe returns null when the probe is not ok', async () => {
	// `headers: new Headers()` matches real `fetch()` — the DPoP-aware
	// path calls `response.headers.get('DPoP-Nonce')` on every reply
	// and would crash on a bare `{ok, status, json}` mock.
	vi.stubGlobal(
		'fetch',
		vi
			.fn()
			.mockResolvedValue({ ok: false, status: 401, headers: new Headers(), json: async () => ({}) })
	);
	await expect(auth.fetchMe()).resolves.toBeNull();
});
it('tryRefresh returns false when the refresh fails', async () => {
	vi.stubGlobal(
		'fetch',
		vi
			.fn()
			.mockResolvedValue({ ok: false, status: 401, headers: new Headers(), json: async () => ({}) })
	);
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
	// Order on the wire:
	//   1. GET  /api/auth/opaque/params
	//   2. POST /api/auth/opaque/login/lookup     → { hasOpaque: false }
	//   3. POST /api/auth/login                   → AuthResponse
	//   4. POST /api/auth/opaque/register/start   (params is cached — no re-fetch)
	//   5. POST /api/auth/opaque/register/finish
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

	const authResponse = await auth.login('alice@example.com', 'correct horse battery staple');
	expect(authResponse.access_token).toBe('at');

	const urls = f.mock.calls.map((c: unknown[]) => c[0] as string);
	expect(urls).toEqual([
		'/api/auth/opaque/params',
		'/api/auth/opaque/login/lookup',
		'/api/auth/login',
		'/api/auth/opaque/register/start',
		'/api/auth/opaque/register/finish'
	]);
});

it('login skips the OPAQUE branch entirely when the substrate is disabled', async () => {
	// /params replies `enabled: false` (mode=off or misconfig). The
	// lookup POST MUST NOT fire (cheap short-circuit inside
	// checkOpaqueAvailable), and the legacy silent-migration hook
	// MUST also short-circuit — no register/start calls either.
	f.mockResolvedValueOnce({
		ok: true,
		status: 200,
		statusText: 'OK',
		json: async () => ({
			enabled: false,
			ciphersuiteVersion: 0,
			ksf: { memoryKib: 0, iterations: 0, parallelism: 0 }
		})
	}).mockResolvedValueOnce({
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
	});

	const authResponse = await auth.login('alice@example.com', 'pw');
	expect(authResponse.access_token).toBe('at');

	const urls = f.mock.calls.map((c: unknown[]) => c[0] as string);
	expect(urls).toEqual(['/api/auth/opaque/params', '/api/auth/login']);
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
			ok: false,
			status: 500,
			statusText: 'Internal Server Error',
			json: async () => ({})
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

	beforeEach(() => {
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
		expect(url).toBe('/api/auth/refresh');
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
