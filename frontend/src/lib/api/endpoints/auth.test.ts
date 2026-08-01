import { it, expect, vi, beforeEach, afterEach } from 'vitest';
vi.mock('$lib/api/client', () => ({ apiFetch: vi.fn(), apiJson: vi.fn() }));
vi.mock('$lib/api/csrf', () => ({ getCsrfHeaders: () => ({}) }));
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
const okRes = { ok: true, status: 200, json: async () => ({}) };
beforeEach(() => {
	vi.clearAllMocks();
	// login() dynamically imports the OPAQUE client and calls
	// syncOpaqueEnvelope, which caches /params results in a
	// module-level singleton. Reset it so each test's mock
	// responses drive a fresh /params fetch.
	__resetOpaqueParamsCache();
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
	vi.stubGlobal(
		'fetch',
		vi.fn().mockResolvedValue({ ok: false, status: 401, json: async () => ({}) })
	);
	await expect(auth.fetchMe()).resolves.toBeNull();
});
it('tryRefresh returns false when the refresh fails', async () => {
	vi.stubGlobal(
		'fetch',
		vi.fn().mockResolvedValue({ ok: false, status: 401, json: async () => ({}) })
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
it('login flips to OPAQUE when the lookup reports hasOpaque: true', async () => {
	// Order on the wire, given the current implementation:
	//   1. GET  /api/auth/opaque/params            (via checkOpaqueAvailable → fetchOpaqueParams)
	//   2. POST /api/auth/opaque/login/lookup     → { hasOpaque: true }
	//   3. POST /api/auth/opaque/login/ke1        → { exchangeId, loginResponse }
	//   4. POST /api/auth/opaque/login/ke3        → AuthResponse
	// The legacy /api/auth/login POST must NOT fire on this branch.
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
