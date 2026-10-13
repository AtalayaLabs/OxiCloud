/** Shared helpers for the photo/video timeline (used by the grid, lightbox,
 * People and Places views). */
import type { FileItem } from '$lib/api/types';

/** True for video tiles (they get a play badge and client-side frame thumbs). */
export function isVideo(p: FileItem): boolean {
	return (p.mime_type ?? '').startsWith('video/');
}

/**
 * EXIF-aware capture timestamp in milliseconds since the epoch. Accepts the
 * modern wire shape (`created_at` as RFC 3339 UTC string) and the legacy
 * `sort_date` numeric fallback (seconds OR milliseconds — scaled via the
 * project-wide `< 1e12 ? *1000 : raw` heuristic). Returns 0 when nothing
 * usable is present.
 */
export function photoTimestamp(p: FileItem): number {
	// Prefer the string wire shape (post-pivot).
	if (p.created_at) {
		const t = Date.parse(p.created_at);
		if (!Number.isNaN(t)) return t;
	}
	// Legacy numeric `sort_date` fallback (fixtures, pre-pivot sources).
	const n = p.sort_date;
	if (typeof n === 'number' && n > 0) {
		return n < 1e12 ? n * 1000 : n;
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
