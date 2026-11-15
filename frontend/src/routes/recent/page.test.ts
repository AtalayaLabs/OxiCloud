import { it, expect, vi, beforeEach } from 'vitest';
import { render, screen, fireEvent, waitFor } from '@testing-library/svelte';

const { confirmDialog, promptDialog } = vi.hoisted(() => ({
	confirmDialog: vi.fn(),
	promptDialog: vi.fn()
}));
// Phase 4 of the OpenAPI migration: the `endpoints/recent.ts` wrapper
// is gone; the page calls the generated SDK operations directly, so the
// mock surface moves to `generated/openapi/sdk.gen` with the renamed
// exports (`clearRecent` → `clearRecentItems`, `fetchRecentPage` →
// `listRecentResources`).
vi.mock('$lib/api/generated/openapi/sdk.gen', () => ({
	clearRecentItems: vi.fn(),
	listRecentResources: vi.fn(),
	removeFromRecent: vi.fn()
}));
// Display helpers moved out of endpoints/favorites — mock their new homes.
vi.mock('$lib/utils/resourceBuckets', () => ({
	dateBucket: () => 'Today',
	sizeBucket: () => 'Small',
	typeLabel: () => 'File'
}));
vi.mock('$lib/utils/ownerName', () => ({
	resolveOwnerName: vi.fn(async () => 'me')
}));
vi.mock('$lib/api/endpoints/files', () => ({
	fileDownloadUrl: () => '/dl',
	renameFile: vi.fn(),
	deleteFile: vi.fn()
}));
vi.mock('$lib/api/endpoints/folders', () => ({ renameFolder: vi.fn(), deleteFolder: vi.fn() }));
vi.mock('$lib/stores/dialogs.svelte', () => ({ confirmDialog, promptDialog }));
// The page no longer owns a copy of the favourite toggle — it calls the
// shared `listActions.toggleFavorite`, which calls these two.
vi.mock('$lib/api/endpoints/favorites', () => ({
	addFavorite: vi.fn(),
	removeFavorite: vi.fn()
}));

import {
	listRecentResources,
	clearRecentItems,
	removeFromRecent
} from '$lib/api/generated/openapi/sdk.gen';
import { addFavorite, removeFavorite } from '$lib/api/endpoints/favorites';
import RecentPage from './+page.svelte';

const m = (fn: unknown) => fn as ReturnType<typeof vi.fn>;

function withOneFile() {
	// The generated SDK's `responseStyle: 'data'` strips the
	// `{ data, request, response }` wrapper, so the mock resolves
	// directly with the payload (same shape as the handwritten
	// wrapper used to return).
	m(listRecentResources).mockResolvedValue({
		items: [
			{
				resource_type: 'file',
				accessed_at: '2024-01-01T00:00:00Z',
				resource: {
					category: 'Document',
					created_at: 0,
					id: 'r1',
					mime_type: 'text/plain',
					modified_at: 0,
					name: 'notes.txt',
					created_by: 'me',
					updated_by: 'me',
					folder_id: 'root',
					path: '/notes.txt',
					size: 4,
					sort_date: 0,
					etag: 'e',
					content_hash: 'h',
					// Both flags are on every listing DTO the server emits, and
					// the star's `aria-pressed` reads `is_favorite` directly —
					// omit it and the attribute is absent rather than "false".
					is_favorite: false,
					is_shared: false
				}
			}
		],
		next_cursor: null
	});
}

beforeEach(() => vi.clearAllMocks());

it('renders recent items returned by the API', async () => {
	withOneFile();
	render(RecentPage);
	await waitFor(() => expect(listRecentResources).toHaveBeenCalled());
	await waitFor(() => expect(screen.getByText('notes.txt')).toBeTruthy());
});

it('clears recent activity on the second click', async () => {
	withOneFile();
	m(clearRecentItems).mockResolvedValue(undefined);
	render(RecentPage);
	const clear = await screen.findByTestId('recent-clear-btn');

	// Placement, not just presence. This button shipped as a bare
	// icon-only `toggle-btn` in the `displayActions` snippet, which
	// ResourceList renders inside `.view-toggle` — so it sat among the
	// grid/list and group-by controls, acting on the page while dressed
	// as view state. Every test stayed green through that, because they
	// all locate it by test id; it was found by eye.
	//
	// `.action-buttons` is the toolbar's act-on-the-page cluster, same
	// as `/files` (Add) and `/trash` (Empty trash).
	expect(clear.closest('.action-buttons')).toBeTruthy();
	expect(clear.closest('.view-toggle')).toBeNull();

	await fireEvent.click(clear);
	expect(clearRecentItems).not.toHaveBeenCalled();
	await fireEvent.click(clear);
	await waitFor(() => expect(clearRecentItems).toHaveBeenCalled());
});

it('removes a recent row via the broom button', async () => {
	// Recent no longer surfaces a favorite star (users go to the item's
	// real home for that). The per-row affordance is now a broom that
	// calls `DELETE /api/recent/{kind}/{id}` — verified end-to-end via
	// the `removeFromRecent` mock. The SDK's call shape is a `path`
	// object rather than positional args, so the assertion matches
	// against `{ path: { item_type, item_id }, … }`.
	withOneFile();
	m(removeFromRecent).mockResolvedValue(undefined);
	render(RecentPage);
	await screen.findByText('notes.txt');
	await fireEvent.click(screen.getByTestId('recent-remove-btn-r1'));
	await waitFor(() =>
		expect(removeFromRecent).toHaveBeenCalledWith(
			expect.objectContaining({ path: { item_type: 'file', item_id: 'r1' } })
		)
	);
});

it('batch-removes-from-recent the selection', async () => {
	// /recent's batch bar was intentionally trimmed to Download +
	// Remove-from-recent. Bulk-deleting the underlying file from
	// this history view (previous behaviour) confused the "this is
	// activity log" semantics — destructive actions belong in the
	// row's context menu, not in the batch bar. This test pins the
	// new shape: batch button just forgets the selection from history.
	withOneFile();
	m(removeFromRecent).mockResolvedValue(undefined);
	render(RecentPage);
	await screen.findByText('notes.txt');
	await fireEvent.click(screen.getByTestId('resource-list-select-r1-checkbox'));
	await fireEvent.click(await screen.findByTestId('recent-batch-remove-btn'));
	await waitFor(() =>
		expect(removeFromRecent).toHaveBeenCalledWith(
			expect.objectContaining({ path: { item_type: 'file', item_id: 'r1' } })
		)
	);
});

// ── The star ─────────────────────────────────────────────────────────────
//
// `/recent` is where the favourite round-trip was reported broken: the star
// lit, `POST /api/favorites/file/{id}` returned 201, and a refresh showed the
// row unstarred again. The server half of that (a listing ETag that ignored
// `is_favorite`, so the reload was answered with a 304 and the pre-star body)
// is pinned in `tests/api/resources_etag_contract.hurl` Scenario E — it could
// not be caught here, because jsdom never revalidates.
//
// What belongs at THIS layer is the other half: the page must actually reach
// the endpoint, and the row must show the result. It is worth pinning because
// the page used to carry its own copy of the toggle and now delegates to
// `listActions.toggleFavorite` — a wiring the next refactor could drop in
// silence, exactly as `/changes` did.
it('stars a recent row: calls the API and the row shows it', async () => {
	withOneFile();
	render(RecentPage);
	const star = await screen.findByTestId('resource-list-favorite-r1-btn');
	expect(star.getAttribute('aria-pressed')).toBe('false');

	await fireEvent.click(star);

	expect(addFavorite).toHaveBeenCalledWith('file', 'r1');
	await waitFor(() => expect(star.getAttribute('aria-pressed')).toBe('true'));
});

// The reverse direction goes through a different endpoint, so it can break on
// its own. A failed write must also put the row back — otherwise the star
// shows a state the server refused.
it('un-stars a recent row, and reverts when the write fails', async () => {
	withOneFile();
	render(RecentPage);
	const star = await screen.findByTestId('resource-list-favorite-r1-btn');
	await fireEvent.click(star);
	await waitFor(() => expect(star.getAttribute('aria-pressed')).toBe('true'));

	m(removeFavorite).mockRejectedValueOnce(new Error('nope'));
	await fireEvent.click(star);

	expect(removeFavorite).toHaveBeenCalledWith('file', 'r1');
	await waitFor(() => expect(star.getAttribute('aria-pressed')).toBe('true'));
});

// THE other half of the same bug: sharing a resource left the row's chip
// alone until a refresh, because the only signal was a `ShareDialog` prop
// that fired on creation and that `/files` alone wired. `ShareDialog` now
// broadcasts the resource's shared state and `ResourceList` patches the row —
// which means every page that paints the chip (`/files`, `/favorites`,
// `/recent`, `/search`, `/shared-with-me`) is served by one listener. This
// pins that listener through a real page, from the event inwards, so the
// emitter's own pins in `ShareDialog.test.ts` meet it in the middle.
it('tints the shared chip when another surface announces a share', async () => {
	withOneFile();
	render(RecentPage);
	const chip = await screen.findByTestId('resource-list-shared-r1-btn');
	expect(chip.getAttribute('aria-pressed')).toBe('false');

	window.dispatchEvent(
		new CustomEvent('oxicloud:row-flags-changed', { detail: { id: 'r1', is_shared: true } })
	);

	await waitFor(() => expect(chip.getAttribute('aria-pressed')).toBe('true'));
});

// And the un-share direction, which had no signal at all before — plus the
// rule that makes one event safe for both flags: a share announcement must
// not disturb the star. A listener that read a missing `is_favorite` as
// `false` would silently un-star every row the share dialog touched.
it('clears the chip on un-share without touching the star', async () => {
	withOneFile();
	render(RecentPage);
	const star = await screen.findByTestId('resource-list-favorite-r1-btn');
	await fireEvent.click(star);
	await waitFor(() => expect(star.getAttribute('aria-pressed')).toBe('true'));

	window.dispatchEvent(
		new CustomEvent('oxicloud:row-flags-changed', { detail: { id: 'r1', is_shared: false } })
	);

	const chip = screen.getByTestId('resource-list-shared-r1-btn');
	await waitFor(() => expect(chip.getAttribute('aria-pressed')).toBe('false'));
	expect(star.getAttribute('aria-pressed')).toBe('true');
});

it('renders an empty state when there is no recent activity', async () => {
	m(listRecentResources).mockResolvedValue({ items: [], next_cursor: null });
	render(RecentPage);
	await waitFor(() => expect(listRecentResources).toHaveBeenCalled());
});
