/** Shared helpers for the photo/video timeline (used by the grid, lightbox,
 * People and Places views). */
import type { FileItem } from '$lib/api/types';

/** True for video tiles (they get a play badge and client-side frame thumbs). */
export function isVideo(p: FileItem): boolean {
	return (p.mime_type ?? '').startsWith('video/');
}

/**
 * Capture timestamp in milliseconds since the epoch, for timeline bucketing.
 *
 * **`sort_date` first, and this ordering is load-bearing.** `sort_date` is the
 * server's `storage.files.media_sort_date` — `COALESCE(captured_at,
 * content_modified_at)` — and it is the column the Photos timeline is ORDERED
 * BY. The timeline virtualiser assumes pages arrive monotonically newest-first
 * and only ever extends the last group, so the field used for bucketing MUST
 * be the field the server sorted on. Bucketing on anything else lets a month
 * reappear after it closed, which Svelte reports as
 * `each_key_duplicate: duplicate key h:YYYY-MM`.
 *
 * That is exactly what happened: this function used to prefer `created_at`,
 * and it worked only by accident, because `media_sort_date` once fell back to
 * `created_at` for files without EXIF. Plan §3 repointed that fallback at the
 * content clock, the two axes diverged, and /photos broke — a file uploaded in
 * October carrying July content sorts among July's photos while `created_at`
 * buckets it under October.
 *
 * `created_at` survives only as a fallback for callers that synthesise an item
 * without a `sort_date` (see {@link minimalPhotoItem}, used by People and
 * Places to open the lightbox by id).
 *
 * `sort_date` is an RFC 3339 string on the wire, like every other timestamp.
 * The numeric branch below remains only for test fixtures and benches that
 * seed a bare number; it applies the project-wide `< 1e12 ? *1000 : raw`
 * seconds-or-millis heuristic. Returns 0 when nothing usable is present.
 */
export function photoTimestamp(p: FileItem): number {
	// The axis the server sorted by. Must win.
	const s = p.sort_date;
	if (typeof s === 'string') {
		const t = Date.parse(s);
		if (!Number.isNaN(t)) return t;
	}
	// Fixtures and benches seed a bare number.
	if (typeof s === 'number' && s > 0) {
		return s < 1e12 ? s * 1000 : s;
	}
	// Synthesised items only — they carry no sort_date.
	if (p.created_at) {
		const t = Date.parse(p.created_at);
		if (!Number.isNaN(t)) return t;
	}
	return 0;
}

/**
 * Build a minimal {@link FileItem} from just an id — used by People and Places
 * to open the lightbox by id and let it lazily fetch the rest (name, EXIF).
 */
export function minimalPhotoItem(id: string): FileItem {
	return {
		category: 'image',
		// Sentinel ISO timestamp — downstream display sites use
		// `new Date(iso)` which parses this as 1970-01-01T00:00:00Z.
		created_at: '1970-01-01T00:00:00Z',
		icon_class: '',
		icon_special_class: '',
		id,
		mime_type: 'image/jpeg',
		modified_at: '1970-01-01T00:00:00Z',
		name: '',
		created_by: null,
		updated_by: null,
		folder_id: '',
		path: '',
		size: 0,
		size_formatted: '',
		sort_date: 0,
		etag: '',
		content_hash: '',
		// Stub item — never wired to a live server response, so the
		// two required wire flags default to the safe "not set" value.
		is_favorite: false,
		is_shared: false
	};
}
