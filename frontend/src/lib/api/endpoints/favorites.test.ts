import { describe, it, expect, vi, beforeEach } from 'vitest';

vi.mock('$lib/api/client', () => ({
	apiFetch: vi.fn(),
	apiJson: vi.fn(),
	withBase: (p: string) => p
}));
vi.mock('$lib/api/csrf', () => ({ getCsrfHeaders: () => ({}) }));

import { apiFetch, apiJson } from '$lib/api/client';
import { addFavorite, removeFavorite } from './favorites';

const fetchMock = apiFetch as unknown as ReturnType<typeof vi.fn>;
const jsonMock = apiJson as unknown as ReturnType<typeof vi.fn>;

// The `dateBucket` / `sizeBucket` / `typeLabel` tests moved to
// `$lib/utils/resourceBuckets.test.ts` when those helpers were
// relocated out of this module. See that file for coverage.

describe('favorites mutations', () => {
	beforeEach(() => {
		vi.clearAllMocks();
		fetchMock.mockResolvedValue({ ok: true, status: 200, json: async () => ({}) });
		jsonMock.mockResolvedValue({});
	});
	it('addFavorite / removeFavorite call the API', async () => {
		await addFavorite('file', 'id1');
		await removeFavorite('file', 'id1');
		expect(fetchMock).toHaveBeenCalled();
	});
});
