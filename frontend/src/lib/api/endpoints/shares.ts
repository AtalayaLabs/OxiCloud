/** Public share-link endpoints (/api/shares) — ported from features/sharing. */
import { apiFetch } from '$lib/api/client';
import { getCsrfHeaders } from '$lib/api/csrf';
import type { ItemType, ShareItem } from '$lib/api/types';

const JSON_HEADERS = { 'Content-Type': 'application/json' };

export interface CreateShareInput {
	itemId: string;
	/** Optional human-readable link name (stored as `item_name`). */
	itemName?: string | null;
	itemType: ItemType;
	password?: string | null;
	/** ISO date string or null; converted to epoch seconds for the wire. */
	expiresAt?: string | null;
}

export async function createShare(input: CreateShareInput): Promise<ShareItem> {
	const body = {
		item_id: input.itemId,
		item_name: input.itemName ?? null,
		item_type: input.itemType,
		password: input.password || null,
		// Server accepts RFC 3339 UTC (`DateTime<Utc>` on the wire).
		// Normalise via `Date.toISOString()` so the server always sees the
		// same canonical form regardless of what the caller passed in.
		expires_at: input.expiresAt ? new Date(input.expiresAt).toISOString() : null
	};
	const res = await apiFetch('/api/shares', {
		method: 'POST',
		credentials: 'same-origin',
		headers: { ...JSON_HEADERS, ...getCsrfHeaders() },
		body: JSON.stringify(body)
	});
	if (!res.ok) {
		const e = (await res.json().catch(() => ({}))) as { error?: string };
		throw new Error(e.error || `create share failed: ${res.status}`);
	}
	return (await res.json()) as ShareItem;
}

export async function listSharesForItem(itemId: string, itemType: ItemType): Promise<ShareItem[]> {
	const params = new URLSearchParams({ item_id: itemId, item_type: itemType });
	const res = await apiFetch(`/api/shares?${params}`, { credentials: 'same-origin' });
	if (!res.ok) return [];
	const data = (await res.json()) as ShareItem[] | { items?: ShareItem[] };
	return Array.isArray(data) ? data : (data.items ?? []);
}

/** Fetch a single share by its UUID (used to resolve a token's URL on demand). */
export async function getShareById(shareId: string): Promise<ShareItem> {
	const res = await apiFetch(`/api/shares/${encodeURIComponent(shareId)}`, {
		credentials: 'same-origin'
	});
	if (!res.ok) throw new Error(`get share failed: ${res.status}`);
	return (await res.json()) as ShareItem;
}

export interface UpdateShareInput {
	/** `null` clears the password; omit to leave it unchanged. */
	password?: string | null;
	/** ISO date string clears/sets; converted to epoch seconds. `null` clears. */
	expiresAt?: string | null;
}

/**
 * Edit an existing public link's password and/or expiry.
 * `PUT /api/shares/{id}` with `{ password, expires_at }`.
 */
export async function updateShare(shareId: string, input: UpdateShareInput): Promise<ShareItem> {
	const body: { password?: string | null; expires_at?: string | null } = {};
	if (input.password !== undefined) body.password = input.password;
	if (input.expiresAt !== undefined) {
		// Server now accepts RFC 3339 UTC strings.
		body.expires_at = input.expiresAt ? new Date(input.expiresAt).toISOString() : null;
	}
	const res = await apiFetch(`/api/shares/${encodeURIComponent(shareId)}`, {
		method: 'PUT',
		credentials: 'same-origin',
		headers: { ...JSON_HEADERS, ...getCsrfHeaders() },
		body: JSON.stringify(body)
	});
	if (!res.ok) {
		const e = (await res.json().catch(() => ({}))) as { error?: string };
		throw new Error(e.error || `update share failed: ${res.status}`);
	}
	return (await res.json()) as ShareItem;
}

export async function deleteShare(shareId: string): Promise<void> {
	const res = await apiFetch(`/api/shares/${shareId}`, {
		method: 'DELETE',
		credentials: 'same-origin',
		headers: getCsrfHeaders()
	});
	if (!res.ok && res.status !== 204) throw new Error(`delete share failed: ${res.status}`);
}

/**
 * Copy a share URL to the clipboard, resolving it against the current origin.
 * Shared by the dialog and My Shares so copy-link logic lives in one place.
 * Returns `true` on success.
 */
export async function copyShareLink(url: string): Promise<boolean> {
	try {
		const absolute = typeof location !== 'undefined' ? new URL(url, location.origin).href : url;
		await navigator.clipboard.writeText(absolute);
		return true;
	} catch {
		return false;
	}
}
