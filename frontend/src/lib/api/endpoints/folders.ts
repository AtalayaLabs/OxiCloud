/** Folder endpoints — ported from filesModel.js + fileOperations.js. */
import { apiFetch, apiJson } from '$lib/api/client';
import { getCsrfHeaders } from '$lib/api/csrf';
import type { FileItem, FolderAncestorsResponse, FolderItem, ItemType } from '$lib/api/types';

const JSON_HEADERS = { 'Content-Type': 'application/json' };
const NO_CACHE: RequestInit = {
	credentials: 'same-origin',
	cache: 'no-store',
	headers: { 'Cache-Control': 'no-cache, no-store, must-revalidate' }
};

export interface FolderListing {
	folders: FolderItem[];
	files: FileItem[];
	// Legacy `favoriteIds` / `sharedIds` fields are gone — the same
	// signals now live inline on every `FileItem` / `FolderItem`
	// (`is_favorite`, `is_shared`). Consumers read those directly.
}

/** Result of a (possibly conditional) listing fetch. */
export interface FolderListingResult {
	/** 200 with a fresh `listing`, or 304 → the caller should keep its cache. */
	status: number;
	listing?: FolderListing;
	etag?: string;
}

// ── In-memory listing cache (stale-while-revalidate) ─────────────────────────
// Lets the files view paint a previously-visited folder instantly on
// back/forward navigation, then revalidate with `If-None-Match` (304 = no body).
interface CachedFolder {
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

// ── Folder name cache (breadcrumbs) ──────────────────────────────────────────
// id → name, learned from every listing (a folder's listing names its children)
// and from getFolder. Lets breadcrumbs resolve with zero requests during normal
// navigation (each ancestor was named by its parent's listing); only a cold
// deep-link fetches the names it hasn't seen.
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

// In-flight dedup (the `resolveUser` pattern): on a cold deep-link the
// breadcrumb builder and the drive-id resolver both request the same folder
// concurrently — collapse duplicates into one GET. Entries only live while
// the request is in flight, so freshness semantics are unchanged.
const folderInflight = new Map<string, Promise<FolderItem>>();

export function getFolder(id: string): Promise<FolderItem> {
	const inflight = folderInflight.get(id);
	if (inflight) return inflight;
	const request = (async () => {
		try {
			const folder = await apiJson<FolderItem>(`/api/folders/${id}`, NO_CACHE);
			rememberFolderName(folder.id, folder.name);
			return folder;
		} finally {
			folderInflight.delete(id);
		}
	})();
	folderInflight.set(id, request);
	return request;
}

// ── Ancestor chain (breadcrumb) ──────────────────────────────────────────
// Backing store + inflight dedup for `GET /api/folders/{id}/ancestors` —
// mirrors the folderInflight pattern for `getFolder`. Rapid navigation
// (files → sub → sub-sub in <1s) folds concurrent requests for the same
// leaf into one round-trip. Response also seeds `folderNames` for every
// ancestor, so subsequent `getFolderName(id)` lookups are cache-free.
const ancestorsInflight = new Map<string, Promise<FolderAncestorsResponse>>();

export function getFolderAncestors(id: string): Promise<FolderAncestorsResponse> {
	const inflight = ancestorsInflight.get(id);
	if (inflight) return inflight;
	const request = (async () => {
		try {
			const chain = await apiJson<FolderAncestorsResponse>(
				`/api/folders/${id}/ancestors`,
				NO_CACHE
			);
			// Prime the shared folder-name cache — the breadcrumb walk
			// happens to be the exact input that populates it.
			for (const a of chain.ancestors) rememberFolderName(a.id, a.name);
			return chain;
		} finally {
			ancestorsInflight.delete(id);
		}
	})();
	ancestorsInflight.set(id, request);
	return request;
}

/** One page of `/api/folders/{id}/resources`. */
export interface FolderPage {
	/**
	 * Items in the exact order the server returned them. Under `order_by=name`,
	 * `type`, `size` the server puts folders first, then files; under
	 * `modified_at` / `created_at` the two kinds interleave. Consumers that
	 * need to preserve the server sort MUST iterate this list — the split
	 * `folders` / `files` arrays lose the interleaving.
	 */
	items: (FolderItem | FileItem)[];
	/** `items` filtered to folder rows (order preserved). */
	folders: FolderItem[];
	/** `items` filtered to file rows (order preserved). */
	files: FileItem[];
	/** Opaque cursor for the next page; `undefined` on the last page. */
	nextCursor?: string;
}

/**
 * Fetch a single page of a folder's listing.
 *
 * `/files` uses this directly and drives its own pagination — the initial
 * `load()` requests page one; the ResourceList's `onloadmore` (fired by an
 * IntersectionObserver at the bottom sentinel) requests the next page with
 * the previous `nextCursor` and appends the results. `orderBy` is passed
 * through so pages come back in the requested server-side sort order; the
 * caller resets state and refetches page one on sort/group change.
 *
 * The legacy `fetchFolderListing` (below) is a thin loop over this — kept
 * for the move-dialog folder tree, which genuinely needs every child at
 * once and doesn't have an infinite-scroll surface.
 */
export async function fetchFolderPage(
	folderId: string,
	opts: {
		orderBy?: string;
		reverse?: boolean;
		cursor?: string;
		limit?: number;
		forceRefresh?: boolean;
	} = {}
): Promise<FolderPage> {
	const params = new URLSearchParams({
		order_by: opts.orderBy ?? 'name',
		limit: String(opts.limit ?? 200)
	});
	if (opts.reverse) params.set('reverse', 'true');
	if (opts.cursor) params.set('cursor', opts.cursor);
	if (opts.forceRefresh) params.set('force_refresh', 'true');
	const res = await apiFetch(`/api/folders/${folderId}/resources?${params.toString()}`, {
		credentials: 'same-origin',
		cache: 'no-store'
	});
	if (res.status === 403) throw Object.assign(new Error('Forbidden'), { status: 403 });
	if (!res.ok) throw new Error(`listing failed: ${res.status}`);
	const page = (await res.json()) as {
		items?: { resource_type: ItemType; resource: FolderItem | FileItem }[];
		next_cursor?: string;
	};
	const items: (FolderItem | FileItem)[] = [];
	const folders: FolderItem[] = [];
	const files: FileItem[] = [];
	for (const it of page.items ?? []) {
		if (it.resource_type === 'folder') {
			const f = it.resource as FolderItem;
			folders.push(f);
			items.push(f);
		} else {
			const f = it.resource as FileItem;
			files.push(f);
			items.push(f);
		}
	}
	// Learn the children's names for breadcrumb resolution.
	for (const f of folders) rememberFolderName(f.id, f.name);
	return { items, folders, files, nextCursor: page.next_cursor };
}

/**
 * Fetch a folder's complete listing (sub-folders + files) by walking every
 * cursor page eagerly. Only the move-dialog tree still needs this shape —
 * `/files` switched to {@link fetchFolderPage} for lazy scroll-driven paging.
 *
 * `opts.etag` is accepted for call-site compatibility but ignored (the
 * `/resources` feed carries no whole-listing ETag). Favorite / share badge
 * sets are unpopulated by this endpoint and come back empty.
 */
export async function fetchFolderListing(
	folderId: string,
	opts: { etag?: string; forceRefresh?: boolean } = {}
): Promise<FolderListingResult> {
	const folders: FolderItem[] = [];
	const files: FileItem[] = [];
	let cursor: string | undefined;
	do {
		const page = await fetchFolderPage(folderId, {
			cursor,
			forceRefresh: opts.forceRefresh
		});
		folders.push(...page.folders);
		files.push(...page.files);
		cursor = page.nextCursor;
	} while (cursor);
	return { status: 200, listing: { folders, files } };
}

/** Non-conditional listing fetch (e.g. the move-dialog folder tree). */
export async function listFolder(folderId: string, forceRefresh = false): Promise<FolderListing> {
	const res = await fetchFolderListing(folderId, { forceRefresh });
	return res.listing ?? { folders: [], files: [] };
}

export async function createFolder(name: string, parentId: string | null): Promise<FolderItem> {
	const res = await apiFetch('/api/folders', {
		method: 'POST',
		credentials: 'same-origin',
		headers: { ...JSON_HEADERS, ...getCsrfHeaders() },
		body: JSON.stringify({ name, parent_id: parentId })
	});
	if (!res.ok) throw new Error(`create folder failed: ${res.status}`);
	return (await res.json()) as FolderItem;
}

export async function renameFolder(folderId: string, name: string): Promise<void> {
	const res = await apiFetch(`/api/folders/${folderId}/rename`, {
		method: 'PUT',
		credentials: 'same-origin',
		headers: { ...JSON_HEADERS, ...getCsrfHeaders() },
		body: JSON.stringify({ name })
	});
	if (!res.ok) throw new Error(`rename folder failed: ${res.status}`);
}

export async function moveFolder(folderId: string, targetFolderId: string | null): Promise<void> {
	const res = await apiFetch(`/api/folders/${folderId}/move`, {
		method: 'PUT',
		credentials: 'same-origin',
		headers: { ...JSON_HEADERS, ...getCsrfHeaders() },
		body: JSON.stringify({ parent_id: targetFolderId || null })
	});
	if (!res.ok) throw new Error(`move folder failed: ${res.status}`);
}

export async function deleteFolder(folderId: string): Promise<void> {
	const res = await apiFetch(`/api/folders/${folderId}`, {
		method: 'DELETE',
		credentials: 'same-origin',
		headers: getCsrfHeaders()
	});
	if (!res.ok) throw new Error(`delete folder failed: ${res.status}`);
}

export function folderZipUrl(folderId: string): string {
	return `/api/folders/${folderId}/download?format=zip`;
}
