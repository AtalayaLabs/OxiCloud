import { it, expect, vi, beforeEach } from 'vitest';

// `folderAccessCached` is the three-state probe cache: true / false /
// undefined. Only an explicit `false` may grey the entry — an UNPROBED row
// must stay enabled, or every first right-click shows a disabled action.
const access = vi.hoisted(() => ({ answer: undefined as boolean | undefined }));
vi.mock('$lib/utils/folderAccess', () => ({
	folderAccessCached: () => access.answer,
	probeFolderAccess: vi.fn(async () => access.answer)
}));
vi.mock('$app/navigation', () => ({ goto: vi.fn() }));
// `base` as well as `resolve`: something in the import graph reads it, and a
// partial mock of `$app/paths` fails at import time rather than in a test.
vi.mock('$app/paths', () => ({ resolve: (p: string) => p, base: '', assets: '' }));
vi.mock('$lib/api/endpoints/favorites', () => ({
	addFavorite: vi.fn(),
	removeFavorite: vi.fn()
}));
vi.mock('$lib/api/endpoints/files', () => ({ fileDownloadUrl: (id: string) => `/dl/${id}` }));
vi.mock('$lib/api/endpoints/folders', () => ({ folderZipUrl: (id: string) => `/zip/${id}` }));

import { addFavorite } from '$lib/api/endpoints/favorites';
import {
	applyRowFlags,
	emitRowFlags,
	onRowFlags,
	openParentContextAction,
	parentFolderIdOf,
	toggleFavorite,
	type RowFlagsChange
} from './listActions';
import type { FileItem, FolderItem } from '$lib/api/types';

const m = (fn: unknown) => fn as ReturnType<typeof vi.fn>;

// `isFile` discriminates on `'mime_type' in item` — NOT on a `type` field.
// A fixture without one is treated as a folder, which is how the first
// version of this test "failed" against correct code.
const file = (folderId: string | null) =>
	({
		id: 'f1',
		name: 'notes.txt',
		folder_id: folderId,
		mime_type: 'text/plain'
	}) as unknown as FileItem;

const folder = (parentId: string | null) =>
	({ id: 'd1', name: 'Docs', parent_id: parentId }) as unknown as FolderItem;

beforeEach(() => {
	access.answer = undefined;
});

// The shape every listing shares: a file carries `folder_id`, a folder carries
// `parent_id`, and a drive-root folder carries neither. Collapsing both to one
// `null` is what lets the action have a single "nothing to open" test.
it('reads the parent from folder_id on files and parent_id on folders', () => {
	expect(parentFolderIdOf(file('p1'))).toBe('p1');
	expect(parentFolderIdOf(folder('p2'))).toBe('p2');
	expect(parentFolderIdOf(folder(null))).toBeNull();
});

// VISIBLE whenever there is a parent to open — for both kinds. This is the
// half that must not regress into "hide when unreachable": on a page with no
// path column, hiding it leaves no route to where the item lives.
it('offers open-parent on files AND folders that have a parent', () => {
	const a = openParentContextAction();
	expect(a.visible?.(file('p1'))).toBe(true);
	expect(a.visible?.(folder('p2'))).toBe(true);
});

// Hidden ONLY for a drive-root folder, where there is literally nothing above
// it. Not a permission decision — a structural one.
it('hides open-parent only for a drive-root folder', () => {
	const a = openParentContextAction();
	expect(a.visible?.(folder(null))).toBe(false);
});

// Unreachable -> greyed, for both kinds. `/favorites` established this and
// the other four pages now inherit it from here.
it('greys open-parent when the parent is known unreadable', () => {
	access.answer = false;
	const a = openParentContextAction();
	expect(a.disabled?.(file('p1'))).toBe(true);
	expect(a.disabled?.(folder('p2'))).toBe(true);
});

// Reachable, and — the subtle one — UNPROBED. `undefined` must not grey, or
// the entry is disabled on the first right-click of every fresh row, before
// `menuPrepare` has answered.
it('leaves open-parent enabled when reachable or not yet probed', () => {
	const a = openParentContextAction();
	access.answer = true;
	expect(a.disabled?.(file('p1'))).toBe(false);
	expect(a.disabled?.(folder('p2'))).toBe(false);
	access.answer = undefined;
	expect(a.disabled?.(file('p1'))).toBe(false);
	expect(a.disabled?.(folder('p2'))).toBe(false);
});

// ── The broadcast ────────────────────────────────────────────────────────
//
// THE bug this exists for: `toggleFavorite` used to only mutate the item it
// was handed, which works where that object IS the page's reactive state and
// silently does nothing where rows are a projection of it. `/changes` maps
// `raw` into `ChangeRow`s, so the star flipped a throwaway copy — the click
// looked dead while the server had recorded the change. These pin the event
// that carries the flip to whoever owns the data.
it('announces the flip so a page with derived rows can follow it', async () => {
	const seen: RowFlagsChange[] = [];
	const off = onRowFlags((c) => seen.push(c));

	const it1 = file('p1');
	it1.is_favorite = false;
	await toggleFavorite(it1);

	expect(seen).toEqual([{ id: 'f1', is_favorite: true }]);
	// Still mutated in place too, for the pages where that is the source of
	// truth — the event is additive, not a replacement.
	expect(it1.is_favorite).toBe(true);
	off();
});

// A failed write must un-announce, or every other surface keeps a star the
// server rejected — the exact split-brain the optimistic flip is meant to
// avoid.
it('announces the revert when the write fails', async () => {
	m(addFavorite).mockRejectedValueOnce(new Error('nope'));
	const seen: RowFlagsChange[] = [];
	const off = onRowFlags((c) => seen.push(c));

	const it1 = file('p1');
	it1.is_favorite = false;
	await toggleFavorite(it1);

	expect(seen).toEqual([
		{ id: 'f1', is_favorite: true },
		{ id: 'f1', is_favorite: false }
	]);
	expect(it1.is_favorite).toBe(false);
	off();
});

// Unsubscribing has to actually detach: these listeners live in `$effect`s,
// so a leak means every navigation adds another patcher of a dead page.
it('stops delivering after the unsubscriber runs', () => {
	const seen: RowFlagsChange[] = [];
	onRowFlags((c) => seen.push(c))();
	emitRowFlags({ id: 'f1', is_favorite: true });
	expect(seen).toEqual([]);
});

// ── The shared chip rides the same event ─────────────────────────────────
//
// THE bug: sharing or unsharing left the row's chip as it was until a page
// refresh. The star had this broadcast; sharing had a `ShareDialog` callback
// that only `/files` wired, and only for the create direction — so
// `/favorites`, `/recent`, `/search`, `/shared-with-me` and `/changes` painted
// a stale chip both ways. One event now carries both flags, which is why
// these pins live beside the favourite ones rather than in a parallel file.
it('carries is_shared on its own, without claiming anything about the star', () => {
	const seen: RowFlagsChange[] = [];
	const off = onRowFlags((c) => seen.push(c));

	emitRowFlags({ id: 'f1', is_shared: true });

	expect(seen).toEqual([{ id: 'f1', is_shared: true }]);
	// The absent flag must stay absent: a listener that treated `undefined`
	// as `false` would un-star every row the share dialog touched.
	expect(seen[0].is_favorite).toBeUndefined();
	off();
});

// `applyRowFlags` is the rule every listener shares. An absent flag leaves
// the row alone — this is the half that stops a share announcement from
// clearing a star.
it('applies only the flags the change carries', () => {
	const row = { is_favorite: true, is_shared: false };

	applyRowFlags(row, { id: 'f1', is_shared: true });

	expect(row).toEqual({ is_favorite: true, is_shared: true });
});

// And a row that does not model a flag must not grow one: a breadcrumb crumb
// carries `is_shared` and has no star, so the favourite half is not its
// business. Without the `in` guard it would gain an `is_favorite` property
// that nothing renders and the next chain fetch silently drops.
it('leaves a row without a flag alone rather than adding it', () => {
	const crumb = { is_shared: false };

	applyRowFlags(crumb, { id: 'd1', is_favorite: true, is_shared: true });

	expect(crumb).toEqual({ is_shared: true });
	expect('is_favorite' in crumb).toBe(false);
});
