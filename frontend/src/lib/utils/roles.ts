/**
 * Account-level roles (`auth.users.role`), mirroring the backend
 * `UserRole`.
 *
 * NOT the same axis as `GrantRole` / `DriveRole` in `lib/api/types.ts`,
 * which describe what a subject may do with one shared resource. Both
 * vocabularies happen to contain the word "owner" and they mean different
 * things: a *drive* owner owns one drive, the *server* owner owns the
 * instance.
 */
export type AccountRole = 'owner' | 'admin' | 'user' | 'anonymous';

/**
 * Privilege order, mirroring `UserRole::rank()` on the backend. Kept as an
 * explicit map rather than array order so a reordering cannot silently
 * invert every comparison.
 */
const RANK: Record<AccountRole, number> = {
	anonymous: 0,
	user: 1,
	admin: 2,
	owner: 3
};

/**
 * An unknown role resolves to the LEAST privileged answer.
 *
 * The server may be newer than this bundle — during a rolling deploy, or
 * simply a stale tab — so an unrecognised role is one this client cannot
 * reason about. Guessing upward would unlock UI the user may not be
 * entitled to; guessing downward only hides it, and a reload fixes that.
 */
function rank(role: string | undefined | null): number {
	return RANK[role as AccountRole] ?? RANK.anonymous;
}

/**
 * True when `role` carries at least administrator authority.
 *
 * **Use this, never `role === 'admin'`.** The server owner outranks admin
 * and must reach every admin surface; an equality check would hide the
 * admin area from the one account that certainly owns it. That was a real
 * regression when `owner` was introduced — the sidebar link, the command
 * palette entry and the profile page all tested for equality.
 */
export function isAtLeastAdmin(role: string | undefined | null): boolean {
	return rank(role) >= RANK.admin;
}

/**
 * True for the server owner specifically — the one account no
 * administrator can act on.
 *
 * For "may this person see admin things?" use {@link isAtLeastAdmin}.
 * This is for the narrower question of badges and owner-only affordances
 * such as transferring ownership.
 */
export function isOwner(role: string | undefined | null): boolean {
	return role === 'owner';
}
