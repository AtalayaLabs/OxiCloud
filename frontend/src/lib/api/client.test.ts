import { beforeEach, describe, expect, it, vi } from 'vitest';

// vi.mock runs BEFORE all imports; vi.hoisted gives us a shared
// binding that both the mock factory and per-test setup can mutate.
// Reset in each test's beforeEach so tests don't leak state.
const dpopState = vi.hoisted(() => ({ proof: 'proof.value.here' as string | null }));
vi.mock('$lib/auth/dpop-proof', async () => {
	const actual =
		await vi.importActual<typeof import('$lib/auth/dpop-proof')>('$lib/auth/dpop-proof');
	return {
		...actual,
		buildDpopProof: vi.fn(async () => dpopState.proof)
	};
});

import { createApiFetch } from './client';

const ORIGIN = 'https://cloud.example';

function jsonResponse(status: number, body: unknown = {}): Response {
	return new Response(JSON.stringify(body), { status });
}

describe('createApiFetch — 401 refresh/retry parity', () => {
	let onSessionExpired: ReturnType<typeof vi.fn<() => void>>;

	beforeEach(() => {
		onSessionExpired = vi.fn<() => void>();
	});

	it('passes through a non-401 response untouched (no refresh)', async () => {
		const rawFetch = vi.fn().mockResolvedValue(jsonResponse(200, { ok: true }));
		const apiFetch = createApiFetch({ rawFetch, onSessionExpired, origin: ORIGIN });

		const res = await apiFetch(`${ORIGIN}/api/files`);

		expect(res.status).toBe(200);
		expect(rawFetch).toHaveBeenCalledTimes(1);
		expect(onSessionExpired).not.toHaveBeenCalled();
	});

	it('on 401 refreshes once then retries the original request', async () => {
		const rawFetch = vi
			.fn()
			.mockResolvedValueOnce(jsonResponse(401)) // original
			.mockResolvedValueOnce(jsonResponse(200)) // refresh ok
			.mockResolvedValueOnce(jsonResponse(200, { retried: true })); // retry
		const apiFetch = createApiFetch({ rawFetch, onSessionExpired, origin: ORIGIN });

		const res = await apiFetch(`${ORIGIN}/api/files`);

		expect(res.status).toBe(200);
		expect(await res.json()).toEqual({ retried: true });
		expect(rawFetch).toHaveBeenNthCalledWith(
			2,
			'/api/auth/refresh',
			expect.objectContaining({ method: 'POST' })
		);
		expect(rawFetch).toHaveBeenCalledTimes(3);
		expect(onSessionExpired).not.toHaveBeenCalled();
	});

	it('fires session-expired and throws when refresh fails', async () => {
		const rawFetch = vi
			.fn()
			.mockResolvedValueOnce(jsonResponse(401)) // original
			.mockResolvedValueOnce(jsonResponse(401)); // refresh fails
		const apiFetch = createApiFetch({ rawFetch, onSessionExpired, origin: ORIGIN });

		await expect(apiFetch(`${ORIGIN}/api/files`)).rejects.toThrow('Session expired');
		expect(onSessionExpired).toHaveBeenCalledTimes(1);
		expect(rawFetch).toHaveBeenCalledTimes(2); // original + refresh, NO retry
	});

	it('deduplicates concurrent 401s into a single refresh', async () => {
		let refreshCalls = 0;
		const rawFetch = vi.fn(async (input: RequestInfo | URL) => {
			const url = typeof input === 'string' ? input : (input as Request).url;
			if (url.includes('/api/auth/refresh')) {
				refreshCalls++;
				await new Promise((r) => setTimeout(r, 10));
				return jsonResponse(200);
			}
			// First hit per resource is a 401; retries (after refresh) succeed.
			return jsonResponse(refreshCalls > 0 ? 200 : 401);
		});
		const apiFetch = createApiFetch({ rawFetch, onSessionExpired, origin: ORIGIN });

		const [a, b] = await Promise.all([
			apiFetch(`${ORIGIN}/api/files`),
			apiFetch(`${ORIGIN}/api/folders`)
		]);

		expect(a.status).toBe(200);
		expect(b.status).toBe(200);
		expect(refreshCalls).toBe(1); // single shared refresh
	});

	it('passes cross-origin 401s through without refreshing', async () => {
		const rawFetch = vi.fn().mockResolvedValue(jsonResponse(401));
		const apiFetch = createApiFetch({ rawFetch, onSessionExpired, origin: ORIGIN });

		const res = await apiFetch('https://third-party.example/api/thing');

		expect(res.status).toBe(401);
		expect(rawFetch).toHaveBeenCalledTimes(1); // no refresh attempt
		expect(onSessionExpired).not.toHaveBeenCalled();
	});

	it.each([
		'/api/auth/login',
		'/api/auth/logout',
		'/api/auth/refresh',
		'/api/auth/register',
		'/api/auth/setup',
		'/api/auth/oidc/start',
		'/api/auth/device/code',
		'/api/s/sometoken'
	])('bypasses refresh for auth primitive / public share: %s', async (path) => {
		const rawFetch = vi.fn().mockResolvedValue(jsonResponse(401));
		const apiFetch = createApiFetch({ rawFetch, onSessionExpired, origin: ORIGIN });

		const res = await apiFetch(`${ORIGIN}${path}`);

		expect(res.status).toBe(401);
		expect(rawFetch).toHaveBeenCalledTimes(1);
		expect(onSessionExpired).not.toHaveBeenCalled();
	});

	it('retries user-data endpoints under /api/auth/ (e.g. me)', async () => {
		const rawFetch = vi
			.fn()
			.mockResolvedValueOnce(jsonResponse(401)) // original /api/auth/me
			.mockResolvedValueOnce(jsonResponse(200)) // refresh ok
			.mockResolvedValueOnce(jsonResponse(200, { id: 'u1' })); // retry
		const apiFetch = createApiFetch({ rawFetch, onSessionExpired, origin: ORIGIN });

		const res = await apiFetch(`${ORIGIN}/api/auth/me`);

		expect(res.status).toBe(200);
		expect(await res.json()).toEqual({ id: 'u1' });
		expect(rawFetch).toHaveBeenCalledTimes(3);
	});
});

describe('createApiFetch — DPoP header injection + nonce challenge', () => {
	beforeEach(() => {
		dpopState.proof = 'proof.value.here';
	});

	it('attaches a DPoP header on same-origin requests', async () => {
		const rawFetch = vi.fn().mockResolvedValue(jsonResponse(200, {}));
		const apiFetch = createApiFetch({
			rawFetch,
			onSessionExpired: () => {},
			origin: ORIGIN
		});

		await apiFetch(`${ORIGIN}/api/files`);
		const [, init] = rawFetch.mock.calls[0];
		const hdrs = new Headers((init as RequestInit)?.headers ?? {});
		expect(hdrs.get('DPoP')).toBe('proof.value.here');
	});

	it('does NOT attach a DPoP header on cross-origin requests', async () => {
		const rawFetch = vi.fn().mockResolvedValue(jsonResponse(200, {}));
		const apiFetch = createApiFetch({
			rawFetch,
			onSessionExpired: () => {},
			origin: ORIGIN
		});

		await apiFetch('https://third-party.example/api/thing');
		const [, init] = rawFetch.mock.calls[0];
		const hdrs = new Headers((init as RequestInit)?.headers ?? {});
		expect(hdrs.get('DPoP')).toBeNull();
	});

	it('skips the DPoP header when the keypair is unavailable (fail-open)', async () => {
		dpopState.proof = null;
		const rawFetch = vi.fn().mockResolvedValue(jsonResponse(200, {}));
		const apiFetch = createApiFetch({
			rawFetch,
			onSessionExpired: () => {},
			origin: ORIGIN
		});

		const res = await apiFetch(`${ORIGIN}/api/files`);
		expect(res.status).toBe(200);
		const [, init] = rawFetch.mock.calls[0];
		const hdrs = new Headers((init as RequestInit)?.headers ?? {});
		expect(hdrs.get('DPoP')).toBeNull();
	});

	it('retries once on a use_dpop_nonce challenge (harvests nonce, rebuilds proof)', async () => {
		const challenge = new Response(null, {
			status: 401,
			headers: {
				'WWW-Authenticate': 'DPoP error="use_dpop_nonce"',
				'DPoP-Nonce': 'srv-fresh'
			}
		});
		const rawFetch = vi
			.fn()
			.mockResolvedValueOnce(challenge)
			.mockResolvedValueOnce(jsonResponse(200, { ok: true }));
		const apiFetch = createApiFetch({
			rawFetch,
			onSessionExpired: () => {},
			origin: ORIGIN
		});

		const res = await apiFetch(`${ORIGIN}/api/files`);
		expect(res.status).toBe(200);
		expect(rawFetch).toHaveBeenCalledTimes(2); // original + one retry
	});

	it('does not loop when the retry ALSO returns use_dpop_nonce', async () => {
		// Use an auth-primitive path so the outer 401-refresh path is
		// bypassed — this test is scoped to the DPoP inner retry
		// only. `mockResolvedValue` (not `Once`) so we can COUNT how
		// many times the interceptor called through — it must be
		// exactly 2 (original + one retry), never 3.
		const challenge = new Response(null, {
			status: 401,
			headers: {
				'WWW-Authenticate': 'DPoP error="use_dpop_nonce"',
				'DPoP-Nonce': 'srv-fresh'
			}
		});
		const rawFetch = vi.fn().mockResolvedValue(challenge);
		const apiFetch = createApiFetch({
			rawFetch,
			onSessionExpired: () => {},
			origin: ORIGIN
		});

		const res = await apiFetch(`${ORIGIN}/api/auth/login`);
		expect(res.status).toBe(401);
		expect(rawFetch).toHaveBeenCalledTimes(2);
	});
});

describe('ApiError + apiJson', () => {
	it('ApiError carries status, statusText, and a descriptive message', async () => {
		const { ApiError } = await import('./client');
		const e = new ApiError(404, 'Not Found', '/api/files/x');
		expect(e.status).toBe(404);
		expect(e.statusText).toBe('Not Found');
		expect(e.name).toBe('ApiError');
		expect(e.message).toContain('404');
		expect(e.message).toContain('/api/files/x');
		expect(e).toBeInstanceOf(Error);
	});
});
