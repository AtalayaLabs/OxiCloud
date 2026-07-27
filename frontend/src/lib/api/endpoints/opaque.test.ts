import { beforeEach, describe, expect, it, vi } from 'vitest';

// Mock the transport layer + CSRF headers so the test asserts on the wire
// shape we send to the backend, not the actual network. WASM handshake
// results are captured by mocking `@serenity-kit/opaque`'s client namespace.
vi.mock('$lib/api/client', () => ({
	apiFetch: vi.fn(),
	ApiError: class ApiError extends Error {
		readonly status: number;
		readonly statusText: string;
		readonly errorType?: string;
		constructor(
			status: number,
			statusText: string,
			_resource: unknown,
			errorType?: string,
			message?: string
		) {
			super(message ?? `${status} ${statusText}`);
			this.status = status;
			this.statusText = statusText;
			this.errorType = errorType;
		}
	}
}));
vi.mock('$lib/api/csrf', () => ({ getCsrfHeaders: () => ({ 'x-csrf-token': 'test' }) }));

// Stub the WASM handshake with deterministic strings so the test can
// assert what the wire body contains without pulling in the real WASM.
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
			finishLoginRequest: 'FIN-L',
			sessionKey: 'SK',
			exportKey: 'EK',
			serverStaticPublicKey: 'SPK'
		}))
	}
}));

import { apiFetch, ApiError } from '$lib/api/client';
import * as opaque from '@serenity-kit/opaque';
import {
	__resetOpaqueParamsCache,
	fetchOpaqueParams,
	opaqueLogin,
	opaqueRegister,
	syncOpaqueEnvelope
} from './opaque';

const f = apiFetch as unknown as ReturnType<typeof vi.fn>;
const fin = opaque.client.finishLogin as unknown as ReturnType<typeof vi.fn>;

const KSF = { memoryKib: 8, iterations: 1, parallelism: 1 };

function okJson(body: unknown, status = 200) {
	return {
		ok: true,
		status,
		statusText: 'OK',
		clone() {
			return this;
		},
		json: async () => body
	};
}

function errJson(status: number, body: unknown) {
	return {
		ok: false,
		status,
		statusText: 'Bad',
		clone() {
			return this;
		},
		json: async () => body
	};
}

beforeEach(() => {
	vi.clearAllMocks();
	// Reset the params-fetch cache — tests below assert first-call
	// behaviour, and the singleton cache would carry a prior test's
	// resolved params into the next test if left alone.
	__resetOpaqueParamsCache();
	// Reset finishLogin to the truthy default; individual tests override.
	fin.mockReturnValue({
		finishLoginRequest: 'FIN-L',
		sessionKey: 'SK',
		exportKey: 'EK',
		serverStaticPublicKey: 'SPK'
	});
});

describe('opaqueRegister', () => {
	it('POSTs both rounds with the WASM-produced payloads', async () => {
		f.mockResolvedValueOnce(okJson({ registrationResponse: 'RESP-R' })).mockResolvedValueOnce(
			okJson({}, 204)
		);
		await opaqueRegister('correct horse battery staple', KSF, 1);

		expect(f).toHaveBeenCalledTimes(2);
		const [startUrl, startInit] = f.mock.calls[0];
		expect(startUrl).toBe('/api/auth/opaque/register/start');
		expect(JSON.parse(startInit.body as string)).toEqual({ registrationRequest: 'REQ-R' });

		const [finishUrl, finishInit] = f.mock.calls[1];
		expect(finishUrl).toBe('/api/auth/opaque/register/finish');
		expect(JSON.parse(finishInit.body as string)).toEqual({
			registrationRecord: 'RECORD-R',
			ciphersuiteVersion: 1
		});
	});

	it('throws ApiError with parsed error_type on start failure', async () => {
		f.mockResolvedValueOnce(
			errJson(409, { error_type: 'OpaqueAlreadyRegistered', message: 'already have envelope' })
		);
		await expect(opaqueRegister('pw', KSF, 1)).rejects.toMatchObject({
			status: 409,
			errorType: 'OpaqueAlreadyRegistered'
		});
	});

	it('never puts the passphrase on the wire', async () => {
		f.mockResolvedValueOnce(okJson({ registrationResponse: 'RESP-R' })).mockResolvedValueOnce(
			okJson({}, 204)
		);
		const secret = 'hunter2';
		await opaqueRegister(secret, KSF, 1);
		for (const [, init] of f.mock.calls) {
			expect(String(init.body ?? '')).not.toContain(secret);
		}
	});
});

describe('opaqueLogin', () => {
	it('POSTs KE1 with the user identifier + KE3 with the exchange id', async () => {
		f.mockResolvedValueOnce(
			okJson({ exchangeId: 'XID-42', loginResponse: 'RESP-L' })
		).mockResolvedValueOnce(
			okJson({
				user: { id: 'u1', email: 'a@x.test' },
				access_token: 'at',
				refresh_token: 'rt',
				token_type: 'Bearer',
				expires_in: 3600
			})
		);
		const auth = await opaqueLogin('alice@example.com', 'pw', KSF);
		expect(auth.access_token).toBe('at');

		const [ke1Url, ke1Init] = f.mock.calls[0];
		expect(ke1Url).toBe('/api/auth/opaque/login/ke1');
		expect(JSON.parse(ke1Init.body as string)).toEqual({
			userIdentifier: 'alice@example.com',
			startLoginRequest: 'REQ-L'
		});

		const [ke3Url, ke3Init] = f.mock.calls[1];
		expect(ke3Url).toBe('/api/auth/opaque/login/ke3');
		expect(JSON.parse(ke3Init.body as string)).toEqual({
			exchangeId: 'XID-42',
			finishLoginRequest: 'FIN-L'
		});
	});

	it('throws InvalidCredentials without touching the server when finishLogin returns undefined', async () => {
		// Wrong-passphrase case in the WASM API: finishLogin returns
		// undefined. Verify we short-circuit locally with the same
		// error_type shape the server would emit — anti-enumeration
		// requires both paths look identical to the caller.
		fin.mockReturnValueOnce(undefined);
		f.mockResolvedValueOnce(okJson({ exchangeId: 'XID', loginResponse: 'RESP-L' }));
		await expect(opaqueLogin('a@x.test', 'wrong', KSF)).rejects.toMatchObject({
			status: 401,
			errorType: 'InvalidCredentials'
		});
		expect(f).toHaveBeenCalledTimes(1); // KE1 only — KE3 must NOT fire
	});

	it('bubbles up the server error_type on KE1 failure', async () => {
		f.mockResolvedValueOnce(errJson(429, { error_type: 'RateLimited', message: 'slow down' }));
		const err = await opaqueLogin('a@x.test', 'pw', KSF).then(
			() => null,
			(e) => e
		);
		expect(err).toBeInstanceOf(ApiError);
		expect(err).toMatchObject({ status: 429, errorType: 'RateLimited' });
	});
});

describe('fetchOpaqueParams', () => {
	it('returns the payload and caches it (second call = no HTTP)', async () => {
		f.mockResolvedValueOnce(okJson({ enabled: true, ciphersuiteVersion: 1, ksf: KSF }));
		const first = await fetchOpaqueParams();
		expect(first.enabled).toBe(true);
		expect(first.ciphersuiteVersion).toBe(1);
		expect(first.ksf).toEqual(KSF);

		// Second call MUST NOT hit the wire — the operator contract is
		// that params change requires a page reload, so caching is safe.
		const second = await fetchOpaqueParams();
		expect(second).toEqual(first);
		expect(f).toHaveBeenCalledTimes(1);
	});

	it('degrades to enabled=false on a broken /params (no crash)', async () => {
		f.mockResolvedValueOnce(errJson(500, {}));
		const params = await fetchOpaqueParams();
		expect(params.enabled).toBe(false);
	});
});

describe('syncOpaqueEnvelope', () => {
	it('is a no-op when params.enabled=false', async () => {
		f.mockResolvedValueOnce(okJson({ enabled: false, ciphersuiteVersion: 0, ksf: KSF }));
		await syncOpaqueEnvelope('any-password');
		// Only the /params fetch — no register/start or register/finish.
		expect(f).toHaveBeenCalledTimes(1);
		expect(f.mock.calls[0][0]).toBe('/api/auth/opaque/params');
	});

	it('runs the register handshake when params.enabled=true', async () => {
		f.mockResolvedValueOnce(okJson({ enabled: true, ciphersuiteVersion: 1, ksf: KSF }))
			.mockResolvedValueOnce(okJson({ registrationResponse: 'RESP-R' }))
			.mockResolvedValueOnce(okJson({}, 204));
		await syncOpaqueEnvelope('correct horse battery staple');
		// /params + /register/start + /register/finish
		expect(f).toHaveBeenCalledTimes(3);
		expect(f.mock.calls[0][0]).toBe('/api/auth/opaque/params');
		expect(f.mock.calls[1][0]).toBe('/api/auth/opaque/register/start');
		expect(f.mock.calls[2][0]).toBe('/api/auth/opaque/register/finish');
	});

	it('swallows opaqueRegister errors — silent-migration retry recovers', async () => {
		// /params succeeds, register/start returns a server error. The
		// contract is "swallow, log, don't throw" so the caller (change-
		// password success handler) doesn't surface a user-facing toast
		// for a migration-hint step they didn't ask for.
		f.mockResolvedValueOnce(
			okJson({ enabled: true, ciphersuiteVersion: 1, ksf: KSF })
		).mockResolvedValueOnce(errJson(500, { error_type: 'InternalError' }));
		const consoleSpy = vi.spyOn(console, 'warn').mockImplementation(() => {});
		await expect(syncOpaqueEnvelope('pw')).resolves.toBeUndefined();
		expect(consoleSpy).toHaveBeenCalled();
		consoleSpy.mockRestore();
	});
});
