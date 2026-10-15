import { it, expect, vi, beforeEach } from 'vitest';
import { render, screen, fireEvent, waitFor } from '@testing-library/svelte';

const { confirmDialog, promptDialog } = vi.hoisted(() => ({
	confirmDialog: vi.fn(),
	promptDialog: vi.fn()
}));
vi.mock('$lib/api/endpoints/favorites', () => ({
	fetchFavoritesPage: vi.fn(),
	removeFavorite: vi.fn()
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
	// ResourceList uses this to build the `<img class="file-thumb">`
	// src for the fallback path; tests don't render actual thumbnails
	// but the module import needs to succeed.
	fileThumbnailUrl: () => '/thumb.png',
	thumbSizeForView: () => 'preview' as const,
	renameFile: vi.fn(),
	deleteFile: vi.fn()
}));
vi.mock('$lib/api/endpoints/folders', () => ({ renameFolder: vi.fn(), deleteFolder: vi.fn() }));
vi.mock('$lib/stores/dialogs.svelte', () => ({ confirmDialog, promptDialog }));

import { fetchFavoritesPage, removeFavorite } from '$lib/api/endpoints/favorites';
import FavoritesPage from './+page.svelte';

const m = (fn: unknown) => fn as ReturnType<typeof vi.fn>;

function withOneFile() {
	m(fetchFavoritesPage).mockResolvedValue({
		items: [
			{
				resource_type: 'file',
				favorited_at: '2024-01-01T00:00:00Z',
				resource: {
					category: 'Image',
					created_at: 0,
					icon_class: 'fa-file',
					icon_special_class: '',
					id: 'f1',
					mime_type: 'image/png',
					modified_at: 0,
					name: 'photo.png',
					created_by: 'me',
					updated_by: 'me',
					folder_id: 'root',
					path: '/photo.png',
					size: 10,
					size_formatted: '10 B',
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

it('renders favorites returned by the API', async () => {
	withOneFile();
	render(FavoritesPage);
	await waitFor(() => expect(fetchFavoritesPage).toHaveBeenCalled());
	await waitFor(() => expect(screen.getByText('photo.png')).toBeTruthy());
});

it('renders an empty state when there are no favorites', async () => {
	m(fetchFavoritesPage).mockResolvedValue({ items: [], next_cursor: null });
	render(FavoritesPage);
	await waitFor(() => expect(fetchFavoritesPage).toHaveBeenCalled());
});

it('unfavorites a row via the star button', async () => {
	withOneFile();
	m(removeFavorite).mockResolvedValue(undefined);
	render(FavoritesPage);
	await screen.findByText('photo.png');
	await fireEvent.click(screen.getByTestId('resource-list-favorite-f1-btn'));
	await waitFor(() => expect(removeFavorite).toHaveBeenCalledWith('file', 'f1'));
});

it('batch-removes-from-favorite the selection', async () => {
	// /favorites' batch bar was intentionally trimmed to Download +
	// Remove-from-favorite. Bulk-deleting the underlying file from
	// this view (previous behaviour) confused the "this is a
	// bookmarks list" semantics — destructive actions belong in the
	// row's context menu, not in the batch bar. This test pins the
	// new shape: batch button just un-stars the selection.
	withOneFile();
	m(removeFavorite).mockResolvedValue(undefined);
	render(FavoritesPage);
	await screen.findByText('photo.png');
	await fireEvent.click(screen.getByTestId('resource-list-select-f1-checkbox'));
	await fireEvent.click(await screen.findByTestId('favorites-batch-remove-btn'));
	await waitFor(() => expect(removeFavorite).toHaveBeenCalledWith('file', 'f1'));
});
