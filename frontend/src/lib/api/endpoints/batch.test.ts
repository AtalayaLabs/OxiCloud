import { describe, it, expect, vi, beforeEach } from 'vitest';

// `apiFetch` is stubbed, but `ApiError` is kept REAL — the assertions below
// are about the error object the module constructs, so mocking it would test
// the mock.
vi.mock('$lib/api/client', async (importOriginal) => ({
	...(await importOriginal<typeof import('$lib/api/client')>()),
	apiFetch: vi.fn()
}));
vi.mock('$lib/api/csrf', () => ({ getCsrfHeaders: () => ({}) }));

import { apiFetch } from '$lib/api/client';
import { copyFiles, copyFolders } from './batch';

const fetchMock = apiFetch as unknown as ReturnType<typeof vi.fn>;

describe('batch copy', () => {
	beforeEach(() => {
		vi.clearAllMocks();
		fetchMock.mockResolvedValue({ ok: true, status: 200, json: async () => ({}) });
	});

	it('short-circuits on empty input', async () => {
		await copyFiles([], null);
		await copyFolders([], 'x');
		expect(fetchMock).not.toHaveBeenCalled();
	});

	it('posts copy requests for files and folders', async () => {
		await copyFiles(['a'], 't');
		await copyFolders(['b'], null);
		expect(fetchMock).toHaveBeenCalledTimes(2);
		expect(fetchMock).toHaveBeenCalledWith(
			'/api/batch/files/copy',
			expect.objectContaining({ method: 'POST' })
		);
	});

	it('throws the server error/message on failure', async () => {
		fetchMock.mockResolvedValue({ ok: false, status: 400, json: async () => ({ error: 'bad' }) });
		await expect(copyFiles(['a'], 't')).rejects.toThrow('bad');
		fetchMock.mockResolvedValue({ ok: false, status: 500, json: async () => ({}) });
		await expect(copyFolders(['b'], 't')).rejects.toThrow(/500/);
	});

	// A failed batch answers in its own shape — `{ successful, failed[], stats }`
	// — not the `{ error }` the rest of the API uses. Reading only `error`
	// always missed, so every batch failure read as "…/copy failed: 400"
	// regardless of cause. The `error_type` must survive onto the thrown
	// `ApiError`: that is what `errorMessage` keys the user-facing sentence on.
	it('carries the first failure and its error_type off a batch response', async () => {
		fetchMock.mockResolvedValue({
			ok: false,
			status: 507,
			statusText: 'Insufficient Storage',
			json: async () => ({
				successful: [],
				stats: { total: 1, successful: 0, failed: 1 },
				failed: [
					{
						id: 'a91ac3a7',
						error: 'Quota Exceeded: Drive quota exceeded: 419764746 + 207929028 > 536870912 bytes',
						error_type: 'Quota Exceeded'
					}
				]
			})
		});
		await expect(copyFiles(['a91ac3a7'], 't')).rejects.toMatchObject({
			name: 'ApiError',
			status: 507,
			errorType: 'Quota Exceeded'
		});
	});
});
