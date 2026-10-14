/**
 * Session-scoped resolver for a user's display name from their id.
 *
 * Moved out of `$lib/api/endpoints/favorites.ts`: this is a
 * cross-cutting lookup (favorites, recent, trash, shared, admin, …)
 * that happens to call one API, not a favorites-specific helper.
 *
 * Mechanics:
 *   - In-process cache keyed by user id — the resolution survives for
 *     the SPA's lifetime; a fresh page load repopulates.
 *   - Inflight-dedup map so N parallel resolutions for the same id
 *     collapse to one network request.
 *   - Fallback is a shortened UUID — the row still renders, just
 *     without the human name.
 *
 * Not using the generated SDK's `getUser` yet; migrated separately in
 * Phase 4 once the SDK's `users` operations are wired through.
 */
import { apiFetch } from '$lib/api/client';

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
