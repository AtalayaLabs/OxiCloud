import { it, expect, vi, beforeEach } from 'vitest';
import { render, screen, fireEvent } from '@testing-library/svelte';
vi.mock('$lib/api/endpoints/files', () => ({
	deleteFile: vi.fn(),
	fileDownloadUrl: () => '/d',
	fileInlineUrl: () => '/i',
	fileThumbnailUrl: () => '/t',
	thumbSizeForView: () => 'preview' as const
}));
vi.mock('$lib/api/endpoints/favorites', () => ({ addFavorite: vi.fn() }));
vi.mock('$lib/api/endpoints/photos', () => ({ fetchFileMetadata: vi.fn() }));
vi.mock('$lib/stores/dialogs.svelte', () => ({ confirmDialog: vi.fn() }));
vi.mock('$lib/utils/errors', () => ({ errorToast: vi.fn() }));
import { fetchFileMetadata } from '$lib/api/endpoints/photos';
import PhotoLightbox from './PhotoLightbox.svelte';
const fm = fetchFileMetadata as unknown as ReturnType<typeof vi.fn>;
function item(id: string) {
	return {
		id,
		name: `${id}.jpg`,
		mime_type: 'image/jpeg',
		category: 'Image',
		folder_id: '',
		created_by: null,
		updated_by: null,
		path: '',
		size: 1,
		modified_at: 0,
		created_at: 0,
		sort_date: 0,
		icon_class: '',
		size_formatted: '1 B',
		etag: '',
		content_hash: ''
	} as never;
}
beforeEach(() => {
	vi.clearAllMocks();
	fm.mockResolvedValue(null);
});
it('opens at an index, navigates next/prev, and closes', async () => {
	render(PhotoLightbox, { props: { items: [item('a'), item('b')], index: 0 } });
	expect(await screen.findByTestId('photo-lightbox')).toBeTruthy();
	await fireEvent.click(screen.getByTestId('photo-lightbox-next-btn'));
	await fireEvent.click(screen.getByTestId('photo-lightbox-prev-btn'));
	await fireEvent.click(screen.getByTestId('photo-lightbox-close-btn'));
});
it('renders nothing at index -1', () => {
	render(PhotoLightbox, { props: { items: [item('a')], index: -1 } });
	expect(screen.queryByTestId('photo-lightbox')).toBeNull();
});
