import { beforeEach, describe, expect, it, vi } from 'vitest';
import type { User } from '$lib/api/types';

// `vi.mock` is hoisted above imports, so the spy it references must be created
// with `vi.hoisted` (a plain top-level const isn't initialised yet when the
// factory runs).
const { fetchMeMock } = vi.hoisted(() => ({ fetchMeMock: vi.fn() }));

vi.mock('$lib/api/endpoints/auth', () => ({
	fetchMe: () => fetchMeMock(),
	tryRefresh: vi.fn(async () => false),
	bindDpopIfPossible: vi.fn(async () => false)
}));

import { session } from './session.svelte';

const userWithUsage = (used: number) => ({ storage_used_bytes: used }) as unknown as User;

describe('session.refresh', () => {
	beforeEach(() => {
		fetchMeMock.mockReset();
		session.reset();
	});

	it('pulls the fresh storage usage into the reactive user (upload/delete sync)', async () => {
		fetchMeMock.mockResolvedValue(userWithUsage(2048));
		await session.refresh();
		expect(session.user?.storage_used_bytes).toBe(2048);
	});

	it('leaves the current user intact when the probe returns null', async () => {
		fetchMeMock.mockResolvedValue(userWithUsage(2048));
		await session.refresh();
		fetchMeMock.mockResolvedValue(null);
		await session.refresh();
		expect(session.user?.storage_used_bytes).toBe(2048);
	});

	it('leaves the current user intact when the probe throws', async () => {
		fetchMeMock.mockResolvedValue(userWithUsage(2048));
		await session.refresh();
		fetchMeMock.mockRejectedValue(new Error('network'));
		await session.refresh();
		expect(session.user?.storage_used_bytes).toBe(2048);
	});
});
