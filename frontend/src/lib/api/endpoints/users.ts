/**
 * Per-user profile resolution via `GET /api/users/{id}`, cached per id.
 *
 * Used to render external (and any non-directory) users in share/recipient UIs
 * with their real name, email, avatar and an internal/external flag — the
 * system address book only lists internal users, so external grant subjects
 * would otherwise show as a bare UUID. Mirrors the original `systemUsers`
 * resolver. The endpoint enforces its own visibility rules; a non-visible
 * profile resolves to `null` so callers fall back to whatever label they have.
 */
import { apiFetch } from '$lib/api/client';

export interface ResolvedUser {
	id: string;
	name: string;
	email: string;
	image: string | null;
	isExternal: boolean;
	/** Presence — TRUE when the server observed a request on any of this
	 * user's non-revoked sessions within the last 5 min (backend
	 * `PublicUserDto.is_online`). Drives the presence dot overlay on
	 * `<UserAvatar>` / `<UserVignette>`. `false` when the caller's
	 * source didn't compute presence (a bare `resolveUser(id)` from
	 * pre-3-layer callers, an older backend build) — dot stays dark. */
	isOnline: boolean;
}

/** Subset of the backend `PublicUserDto` we consume here. */
interface PublicUserShape {
	id: string;
	username?: string | null;
	email?: string | null;
	image?: string | null;
	is_external: boolean;
	is_online?: boolean;
}

// id → in-flight/resolved lookup (the Promise is cached so concurrent callers
// for the same id share one request, and a `null` result isn't re-fetched).
const cache = new Map<string, Promise<ResolvedUser | null>>();

export function resolveUser(id: string): Promise<ResolvedUser | null> {
	const hit = cache.get(id);
	if (hit) return hit;

	const pending = (async (): Promise<ResolvedUser | null> => {
		try {
			const res = await apiFetch(`/api/users/${encodeURIComponent(id)}`, {
				credentials: 'same-origin'
			});
			if (!res.ok) return null;
			const u = (await res.json()) as PublicUserShape;
			return {
				id: u.id,
				name: u.username?.trim() || u.email || u.id,
				email: u.email ?? '',
				image: u.image ?? null,
				isExternal: u.is_external,
				isOnline: u.is_online ?? false
			};
		} catch {
			return null;
		}
	})();

	cache.set(id, pending);
	return pending;
}

/**
 * Prime the resolver cache from data the caller already has in hand.
 * When a list endpoint (e.g. `/api/admin/users`) ships full
 * `PublicUser` rows, the admin page seeds this cache in its load path
 * so every subsequent `resolveUser(id)` call (from `UserVignette`
 * mounted per-row) hits the cache synchronously — no per-row
 * `/api/users/{id}` follow-up fetch. Kills the N+1 that motivated
 * widening `/api/admin/users` to include the avatar (see
 * `docs/plan/userdto-refactor.md` § N+1).
 *
 * No-op when the id is already cached (in-flight or resolved). This
 * makes seeding safe to call unconditionally — never clobbers an
 * authoritative in-flight lookup with a stale seed.
 */
export function seedUser(u: {
	id: string;
	username?: string | null;
	email: string;
	image?: string | null;
	is_external: boolean;
	is_online?: boolean;
}): void {
	if (cache.has(u.id)) return;
	const resolved: ResolvedUser = {
		id: u.id,
		name: u.username?.trim() || u.email || u.id,
		email: u.email,
		image: u.image ?? null,
		isExternal: u.is_external,
		isOnline: u.is_online ?? false
	};
	cache.set(u.id, Promise.resolve(resolved));
}
