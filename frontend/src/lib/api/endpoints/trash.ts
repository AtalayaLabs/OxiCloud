/** Trash endpoints — ported from trashModel.js + views/trash. */
import { apiFetch } from '$lib/api/client';
import { getCsrfHeaders } from '$lib/api/csrf';
import { t } from '$lib/i18n/index.svelte';
import { fetchResourcePage, type ResourcePage, type ResourcePageOpts } from './resources';
import type { TrashResourceItem } from '$lib/api/types';

export function fetchTrashPage(opts?: ResourcePageOpts): Promise<ResourcePage<TrashResourceItem>> {
	return fetchResourcePage<TrashResourceItem>('/api/trash/resources', 'deletion_date', opts);
}

/** Days from now until `value` (negative when already past). */
function daysUntil(value: number | string | Date | null | undefined): number | null {
	if (value === null || value === undefined) return null;
	let date: Date;
	if (value instanceof Date) date = value;
	else if (typeof value === 'number') date = new Date(value < 1e12 ? value * 1000 : value);
	else date = new Date(value);
	if (Number.isNaN(date.getTime())) return null;
	return Math.floor((date.getTime() - Date.now()) / 86_400_000);
}

export type ExpiryTier = 'never' | 'normal' | 'caution' | 'soon' | 'urgent' | 'expired';

export interface ExpiryChip {
	tier: ExpiryTier;
	icon: string;
	label: string;
}

/**
 * Tiered "remaining lifetime" chip for a trash deletion date — ported from
 * `formatExpiryChip` in static/js/core/formatters.js. `null` means "Never".
 */
export function expiryChip(value: number | string | null | undefined): ExpiryChip {
	if (value === null || value === undefined) {
		return { tier: 'never', icon: 'infinity', label: t('expiryChip.never', 'Never expires') };
	}
	const days = daysUntil(value);
	if (days === null) {
		return { tier: 'normal', icon: 'calendar', label: String(value) };
	}
	if (days < 0)
		return {
			tier: 'expired',
			icon: 'exclamation-triangle',
			label: t('expiryChip.expired', 'Expired')
		};
	if (days === 0)
		return { tier: 'urgent', icon: 'clock', label: t('expiryChip.today', 'Expires today') };
	if (days === 1)
		return { tier: 'urgent', icon: 'clock', label: t('expiryChip.tomorrow', 'Expires tomorrow') };
	if (days <= 7)
		return {
			tier: 'soon',
			icon: 'calendar',
			label: t('expiryChip.inDays', { count: days }, 'Expires in {{count}} days')
		};
	if (days <= 30)
		return {
			tier: 'caution',
			icon: 'calendar',
			label: t('expiryChip.inDays', { count: days }, 'Expires in {{count}} days')
		};
	return {
		tier: 'normal',
		icon: 'calendar',
		label: t('expiryChip.onDate', { count: days }, 'Expires in {{count}} days')
	};
}

/**
 * Coarse "remaining days" bucket label for the trash group-by swimlanes —
 * ported from `normalizeExpiryBucket`.
 */
export function remainingDaysBucket(value: number | string | null | undefined): string {
	const days = daysUntil(value);
	if (days === null) return t('expiryBucket.noExpiry', 'No expiration');
	if (days < 0) return t('expiryBucket.expired', 'Expired');
	if (days === 0) return t('expiryBucket.today', 'Today');
	if (days === 1) return t('expiryBucket.tomorrow', 'Tomorrow');
	if (days <= 7) return t('expiryBucket.week', 'In less than 7 days');
	if (days <= 30) return t('expiryBucket.month', 'In less than 30 days');
	return t('expiryBucket.later', 'Later');
}

export async function restoreTrashItem(trashId: string): Promise<void> {
	const res = await apiFetch(`/api/trash/${trashId}/restore`, {
		method: 'POST',
		credentials: 'same-origin',
		headers: { 'Content-Type': 'application/json', ...getCsrfHeaders() },
		body: '{}'
	});
	if (!res.ok) throw new Error(`restore failed: ${res.status}`);
}

export async function deleteTrashItem(trashId: string): Promise<void> {
	const res = await apiFetch(`/api/trash/${trashId}`, {
		method: 'DELETE',
		credentials: 'same-origin',
		headers: getCsrfHeaders()
	});
	if (!res.ok) throw new Error(`permanent delete failed: ${res.status}`);
}

export async function emptyTrash(): Promise<void> {
	const res = await apiFetch('/api/trash/empty', {
		method: 'DELETE',
		credentials: 'same-origin',
		headers: getCsrfHeaders()
	});
	if (!res.ok) throw new Error(`empty trash failed: ${res.status}`);
}

/**
 * `DELETE /api/trash/drive/{drive_id}` — empty the trash within a
 * single drive. Used by the trash page's Drive group-by, where each
 * bucket header carries a per-drive Empty button so multi-drive
 * owners don't have to wipe everything at once. Refused 404 when the
 * caller lacks Delete on the named drive (anti-enum).
 */
export async function emptyTrashForDrive(driveId: string): Promise<void> {
	const res = await apiFetch(`/api/trash/drive/${encodeURIComponent(driveId)}`, {
		method: 'DELETE',
		credentials: 'same-origin',
		headers: getCsrfHeaders()
	});
	if (!res.ok) throw new Error(`empty drive trash failed: ${res.status}`);
}
