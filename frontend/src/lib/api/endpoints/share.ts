/**
 * Public share endpoints (/api/s/{token}).
 *
 * Two calls, and they are a LOGIN: they resolve the token, enforce the
 * password gate, and get back the `oxi_shares` ring cookie. Everything a
 * visitor does afterwards — browse, thumbnail, download, zip — goes through
 * the ordinary `folders` / `files` endpoints, because the ring makes them an
 * authenticated caller. The six `/api/s/*` browsing endpoints that used to
 * back this module were a parallel implementation of that surface and are
 * gone.
 *
 * These run through apiFetch, which bypasses the refresh-and-retry path for
 * /api/s/ — a 401 here means "password required", not "session expired".
 */
import { apiFetch } from '$lib/api/client';
import type { ItemType } from '$lib/api/types';

export interface ShareMeta {
	/**
	 * The shared resource's own id — a folder id or a file id per
	 * `item_type`. This is the handoff point between the share surface and
	 * the ordinary one: `GET /api/s/{token}` is what issues the `oxi_shares`
	 * ring cookie, and from here on the page addresses the resource through
	 * `/api/folders/*` and `/api/files/*` like any other caller.
	 */
	item_id: string;
	item_type: ItemType;
	item_name: string;
}

export type ShareMetaResult =
	| { status: 'ok'; data: ShareMeta }
	| { status: 'password' }
	| { status: 'expired' }
	| { status: 'invalid' };

const enc = encodeURIComponent;

export async function getShareMeta(token: string): Promise<ShareMetaResult> {
	const res = await apiFetch(`/api/s/${enc(token)}`);
	if (res.ok) return { status: 'ok', data: (await res.json()) as ShareMeta };
	if (res.status === 401) {
		const body = (await res.json().catch(() => null)) as { requiresPassword?: boolean } | null;
		if (body?.requiresPassword) return { status: 'password' };
		throw new Error('Unauthorized');
	}
	if (res.status === 410) return { status: 'expired' };
	// 404 means the token doesn't resolve to any share — a bad/typo'd link.
	if (res.status === 404) return { status: 'invalid' };
	throw new Error(`HTTP ${res.status}`);
}

/** Returns true on success, false on incorrect password. */
export async function verifySharePassword(token: string, password: string): Promise<boolean> {
	const res = await apiFetch(`/api/s/${enc(token)}/verify`, {
		method: 'POST',
		headers: { 'Content-Type': 'application/json' },
		body: JSON.stringify({ password })
	});
	if (res.ok) return true;
	if (res.status === 401) return false;
	throw new Error(`HTTP ${res.status}`);
}
