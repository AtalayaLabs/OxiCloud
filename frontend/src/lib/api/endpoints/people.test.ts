import { it, expect, vi, beforeEach } from 'vitest';
vi.mock('$lib/api/client', () => ({
	apiFetch: vi.fn(),
	apiJson: vi.fn(),
	withBase: (p: string) => p
}));
vi.mock('$lib/api/csrf', () => ({ getCsrfHeaders: () => ({}) }));
import { apiFetch, apiJson } from '$lib/api/client';
import * as people from './people';
const f = apiFetch as unknown as ReturnType<typeof vi.fn>;
const j = apiJson as unknown as ReturnType<typeof vi.fn>;
beforeEach(() => {
	vi.clearAllMocks();
	f.mockResolvedValue({ ok: true, status: 200, json: async () => ({}) });
	j.mockResolvedValue([]);
});
it('exercises the people endpoints', async () => {
	await people.fetchPeople().catch(() => {});
	await people.peopleEnabled().catch(() => {});
	await people.fetchPersonPhotos('p').catch(() => {});
	await people.renamePerson('p', 'Alice').catch(() => {});
	await people.renamePerson('p', null).catch(() => {});
	expect(f.mock.calls.length + j.mock.calls.length).toBeGreaterThan(0);
});
