/** Photos timeline endpoint — ported from features/library/photos.js. */
import { apiFetch } from '$lib/api/client';
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
	const res = await apiFetch(`/api/photos/resources?${q}`, { credentials: 'same-origin' });
	if (!res.ok) throw new Error(`photos failed: ${res.status}`);
	const envelope = (await res.json()) as PhotosEnvelope;
	const items: PhotoItem[] = (envelope.items ?? []).map((row) => ({
		...row.resource,
		width: row.width,
		height: row.height,
		sort_date: row.sort_date,
		captured_at: row.captured_at,
		orientation: row.orientation,
		has_gps: row.has_gps
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
