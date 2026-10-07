import { it, expect, vi, beforeEach } from 'vitest';
import { render, screen, fireEvent, waitFor } from '@testing-library/svelte';

const { ui, promptDialog, confirmDialog } = vi.hoisted(() => ({
	ui: { notify: vi.fn() },
	promptDialog: vi.fn(),
	confirmDialog: vi.fn()
}));
vi.mock('$lib/stores/ui.svelte', () => ({ ui }));
vi.mock('$lib/stores/dialogs.svelte', () => ({ promptDialog, confirmDialog }));
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
	renamePlaylist,
	deletePlaylist
} from '$lib/api/endpoints/music';
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
	promptDialog.mockResolvedValue('Chill');
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

it('renames the current playlist', async () => {
	m(listPlaylists).mockResolvedValue([playlist]);
	promptDialog.mockResolvedValue('Renamed');
	m(renamePlaylist).mockResolvedValue(undefined);
	render(MusicPage);
	await fireEvent.click(await screen.findByTestId('music-rename-playlist-btn'));
	await waitFor(() => expect(renamePlaylist).toHaveBeenCalledWith('p1', 'Renamed'));
});

it('deletes the current playlist after confirmation', async () => {
	m(listPlaylists).mockResolvedValue([playlist]);
	confirmDialog.mockResolvedValue(true);
	m(deletePlaylist).mockResolvedValue(undefined);
	render(MusicPage);
	await fireEvent.click(await screen.findByTestId('music-delete-playlist-btn'));
	await waitFor(() => expect(deletePlaylist).toHaveBeenCalledWith('p1'));
});
