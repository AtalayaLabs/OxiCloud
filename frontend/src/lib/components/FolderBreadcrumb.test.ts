import { it, expect, vi, beforeEach } from 'vitest';
import { render, screen, fireEvent, waitFor } from '@testing-library/svelte';
import FolderBreadcrumb from './FolderBreadcrumb.svelte';
import BreadcrumbTokenHarness from './__fixtures__/BreadcrumbTokenHarness.svelte';
import type { FolderAncestorsResponse } from '$lib/api/types';

// This component had no tests at all, and it now carries three opt-in
// affordances per crumb (drop target, share button, context menu) whose whole
// point is that they stay OFF unless the host asks — `/s/[token]` renders the
// same trail for anonymous visitors. These pin that gate, plus the refetch
// that keeps a renamed crumb's label from going stale.

const { getFolderAncestors } = vi.hoisted(() => ({ getFolderAncestors: vi.fn() }));
vi.mock('$lib/api/endpoints/folders', () => ({ getFolderAncestors }));

vi.mock('$lib/stores/serverConfig.svelte', () => ({
	serverConfig: { features: { sharing: true } }
}));

function chain(): FolderAncestorsResponse {
	return {
		access_source: { kind: 'drive', drive: { id: 'd1', name: 'Personal', kind: 'personal' } },
		// First entry is the drive root, which the component dedups away —
		// the root chip already carries the drive name.
		ancestors: [
			{ id: 'root', name: 'Personal', parent_id: null, drive_id: 'd1', is_shared: false },
			{ id: 'a', name: 'folderA', parent_id: 'root', drive_id: 'd1', is_shared: false },
			{ id: 'b', name: 'folderB', parent_id: 'a', drive_id: 'd1', is_shared: true }
		]
	} as FolderAncestorsResponse;
}

beforeEach(() => {
	getFolderAncestors.mockReset();
	getFolderAncestors.mockResolvedValue(chain());
});

it('forwards a right-click on a crumb with that crumb', async () => {
	const oncontextmenu = vi.fn();
	render(FolderBreadcrumb, { folderId: 'b', oncontextmenu });

	const crumb = await screen.findByTestId('folder-breadcrumb-a');
	await fireEvent.contextMenu(crumb);

	expect(oncontextmenu).toHaveBeenCalledTimes(1);
	// The crumb itself, not just its id: the host needs the name to label
	// the menu's rename/share actions without a second fetch.
	expect(oncontextmenu.mock.calls[0][1]).toMatchObject({ id: 'a', name: 'folderA' });

	// The leaf is a crumb like any other — right-clicking "where I am"
	// has to work, or the most likely target is the one that does nothing.
	await fireEvent.contextMenu(await screen.findByTestId('folder-breadcrumb-current-b'));
	expect(oncontextmenu).toHaveBeenCalledTimes(2);
	expect(oncontextmenu.mock.calls[1][1]).toMatchObject({ id: 'b', name: 'folderB' });
});

// The gate that keeps the menu off `/s/[token]`: no handler, no interception,
// so the browser's own menu still opens. A component that always called
// `preventDefault` would silently break right-click for anonymous visitors.
it('leaves the native menu alone when the host passes no handler', async () => {
	render(FolderBreadcrumb, { folderId: 'b' });

	const crumb = await screen.findByTestId('folder-breadcrumb-a');
	const ev = new MouseEvent('contextmenu', { bubbles: true, cancelable: true });
	crumb.dispatchEvent(ev);

	expect(ev.defaultPrevented).toBe(false);
});

// The root chip is the DRIVE, whose root folder is not an ordinary folder to
// rename or delete. Wiring the menu there would have offered exactly that.
it('does not open the menu from the drive-root chip', async () => {
	const oncontextmenu = vi.fn();
	render(FolderBreadcrumb, { folderId: 'b', oncontextmenu });

	const root = await screen.findByTestId('folder-breadcrumb-root-link');
	await fireEvent.contextMenu(root);

	expect(oncontextmenu).not.toHaveBeenCalled();
});

// Renaming a crumb changes no folder id, so the `folderId`-keyed fetch never
// re-ran and the trail kept showing the old name until the next navigation.
//
// Driven through a harness that owns the token in reactive state rather than
// through `rerender()`: that helper re-runs the component's effects on any
// prop change, so the same test written with it passed with the dependency
// deleted — see `__fixtures__/BreadcrumbTokenHarness.svelte`.
it('refetches the chain when reloadToken changes, and not otherwise', async () => {
	render(BreadcrumbTokenHarness, { folderId: 'b' });

	await screen.findByTestId('folder-breadcrumb-a');
	expect(getFolderAncestors).toHaveBeenCalledTimes(1);

	// A render triggered by something else must NOT refetch — otherwise the
	// trail re-requests its chain on every unrelated state change.
	await fireEvent.click(screen.getByTestId('harness-unrelated'));
	await waitFor(() => expect(screen.getByTestId('harness-unrelated').textContent).toContain('1'));
	expect(getFolderAncestors).toHaveBeenCalledTimes(1);

	const renamed = chain();
	renamed.ancestors[1].name = 'folderA-renamed';
	getFolderAncestors.mockResolvedValue(renamed);

	await fireEvent.click(screen.getByTestId('harness-bump'));

	await waitFor(() => expect(getFolderAncestors).toHaveBeenCalledTimes(2));
	await waitFor(() =>
		expect(screen.getByTestId('folder-breadcrumb-a').textContent).toContain('folderA-renamed')
	);
});

// Per-crumb share buttons, tinted from `is_shared` — the endpoint computes it
// with the same predicate the row listing uses, so a folder reads identically
// in both places. `folderB` is shared in the fixture; `folderA` is not.
it('renders a share button per crumb, tinted only where a grant exists', async () => {
	render(FolderBreadcrumb, { folderId: 'b', onShare: vi.fn() });

	const a = await screen.findByTestId('folder-breadcrumb-share-a');
	const b = await screen.findByTestId('folder-breadcrumb-share-b');
	expect(a.classList.contains('active')).toBe(false);
	expect(b.classList.contains('active')).toBe(true);
});

it('renders no share button when the host passes no onShare', async () => {
	render(FolderBreadcrumb, { folderId: 'b' });

	await screen.findByTestId('folder-breadcrumb-a');
	expect(screen.queryByTestId('folder-breadcrumb-share-a')).toBeNull();
});
