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
import { drives } from '$lib/stores/drives.svelte';
import type { User } from '$lib/api/types';
import { ensureActiveUser } from '$lib/utils/localStoragePrefs';

class SessionStore {
	user = $state<User | null>(null);
	loaded = $state(false);
	homeFolderId = $state<string | null>(null);
	homeFolderName = $state<string | null>(null);

	isExternalUser = $derived(this.user?.is_external ?? false);
	isAuthenticated = $derived(this.user !== null);
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
	mustChangePassword = $derived(this.user?.force_password_change === true);

	/**
	 * Resolve the session once. Probes /api/auth/me; on 401 it makes a single
	 * refresh attempt and re-probes. Never redirects — the layout guard decides
	 * what to do with an unauthenticated result. Idempotent: subsequent calls
	 * return the cached result (so client-side navigation doesn't re-probe).
	 */
	async load(): Promise<User | null> {
		if (this.loaded) return this.user;
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
			} else this.user = null;
		} catch {
			this.user = null;
		}
		this.loaded = true;
		return this.user;
	}

	/**
	 * Set the authenticated user AND run per-user localStorage cleanup
	 * (see `$lib/utils/localStoragePrefs::ensureActiveUser`). Direct
	 * `session.user = …` assignments skip the cleanup — always call
	 * `setUser` on login-flow entry points (form login, OIDC exchange,
	 * existing-session probe) so a switch-account flow inside the same
	 * tab observes the wipe.
	 */
	setUser(user: User): void {
		this.user = user;
		ensureActiveUser(user.id);
		// Any successful login clears the session-teardown gate. Without
		// this, a logout → login within the same SPA session leaves the
		// gate stuck at `true` — the login POST is exempted via
		// `AUTH_PRIMITIVES`, but the /me + /drives + … fetches the app
		// fires post-login would all abort with "Session terminated".
		setLogoutInProgress(false);
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
			if (me) this.user = me;
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
		this.user = null;
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
