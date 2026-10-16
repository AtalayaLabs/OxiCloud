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

import { test, expect, uiLogin, TEST_ADMIN } from './coverage-helpers';

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

		// 5. The username on the user menu is the one we logged in as —
		//    proves fetchMe#2 landed a real SelfUser payload, not a stale
		//    cached value from the pre-reload session.
		await page.getByTestId('appshell-user-menu-btn').click();
		await expect(page.getByTestId('appshell-user-menu-username')).toContainText(
			TEST_ADMIN.username
		);
	});
});
