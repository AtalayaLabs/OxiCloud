/** Photos timeline endpoint — ported from features/library/photos.js. */
import { apiFetch } from '$lib/api/client';
import { getCsrfHeaders } from '$lib/api/csrf';
import type { FileItem } from '$lib/api/types';
import type { PhotoResourceItemDto } from '$lib/api/generated/openapi/types.gen';

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
	/**
	 * Always present on the envelope: the value of the ACTIVE sort axis,
	 * RFC 3339 UTC. Group and bucket on THIS field — it tracks the
	 * ordering, and grouping on any other date can reopen a closed month.
	 */
	sort_date: string;
	/** Raw EXIF `DateTimeOriginal` (RFC 3339 UTC). Absent when the file carries no EXIF date. */
	captured_at?: string;
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

/**
 * One row of the `/api/photos/resources` envelope — item-level fields plus the
 * nested `resource`.
 *
 * **Derived from the generated SDK type, not hand-written.** Every field except
 * `resource` comes from `PhotoResourceItemDto`, so a server-side change to
 * `sort_date`, `captured_at`, `width`, … breaks this build instead of silently
 * mismatching at runtime. That guard did not exist before: the DTO was absent
 * from the OpenAPI spec entirely, so there was no generated twin to check
 * against, which is how `captured_at` stayed typed `number` long after the
 * server began sending RFC 3339.
 *
 * `resource` is narrowed from the generated `ResourceContentDto` (an untagged
 * `FileDto | FolderDto | DriveDto`) to `FileItem`, because this endpoint only
 * ever returns files — `resource_type` is always `'file'`. Keeping the union
 * would force a cast at every flatten site for no gain.
 *
 * Note `check-types-overlap.mjs` cannot catch this class of drift: it compares
 * NAMES, and `PhotoEnvelopeItem` never collided with `PhotoResourceItemDto`.
 * Deriving the type is the structural fix that script's own docs recommend.
 */
type PhotoEnvelopeItem = Omit<PhotoResourceItemDto, 'resource' | 'resource_type'> & {
	resource_type: 'file';
	resource: FileItem;
};

interface PhotosEnvelope {
	items: PhotoEnvelopeItem[];
	/** Omitted when this is the last page (not `null`). */
	next_cursor?: string;
}

/** EXIF metadata returned by `/api/files/{id}/metadata` (subset used by the lightbox). */
export interface FileMetadata {
	file_id: string;
	captured_at?: string;
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

/** Sort axis on the Photos timeline (§3). `captured_at` (default) orders by
 *  EXIF capture date with upload-time fallback; `created_at` is pure upload
 *  time; `updated_at` surfaces recently-touched rows. */
export type PhotosOrderBy = 'captured_at' | 'created_at' | 'updated_at';

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
	/** Sort axis (§3). Default `captured_at`; absent query param on default. */
	orderBy?: PhotosOrderBy;
	/** Flip to oldest-first on whichever axis `orderBy` selects. */
	reverse?: boolean;
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
	if (opts.orderBy && opts.orderBy !== 'captured_at') q.set('order_by', opts.orderBy);
	if (opts.reverse) q.set('reverse', 'true');
	// §2 server-side ETag + the browser's HTTP cache deliver the
	// fast-tab-switch 304 fast path without an SPA-level cache; §2b
	// was dropped in favour of that native path (see
	// docs/plan/photos-resources-migration.md §2b for the rationale).
	const res = await apiFetch(`/api/photos/resources?${q}`, { credentials: 'same-origin' });
	if (!res.ok) throw new Error(`photos failed: ${res.status}`);
	const envelope = (await res.json()) as PhotosEnvelope;
	// `?? undefined` on the optionals is a null→absent normalisation, not
	// defensive noise. The server marks these `skip_serializing_if =
	// "Option::is_none"`, so the wire omits them and never sends null — but
	// utoipa renders `Option<T>` as `[T, null]`, so the generated type says
	// `| null`. Collapsing it here keeps `PhotoItem` (the FE-internal
	// flattened shape) honest about the real contract instead of propagating
	// a generator artifact into every consumer.
	const items: PhotoItem[] = (envelope.items ?? []).map((row) => ({
		...row.resource,
		width: row.width ?? undefined,
		height: row.height ?? undefined,
		sort_date: row.sort_date,
		captured_at: row.captured_at ?? undefined,
		orientation: row.orientation ?? undefined,
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
