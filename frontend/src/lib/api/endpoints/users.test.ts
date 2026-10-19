/**
 * Unit tests for `users.ts` — specifically the two invariants a Phase-4
 * SDK swap must preserve:
 *
 *   1. `resolveUser` dedupes concurrent calls for the same id by caching
 *      the IN-FLIGHT Promise. Breaking this reintroduces an N+1 on admin
 *      pages that mount `<UserVignette>` per row — Playwright wouldn't
 *      flag it; only a fetch-call count would.
 *
 *   2. `seedUser` primes the cache so subsequent `resolveUser(id)` is
 *      synchronous and makes no HTTP call. This is the explicit N+1
 *      kill-switch the admin page relies on.
 *
 * No test file existed before — the cache logic was covered only
 * transitively by components that happen to call it. Added before any
 * migration that touches users.ts.
 */

import { describe, it, expect, vi, beforeEach } from 'vitest';

// Each test uses a UNIQUE id so the module-level cache in `users.ts`
// doesn't need a reset hook. Tests don't share state via ids.
vi.mock('$lib/api/client', () => ({
	apiFetch: vi.fn(),
	apiJson: vi.fn(),
	withBase: (p: string) => p
}));

import { apiFetch } from '$lib/api/client';
import * as users from './users';

const f = apiFetch as unknown as ReturnType<typeof vi.fn>;

function okUser(id: string) {
	return {
		ok: true,
		status: 200,
		json: async () => ({
			id,
			username: `user-${id}`,
			email: `${id}@example.com`,
			image: null,
			is_external: false,
			is_online: true
		})
	};
}

beforeEach(() => {
	vi.clearAllMocks();
});

describe('resolveUser cache invariants', () => {
	it('dedupes concurrent calls for the same id (one HTTP call for two awaits)', async () => {
		f.mockResolvedValue(okUser('u-dedupe'));
		const [a, b] = await Promise.all([
			users.resolveUser('u-dedupe'),
			users.resolveUser('u-dedupe')
		]);
		expect(a).not.toBeNull();
		// Both callers get the SAME resolved object — not just equal values,
		// identity. That proves the Promise was cached, not re-fetched.
		expect(a).toBe(b);
		expect(f).toHaveBeenCalledTimes(1);
	});

	it('caches a resolved result (second call makes no HTTP request)', async () => {
		f.mockResolvedValue(okUser('u-resolved'));
		await users.resolveUser('u-resolved');
		expect(f).toHaveBeenCalledTimes(1);
		await users.resolveUser('u-resolved');
		expect(f).toHaveBeenCalledTimes(1); // still 1
	});

	it('caches null results too (a 404 is not re-queried)', async () => {
		f.mockResolvedValue({ ok: false, status: 404, json: async () => ({}) });
		const first = await users.resolveUser('u-missing');
		expect(first).toBeNull();
		expect(f).toHaveBeenCalledTimes(1);
		const second = await users.resolveUser('u-missing');
		expect(second).toBeNull();
		expect(f).toHaveBeenCalledTimes(1); // the null is cached as a resolved Promise
	});
});

describe('seedUser priming', () => {
	it('primes the cache so resolveUser makes NO HTTP call', async () => {
		users.seedUser({
			id: 'u-seeded',
			username: 'seed',
			email: 'seed@example.com',
			image: null,
			is_external: false,
			is_online: false
		});
		const u = await users.resolveUser('u-seeded');
		expect(u).not.toBeNull();
		expect(u?.name).toBe('seed');
		expect(f).toHaveBeenCalledTimes(0);
	});

	it('does NOT clobber an in-flight lookup', async () => {
		// Start an in-flight resolveUser, then seed with different data for
		// the same id — the authoritative in-flight result must win.
		let resolveHttp!: (v: unknown) => void;
		const pending = new Promise((r) => {
			resolveHttp = r;
		});
		f.mockReturnValueOnce(pending);

		const inflight = users.resolveUser('u-inflight');
		// Seed should be a no-op because the id is already in the cache
		// (as an in-flight Promise, not a resolved one — same cache slot).
		users.seedUser({
			id: 'u-inflight',
			username: 'stale-seed-should-be-ignored',
			email: 'stale@example.com',
			image: null,
			is_external: true
		});

		resolveHttp(okUser('u-inflight'));
		const result = await inflight;
		expect(result?.name).toBe('user-u-inflight'); // from the HTTP response, not the stale seed
	});
});
