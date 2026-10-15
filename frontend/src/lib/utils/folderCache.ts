/**
 * In-memory folder-listing cache + folder-name map.
 *
 * Lives in `utils/` rather than next to the folder HTTP wrappers in
 * `$lib/api/endpoints/folders.ts` — these two caches are pure
 * client-side state (Map LRUs, no network) that happen to be fed by
 * folder-fetch call sites. Keeping them out of the endpoints tree
 * means the Phase 4 OpenAPI migration can retire that wrapper
 * module without taking the cache with it.
 *
 * Two layers:
 *
 *   1. Listing cache (`folderCache`). Lets the files view paint a
 *      previously-visited folder instantly on back/forward
 *      navigation, then revalidate with `If-None-Match` (304 = no
 *      body). Bounded LRU (40 entries) — a user who's visited more
 *      than that in a session pays the fetch again for the oldest
 *      one, which beats carrying unbounded state.
 *
 *   2. Name cache (`folderNames`). id → display name, learned from
 *      every listing (a folder's listing names its children) and
 *      from single-folder GETs. Lets breadcrumbs resolve with zero
 *      requests during normal navigation; only a cold deep-link
 *      fetches names it hasn't seen. Capped at 1 000 entries.
 */
import type { FolderListing } from '$lib/api/endpoints/folders';

/** The row stored per folder in the listing cache — paired `listing`
 *  + optional `etag` for conditional revalidation. */
export interface CachedFolder {
	listing: FolderListing;
	etag?: string;
}

const FOLDER_CACHE_MAX = 40;
const folderCache = new Map<string, CachedFolder>();

/** Cached listing for a folder, bumped to most-recently-used. */
export function getCachedFolder(folderId: string): CachedFolder | undefined {
	const hit = folderCache.get(folderId);
	if (hit) {
		folderCache.delete(folderId);
		folderCache.set(folderId, hit);
	}
	return hit;
}

export function cacheFolder(folderId: string, listing: FolderListing, etag?: string): void {
	// Learn the children's names for breadcrumb resolution.
	for (const f of listing.folders) rememberFolderName(f.id, f.name);
	folderCache.delete(folderId);
	folderCache.set(folderId, { listing, etag });
	// Evict the least-recently-used entries past the cap.
	while (folderCache.size > FOLDER_CACHE_MAX) {
		const oldest = folderCache.keys().next().value;
		if (oldest === undefined) break;
		folderCache.delete(oldest);
	}
}

/** Drop one folder, or the whole cache (no id), after a mutation. */
export function invalidateFolderCache(folderId?: string): void {
	if (folderId === undefined) folderCache.clear();
	else folderCache.delete(folderId);
}

const FOLDER_NAMES_MAX = 1000;
const folderNames = new Map<string, string>();

export function rememberFolderName(id: string, name: string): void {
	folderNames.delete(id);
	folderNames.set(id, name);
	while (folderNames.size > FOLDER_NAMES_MAX) {
		const oldest = folderNames.keys().next().value;
		if (oldest === undefined) break;
		folderNames.delete(oldest);
	}
}

export function getFolderName(id: string): string | undefined {
	return folderNames.get(id);
}
