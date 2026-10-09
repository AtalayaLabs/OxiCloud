/**
 * Deduplication endpoints — content-addressable blob accounting.
 *
 * The surface the SPA talks to today is the user-scoped siblings probe at
 * `GET /api/dedup/check/{hash}`. It drives the §9b delete-UX: a photo whose
 * listing row carries `has_blob_siblings: true` points here so the user can
 * see every file (live or trashed) that shares the same bytes before deciding
 * what to trash. See `docs/plan/photos-resources-migration.md` §9b.
 *
 * The sibling admin accounting (`ref_count`) lives on the admin split
 * `GET /api/admin/dedup/check/{hash}` and is not consumed here — the SPA has
 * no view that needs it.
 */
import { apiFetch } from '$lib/api/client';

/**
 * One file visible to the caller that references the probed blob hash.
 * Live and trashed rows share the same shape — `is_trashed` is the only
 * discriminator — so a chooser can colour or group them without a second
 * round trip. Trashed siblings are included because they still hold a
 * blob reference until the trash entry is purged, so hiding them would
 * mislead the UX about whether a delete will actually free bytes.
 */
export interface HashSibling {
	file_id: string;
	name: string;
	drive_id: string;
	folder_id: string | null;
	is_trashed: boolean;
	/** The caller has Delete on this file (via the service-layer authz engine). */
	can_delete: boolean;
	/** Rename / overwrite content. */
	can_update: boolean;
	/** Grant others access. */
	can_share: boolean;
}

/** Response from `GET /api/dedup/check/{hash}` on a hit (HTTP 200). */
export interface HashCheckResponse {
	hash: string;
	existing_size: number;
	/** Length of `siblings[]` in this response. Capped by the server at `MAX_SIBLINGS`. */
	count: number;
	/** True when more visible siblings exist than the server returned. */
	truncated: boolean;
	siblings: HashSibling[];
}

/**
 * Fetch the caller's visible siblings for a blob hash. Returns `null` when
 * the server responds 404 — i.e. the caller holds no visible reference to
 * this blob (anti-enum shape; matches "unknown hash" and "known but not
 * visible to this caller" alike). Throws on any other error.
 */
export async function fetchHashSiblings(hash: string): Promise<HashCheckResponse | null> {
	const res = await apiFetch(`/api/dedup/check/${encodeURIComponent(hash)}`, {
		credentials: 'same-origin'
	});
	if (res.status === 404) return null;
	if (!res.ok) throw new Error(`dedup/check failed: ${res.status}`);
	return (await res.json()) as HashCheckResponse;
}
