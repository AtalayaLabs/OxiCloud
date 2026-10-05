/**
 * Admin-only advisory flags surfaced on the global banner stack.
 *
 * Shared between the admin dashboard loader (which actually fetches
 * `/api/admin/dashboard` and knows the values) and `AppShell` (which
 * renders the banner conditionally when the operator is on an
 * `/admin/*` route). Keeps AppShell from issuing its own
 * `/api/admin/dashboard` request — the dashboard is already fetched
 * on every admin tab visit, so piggy-backing on that cheaper.
 *
 * Admin-only visibility is enforced by the AppShell render — not by
 * this store. A non-admin user whose session isn't allowed to hit
 * `/api/admin/dashboard` never gets the setter called, so the flags
 * stay at their defaults; and even if they did (via a legacy value),
 * `AppShell` gates the banner on `isAdminSection && isAdmin` before
 * rendering.
 *
 * Not currently persisted across page reloads — the admin page's
 * tab-switch effect calls `loadDashboard()` as the first thing on
 * entering admin, so the flags re-populate within one network round-
 * trip of landing on an admin URL. Fine for the advisory-banner
 * use case; promote to localStorage if a "nag immediately on
 * reload" UX is ever asked for.
 */

interface AdminAdvisoryState {
	/** `storage_cache_recommended` from the dashboard DTO. True when
	 *  the active backend is remote (S3 / Azure) AND the local disk
	 *  cache is disabled (`OXICLOUD_STORAGE_CACHE_ENABLED=false`).
	 *  Drives the "Enable the local blob cache" advisory banner. */
	cacheRecommended: boolean;
	/** `notification_sink_missing` from the dashboard DTO. True when
	 *  the server has NEITHER `OXICLOUD_JOBS_NOTIFY_EMAIL_TO` nor
	 *  `OXICLOUD_WEBHOOK_URL` configured — job-failure findings have
	 *  nowhere to go and ops would miss them unless they're actively
	 *  checking `/admin/jobs`. Drives the "configure a notification
	 *  sink" advisory banner. */
	notificationSinkMissing: boolean;
}

const state = $state<AdminAdvisoryState>({
	cacheRecommended: false,
	notificationSinkMissing: false
});

/** Reactive accessor — subscribe from a Svelte component via
 *  `adminAdvisories()` inside a template or $derived. */
export function adminAdvisories(): AdminAdvisoryState {
	return state;
}

/** Called from the admin page's `loadDashboard()` after it resolves
 *  — mirrors the fresh `storage_cache_recommended` into the shared
 *  reactive store so AppShell's banner stack can read it without
 *  issuing a second `/api/admin/dashboard` request. */
export function setCacheRecommended(value: boolean): void {
	if (state.cacheRecommended !== value) state.cacheRecommended = value;
}

/** Mirror the `notification_sink_missing` flag — see field doc. */
export function setNotificationSinkMissing(value: boolean): void {
	if (state.notificationSinkMissing !== value) state.notificationSinkMissing = value;
}
