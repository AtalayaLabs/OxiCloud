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

import {
	listRecentResources,
	clearRecentItems,
	removeFromRecent
} from '$lib/api/generated/openapi/sdk.gen';
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
					icon_class: 'fa-file',
					icon_special_class: '',
					id: 'r1',
					mime_type: 'text/plain',
					modified_at: 0,
					name: 'notes.txt',
					created_by: 'me',
					updated_by: 'me',
					folder_id: 'root',
					path: '/notes.txt',
					size: 4,
					size_formatted: '4 B',
					sort_date: 0,
					etag: 'e',
					content_hash: 'h'
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

it('clears recent activity after confirmation', async () => {
	withOneFile();
	confirmDialog.mockResolvedValue(true);
	m(clearRecentItems).mockResolvedValue(undefined);
	render(RecentPage);
	await fireEvent.click(await screen.findByTestId('recent-clear-btn'));
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

it('renders an empty state when there is no recent activity', async () => {
	m(listRecentResources).mockResolvedValue({ items: [], next_cursor: null });
	render(RecentPage);
	await waitFor(() => expect(listRecentResources).toHaveBeenCalled());
});
