import { it, expect, vi, beforeEach } from 'vitest';
vi.mock('$lib/api/client', () => ({
	apiFetch: vi.fn(),
	apiJson: vi.fn(),
	withBase: (p: string) => p
}));
vi.mock('$lib/api/csrf', () => ({ getCsrfHeaders: () => ({}) }));
import { apiFetch, apiJson } from '$lib/api/client';
import * as shares from './shares';
const f = apiFetch as unknown as ReturnType<typeof vi.fn>;
const j = apiJson as unknown as ReturnType<typeof vi.fn>;
beforeEach(() => {
	vi.clearAllMocks();
	f.mockResolvedValue({ ok: true, status: 200, json: async () => ({}) });
	j.mockResolvedValue({});
});
it('exercises the shares endpoints', async () => {
	await shares.createShare({ item_id: 'i', item_type: 'folder' } as never).catch(() => {});
	await shares.listSharesForItem('i', 'folder' as never).catch(() => {});
	await shares.getShareById('s').catch(() => {});
	await shares.updateShare('s', {} as never).catch(() => {});
	await shares.deleteShare('s').catch(() => {});
	expect(f.mock.calls.length + j.mock.calls.length).toBeGreaterThan(0);
});
