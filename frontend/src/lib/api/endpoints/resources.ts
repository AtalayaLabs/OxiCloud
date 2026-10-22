/**
 * Shared cursor-pagination helper for the favorites/recent/trash "resources"
 * endpoints, which all take the same query params. Ported from the original
 * favoritesModel/recentModel/trashModel.
 */
import { apiFetch } from '$lib/api/client';
import type { FileItem, FolderItem, ItemType } from '$lib/api/types';

/**
 * Page options for a cursor-paginated `/resources` listing.
 *
 * Generic over the sort axis (§7 part 3g). It defaults to `string` so a
 * caller whose endpoint has no published `*OrderBy` enum still compiles,
 * but every endpoint module here pins its own union — see `trash.ts` /
 * `favorites.ts`. That is what makes a retired axis a `npm run check`
 * failure instead of a runtime 400: §7 part 2 renamed `deletion_date` to
 * `expires_at` and `/trash` sent that value from five places, none of
 * which the build would have caught.
 */
export interface ResourcePageOpts<Axis extends string = string> {
	cursor?: string;
	orderBy?: Axis;
	limit?: number;
	reverse?: boolean;
	resourceTypes?: ItemType[];
}

export interface ResourcePage<TItem> {
	items: TItem[];
	next_cursor?: string;
}

export type ResourceBody = FileItem | FolderItem;

export function buildResourceParams<Axis extends string = string>(
	opts: ResourcePageOpts<Axis>,
	defaultOrderBy: Axis
): string {
	const { cursor, orderBy = defaultOrderBy, limit = 50, reverse = false, resourceTypes } = opts;
	const params = new URLSearchParams({ order_by: orderBy, limit: String(limit) });
	if (cursor) params.set('cursor', cursor);
	if (reverse) params.set('reverse', 'true');
	if (resourceTypes?.length) params.set('resource_types', resourceTypes.join(','));
	return params.toString();
}

export async function fetchResourcePage<TItem, Axis extends string = string>(
	base: string,
	defaultOrderBy: Axis,
	opts: ResourcePageOpts<Axis> = {}
): Promise<ResourcePage<TItem>> {
	const qs = buildResourceParams(opts, defaultOrderBy);
	const res = await apiFetch(`${base}?${qs}`, { credentials: 'same-origin', cache: 'no-store' });
	if (!res.ok) throw new Error(`GET ${base} failed: ${res.status}`);
	return (await res.json()) as ResourcePage<TItem>;
}
