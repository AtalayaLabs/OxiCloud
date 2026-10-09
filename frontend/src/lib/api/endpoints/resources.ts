/**
 * Shared cursor-pagination helper for the favorites/recent/trash "resources"
 * endpoints, which all take the same query params. Ported from the original
 * favoritesModel/recentModel/trashModel.
 */
import { apiFetchEtagged } from '$lib/api/etagCache';
import type { FileItem, FolderItem, ItemType } from '$lib/api/types';

export interface ResourcePageOpts {
	cursor?: string;
	orderBy?: string;
	limit?: number;
	reverse?: boolean;
	resourceTypes?: ItemType[];
}

export interface ResourcePage<TItem> {
	items: TItem[];
	next_cursor?: string;
}

export type ResourceBody = FileItem | FolderItem;

export function buildResourceParams(opts: ResourcePageOpts, defaultOrderBy: string): string {
	const { cursor, orderBy = defaultOrderBy, limit = 50, reverse = false, resourceTypes } = opts;
	const params = new URLSearchParams({ order_by: orderBy, limit: String(limit) });
	if (cursor) params.set('cursor', cursor);
	if (reverse) params.set('reverse', 'true');
	if (resourceTypes?.length) params.set('resource_types', resourceTypes.join(','));
	return params.toString();
}

export async function fetchResourcePage<TItem>(
	base: string,
	defaultOrderBy: string,
	opts: ResourcePageOpts = {}
): Promise<ResourcePage<TItem>> {
	const qs = buildResourceParams(opts, defaultOrderBy);
	// §2b — the shared helper for favorites, recent, trash. All three
	// endpoints emit ETags via `CursorListResponse::weak_etag`, so one
	// adoption here covers the three callers (favorites.ts / recent.ts
	// / trash.ts) in one place. `cache: 'no-store'` is dropped: the
	// browser-level cache stays disabled by default on these fetches,
	// but the SPA ETag cache attaches `If-None-Match` on refetch and
	// reuses the previous body on 304. See `etagCache.ts`.
	const { body } = await apiFetchEtagged<ResourcePage<TItem>>(`${base}?${qs}`, {
		credentials: 'same-origin'
	});
	return body;
}
