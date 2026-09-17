import { it, expect, vi, beforeEach } from 'vitest';
vi.mock('$lib/api/client', () => ({
	apiFetch: vi.fn(),
	apiJson: vi.fn(),
	withBase: (p: string) => p
}));
vi.mock('$lib/api/csrf', () => ({ getCsrfHeaders: () => ({}) }));
import { apiFetch } from '$lib/api/client';
import { fetchRecentPage, clearRecent } from './recent';
const f = apiFetch as unknown as ReturnType<typeof vi.fn>;
beforeEach(() => {
	vi.clearAllMocks();
	f.mockResolvedValue({
		ok: true,
		status: 200,
		json: async () => ({ items: [], next_cursor: null })
	});
});
it('fetches and clears recent', async () => {
	await fetchRecentPage({}).catch(() => {});
	await clearRecent().catch(() => {});
	expect(f).toHaveBeenCalled();
});
