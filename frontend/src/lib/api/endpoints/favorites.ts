/** Favorites endpoints — ported from favoritesModel.js + features/library. */
import { apiFetch } from '$lib/api/client';
import { getCsrfHeaders } from '$lib/api/csrf';
import { t } from '$lib/i18n/index.svelte';
import {
	fetchResourcePage,
	type ResourceBody,
	type ResourcePage,
	type ResourcePageOpts
} from './resources';
import type { ItemType } from '$lib/api/types';

/**
 * Coarse "how long ago" bucket for date group-bys (favorited/accessed/modified)
 * — ported from `normalizeDateBucket` in static/js/core/formatters.js.
 */
export function dateBucket(value: number | string | null | undefined): string | null {
	if (value === null || value === undefined) return null;
	let date: Date;
	if (typeof value === 'number') date = new Date(value < 1e12 ? value * 1000 : value);
	else date = new Date(value);
	if (Number.isNaN(date.getTime())) return null;
	const diffDays = Math.floor((Date.now() - date.getTime()) / 86_400_000);
	if (diffDays <= 0) return t('dateBucket.today', 'Today');
	if (diffDays <= 7) return t('dateBucket.last7days', 'Last 7 days');
	if (diffDays <= 30) return t('dateBucket.last30days', 'Last 30 days');
	return String(date.getFullYear());
}

/**
 * Coarse size bucket label — ported from `sizeBucket`. Pass `null` for folders
 * (they receive the "Folders" label).
 */
export function sizeBucket(bytes: number | null | undefined): string {
	if (bytes === null || bytes === undefined) return t('sizeBucket.folders', 'Folders');
	if (bytes === 0) return t('sizeBucket.empty', 'Empty (0 B)');
	if (bytes < 1_048_576) return t('sizeBucket.tiny', '< 1 MB');
	if (bytes < 104_857_600) return t('sizeBucket.small', '1 – 100 MB');
	if (bytes < 1_073_741_824) return t('sizeBucket.medium', '100 MB – 1 GB');
	if (bytes < 5 * 1_073_741_824) return t('sizeBucket.large', '1 – 5 GB');
	return t('sizeBucket.huge', '> 5 GB');
}

/** Human label for a resource `category` / type group-by bucket. */
export function typeLabel(category: string): string {
	const labels: Record<string, string> = {
		Folder: t('groupby.folders', 'Folders'),
		Image: t('category.images', 'Images'),
		Video: t('category.videos', 'Videos'),
		Audio: t('category.audio', 'Audio'),
		PDF: 'PDF',
		Document: t('category.documents', 'Documents'),
		Spreadsheet: t('category.spreadsheets', 'Spreadsheets'),
		Presentation: t('category.presentations', 'Presentations'),
		Archive: t('category.archives', 'Archives'),
		Code: t('category.code', 'Code'),
		Markdown: t('category.markdown', 'Markdown'),
		Text: t('category.text', 'Text'),
		Installer: t('category.installers', 'Installers')
	};
	return labels[category] ?? category;
}

export interface FavoritesResourceItem {
	resource_type: ItemType;
	favorited_at: string;
	resource: ResourceBody;
}

/** userId → resolved display name (best-effort, cached across the session). */
const ownerNameCache = new Map<string, string>();
const ownerInflight = new Map<string, Promise<string>>();

function shortId(id: string): string {
	return id.length > 8 ? `${id.slice(0, 8)}…` : id;
}

/**
 * Best-effort owner display-name lookup via `/api/users/{id}`, de-duplicated
 * and cached. Falls back to a shortened UUID on any failure. Ported from the
 * `systemUsers` resolver in the legacy frontend.
 */
export async function resolveOwnerName(ownerId: string): Promise<string> {
	if (!ownerId) return '';
	const cached = ownerNameCache.get(ownerId);
	if (cached) return cached;
	const pending = ownerInflight.get(ownerId);
	if (pending) return pending;

	const promise = (async () => {
		let name = shortId(ownerId);
		try {
			const res = await apiFetch(`/api/users/${encodeURIComponent(ownerId)}`, {
				credentials: 'same-origin'
			});
			if (res.ok) {
				const u = (await res.json()) as {
					username?: string;
					given_name?: string;
					family_name?: string;
					email?: string;
				};
				const full = [u.given_name, u.family_name].filter(Boolean).join(' ').trim();
				name = u.username || full || u.email || name;
			}
		} catch {
			// keep the UUID fallback
		} finally {
			ownerInflight.delete(ownerId);
		}
		ownerNameCache.set(ownerId, name);
		return name;
	})();
	ownerInflight.set(ownerId, promise);
	return promise;
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
