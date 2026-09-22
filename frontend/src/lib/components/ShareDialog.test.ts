import { it, expect, vi, beforeEach } from 'vitest';
import { render, screen, fireEvent, waitFor } from '@testing-library/svelte';

const { ui } = vi.hoisted(() => ({ ui: { notify: vi.fn() } }));
vi.mock('$lib/stores/ui.svelte', () => ({ ui }));
vi.mock('$lib/utils/errors', () => ({ errorToast: vi.fn() }));
vi.mock('$lib/api/endpoints/shares', () => ({
	copyShareLink: vi.fn(),
	createShare: vi.fn(),
	deleteShare: vi.fn(),
	listSharesForItem: vi.fn(),
	updateShare: vi.fn()
}));
vi.mock('$lib/api/endpoints/grants', () => ({
	createGrant: vi.fn(),
	expiryToIso: (v: string | null) => v,
	displayRole: (r: string) => r,
	fetchGrantsForResource: vi.fn(),
	notifyGrantRecipient: vi.fn(),
	revokeGrant: vi.fn(),
	todayIso: () => '2026-07-22',
	updateGrantRole: vi.fn()
}));
vi.mock('$lib/api/endpoints/recipients', () => ({
	ensureResolvers: vi.fn(),
	isDirectoryAvailable: () => true,
	resolveRecipient: (_t: string, id: string) => ({ id, label: id }),
	searchRecipients: vi.fn(async () => [])
}));
// Inherited access — the ancestor walk and the file→parent lookup.
vi.mock('$lib/api/endpoints/folders', () => ({ getFolderAncestorsWithGrants: vi.fn() }));
vi.mock('$lib/api/endpoints/files', () => ({ getFile: vi.fn() }));

import { createShare, listSharesForItem } from '$lib/api/endpoints/shares';
import { fetchGrantsForResource } from '$lib/api/endpoints/grants';
import { getFolderAncestorsWithGrants } from '$lib/api/endpoints/folders';
import ShareDialog from './ShareDialog.svelte';

const m = (fn: unknown) => fn as ReturnType<typeof vi.fn>;
const item = { id: 'f1', name: 'doc.txt', kind: 'file' as const };

/** A folder whose access all arrives from the ancestor above it. */
const folderItem = { id: 'fold1', name: 'Sub', kind: 'folder' as const };

/**
 * Ancestor walk for `fold1`: parent `p1` carries one group grant and one
 * public link, `fold1` itself carries nothing.
 *
 * The group subject is deliberate — a `user` subject renders
 * `UserVignette`, which resolves profiles of its own accord. Groups take
 * the plain icon+label branch, so these tests exercise the inheritance
 * logic without standing up an unrelated component.
 */
const chainWithInherited = {
	ancestors: [
		{ id: 'p1', name: 'Projects', parent_id: null, drive_id: 'd1' },
		{ id: 'fold1', name: 'Sub', parent_id: 'p1', drive_id: 'd1' }
	],
	access_source: { kind: 'drive', drive: { id: 'd1', name: 'Personal', kind: 'personal' } },
	effective_grants: [
		{
			id: 'g-inherited',
			subject: { type: 'group', id: 'team-a' },
			role: 'editor',
			resource: { type: 'folder', id: 'p1' }
		}
	],
	effective_links: [
		{
			grant_id: 'lg1',
			share_id: 's9',
			name: 'Team link',
			has_password: true,
			resource: { type: 'folder', id: 'p1' }
		}
	]
};

/**
 * Same walk, but the access sits on the DRIVE rather than a folder.
 *
 * Drive-anchored grants take a different branch: they cannot link into
 * `/files` (a drive has no browsable folder URL of its own), so the chip
 * points at the drive's config page, where drive membership is actually
 * managed.
 */
const chainWithDriveInherited = {
	ancestors: [{ id: 'fold1', name: 'Sub', parent_id: null, drive_id: 'd1' }],
	access_source: { kind: 'drive', drive: { id: 'd1', name: 'Marketing', kind: 'shared' } },
	effective_grants: [
		{
			id: 'g-drive',
			subject: { type: 'group', id: 'team-a' },
			role: 'editor',
			resource: { type: 'drive', id: 'd1' }
		}
	],
	effective_links: [
		{
			grant_id: 'lg-drive',
			share_id: 's10',
			name: 'Drive-wide link',
			has_password: false,
			resource: { type: 'drive', id: 'd1' }
		}
	]
};

beforeEach(() => {
	vi.clearAllMocks();
	m(fetchGrantsForResource).mockResolvedValue([]);
	m(listSharesForItem).mockResolvedValue([]);
	// Default: nothing inherited. Tests that care override it.
	m(getFolderAncestorsWithGrants).mockResolvedValue({
		ancestors: [],
		access_source: { kind: 'drive' },
		effective_grants: [],
		effective_links: []
	});
});

it('loads grants and shares when opened', async () => {
	render(ShareDialog, { props: { open: true, item } });
	await screen.findByTestId('share-dialog');
	await waitFor(() => expect(fetchGrantsForResource).toHaveBeenCalledWith('file', 'f1'));
	await waitFor(() => expect(listSharesForItem).toHaveBeenCalledWith('f1', 'file'));
});

it('switches to the link tab and creates a public link', async () => {
	m(createShare).mockResolvedValue({ id: 's1', token: 'abc', has_password: false });
	render(ShareDialog, { props: { open: true, item } });
	await fireEvent.click(await screen.findByTestId('share-dialog-link-tab'));
	await fireEvent.input(screen.getByTestId('share-dialog-link-name-input'), {
		target: { value: 'My link' }
	});
	await fireEvent.click(screen.getByTestId('share-dialog-create-btn'));
	await waitFor(() =>
		expect(createShare).toHaveBeenCalledWith(
			expect.objectContaining({ itemId: 'f1', itemName: 'My link', itemType: 'file' })
		)
	);
});

it('does not load when closed', () => {
	render(ShareDialog, { props: { open: false, item } });
	expect(fetchGrantsForResource).not.toHaveBeenCalled();
});

// ── Inherited access ───────────────────────────────────────────────────
//
// Access mostly arrives by cascade, so a dialog that lists only direct
// grants answers "who can reach this?" wrongly — it can show an empty
// list for a folder half the company can open.

it('lists a grant inherited from an ancestor, read-only, with its source', async () => {
	m(getFolderAncestorsWithGrants).mockResolvedValue(chainWithInherited);
	render(ShareDialog, { props: { open: true, item: folderItem } });

	// Present, and labelled with the folder the grant actually sits on.
	// Plain `textContent` rather than jest-dom's `toHaveTextContent`: the
	// matchers load at runtime via vitest-setup, but no other test in this
	// codebase uses them and their types are not wired into svelte-check,
	// so asserting with them fails `npm run check` while passing vitest.
	const source = await screen.findByTestId('share-dialog-member-source-group-team-a');
	expect(source.textContent).toContain('Projects');

	// …and NOT editable here. The grant belongs to another folder, so a
	// role change or revoke from this dialog would silently mutate a
	// different folder's sharing. This assertion is the guard against
	// that: it fails the moment the controls leak into an inherited row.
	expect(screen.queryByTestId('share-dialog-member-role-group-team-a')).toBeNull();
	expect(screen.queryByTestId('share-dialog-member-remove-group-team-a')).toBeNull();
});

it('re-points the dialog when an inherited source is clicked', async () => {
	m(getFolderAncestorsWithGrants).mockResolvedValue(chainWithInherited);
	const onretarget = vi.fn();
	render(ShareDialog, { props: { open: true, item: folderItem, onretarget } });

	await fireEvent.click(await screen.findByTestId('share-dialog-member-source-group-team-a'));

	// Without this the chip would navigate /files → /files, which is the
	// same route: nothing remounts, and the dialog sits there still
	// showing the folder the user just left — grant still greyed.
	expect(onretarget).toHaveBeenCalledWith({ id: 'p1', name: 'Projects', kind: 'folder' });
});

it('counts inherited access in the tab badges', async () => {
	m(getFolderAncestorsWithGrants).mockResolvedValue(chainWithInherited);
	render(ShareDialog, { props: { open: true, item: folderItem } });

	// Both are inherited and neither is direct, so a badge counting only
	// direct grants would read (0) beside a folder two parties can reach.
	await waitFor(() =>
		expect(screen.getByTestId('share-dialog-people-tab').textContent).toContain('1')
	);
	expect(screen.getByTestId('share-dialog-link-tab').textContent).toContain('1');
});

it('lists a public link inherited from an ancestor', async () => {
	m(getFolderAncestorsWithGrants).mockResolvedValue(chainWithInherited);
	render(ShareDialog, { props: { open: true, item: folderItem } });

	await fireEvent.click(await screen.findByTestId('share-dialog-link-tab'));

	// The link's own name, not just "a parent is shared" — that name is
	// why the backend joins `storage.shares`; the token grant alone
	// carries nothing displayable.
	expect(await screen.findByText('Team link')).not.toBeNull();
	expect(screen.getByTestId('share-dialog-inherited-link-source-lg1').textContent).toContain(
		'Projects'
	);
});

it('points a drive-inherited grant at the drive config page', async () => {
	m(getFolderAncestorsWithGrants).mockResolvedValue(chainWithDriveInherited);
	const onretarget = vi.fn();
	render(ShareDialog, { props: { open: true, item: folderItem, onretarget } });

	const source = await screen.findByTestId('share-dialog-member-source-group-team-a');
	expect(source.getAttribute('href')).toContain('/config/drive/d1');
	expect(source.textContent).toContain('Marketing');

	// Navigation only. Re-pointing THIS dialog at a drive would offer
	// folder controls for a resource that has none — drive grants use a
	// narrower role ladder and support no public links, which is why the
	// config page runs the dialog with allowLinks=false.
	await fireEvent.click(source);
	expect(onretarget).not.toHaveBeenCalled();
});

it('points a drive-inherited public link at the drive config page', async () => {
	m(getFolderAncestorsWithGrants).mockResolvedValue(chainWithDriveInherited);
	render(ShareDialog, { props: { open: true, item: folderItem } });

	await fireEvent.click(await screen.findByTestId('share-dialog-link-tab'));

	const source = await screen.findByTestId('share-dialog-inherited-link-source-lg-drive');
	expect(source.getAttribute('href')).toContain('/config/drive/d1');
	// Before this the row was flat text — visible, but a dead end.
	expect(source.tagName).toBe('A');
});

it('walks from the parent folder when sharing a file', async () => {
	const { getFile } = await import('$lib/api/endpoints/files');
	m(getFile).mockResolvedValue({ id: 'f1', folder_id: 'parent-of-f1' });
	render(ShareDialog, { props: { open: true, item } });

	// A file has no ancestors of its own — the climb starts one level up.
	// Its own grants come from `fetchGrantsForResource`, so everything the
	// walk returns is inherited and none of it is filtered out.
	await waitFor(() => expect(getFolderAncestorsWithGrants).toHaveBeenCalledWith('parent-of-f1'));
});
