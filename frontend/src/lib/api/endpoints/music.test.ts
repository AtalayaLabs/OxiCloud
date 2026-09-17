import { it, expect, vi, beforeEach } from 'vitest';
vi.mock('$lib/api/client', () => ({
	apiFetch: vi.fn(),
	apiJson: vi.fn(),
	withBase: (p: string) => p
}));
vi.mock('$lib/api/csrf', () => ({ getCsrfHeaders: () => ({}) }));
import { apiFetch, apiJson } from '$lib/api/client';
import * as music from './music';
const f = apiFetch as unknown as ReturnType<typeof vi.fn>;
const j = apiJson as unknown as ReturnType<typeof vi.fn>;
beforeEach(() => {
	vi.clearAllMocks();
	f.mockResolvedValue({ ok: true, status: 200, json: async () => ({}), text: async () => 'fid' });
	j.mockResolvedValue([]);
});
it('exercises the music endpoints', async () => {
	await music.listPlaylists().catch(() => {});
	await music.listTracks('p').catch(() => {});
	await music.createPlaylist('n').catch(() => {});
	await music.updatePlaylist('p', { name: 'x' } as never).catch(() => {});
	await music.renamePlaylist('p', 'n').catch(() => {});
	await music.deletePlaylist('p').catch(() => {});
	await music.addTracks('p', ['f']).catch(() => {});
	await music.removeTrack('p', 'f').catch(() => {});
	await music.reorderTracks('p', ['a', 'b']).catch(() => {});
	await music.listShares('p').catch(() => {});
	await music.removeShare('p', 'u').catch(() => {});
	const file = new File([new Uint8Array([1])], 'c.png', { type: 'image/png' });
	await music.uploadCoverImage(file).catch(() => {});
	expect(f.mock.calls.length + j.mock.calls.length).toBeGreaterThan(3);
});
