/** Photos timeline endpoint — ported from features/library/photos.js. */
import { apiFetch } from '$lib/api/client';
import { apiFetchEtagged, invalidatePrefix } from '$lib/api/etagCache';
import { getCsrfHeaders } from '$lib/api/csrf';
import type { FileItem } from '$lib/api/types';

/**
 * A timeline photo/video. The `/api/photos/resources` envelope carries file
 * fields at `.resource.*` (as every other `/resources` endpoint does), but
 * the SPA's photos code — grid layout, lightbox, selection — reads
 * `.id`/`.name`/`.width`/`.height`/`.sort_date` directly, so {@link fetchPhotos}
 * flattens the envelope item: file fields spread onto the top level, plus the
 * item-level signals (`width`, `height`, `sort_date`, `captured_at`,
 * `orientation`, `has_gps`) that live outside `resource` on the wire. Keeps
 * every consumer unchanged across the §4 cutover from `/api/photos` → `/api/photos/resources`.
 */
export interface PhotoItem extends FileItem {
	width?: number;
	height?: number;
	/** Always present on the envelope — epoch seconds; the item's sort axis value. */
	sort_date: number;
	/** Raw EXIF `DateTimeOriginal`, epoch seconds. Absent when the file carries no EXIF date. */
	captured_at?: number;
	/** Raw EXIF orientation (TIFF 1-8). Absent when the file carries no orientation tag. */
	orientation?: number;
	/** `true` when the file has both lat AND lng. Raw coordinates stay off the listing. */
	has_gps: boolean;
	/**
	 * `true` when the §9 within-drive `DISTINCT ON (blob_hash)` hid at least
	 * one sibling behind this tile — another non-trashed media file in the
	 * same drive referencing the same bytes. Powers the delete-UX in §9b
	 * Layer 1: a `true` tile warns the user that trashing it removes just
	 * one copy of the content, so the gallery can fetch the siblings list
	 * from `GET /api/dedup/check/{hash}` (Layer 2) before committing.
	 * `false` is the common case; the gallery can skip the sibling fetch
	 * entirely in that branch.
	 */
	has_blob_siblings: boolean;
}

export interface PhotoPage {
	items: PhotoItem[];
	nextCursor: string | null;
}

/** One row of the `/api/photos/resources` envelope — item-level fields + nested `resource`. */
interface PhotoEnvelopeItem {
	resource_type: 'file';
	resource: FileItem;
	width?: number;
	height?: number;
	sort_date: number;
	captured_at?: number;
	orientation?: number;
	has_gps: boolean;
	has_blob_siblings: boolean;
}

interface PhotosEnvelope {
	items: PhotoEnvelopeItem[];
	/** Omitted when this is the last page (not `null`). */
	next_cursor?: string;
}

/** EXIF metadata returned by `/api/files/{id}/metadata` (subset used by the lightbox). */
export interface FileMetadata {
	file_id: string;
	captured_at?: number;
	latitude?: number | null;
	longitude?: number | null;
	camera_make?: string | null;
	camera_model?: string | null;
	orientation?: number | null;
	width?: number | null;
	height?: number | null;
}

/** Result of a batch trash request (200 = all, 206 = partial success). */
export interface BatchTrashResult {
	successful: string[];
	failed: string[];
}

/** One server-side photo cluster for the Places map (`GET /api/photos/geo`). */
export interface GeoCluster {
	lng: number;
	lat: number;
	count: number;
	sample_file_id: string;
}

/**
 * Fetch geotagged-photo clusters for a viewport. The backend aggregates
 * server-side on a grid keyed by zoom, so the client draws one lightweight
 * marker per cluster — no client-side clustering needed. `bbox` is
 * `"west,south,east,north"` in decimal degrees. Available only when the
 * Places feature is enabled (otherwise the route 404s).
 */
export async function fetchPhotosGeo(bbox: string, zoom: number): Promise<GeoCluster[]> {
	const res = await apiFetch(`/api/photos/geo?bbox=${encodeURIComponent(bbox)}&zoom=${zoom}`, {
		credentials: 'same-origin'
	});
	if (!res.ok) throw new Error(`photos geo failed: ${res.status}`);
	return (await res.json()) as GeoCluster[];
}

/** Backend `MAX_BATCH_SIZE` — chunk larger selections into separate requests. */
const BATCH_CHUNK_SIZE = 1000;

/** Media-kind filter on the Photos timeline. `'all'` is the default
 *  (both image AND video rows); `'photo'` / `'video'` narrow to one
 *  mime family. Omits the query param on `'all'` so the URL stays
 *  short on the default view. */
export type PhotosKind = 'all' | 'photo' | 'video';

/** Optional filter / cursor knobs on {@link fetchPhotos}. All default
 *  to "no filter"; the resulting URL omits every absent param. */
export interface FetchPhotosOptions {
	/** Opaque cursor from a prior response. Absent for page 1. */
	cursor?: string | null;
	/** Narrow to photos or videos. `'all'` (default) keeps both. */
	kind?: PhotosKind;
	/** Restrict to a single drive the caller can access. Absent → cross-drive view. */
	driveId?: string | null;
	/** Narrow to the caller's favourited rows only. `false`/omitted keeps the full feed. */
	favoriteOnly?: boolean;
}

/**
 * Fetch one page of the photo timeline from `/api/photos/resources` — the
 * normalized `CursorListResponse<PhotoResourceItemDto>` envelope that
 * replaced the bare-array `/api/photos` endpoint in §4 of
 * `docs/plan/photos-resources-migration.md`.
 *
 * The envelope's items have the file at `.resource` and the photo-level
 * signals as siblings. We flatten them into {@link PhotoItem} so the SPA's
 * grid, lightbox, and selection paths read `.id`/`.name`/`.width`/`.height`
 * directly — the cutover is transparent to every consumer. The next-page
 * cursor comes from the `next_cursor` body field (omitted on the last page,
 * `undefined` in the parsed envelope) rather than a response header.
 *
 * Filter knobs (§6 / §6b) ride in {@link FetchPhotosOptions}. A cursor
 * MUST be reused against the same filter combo that minted it — the server
 * returns 400 on mismatch, which the caller should treat as a signal to
 * reset pagination (page 1 with the new filter set) rather than retry.
 */
export async function fetchPhotos(limit = 60, opts: FetchPhotosOptions = {}): Promise<PhotoPage> {
	const q = new URLSearchParams({ limit: String(limit) });
	if (opts.cursor) q.set('cursor', opts.cursor);
	if (opts.kind && opts.kind !== 'all') q.set('kind', opts.kind);
	if (opts.driveId) q.set('drive_id', opts.driveId);
	if (opts.favoriteOnly) q.set('favorite_only', 'true');
	// §2b — `apiFetchEtagged` attaches `If-None-Match` from a prior
	// response, so a tab-switch back to the same filter set resolves on
	// a 304 with no body re-transfer and reuses the cached envelope.
	// Each unique query string (filter combo + cursor) gets its own
	// entry; mutations invalidate the whole `/api/photos/resources`
	// prefix (see `batchTrash` below).
	const { body: envelope } = await apiFetchEtagged<PhotosEnvelope>(`/api/photos/resources?${q}`, {
		credentials: 'same-origin'
	});
	const items: PhotoItem[] = (envelope.items ?? []).map((row) => ({
		...row.resource,
		width: row.width,
		height: row.height,
		sort_date: row.sort_date,
		captured_at: row.captured_at,
		orientation: row.orientation,
		has_gps: row.has_gps,
		has_blob_siblings: row.has_blob_siblings
	}));
	return {
		items,
		nextCursor: envelope.next_cursor ?? null
	};
}

/** Fetch EXIF metadata for a file. Returns `null` on any error (non-critical). */
export async function fetchFileMetadata(fileId: string): Promise<FileMetadata | null> {
	try {
		const res = await apiFetch(`/api/files/${fileId}/metadata`, { credentials: 'same-origin' });
		if (!res.ok) return null;
		return (await res.json()) as FileMetadata;
	} catch {
		return null;
	}
}

/**
 * Move files to trash in batches via `POST /api/batch/trash`. One request per
 * chunk (up to {@link BATCH_CHUNK_SIZE} ids); 200 = all trashed, 206 = partial.
 * Returns the set of ids that were actually trashed across all chunks.
 */
export async function batchTrash(fileIds: string[]): Promise<Set<string>> {
	const trashed = new Set<string>();
	// §2b cache: any trash moves a photo out of every photos/favorites/
	// recents/folder listing it was in. Drop the matching prefixes so the
	// next read fetches fresh — the browser-level HTTP cache still gets
	// a 304 for pages the server considers unchanged, but the SPA layer
	// can't rely on that alone (private tabs, mobile eviction).
	invalidatePrefix('/api/photos/resources');
	invalidatePrefix('/api/favorites/resources');
	for (let i = 0; i < fileIds.length; i += BATCH_CHUNK_SIZE) {
		const chunk = fileIds.slice(i, i + BATCH_CHUNK_SIZE);
		const res = await apiFetch('/api/batch/trash', {
			method: 'POST',
			credentials: 'same-origin',
			headers: { 'Content-Type': 'application/json', ...getCsrfHeaders() },
			body: JSON.stringify({ file_ids: chunk, folder_ids: [] })
		});
		// 200 = all trashed, 206 = partial; both carry `successful`.
		if (!res.ok && res.status !== 206) continue;
		const data = (await res.json().catch(() => ({}))) as Partial<BatchTrashResult>;
		const ok = Array.isArray(data?.successful) ? data.successful : chunk;
		for (const id of ok) trashed.add(id);
	}
	return trashed;
}
