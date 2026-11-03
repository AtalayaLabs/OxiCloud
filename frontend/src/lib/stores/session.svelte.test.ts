import { beforeEach, describe, expect, it, vi } from 'vitest';
import type { SelfUser } from '$lib/api/types';

// `vi.mock` is hoisted above imports, so the spy it references must be created
// with `vi.hoisted` (a plain top-level const isn't initialised yet when the
// factory runs).
const { fetchMeMock, listDrivesMock } = vi.hoisted(() => ({
	fetchMeMock: vi.fn(),
	listDrivesMock: vi.fn()
}));

vi.mock('$lib/api/endpoints/drives', () => ({ listDrives: () => listDrivesMock() }));

vi.mock('$lib/api/endpoints/auth', () => ({
	fetchMe: () => fetchMeMock(),
	tryRefresh: vi.fn(async () => false),
	bindDpopIfPossible: vi.fn(async () => false)
}));

import { session } from './session.svelte';
import { drives } from './drives.svelte';

// `storage_used_bytes` moved to `FullUser` (embedded inside `SelfUser`)
// as part of the three-layer UserDto refactor
// (`docs/plan/userdto-refactor.md`). Build a minimal SelfUser shape that
// satisfies the type checker without hand-populating every field the
// production shape carries — the test only cares about the usage read
// path (`session.me.full.storage_used_bytes`).
const userWithUsage = (used: number) =>
	({ full: { storage_used_bytes: used } }) as unknown as SelfUser;

describe('session.refresh', () => {
	beforeEach(() => {
		fetchMeMock.mockReset();
		session.reset();
	});

	it('pulls the fresh storage usage into the reactive user (upload/delete sync)', async () => {
		fetchMeMock.mockResolvedValue(userWithUsage(2048));
		await session.refresh();
		expect(session.me?.full.storage_used_bytes).toBe(2048);
	});

	it('leaves the current user intact when the probe returns null', async () => {
		fetchMeMock.mockResolvedValue(userWithUsage(2048));
		await session.refresh();
		fetchMeMock.mockResolvedValue(null);
		await session.refresh();
		expect(session.me?.full.storage_used_bytes).toBe(2048);
	});

	it('leaves the current user intact when the probe throws', async () => {
		fetchMeMock.mockResolvedValue(userWithUsage(2048));
		await session.refresh();
		fetchMeMock.mockRejectedValue(new Error('network'));
		await session.refresh();
		expect(session.me?.full.storage_used_bytes).toBe(2048);
	});
});

// Regression: logout → login as another user in the same tab resolved the
// new user's home from the previous user's cached drives (404 on /files).
// `session.reset()` never cleared the drives store. Store layer.
describe('session.reset', () => {
	it('drops the cached drives so the next user does not inherit them', () => {
		drives.drives = [{ id: 'd1', root_folder_id: 'r1' } as never];
		drives.loaded = true;
		session.reset();
		expect(drives.drives).toEqual([]);
		expect(drives.loaded).toBe(false);
		expect(drives.findDefault()).toBeNull();
	});

	it('ignores a drives fetch that settles after the reset', async () => {
		let resolve!: (v: never[]) => void;
		listDrivesMock.mockReturnValue(new Promise<never[]>((r) => (resolve = r)));
		const pending = drives.load();
		session.reset();
		resolve([{ id: 'old', root_folder_id: 'r-old' } as never]);
		await pending;
		expect(drives.drives).toEqual([]);
		expect(drives.loaded).toBe(false);
	});
});
