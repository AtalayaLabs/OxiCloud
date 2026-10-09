/**
 * Unit tests for the §2b SPA ETag cache. Mocks `$lib/api/client` so the
 * cache's `apiFetch` dependency is a plain vi.fn we can script per test.
 */
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

// Scripted responses for the mocked apiFetch — one entry consumed per
// call in order. `vi.hoisted` makes the array visible from both the mock
// factory (which runs before imports) and per-test setup.
const scripted = vi.hoisted(() => ({ queue: [] as Response[] }));
const fetchCalls = vi.hoisted(() => ({ list: [] as Array<[string, RequestInit | undefined]> }));

vi.mock('$lib/api/client', () => ({
	apiFetch: vi.fn(async (url: string, init?: RequestInit) => {
		fetchCalls.list.push([url, init]);
		const next = scripted.queue.shift();
		if (!next) throw new Error('apiFetch mock: queue empty');
		return next;
	})
}));

import { apiFetchEtagged, invalidatePrefix, _resetEtagCacheForTest } from './etagCache';

function jsonResponse(status: number, body: unknown = {}, etag?: string): Response {
	const headers: Record<string, string> = { 'content-type': 'application/json' };
	if (etag) headers['etag'] = etag;
	return new Response(JSON.stringify(body), { status, headers });
}

function emptyResponse(status: number): Response {
	return new Response(null, { status });
}

describe('apiFetchEtagged', () => {
	beforeEach(() => {
		_resetEtagCacheForTest();
		scripted.queue.length = 0;
		fetchCalls.list.length = 0;
	});
	afterEach(() => {
		scripted.queue.length = 0;
		fetchCalls.list.length = 0;
	});

	it('caches the body on 200 and returns `from: network`', async () => {
		scripted.queue.push(jsonResponse(200, { items: [1, 2] }, 'W/"abc"'));

		const r = await apiFetchEtagged<{ items: number[] }>('/api/photos/resources?p=1');

		expect(r.body).toEqual({ items: [1, 2] });
		expect(r.from).toBe('network');
	});

	it('sends If-None-Match on the second fetch and returns cached body on 304', async () => {
		scripted.queue.push(jsonResponse(200, { items: [1, 2] }, 'W/"abc"'));
		scripted.queue.push(emptyResponse(304));

		await apiFetchEtagged('/api/photos/resources?p=1');
		const r = await apiFetchEtagged<{ items: number[] }>('/api/photos/resources?p=1');

		expect(r.body).toEqual({ items: [1, 2] });
		expect(r.from).toBe('revalidated');

		// Second call must carry If-None-Match with the stored ETag.
		const secondInit = fetchCalls.list[1][1];
		const headers = new Headers(secondInit?.headers);
		expect(headers.get('If-None-Match')).toBe('W/"abc"');
	});

	it('replaces the cached body when the server returns a fresh 200 with a new ETag', async () => {
		scripted.queue.push(jsonResponse(200, { v: 1 }, 'W/"v1"'));
		scripted.queue.push(jsonResponse(200, { v: 2 }, 'W/"v2"'));
		scripted.queue.push(emptyResponse(304));

		await apiFetchEtagged('/api/photos/resources?p=1');
		const second = await apiFetchEtagged<{ v: number }>('/api/photos/resources?p=1');
		const third = await apiFetchEtagged<{ v: number }>('/api/photos/resources?p=1');

		expect(second.body).toEqual({ v: 2 });
		expect(second.from).toBe('network');
		// Third 304 reuses the fresh (v=2) body, not the stale v=1 one.
		expect(third.body).toEqual({ v: 2 });
		expect(third.from).toBe('revalidated');
	});

	it('skips If-None-Match on first fetch (no prior cached entry)', async () => {
		scripted.queue.push(jsonResponse(200, {}, 'W/"abc"'));

		await apiFetchEtagged('/api/photos/resources?p=1');

		const headers = new Headers(fetchCalls.list[0][1]?.headers);
		expect(headers.get('If-None-Match')).toBeNull();
	});

	it('does not cache responses that lack an ETag header', async () => {
		scripted.queue.push(jsonResponse(200, { v: 1 } /* no etag */));
		scripted.queue.push(jsonResponse(200, { v: 2 }));

		await apiFetchEtagged('/api/photos/resources?p=1');
		const second = await apiFetchEtagged('/api/photos/resources?p=1');

		// Second call sends no If-None-Match because the first wasn't
		// cached (no ETag to key off).
		const headers = new Headers(fetchCalls.list[1][1]?.headers);
		expect(headers.get('If-None-Match')).toBeNull();
		expect(second.from).toBe('network');
	});

	it('throws on non-ok, non-304 responses with .status attached', async () => {
		scripted.queue.push(emptyResponse(500));

		let caught: unknown;
		try {
			await apiFetchEtagged('/api/photos/resources?p=1');
		} catch (e) {
			caught = e;
		}
		expect(caught).toBeInstanceOf(Error);
		const err = caught as Error & { status?: number };
		expect(err.status).toBe(500);
		expect(err.message).toMatch(/500/);
	});

	it('isolates cache entries by full URL (filter combos do not collide)', async () => {
		scripted.queue.push(jsonResponse(200, { kind: 'photo' }, 'W/"P"'));
		scripted.queue.push(jsonResponse(200, { kind: 'video' }, 'W/"V"'));

		const a = await apiFetchEtagged('/api/photos/resources?kind=photo');
		const b = await apiFetchEtagged('/api/photos/resources?kind=video');

		expect(a.body).toEqual({ kind: 'photo' });
		expect(b.body).toEqual({ kind: 'video' });
		expect(fetchCalls.list).toHaveLength(2);
	});
});

describe('invalidatePrefix', () => {
	beforeEach(() => {
		_resetEtagCacheForTest();
		scripted.queue.length = 0;
		fetchCalls.list.length = 0;
	});
	afterEach(() => {
		scripted.queue.length = 0;
		fetchCalls.list.length = 0;
	});

	it('drops every entry under the prefix and leaves the rest intact', async () => {
		scripted.queue.push(jsonResponse(200, { photos: 1 }, 'W/"P"'));
		scripted.queue.push(jsonResponse(200, { favs: 1 }, 'W/"F"'));
		// after invalidation photos is refetched; favorites still revalidates
		scripted.queue.push(jsonResponse(200, { photos: 2 }, 'W/"P2"'));
		scripted.queue.push(emptyResponse(304));

		await apiFetchEtagged('/api/photos/resources?p=1');
		await apiFetchEtagged('/api/favorites/resources?p=1');

		invalidatePrefix('/api/photos/resources');

		// Photos refetches fresh.
		const after = await apiFetchEtagged<{ photos: number }>('/api/photos/resources?p=1');
		expect(after.from).toBe('network');
		expect(after.body).toEqual({ photos: 2 });

		// Favorites still has its cached ETag and reuses it.
		const fav = await apiFetchEtagged('/api/favorites/resources?p=1');
		expect(fav.from).toBe('revalidated');
	});
});
