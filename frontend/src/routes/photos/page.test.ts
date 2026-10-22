import { it, expect, vi, beforeEach } from 'vitest';
import { render, screen, waitFor } from '@testing-library/svelte';

const { ui, confirmDialog } = vi.hoisted(() => ({
	ui: { notify: vi.fn() },
	confirmDialog: vi.fn()
}));
vi.mock('$lib/stores/ui.svelte', () => ({ ui }));
vi.mock('$lib/stores/dialogs.svelte', () => ({ confirmDialog }));
vi.mock('$lib/api/endpoints/photos', () => ({
	batchTrash: vi.fn(),
	fetchPhotos: vi.fn(),
	uploadThumbnail: vi.fn()
}));
vi.mock('$lib/api/endpoints/people', () => ({ peopleEnabled: vi.fn() }));
vi.mock('$lib/api/endpoints/files', () => ({
	fileDownloadUrl: () => '/dl',
	fileThumbnailUrl: () => '/thumb',
	thumbSizeForView: () => 'preview' as const
}));

import { fetchPhotos } from '$lib/api/endpoints/photos';
import { peopleEnabled } from '$lib/api/endpoints/people';
import PhotosPage from './+page.svelte';

const m = (fn: unknown) => fn as ReturnType<typeof vi.fn>;

function photo(id: string) {
	return {
		category: 'Image',
		created_at: 0,
		icon_class: 'fa-image',
		id,
		mime_type: 'image/jpeg',
		modified_at: 0,
		name: id + '.jpg',
		created_by: 'me',
		updated_by: 'me',
		folder_id: 'home',
		path: '/' + id + '.jpg',
		size: 100,
		size_formatted: '100 B',
		sort_date: 1_700_000_000,
		etag: 'e',
		content_hash: 'h',
		width: 100,
		height: 100
	};
}

beforeEach(() => {
	vi.clearAllMocks();
	m(peopleEnabled).mockResolvedValue(false);
});

it('loads the first page of photos on mount', async () => {
	m(fetchPhotos).mockResolvedValue({ items: [photo('a')], nextCursor: null });
	render(PhotosPage);
	await waitFor(() => expect(fetchPhotos).toHaveBeenCalled());
	expect(m(fetchPhotos).mock.calls[0][0]).toBe(60);
});

it('shows an empty state when there are no photos', async () => {
	m(fetchPhotos).mockResolvedValue({ items: [], nextCursor: null });
	render(PhotosPage);
	await waitFor(() => expect(fetchPhotos).toHaveBeenCalled());
	await waitFor(() => expect(screen.getByTestId('photos-tab-moments')).toBeTruthy());
});

it('surfaces a load error', async () => {
	m(fetchPhotos).mockRejectedValue(new Error('photos boom'));
	render(PhotosPage);
	await waitFor(() => expect(screen.getByText('photos boom')).toBeTruthy());
});

it('hides the People tab when face detection is disabled', async () => {
	m(fetchPhotos).mockResolvedValue({ items: [], nextCursor: null });
	m(peopleEnabled).mockResolvedValue(false);
	render(PhotosPage);
	await waitFor(() => expect(peopleEnabled).toHaveBeenCalled());
	expect(screen.queryByTestId('photos-tab-people')).toBeNull();
});

it('shows the People tab when face detection is enabled', async () => {
	m(fetchPhotos).mockResolvedValue({ items: [], nextCursor: null });
	m(peopleEnabled).mockResolvedValue(true);
	render(PhotosPage);
	await waitFor(() => expect(screen.getByTestId('photos-tab-people')).toBeTruthy());
});
