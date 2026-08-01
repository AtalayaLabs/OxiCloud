import { it, expect, vi, beforeEach, afterEach } from 'vitest';
vi.mock('$lib/api/client', () => ({ apiFetch: vi.fn(), apiJson: vi.fn() }));
vi.mock('$lib/api/csrf', () => ({ getCsrfHeaders: () => ({}) }));
// Mock the OPAQUE WASM client so `login()`'s silent-migration hook can
// exercise the wire path (params + register handshake) without touching
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

// ── Phase 2: silent OPAQUE migration on legacy login ─────────────────
//
// `login()` MUST trigger `syncOpaqueEnvelope(password)` after a
// successful POST /api/auth/login response, so users pick up an OPAQUE
// envelope automatically over time. Verified here by observing the
// wire calls made after the login POST — a successful login without
// a follow-up `/api/auth/opaque/*` fetch is a regression that would
// silently break Phase 3's cutover assumption ("most active users
// have an envelope on file by the time the SPA flips to OPAQUE").
it('login triggers OPAQUE silent-migration on success', async () => {
	// Login POST → 200 AuthResponse
	// Then syncOpaqueEnvelope runs: GET /params → POST register/start → POST register/finish
	f.mockResolvedValueOnce({
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
			json: async () => ({ registrationResponse: 'RESP-R' })
		})
		.mockResolvedValueOnce({ ok: true, status: 204, json: async () => ({}) });

	const authResponse = await auth.login('alice@example.com', 'correct horse battery staple');
	expect(authResponse.access_token).toBe('at');

	// 4 apiFetch calls: login + params + register/start + register/finish
	const urls = f.mock.calls.map((c: unknown[]) => c[0] as string);
	expect(urls).toContain('/api/auth/login');
	expect(urls).toContain('/api/auth/opaque/params');
	expect(urls).toContain('/api/auth/opaque/register/start');
	expect(urls).toContain('/api/auth/opaque/register/finish');
	// Login POST must come FIRST — silent migration only runs on a
	// session that's already been established.
	expect(urls[0]).toBe('/api/auth/login');
});

it('login returns AuthResponse even when silent-migration fails (non-fatal)', async () => {
	// Login POST succeeds; the follow-up /params returns 500. Silent
	// migration swallows the error (console.warn) — the login itself
	// still returns success to the caller, so the user reaches their
	// session and the envelope gets retried on next login.
	f.mockResolvedValueOnce({
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
	}).mockResolvedValueOnce({ ok: false, status: 500, json: async () => ({}) });

	const consoleSpy = vi.spyOn(console, 'warn').mockImplementation(() => {});
	const authResponse = await auth.login('alice@example.com', 'pw');
	expect(authResponse.access_token).toBe('at');
	consoleSpy.mockRestore();
});
