import { it, expect, vi, beforeEach } from 'vitest';
vi.mock('$lib/api/client', () => ({ apiFetch: vi.fn(), apiJson: vi.fn() }));
vi.mock('$lib/api/csrf', () => ({ getCsrfHeaders: () => ({}) }));
import { apiFetch, apiJson } from '$lib/api/client';
import { searchResources, searchSuggest, clearSearchCache } from './search';
const f = apiFetch as unknown as ReturnType<typeof vi.fn>;
const j = apiJson as unknown as ReturnType<typeof vi.fn>;
beforeEach(() => {
	vi.clearAllMocks();
	f.mockResolvedValue({ ok: true, status: 200, json: async () => ({}) });
	j.mockResolvedValue({ items: [], query_time_ms: 0 });
});
it('builds search requests including filters', async () => {
	await searchResources('q', {
		recursive: true,
		fileTypes: ['mp3', 'wav'],
		minSize: 1,
		maxSize: 9,
		sortBy: 'date'
	}).catch(() => {});
	expect(j).toHaveBeenCalledWith(expect.stringContaining('type=mp3%2Cwav'), expect.anything());
	// Sort dimension is sent on the wire as `order_by`, matching the
	// backend's `SearchResourcesQuery` (post-normalization).
	expect(j).toHaveBeenCalledWith(expect.stringContaining('order_by=date'), expect.anything());
	await searchSuggest('q').catch(() => {});
	await clearSearchCache().catch(() => {});
	expect(f.mock.calls.length + j.mock.calls.length).toBeGreaterThan(1);
});

it('forwards cursor pagination and resource-type filter', async () => {
	await searchResources('q', {
		cursor: 'abc',
		resourceTypes: ['file'],
		limit: 25
	}).catch(() => {});
	const call = j.mock.calls.at(-1)?.[0] as string;
	expect(call).toContain('cursor=abc');
	expect(call).toContain('resource_types=file');
	expect(call).toContain('limit=25');
});
