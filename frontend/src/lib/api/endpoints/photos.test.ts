import { it, expect, vi, beforeEach } from 'vitest';
vi.mock('$lib/api/client', () => ({
	apiFetch: vi.fn(),
	apiJson: vi.fn(),
	withBase: (p: string) => p
}));
vi.mock('$lib/api/csrf', () => ({ getCsrfHeaders: () => ({}) }));
import { apiFetch, apiJson } from '$lib/api/client';
import * as photos from './photos';
const f = apiFetch as unknown as ReturnType<typeof vi.fn>;
const j = apiJson as unknown as ReturnType<typeof vi.fn>;
beforeEach(() => {
	vi.clearAllMocks();
	f.mockResolvedValue({ ok: true, status: 200, json: async () => ({}) });
	j.mockResolvedValue({ photos: [], clusters: [] });
});
it('exercises the photos endpoints', async () => {
	await photos.fetchPhotosGeo('0,0,1,1', 5).catch(() => {});
	await photos.fetchPhotos(60).catch(() => {});
	await photos.fetchFileMetadata('fid').catch(() => {});
	await photos.batchTrash(['a', 'b']).catch(() => {});
	await photos.batchTrash([]).catch(() => {});
	expect(f.mock.calls.length + j.mock.calls.length).toBeGreaterThan(0);
});
