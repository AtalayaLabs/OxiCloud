import { it, expect, vi, beforeEach } from 'vitest';
import { render, screen, fireEvent, waitFor } from '@testing-library/svelte';
vi.mock('$lib/api/endpoints/files', () => ({
	deleteFile: vi.fn(),
	fileDownloadUrl: () => '/d',
	fileInlineUrl: () => '/i',
	fileThumbnailUrl: () => '/t',
	thumbSizeForView: () => 'preview' as const
}));
vi.mock('$lib/api/endpoints/favorites', () => ({ addFavorite: vi.fn() }));
vi.mock('$lib/api/endpoints/photos', () => ({ fetchFileMetadata: vi.fn() }));
// No `dialogs.svelte` mock: the lightbox's delete is an inline
// ConfirmButton (arm, then confirm), not a modal — nothing in the render
// tree imports `confirmDialog` any more.
vi.mock('$lib/utils/errors', () => ({ errorToast: vi.fn() }));
import { deleteFile } from '$lib/api/endpoints/files';
import { fetchFileMetadata } from '$lib/api/endpoints/photos';
import { serverConfig } from '$lib/stores/serverConfig.svelte';
import PhotoLightbox from './PhotoLightbox.svelte';
const fm = fetchFileMetadata as unknown as ReturnType<typeof vi.fn>;
const df = deleteFile as unknown as ReturnType<typeof vi.fn>;
function item(id: string, size = 1) {
	return {
		id,
		name: `${id}.jpg`,
		mime_type: 'image/jpeg',
		category: 'Image',
		folder_id: '',
		created_by: null,
		updated_by: null,
		path: '',
		size,
		modified_at: 0,
		created_at: 0,
		sort_date: 0,
		etag: '',
		content_hash: ''
	} as never;
}
beforeEach(() => {
	vi.clearAllMocks();
	fm.mockResolvedValue(null);
	// Default trash on so pre-existing tests see the familiar "Move to
	// trash" copy; the two new pins below flip it off per-test.
	serverConfig.features = { ...serverConfig.features, trash: true };
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

/**
 * Regression pin — the caption's size comes from `formatBytes(size)`, and is
 * omitted entirely when `size` is 0.
 *
 * The lightbox used to render the server's `size_formatted` string. That made
 * it the only place in the SPA displaying a backend-rendered size, and it
 * disagreed with the grid it sits on top of: `ResourceList` has always used
 * `formatBytes`, so the same 3 423 744-byte file read "3.27 MB" in the
 * lightbox and "3.3 MB" one Escape away. Dropping the field (step 5 of
 * retire-precomputed-display-fields.md) settles it on the SPA's own
 * formatter — this pin is what stops a future change from reintroducing a
 * second convention.
 *
 * The zero case is the load-bearing half. `minimalPhotoItem`
 * (`utils/media.ts`) is how People and Places open the lightbox by id with
 * `size: 0` while the real metadata is still in flight; the old code hid the
 * size there because `size_formatted` was `''`. Formatting unconditionally
 * would flash "0 B" at the user for every such open, so the `size > 0` guard
 * is behaviour, not defensiveness.
 */
it('formats the caption size with formatBytes, and omits it when size is 0', async () => {
	render(PhotoLightbox, { props: { items: [item('a', 3_423_744)], index: 0 } });

	const meta = await screen.findByTestId('photo-lightbox-meta');
	// formatBytes' 1-decimal form, NOT the retired server string "3.27 MB".
	expect(meta.textContent).toContain('3.3 MB');
	expect(meta.textContent).not.toContain('3.27 MB');
});

it('omits the caption size for a lazily-opened stub (size 0)', async () => {
	render(PhotoLightbox, { props: { items: [item('b', 0)], index: 0 } });

	const meta = await screen.findByTestId('photo-lightbox-meta');
	// Assert on the " · " separator rather than on the absence of a unit
	// string: the date half is locale-formatted, so "B"/"KB" could in
	// principle collide with a month abbreviation. With no EXIF metadata
	// resolved (fetchFileMetadata mocked to null), the caption is the date
	// alone, so a separator appearing at all means a size came back.
	expect(meta.textContent).not.toContain('·');
	// …and the date itself still renders — this is a suppressed size, not an
	// empty caption.
	expect(meta.textContent?.trim()).not.toBe('');
});

// ─── Delete-confirm copy ─ features.trash adapt ──────────────────────────
// The dialog opens via the trash-icon toolbar button. The two pins below
// cover both sides of `serverConfig.features.trash` to keep the i18n key
// selection honest: a regression that hardcoded either variant would
// mislead the user on the opposite deployment (trash-off hard-deletes;
// trash-on reversible move).

it('arms with "Really?" when features.trash is true', async () => {
	serverConfig.features = { ...serverConfig.features, trash: true };
	render(PhotoLightbox, { props: { items: [item('a')], index: 0 } });
	// First click only ARMS — it must not delete anything yet.
	await fireEvent.click(screen.getByTestId('photo-lightbox-delete-btn'));
	await waitFor(() => expect(screen.getByText('Really?')).toBeTruthy());
	expect(screen.queryByText('Permanently?')).toBeNull();
	expect(df).not.toHaveBeenCalled();
});

it('arms with "Permanently?" when features.trash is false', async () => {
	serverConfig.features = { ...serverConfig.features, trash: false };
	render(PhotoLightbox, { props: { items: [item('a')], index: 0 } });
	await fireEvent.click(screen.getByTestId('photo-lightbox-delete-btn'));
	await waitFor(() => expect(screen.getByText('Permanently?')).toBeTruthy());
	expect(screen.queryByText('Really?')).toBeNull();
	expect(df).not.toHaveBeenCalled();
});

it('deletes only on the second click, once armed', async () => {
	// The arm-then-confirm contract itself. Pinned because the wording
	// pins above would still pass if the FIRST click deleted — they only
	// read the label.
	serverConfig.features = { ...serverConfig.features, trash: true };
	render(PhotoLightbox, { props: { items: [item('a')], index: 0 } });
	const btn = screen.getByTestId('photo-lightbox-delete-btn');
	await fireEvent.click(btn);
	await waitFor(() => expect(screen.getByText('Really?')).toBeTruthy());
	await fireEvent.click(btn);
	await waitFor(() => expect(df).toHaveBeenCalledWith('a'));
});
