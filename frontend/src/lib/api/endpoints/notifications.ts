/**
 * Persistent notifications (bell) — REST client.
 *
 * Backs `useNotifications` (composable) and `NotificationBell`
 * (component). The bell reads from these; the message bus is a
 * cache-invalidation hint that triggers a refetch, not a data path.
 * See `docs/plan/message-bus.md § Slice E` for the pattern.
 */
import { apiJson } from '$lib/api/client';
import { apiFetch } from '$lib/api/client';
import { getCsrfHeaders } from '$lib/api/csrf';
import type {
	MarkAllReadResponse,
	Notification,
	NotificationListResponse,
	UnreadCountResponse
} from '$lib/api/types';

/** List newest-first. Optional `unread` filter, `before` cursor, `limit` cap. */
export async function listNotifications(opts?: {
	unread?: boolean;
	before?: string;
	limit?: number;
}): Promise<NotificationListResponse> {
	const q = new URLSearchParams();
	if (opts?.unread) q.set('unread', 'true');
	if (opts?.before) q.set('before', opts.before);
	if (opts?.limit !== undefined) q.set('limit', String(opts.limit));
	const suffix = q.toString();
	return apiJson<NotificationListResponse>(`/api/notifications${suffix ? `?${suffix}` : ''}`);
}

/** Badge-only fast path — no payloads fetched. */
export async function getUnreadCount(): Promise<number> {
	const res = await apiJson<UnreadCountResponse>('/api/notifications/unread');
	return res.unread_count;
}

/**
 * Mark one notification as read. Always resolves — the server responds
 * 204 regardless of whether the row existed or belonged to the caller
 * (anti-enumeration). Duplicated calls are safe.
 */
export async function markNotificationRead(id: string): Promise<void> {
	const res = await apiFetch(`/api/notifications/${encodeURIComponent(id)}/read`, {
		method: 'POST',
		headers: getCsrfHeaders()
	});
	if (!res.ok && res.status !== 204) {
		throw new Error(`markNotificationRead failed: HTTP ${res.status}`);
	}
}

/** Bulk mark-all-read. Returns the number of rows the server flipped. */
export async function markAllNotificationsRead(): Promise<number> {
	const res = await apiJson<MarkAllReadResponse>('/api/notifications/read-all', {
		method: 'POST',
		headers: getCsrfHeaders()
	});
	return res.marked;
}

/** Hard-delete one row. Same anti-enum shape as mark-read. */
export async function deleteNotification(id: string): Promise<void> {
	const res = await apiFetch(`/api/notifications/${encodeURIComponent(id)}`, {
		method: 'DELETE',
		headers: getCsrfHeaders()
	});
	if (!res.ok && res.status !== 204) {
		throw new Error(`deleteNotification failed: HTTP ${res.status}`);
	}
}

// Convenience: expose the row shape for consumers that don't want
// to import from `$lib/api/types` too.
export type { Notification };
