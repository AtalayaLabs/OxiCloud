import { describe, expect, it, vi, beforeEach } from 'vitest';

vi.mock('$lib/api/client', () => ({
	apiFetch: vi.fn(),
	apiJson: vi.fn(),
	withBase: (p: string) => p
}));

import { apiJson } from '$lib/api/client';
import type { FolderItem } from '$lib/api/types';
import { getFolder } from './folders';

/**
 * Benchmark gate for the in-flight dedup in {@link getFolder}.
 *
 * Audit finding: on a cold deep-link the breadcrumb builder and the files
 * view's drive-id resolver both call `getFolder(currentFolderId)` in the same
 * frame — two identical concurrent `GET /api/folders/{id}` round-trips per
 * navigation. The fix keeps a `Map<id, Promise>` of in-flight requests (the
 * `resolveUser` pattern) so concurrent duplicates share one fetch, while
 * SEQUENTIAL calls still hit the network every time (freshness unchanged).
 *
 * Gates:
 *  1. Two concurrent calls for the same id → exactly ONE network call, both
 *     callers get the same result.
 *  2. Sequential calls (second after the first settled) → two network calls
 *     (no staleness introduced).
 *  3. Distinct ids in flight do not cross-talk.
 */

const mockedApiJson = vi.mocked(apiJson);

function folder(id: string): FolderItem {
	return { id, name: `Folder ${id}` } as unknown as FolderItem;
}

beforeEach(() => {
	mockedApiJson.mockReset();
});

describe('getFolder in-flight dedup (benchmark gate)', () => {
	it('concurrent duplicate calls collapse to one request', async () => {
		let release!: (v: FolderItem) => void;
		mockedApiJson.mockImplementation(
			() => new Promise<FolderItem>((r) => (release = r)) as Promise<never>
		);

		const a = getFolder('f1');
		const b = getFolder('f1');
		expect(mockedApiJson).toHaveBeenCalledTimes(1); // the dedup win

		release(folder('f1'));
		const [ra, rb] = await Promise.all([a, b]);
		expect(ra).toEqual(rb);
		expect(ra.id).toBe('f1');
		console.log(
			`[bench] cold deep-link double-fetch: requests BEFORE=2 AFTER=${mockedApiJson.mock.calls.length}`
		);
	});

	it('sequential calls still refetch (freshness preserved)', async () => {
		mockedApiJson.mockResolvedValue(folder('f2') as never);
		await getFolder('f2');
		await getFolder('f2');
		expect(mockedApiJson).toHaveBeenCalledTimes(2);
	});

	it('distinct ids resolve independently', async () => {
		mockedApiJson.mockImplementation(((url: string) => {
			const id = String(url).split('/').pop() ?? '';
			return Promise.resolve(folder(id));
		}) as never);
		const [x, y] = await Promise.all([getFolder('fx'), getFolder('fy')]);
		expect(x.id).toBe('fx');
		expect(y.id).toBe('fy');
		expect(mockedApiJson).toHaveBeenCalledTimes(2);
	});

	it('a failed in-flight request clears the slot so a retry refetches', async () => {
		mockedApiJson.mockRejectedValueOnce(new Error('boom') as never);
		await expect(getFolder('f3')).rejects.toThrow('boom');
		mockedApiJson.mockResolvedValue(folder('f3') as never);
		await expect(getFolder('f3')).resolves.toMatchObject({ id: 'f3' });
	});
});
