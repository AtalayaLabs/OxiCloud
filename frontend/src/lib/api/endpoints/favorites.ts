/**
 * Favorites API wrappers.
 *
 * This module used to carry display helpers (`dateBucket`,
 * `sizeBucket`, `typeLabel`, `resolveOwnerName`) too — those were
 * reused across every listing route and have moved to
 * `$lib/utils/resourceBuckets.ts` and `$lib/utils/ownerName.ts`.
 * What remains here are the three favorites-specific HTTP calls,
 * which will migrate to the generated SDK with the rest of Phase 4.
 */
import { apiFetch } from '$lib/api/client';
import { getCsrfHeaders } from '$lib/api/csrf';
import {
	fetchResourcePage,
	type ResourceBody,
	type ResourcePage,
	type ResourcePageOpts
} from './resources';
import type { ItemType } from '$lib/api/types';

export interface FavoritesResourceItem {
	resource_type: ItemType;
	favorited_at: string;
	resource: ResourceBody;
}

export function fetchFavoritesPage(
	opts?: ResourcePageOpts
): Promise<ResourcePage<FavoritesResourceItem>> {
	return fetchResourcePage<FavoritesResourceItem>('/api/favorites/resources', 'name', opts);
}

export async function addFavorite(type: ItemType, id: string): Promise<void> {
	const res = await apiFetch(`/api/favorites/${type}/${id}`, {
		method: 'POST',
		credentials: 'same-origin',
		headers: { 'Content-Type': 'application/json', ...getCsrfHeaders() },
		body: '{}'
	});
	if (!res.ok) throw new Error(`add favorite failed: ${res.status}`);
}

export async function removeFavorite(type: ItemType, id: string): Promise<void> {
	const res = await apiFetch(`/api/favorites/${type}/${id}`, {
		method: 'DELETE',
		credentials: 'same-origin',
		headers: getCsrfHeaders()
	});
	if (!res.ok) throw new Error(`remove favorite failed: ${res.status}`);
}
