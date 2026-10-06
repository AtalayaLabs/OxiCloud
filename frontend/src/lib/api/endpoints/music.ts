/** Music / playlist endpoints — ported from features/library/music.js. */
import { apiFetch, apiJson } from '$lib/api/client';
import { getCsrfHeaders } from '$lib/api/csrf';

const JSON_HEADERS = { 'Content-Type': 'application/json' };

export interface Playlist {
	id: string;
	name: string;
	description: string | null;
	owner_id: string;
	cover_file_id: string | null;
	track_count: number;
	total_duration_secs: number;
	created_at: number;
	updated_at: number;
}

export interface PlaylistItem {
	id: string;
	playlist_id: string;
	file_id: string;
	position: number;
	added_at: number;
	file_name: string | null;
	file_size: number | null;
	mime_type: string | null;
	title: string | null;
	artist: string | null;
	album: string | null;
	duration_secs: number | null;
}

/** A user a playlist is shared with (`/api/playlists/{id}/shares`). */
export interface MusicShare {
	user_id: string;
	can_write: boolean | null;
}

/** Fields that can be patched on a playlist via PUT. */
export interface PlaylistUpdate {
	name?: string;
	description?: string | null;
	cover_file_id?: string | null;
}

export function listPlaylists(): Promise<Playlist[]> {
	return apiJson<Playlist[]>('/api/playlists', { credentials: 'same-origin' });
}

export function listTracks(playlistId: string): Promise<PlaylistItem[]> {
	return apiJson<PlaylistItem[]>(`/api/playlists/${playlistId}/tracks`, {
		credentials: 'same-origin'
	});
}

export async function createPlaylist(name: string): Promise<Playlist> {
	const res = await apiFetch('/api/playlists', {
		method: 'POST',
		credentials: 'same-origin',
		headers: { ...JSON_HEADERS, ...getCsrfHeaders() },
		body: JSON.stringify({ name, description: null })
	});
	if (!res.ok) throw new Error(`create playlist failed: ${res.status}`);
	return (await res.json()) as Playlist;
}

/** Patch one or more playlist fields (name, description, public flag, cover). */
export async function updatePlaylist(playlistId: string, patch: PlaylistUpdate): Promise<void> {
	const res = await apiFetch(`/api/playlists/${playlistId}`, {
		method: 'PUT',
		credentials: 'same-origin',
		headers: { ...JSON_HEADERS, ...getCsrfHeaders() },
		body: JSON.stringify(patch)
	});
	if (!res.ok) throw new Error(`update playlist failed: ${res.status}`);
}

export function renamePlaylist(playlistId: string, name: string): Promise<void> {
	return updatePlaylist(playlistId, { name });
}

export async function deletePlaylist(playlistId: string): Promise<void> {
	const res = await apiFetch(`/api/playlists/${playlistId}`, {
		method: 'DELETE',
		credentials: 'same-origin',
		headers: getCsrfHeaders()
	});
	if (!res.ok) throw new Error(`delete playlist failed: ${res.status}`);
}

export async function addTracks(playlistId: string, fileIds: string[]): Promise<void> {
	const res = await apiFetch(`/api/playlists/${playlistId}/tracks`, {
		method: 'POST',
		credentials: 'same-origin',
		headers: { ...JSON_HEADERS, ...getCsrfHeaders() },
		body: JSON.stringify({ file_ids: fileIds })
	});
	if (!res.ok) throw new Error(`add tracks failed: ${res.status}`);
}

export async function removeTrack(playlistId: string, fileId: string): Promise<void> {
	const res = await apiFetch(`/api/playlists/${playlistId}/tracks/${encodeURIComponent(fileId)}`, {
		method: 'DELETE',
		credentials: 'same-origin',
		headers: getCsrfHeaders()
	});
	if (!res.ok) throw new Error(`remove track failed: ${res.status}`);
}

/** Persist a new track order. `itemIds` are PlaylistItem ids in the desired order. */
export async function reorderTracks(playlistId: string, itemIds: string[]): Promise<void> {
	const res = await apiFetch(`/api/playlists/${playlistId}/reorder`, {
		method: 'PUT',
		credentials: 'same-origin',
		headers: { ...JSON_HEADERS, ...getCsrfHeaders() },
		body: JSON.stringify({ item_ids: itemIds })
	});
	if (!res.ok) throw new Error(`reorder failed: ${res.status}`);
}

export function listShares(playlistId: string): Promise<MusicShare[]> {
	return apiJson<MusicShare[]>(`/api/playlists/${playlistId}/shares`, {
		credentials: 'same-origin'
	});
}

export async function sharePlaylist(
	playlistId: string,
	userId: string,
	canWrite = false
): Promise<void> {
	const res = await apiFetch(`/api/playlists/${playlistId}/share`, {
		method: 'POST',
		credentials: 'same-origin',
		headers: { ...JSON_HEADERS, ...getCsrfHeaders() },
		body: JSON.stringify({ user_id: userId, can_write: canWrite })
	});
	if (!res.ok) throw new Error(`share playlist failed: ${res.status}`);
}

export async function removeShare(playlistId: string, userId: string): Promise<void> {
	const res = await apiFetch(`/api/playlists/${playlistId}/share/${encodeURIComponent(userId)}`, {
		method: 'DELETE',
		credentials: 'same-origin',
		headers: getCsrfHeaders()
	});
	if (!res.ok) throw new Error(`remove share failed: ${res.status}`);
}

/** Upload an image and return its new file id (used to set a playlist cover). */
export async function uploadCoverImage(file: File, folderId = ''): Promise<string> {
	const form = new FormData();
	form.append('file', file);
	form.append('folder_id', folderId);
	const res = await apiFetch('/api/files/upload', {
		method: 'POST',
		credentials: 'same-origin',
		headers: getCsrfHeaders(),
		body: form
	});
	if (!res.ok) throw new Error(`cover upload failed: ${res.status}`);
	const uploaded = (await res.json()) as { id?: string };
	if (!uploaded.id) throw new Error('cover upload returned no file id');
	return uploaded.id;
}
