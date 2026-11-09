import { it, expect, vi, beforeEach } from 'vitest';
import { render, screen, fireEvent, waitFor } from '@testing-library/svelte';

const { ui, promptDialog, formDialog, confirmDialog } = vi.hoisted(() => ({
	ui: { notify: vi.fn() },
	promptDialog: vi.fn(),
	formDialog: vi.fn(),
	confirmDialog: vi.fn()
}));
vi.mock('$lib/stores/ui.svelte', () => ({ ui }));
vi.mock('$lib/stores/dialogs.svelte', () => ({ promptDialog, formDialog, confirmDialog }));
vi.mock('$lib/api/endpoints/files', () => ({ fileInlineUrl: () => '/in' }));
vi.mock('$lib/api/endpoints/search', () => ({ searchFiles: vi.fn(async () => ({ items: [] })) }));
vi.mock('$lib/api/endpoints/music', () => ({
	addTracks: vi.fn(),
	createPlaylist: vi.fn(),
	deletePlaylist: vi.fn(),
	listPlaylists: vi.fn(),
	listShares: vi.fn(async () => []),
	listTracks: vi.fn(async () => []),
	removeShare: vi.fn(),
	removeTrack: vi.fn(),
	renamePlaylist: vi.fn(),
	reorderTracks: vi.fn(),
	sharePlaylist: vi.fn(),
	updatePlaylist: vi.fn(),
	uploadCoverImage: vi.fn()
}));

import {
	listPlaylists,
	listTracks,
	createPlaylist,
	updatePlaylist,
	deletePlaylist,
	removeTrack,
	reorderTracks
} from '$lib/api/endpoints/music';
import { player } from '$lib/stores/player.svelte';
import MusicPage from './+page.svelte';

const m = (fn: unknown) => fn as ReturnType<typeof vi.fn>;

const playlist = {
	id: 'p1',
	name: 'Roadtrip',
	description: 'songs',
	owner_id: 'me',
	cover_file_id: null,
	track_count: 1,
	total_duration_secs: 180,
	created_at: 0,
	updated_at: 0
};
const track = {
	id: 't1',
	playlist_id: 'p1',
	file_id: 'f1',
	position: 0,
	added_at: 0,
	file_name: 'song.mp3',
	file_size: 100,
	mime_type: 'audio/mpeg',
	title: 'Song',
	artist: 'Artist',
	album: 'Album',
	duration_secs: 180
};

beforeEach(() => {
	vi.clearAllMocks();
	m(listTracks).mockResolvedValue([track]);
	// The player store is a singleton shared with every other suite in this
	// file's process; a queue left behind would make "queues a track" pass
	// whatever the row action did.
	player.queue = [];
	player.index = -1;
});

it('renders an empty state when there are no playlists', async () => {
	m(listPlaylists).mockResolvedValue([]);
	render(MusicPage);
	await waitFor(() => expect(listPlaylists).toHaveBeenCalled());
	await screen.findByTestId('music-create-playlist-empty-btn');
});

it('selects the first playlist and loads its tracks', async () => {
	m(listPlaylists).mockResolvedValue([playlist]);
	render(MusicPage);
	await waitFor(() => expect(listTracks).toHaveBeenCalledWith('p1'));
	await waitFor(() => expect(screen.getAllByText('Roadtrip').length).toBeGreaterThan(0));
});

it('creates a playlist from the header button', async () => {
	m(listPlaylists).mockResolvedValue([playlist]);
	// `promptDialog` callers migrated to the inline-error UX pattern pass
	// `action: async (name) => createPlaylist(name)` instead of running
	// the mutation outside the await. The real `dialogs.resolve` invokes
	// `action` on submit; a plain `mockResolvedValue` short-circuits it,
	// so `createPlaylist` never runs and the assertion fails. Simulate
	// the real flow by invoking `opts.action` with the submitted value
	// before resolving. See `frontend/AGENTS.md § Dialog error UX`.
	promptDialog.mockImplementation(async (opts) => {
		if (opts.action) await opts.action('Chill');
		return 'Chill';
	});
	m(createPlaylist).mockResolvedValue({ ...playlist, id: 'p2', name: 'Chill' });
	render(MusicPage);
	await screen.findByTestId('music-create-playlist-btn');
	await fireEvent.click(screen.getByTestId('music-create-playlist-btn'));
	await waitFor(() => expect(createPlaylist).toHaveBeenCalledWith('Chill'));
});

it('surfaces a load error', async () => {
	m(listPlaylists).mockRejectedValue(new Error('boom'));
	render(MusicPage);
	await waitFor(() => expect(listPlaylists).toHaveBeenCalled());
	await waitFor(() => expect(screen.getByText('boom')).toBeTruthy());
});

it('edits name and description together', async () => {
	m(listPlaylists).mockResolvedValue([playlist]);
	formDialog.mockResolvedValue({ name: 'Renamed', description: 'new description' });
	m(updatePlaylist).mockResolvedValue(undefined);
	render(MusicPage);
	await fireEvent.click(await screen.findByTestId('music-rename-playlist-btn'));
	await waitFor(() =>
		expect(updatePlaylist).toHaveBeenCalledWith('p1', {
			name: 'Renamed',
			description: 'new description'
		})
	);
});

// Regression: `track_count` is Option<i64> on the wire and reaches the client
// as null, so the raw interpolation rendered "null tracks" in the sidebar.
it('does not render a null track count', async () => {
	m(listPlaylists).mockResolvedValue([{ ...playlist, track_count: null }]);
	render(MusicPage);
	// The row's hook is fixed (`music-playlist-item`) rather than the playlist
	// name, so a wrong name reports as a text mismatch instead of a missing
	// element — see the comment on that testid in `+page.svelte`.
	await screen.findByTestId('music-playlist-item');
	expect(screen.getByTestId('music-playlist-name').textContent).toBe('Roadtrip');
	expect(screen.queryByText(/null/)).toBeNull();
});

it('deletes the current playlist on the second click', async () => {
	m(listPlaylists).mockResolvedValue([playlist]);
	m(deletePlaylist).mockResolvedValue(undefined);
	render(MusicPage);
	const del = await screen.findByTestId('music-delete-playlist-btn');
	await fireEvent.click(del);
	expect(deletePlaylist).not.toHaveBeenCalled();
	await fireEvent.click(del);
	await waitFor(() => expect(deletePlaylist).toHaveBeenCalledWith('p1'));
});

// ── The track list's wiring into `GestureList` ────────────────────────────
//
// The gestures themselves are pinned in `GestureList.test.ts`. What belongs
// here is which handler each declared capability points at — a swap of
// `onmove` for `swipeLeft.run` passes every test in that file.

const second = { ...track, id: 't2', file_id: 'f2', title: 'Second', position: 1 };

/** jsdom has no layout; the drag compares the pointer to row midpoints. */
function layoutRows(rows: ArrayLike<HTMLElement>): void {
	Array.from(rows).forEach((row, i) => {
		row.getBoundingClientRect = () =>
			({
				top: i * 40,
				bottom: i * 40 + 40,
				height: 40,
				left: 0,
				right: 300,
				width: 300,
				x: 0,
				y: i * 40,
				toJSON: () => ({})
			}) as DOMRect;
	});
}

async function dragFirstRowDown(container: HTMLElement) {
	const rows = container.querySelectorAll<HTMLElement>('[data-row-index]');
	layoutRows(rows);
	const grip = rows[0].querySelector<HTMLElement>('.gesture-grip')!;
	grip.setPointerCapture = vi.fn();
	await fireEvent.pointerDown(grip, { pointerId: 1 });
	await fireEvent.pointerMove(grip, { pointerId: 1, clientX: 0, clientY: 70 });
	await fireEvent.pointerUp(grip, { pointerId: 1 });
}

// A reorder is PERSISTED. The list used to shuffle itself on every `dragover`
// and save on drop; it now moves once, on release, and saves that.
it('persists a reorder and tells the user', async () => {
	m(listPlaylists).mockResolvedValue([playlist]);
	m(listTracks).mockResolvedValue([track, second]);
	m(reorderTracks).mockResolvedValue(undefined);
	const { container } = render(MusicPage);
	await screen.findByTestId('music-track-play-t1');

	await dragFirstRowDown(container);

	await waitFor(() => expect(reorderTracks).toHaveBeenCalledWith('p1', ['t2', 't1']));
	expect(ui.notify).toHaveBeenCalled();
});

// A failed save is not patched back row by row: the conflict may be someone
// else's reorder, so the playlist is re-fetched and the server's order wins.
it('re-fetches the playlist when the reorder cannot be saved', async () => {
	m(listPlaylists).mockResolvedValue([playlist]);
	m(listTracks).mockResolvedValue([track, second]);
	m(reorderTracks).mockRejectedValue(new Error('conflict'));
	const { container } = render(MusicPage);
	await screen.findByTestId('music-track-play-t1');
	m(listTracks).mockClear();

	await dragFirstRowDown(container);

	await waitFor(() => expect(listTracks).toHaveBeenCalledWith('p1'));
});

// The right-hand action is "add to queue", not "play this": it appends and
// leaves whatever is playing alone.
it('queues a track from the row action', async () => {
	m(listPlaylists).mockResolvedValue([playlist]);
	render(MusicPage);
	await screen.findByTestId('music-track-play-t1');

	await fireEvent.click(screen.getByTestId('music-track-queue-t1'));

	expect(player.queue.map((t) => t.id)).toEqual(['t1']);
	// Named in the toast: the queue takes the same track more than once, so
	// "Added to queue" alone would not say which row the gesture caught.
	expect(m(ui.notify).mock.calls[0][0]).toContain('Song');
});

// Removal is the left-hand action, and the same declaration renders the hover
// button — one handler, so the two routes to it cannot diverge.
it('removes a track from the row action', async () => {
	m(listPlaylists).mockResolvedValue([playlist]);
	m(removeTrack).mockResolvedValue(undefined);
	render(MusicPage);
	await screen.findByTestId('music-track-play-t1');

	await fireEvent.click(screen.getByTestId('music-track-remove-t1'));

	await waitFor(() => expect(removeTrack).toHaveBeenCalledWith('p1', 'f1'));
});
