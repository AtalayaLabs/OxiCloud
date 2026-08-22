/**
 * Session store — the authenticated user and derived flags.
 *
 * Replaces the user-related fields of the original `app` state object
 * (isExternalUser, userHomeFolderId/Name). `isExternalUser` drives default
 * routing: externals (magic-link / OIDC-only / OCM recipients) have no home
 * folder and land on the shared-with-me view.
 */
import { bindDpopIfPossible, fetchMe, tryRefresh } from '$lib/api/endpoints/auth';
import { setLogoutInProgress } from '$lib/api/client';
import { hasSessionHint } from '$lib/api/csrf';
import { seedNonceFromCookie } from '$lib/auth/dpop-proof';
import { drives } from '$lib/stores/drives.svelte';
import type { PublicUser, SelfUser } from '$lib/api/types';
import { ensureActiveUser } from '$lib/utils/localStoragePrefs';

/**
 * Session store — the authenticated user and derived flags.
 *
 * Post the three-layer UserDto refactor (`docs/plan/userdto-refactor.md`),
 * `/api/auth/me` returns `SelfUser` (composed:
 * `SelfUser.full.user: PublicUser`). Two shorthand accessors keep every
 * existing consumer readable:
 *
 * - `session.user` → `PublicUser` (via `me.full.user`). Every callsite
 *   that read `session.user.username / email / id / role / image /
 *   is_external / given_name / family_name / is_online` keeps working.
 * - `session.me`   → full `SelfUser`. New code that needs self-only or
 *   admin-visible fields (`has_password`, `is_dpop_bound`, `active`,
 *   `ui_preferences`, `federation_kind`, `last_login_at`, quotas, …)
 *   reads through `session.me.full.foo` or `session.me.foo`.
 */
class SessionStore {
	/** Full `/api/auth/me` payload. Null when unauthenticated. */
	me = $state<SelfUser | null>(null);
	loaded = $state(false);
	homeFolderId = $state<string | null>(null);
	homeFolderName = $state<string | null>(null);

	/** Public-identity shorthand — same fields any authenticated caller
	 * can see. Every legacy `session.user.foo` read (username, email, id,
	 * role, image, is_external, given_name, family_name, is_online) still
	 * works via this derived accessor. */
	user = $derived<PublicUser | null>(this.me?.full.user ?? null);
	isExternalUser = $derived(this.me?.full.user.is_external ?? false);
	isAuthenticated = $derived(this.me !== null);
	/**
	 * TRUE when the backend has set `force_password_change_at_next_login`
	 * on this account — an admin picked a temporary password and the
	 * user MUST change it before doing anything else. Drives the root
	 * layout's mandatory-mode redirect: any protected route other than
	 * `/profile` bounces back until the flag flips to false.
	 *
	 * Set to false by default so an older backend that predates the
	 * flag (or a malformed `/me` response) doesn't accidentally
	 * quarantine every user.
	 */
	mustChangePassword = $derived(this.me?.force_password_change === true);

	/**
	 * Resolve the session once. Probes /api/auth/me; on 401 it makes a single
	 * refresh attempt and re-probes. Never redirects — the layout guard decides
	 * what to do with an unauthenticated result. Idempotent: subsequent calls
	 * return the cached result (so client-side navigation doesn't re-probe).
	 */
	async load(): Promise<SelfUser | null> {
		if (this.loaded) return this.me;
		// No JS-visible session hint ⇒ nothing to probe. The server sets
		// `oxicloud_csrf` alongside the HttpOnly session cookies and clears
		// it on logout, so a missing hint means no session. Skips the
		// doomed 2× /me + /refresh burst that would otherwise fire on
		// every first landing / post-logout re-mount with no cookies.
		if (!hasSessionHint()) {
			this.me = null;
			this.loaded = true;
			return null;
		}
		try {
			let me = await fetchMe();
			if (!me && (await tryRefresh())) {
				me = await fetchMe();
			}
			if (me) {
				this.setUser(me);
				// Post-redirect DPoP bind — catches OIDC / magic-link
				// flows whose server-side callback creates the session
				// UNBOUND (no way for the redirect to carry the JKT in
				// the callback body). Gate on `is_dpop_bound` so we
				// don't call the endpoint on every SPA load: password
				// login already binds at session-mint time, so `/me`
				// reports `true` on the very first request and skip
				// avoids the 409 `already_bound` reject that would
				// otherwise clutter the audit stream. Fire-and-forget
				// so a slow IndexedDB open doesn't stall app boot.
				if (me.is_dpop_bound === false) void bindDpopIfPossible();
			} else this.me = null;
		} catch {
			this.me = null;
		}
		this.loaded = true;
		return this.me;
	}

	/**
	 * Set the authenticated user AND run per-user localStorage cleanup
	 * (see `$lib/utils/localStoragePrefs::ensureActiveUser`). Direct
	 * `session.me = …` assignments skip the cleanup — always call
	 * `setUser` on login-flow entry points (form login, OIDC exchange,
	 * existing-session probe) so a switch-account flow inside the same
	 * tab observes the wipe.
	 */
	setUser(me: SelfUser): void {
		this.me = me;
		ensureActiveUser(me.full.user.id);
		// Any successful login clears the session-teardown gate. Without
		// this, a logout → login within the same SPA session leaves the
		// gate stuck at `true` — the login POST is exempted via
		// `AUTH_PRIMITIVES`, but the /me + /drives + … fetches the app
		// fires post-login would all abort with "Session terminated".
		setLogoutInProgress(false);
		// Consume the one-shot `oxicloud_dpop_nonce` cookie the login
		// response set. For POST logins (OPAQUE, legacy, magic-link
		// SPA-side, OIDC exchange) this is where the seed lands — the
		// hooks.client boot pass fires too early (before any login).
		// Redirect-flow logins are seeded at boot; both paths are safe
		// to double-run (idempotent, cookie is single-shot).
		seedNonceFromCookie();
	}

	/**
	 * Re-fetch the authenticated user from the server, bypassing the one-shot
	 * `load()` cache. Call after operations that change server-side user state —
	 * chiefly storage usage after uploads / deletes — so the UI reflects the new
	 * `storage_used_bytes` instead of the value cached at login. A transient
	 * failure leaves the current user untouched (never logs the UI out).
	 */
	async refresh(): Promise<void> {
		try {
			const me = await fetchMe();
			if (me) this.me = me;
		} catch {
			/* keep the existing user on a transient /api/auth/me failure */
		}
	}

	/**
	 * Resolve the caller's default personal drive's root folder — the landing
	 * point for `/files` and the `/` redirect. Externals (grant-only) have no
	 * personal drive, so this is skipped for them.
	 *
	 * Identifies the default via `default_for_user`, not folder name: users
	 * can rename "Personal" without breaking this lookup.
	 */
	async loadHomeFolder(): Promise<string | null> {
		if (this.homeFolderId) return this.homeFolderId;
		if (this.isExternalUser) return null;
		await drives.load();
		const def = drives.findDefault();
		if (def) {
			this.homeFolderId = def.root_folder_id;
			this.homeFolderName = def.name;
		}
		return this.homeFolderId;
	}

	reset(): void {
		this.me = null;
		this.homeFolderId = null;
		this.homeFolderName = null;
		// Mark the store as `loaded` so any subsequent `session.load()` —
		// notably the login page's existing-session probe and the root
		// layout's post-nav mount — short-circuits to `null` instead of
		// re-probing `/api/auth/me`. After an explicit logout we know for
		// a fact the session is gone; a probe would 401, the interceptor
		// would retry via /refresh (also 401), and `sessionExpiredHandler`
		// would divert to `/login?source=session_expired` — clobbering the
		// nice "logged out" landing. On a hard nav (natural expiry path)
		// module state is fresh and this flag is `false` again, so the
		// probe still runs there.
		this.loaded = true;
	}
}

export const session = new SessionStore();
