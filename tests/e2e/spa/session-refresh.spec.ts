// =============================================================
// SPA · session refresh survives a deleted access cookie
// =============================================================
// Regression pin for the Phase-4 fetchMe/tryRefresh migration:
// a user whose `oxicloud_access` cookie is cleared (expired JWT,
// stale tab, manual delete) must stay logged in via the still-
// valid refresh cookie — session.load's /me (401) → /refresh (200)
// → /me (200) chain re-mints the access cookie and the SPA
// continues onto `/files` without ever surfacing the login page.
//
// The regression this guards against: an earlier Phase-4 commit
// routed /me and /refresh through Hey API's `throwOnError: true`
// path, which throws the parsed BODY of a non-ok response (not a
// wrapper object). fetchMe tried to read `.response.status` on
// that body and found undefined, so it rethrew; session.load's
// catch swallowed it and returned null; layout routed to /login.
// The server HAD minted a fresh access cookie on /refresh; the
// SPA just never used it.
//
// Lives in e2e because the regression is cross-layer (SDK shape
// × session store × layout guard × browser cookie jar); nothing
// at any single lower layer alone catches it. Per the
// "every regression gets its own coverage" rule in AGENTS.md.

import { test, expect, uiLogin } from './coverage-helpers';

test.describe('SPA · session refresh survives a deleted access cookie', () => {
	test('reload with only oxicloud_access removed stays logged in', async ({ page, context }) => {
		// 1. Clean login — all four cookies live after this call:
		//    oxicloud_access, oxicloud_refresh, oxicloud_csrf, oxicloud_dpop_nonce.
		await uiLogin(page);
		await expect(page).toHaveURL(/\/files/);
		await expect(page.getByTestId('appshell-user-menu-btn')).toBeVisible();

		// 2. Simulate "access token expired or deleted" by clearing
		//    ONLY the access cookie — the refresh cookie stays, which
		//    is what the real-world expired-JWT case looks like.
		await context.clearCookies({ name: 'oxicloud_access' });

		// 3. Reload. session.load() in the layout fires:
		//       fetchMe#1 → 401 (no access cookie)
		//       tryRefresh → 200 (refresh cookie still valid)
		//       fetchMe#2 → 200 (fresh access cookie from /refresh)
		//    User should land back on /files, NOT on /login.
		await page.reload();

		// 4. Assert still on /files, app shell visible, no login redirect.
		await expect(page).toHaveURL(/\/files/);
		await expect(page.getByTestId('appshell-user-menu-btn')).toBeVisible();
		// Login form must NOT be present — any regression that routes
		// back through /login would show it.
		await expect(page.getByTestId('login-form')).toHaveCount(0);

		// 5. The user-menu dropdown shows the admin-only entry, which
		//    the AppShell gates on `isAdmin === true` — a flag that
		//    comes from fetchMe#2's SelfUser payload. If the pre-reload
		//    session had been reused from any cache without a fresh
		//    probe, the role flag would be absent and the admin-item
		//    testid wouldn't render. Keeping the proof role-based (not
		//    username text) also decouples this pin from the exact
		//    text displayed in the menu — i18n changes or future
		//    rename of the username row never spuriously break it.
		await page.getByTestId('appshell-user-menu-btn').click();
		await expect(page.getByTestId('appshell-user-menu-admin-item')).toBeVisible();
	});

	test('reload with no cookies does not fire /me on bootstrap', async ({ page, context }) => {
		// Mirror of the test above. Where the previous one covers "access
		// deleted, refresh survives → stay logged in via /refresh", this
		// one covers the fully-logged-out boot path: ALL cookies cleared.
		//   * session.load must bail on `hasSessionHint()` (which reads
		//     the JS-visible `oxicloud_csrf` cookie — now absent) — so
		//     /api/auth/me MUST NOT fire. A /me firing is a regression:
		//     `hasSessionHint` leaked, or session.load forgot to gate.
		//   * `/api/auth/refresh` is NOT asserted absent here. On a
		//     fully-logged-out reload, some non-auth-primitive /api/*
		//     bootstrap probe (e.g. config/providers if any ever go
		//     authed, or a lingering interceptor) may 401 and the
		//     transport's refresh-and-retry path at `transport.ts:109`
		//     legitimately fires one `/refresh` to detect
		//     session-expired. That IS the correct contract — a single
		//     refresh is cheap and the server tells us "nope" cleanly.
		//     The test's job is to guard `session.load`, not the
		//     transport's own 401-handling.
		//   * SPA lands on /login cleanly, no toast cascade.
		await uiLogin(page);
		await context.clearCookies();

		const meRequests: string[] = [];
		page.on('request', (r) => {
			if (r.url().includes('/api/auth/me')) {
				meRequests.push(new URL(r.url()).pathname);
			}
		});

		// This reload is EXPECTED to be superseded. With no cookies a
		// bootstrap `/api/*` call 401s, the transport retries once through
		// `/refresh` (also 401), and `sessionExpiredHandler` then
		// HARD-navigates — `transport.ts:317` assigns
		// `window.location.href = '/login?source=session_expired'`. A full
		// navigation aborts the in-flight reload, so the reload promise
		// rejects with `net::ERR_ABORTED; maybe frame was detached?`
		// whenever that 401 chain wins the race. The saved failure snapshot
		// showed the login screen with the session-expired notice already
		// painted — the app is right, the await was the wrong shape.
		//
		// `waitUntil: 'commit'` was the first attempt at this and is NOT
		// enough: it narrows the window (the abort has to land before the
		// commit rather than before the load) without closing it. Measured
		// on this spec with `--repeat-each=20`: 4 failures in 20 runs.
		//
		// So tolerate exactly the one rejection the design guarantees, and
		// rethrow everything else — a reload that fails for any other
		// reason must still fail the test rather than quietly turn into a
		// URL-assertion timeout. The assertions below do the real waiting.
		//
		// Note the reload above (access cookie deleted, refresh cookie kept)
		// deliberately does NOT use this: that case must stay on /files, so
		// a plain `load` is exactly what it should be asserting.
		// `commit` was not early enough either: the hard navigation can abort
		// the reload BEFORE it commits, and then even that await throws the
		// same `ERR_ABORTED`.
		//
		// This is PLAYWRIGHT's navigation semantics, not an app or
		// test-design fault: `page.reload()` / `page.goto()` reject on
		// `net::ERR_ABORTED` even when the abort is caused by the page
		// legitimately navigating elsewhere, and there is no "tolerate being
		// superseded" option — no `waitUntil: 'none'`. So no value of
		// `waitUntil` fixes this; the previous attempt moved `load` → `commit`
		// and only narrowed the window. Catching that one error is the
		// accepted workaround.
		//
		// Swallowed only when it IS that abort; anything else still fails,
		// and the assertions below do the waiting.
		await page.reload({ waitUntil: 'commit' }).catch((e: unknown) => {
			if (!/ERR_ABORTED|frame was detached/.test(String(e))) throw e;
		});

		await expect(page).toHaveURL(/\/login/);
		await expect(page.getByTestId('login-form')).toBeVisible();
		// Critical: no /me on reload. `hasSessionHint()`'s early return
		// in session.load keeps the probe-and-refresh chain unfired at
		// the layer this test owns.
		expect(meRequests).toEqual([]);
	});
});
