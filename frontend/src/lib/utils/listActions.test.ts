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

import { openParentContextAction, parentFolderIdOf } from './listActions';
import type { FileItem, FolderItem } from '$lib/api/types';

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
