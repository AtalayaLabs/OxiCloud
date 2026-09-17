import { describe, it, expect, vi, beforeEach } from 'vitest';

vi.mock('$lib/api/client', () => ({
	apiFetch: vi.fn(),
	apiJson: vi.fn(),
	withBase: (p: string) => p
}));

import { apiFetch, apiJson } from '$lib/api/client';
import type { FolderItem } from '$lib/api/types';
import {
	fetchFolderListing,
	getCachedFolder,
	cacheFolder,
	invalidateFolderCache,
	getFolder,
	getFolderName,
	rememberFolderName,
	type FolderListing
} from './folders';

type ResourceItem = { resource_type: 'file' | 'folder'; resource: { id: string; name?: string } };
type ResourcePage = { items?: ResourceItem[]; next_cursor?: string };

function fakeRes(opts: { status: number; body?: ResourcePage }): Response {
	return {
		status: opts.status,
		ok: opts.status >= 200 && opts.status < 300,
		json: async () => opts.body ?? {},
		headers: { get: () => null }
	} as unknown as Response;
}

const emptyListing = (): FolderListing => ({
	folders: [],
	files: []
});

beforeEach(() => {
	vi.clearAllMocks();
	invalidateFolderCache();
});

describe('fetchFolderListing (cursor-paginated /resources)', () => {
	it('splits one page of resources into folders + files', async () => {
		vi.mocked(apiFetch).mockResolvedValue(
			fakeRes({
				status: 200,
				body: {
					items: [
						{ resource_type: 'folder', resource: { id: 'd1', name: 'Docs' } },
						{ resource_type: 'file', resource: { id: 'x1', name: 'a.txt' } }
					]
				}
			})
		);
		const r = await fetchFolderListing('f1');
		expect(r.status).toBe(200);
		expect(r.listing?.folders.map((f) => f.id)).toEqual(['d1']);
		expect(r.listing?.files.map((f) => f.id)).toEqual(['x1']);
		expect(vi.mocked(apiFetch).mock.calls[0][0]).toContain('/api/folders/f1/resources');
	});

	it('follows next_cursor across pages', async () => {
		vi.mocked(apiFetch)
			.mockResolvedValueOnce(
				fakeRes({
					status: 200,
					body: { items: [{ resource_type: 'file', resource: { id: 'p1' } }], next_cursor: 'c2' }
				})
			)
			.mockResolvedValueOnce(
				fakeRes({
					status: 200,
					body: { items: [{ resource_type: 'file', resource: { id: 'p2' } }] }
				})
			);
		const r = await fetchFolderListing('f1');
		expect(r.listing?.files.map((f) => f.id)).toEqual(['p1', 'p2']);
		expect(vi.mocked(apiFetch)).toHaveBeenCalledTimes(2);
		expect(vi.mocked(apiFetch).mock.calls[1][0]).toContain('cursor=c2');
	});

	it('throws a 403 carrying its status', async () => {
		vi.mocked(apiFetch).mockResolvedValue(fakeRes({ status: 403 }));
		await expect(fetchFolderListing('f1')).rejects.toMatchObject({ status: 403 });
	});
});

describe('folder listing cache (LRU + invalidation)', () => {
	it('stores and retrieves a listing + its ETag', () => {
		cacheFolder('a', emptyListing(), '"1"');
		expect(getCachedFolder('a')?.etag).toBe('"1"');
		expect(getCachedFolder('missing')).toBeUndefined();
	});

	it('evicts the least-recently-used entry past the cap', () => {
		for (let i = 0; i < 45; i++) cacheFolder(`f${i}`, emptyListing());
		expect(getCachedFolder('f0')).toBeUndefined(); // evicted (cap is 40)
		expect(getCachedFolder('f44')).toBeDefined();
	});

	it('a read bumps recency so the touched entry survives eviction', () => {
		for (let i = 0; i < 40; i++) cacheFolder(`f${i}`, emptyListing());
		getCachedFolder('f0'); // bump f0 to most-recent
		cacheFolder('extra', emptyListing()); // forces one eviction
		expect(getCachedFolder('f0')).toBeDefined();
		expect(getCachedFolder('f1')).toBeUndefined(); // f1 was now the oldest
	});

	it('invalidates a single folder, or the whole cache', () => {
		cacheFolder('a', emptyListing());
		cacheFolder('b', emptyListing());
		invalidateFolderCache('a');
		expect(getCachedFolder('a')).toBeUndefined();
		expect(getCachedFolder('b')).toBeDefined();
		invalidateFolderCache();
		expect(getCachedFolder('b')).toBeUndefined();
	});
});

describe('folder name cache (breadcrumbs)', () => {
	const folder = (id: string, name: string): FolderItem => ({ id, name }) as unknown as FolderItem;

	it("learns its children's names from a cached listing", () => {
		cacheFolder('nc-parent', {
			folders: [folder('nc-a', 'Alpha'), folder('nc-b', 'Beta')],
			files: []
		});
		expect(getFolderName('nc-a')).toBe('Alpha');
		expect(getFolderName('nc-b')).toBe('Beta');
		expect(getFolderName('nc-unknown')).toBeUndefined();
	});

	it('records the name fetched by getFolder', async () => {
		vi.mocked(apiJson).mockResolvedValue(folder('gf-1', 'Reports') as never);
		const f = await getFolder('gf-1');
		expect(f.name).toBe('Reports');
		expect(getFolderName('gf-1')).toBe('Reports');
	});

	it('rememberFolderName overwrites a stale name (e.g. after a rename)', () => {
		rememberFolderName('rn-1', 'Old');
		expect(getFolderName('rn-1')).toBe('Old');
		rememberFolderName('rn-1', 'New');
		expect(getFolderName('rn-1')).toBe('New');
	});
});
