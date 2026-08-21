import { beforeEach, describe, expect, it, vi } from 'vitest';
import type { SelfUser } from '$lib/api/types';

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
