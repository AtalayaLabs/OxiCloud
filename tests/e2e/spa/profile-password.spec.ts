// =============================================================
// SPA · change password — full end-to-end round-trip
// =============================================================
// Pins the single cross-layer invariant the Phase-4 migration of
// `profile.ts::changePassword` can't verify at any lower layer:
//
//   1. User changes password through /profile.
//   2. Logout fires + lands on /login.
//   3. Logging in with the OLD password MUST fail (session rejects).
//   4. Logging in with the NEW password MUST succeed and land on
//      /files.
//
// Why this test lives at the e2e layer:
//   * Vitest Pin 4 (`profile.test.ts`) asserts the WRAPPER does not
//     sync OPAQUE when the SDK rejects the change. That's the FE
//     promise; it mocks the SDK and cannot witness the actual
//     backend transition — can't tell that the server stored the
//     new hash, or that OPAQUE's envelope was re-minted under the
//     new passphrase.
//   * Hurl covers the wire shape of the PUT /change-password call
//     + the single-request error branches (admin_user_ops.hurl), but
//     can't drive a full logout → login cycle through the SPA to
//     prove the new credential actually works through the primary
//     auth path.
//   * The only regression this test uniquely catches: wrapper
//     migration subtly breaks the ordering (opaque sync fires with
//     NEW password before the server-side change actually
//     committed → split state → user can log in with NEITHER
//     old nor new password). Vitest mocks can't produce that
//     split; only a real backend can.
//
// Follows the pattern + helper reuse from `auth.spec.ts` and
// `session-refresh.spec.ts`. Lives in its own file so a login
// regression in one spec can't poison this suite's state.

import { test, expect, uiLogin, TEST_ADMIN } from './coverage-helpers';

// The test runs through 3 full login cycles + 2 profile forms,
// which is more work than the suite's default 30s per-test
// budget comfortably fits. Bump to 90s so timing slack isn't
// what fails this test when the server is warming the OPAQUE
// envelope or the SPA is doing its full session.load after
// each navigation.
test.setTimeout(90_000);

// Not using TEST_ADMIN's password across the two sign-in cycles
// because this test MUTATES that credential. Rotate to a unique
// value, verify both branches of the login, then rotate back —
// leaves the shared TEST_ADMIN fixture usable by every other spec.
const ROTATED_PASSWORD = 'RotatedTestPassword2!';

test.describe('SPA · change password end-to-end', () => {
	test('new password works for the next login; old password is rejected', async ({ page }) => {
		// Phase 1 — login with the baseline password, navigate to /profile.
		await uiLogin(page);
		await page.getByTestId('appshell-user-menu-btn').click();
		await page.getByTestId('appshell-user-menu-profile-item').click();
		await expect(page).toHaveURL(/\/profile/);
		await expect(page.getByTestId('profile-password-form')).toBeVisible();

		// Phase 2 — fill the three-field form and submit. The three
		// testids (`profile-current-password-input` /
		// `profile-new-password-input` / `profile-confirm-password-input`)
		// are stable in VITE_E2E builds; the confirm field is a
		// client-side check (matches new-password) before the wrapper
		// is called.
		await page.getByTestId('profile-current-password-input').fill(TEST_ADMIN.password);
		await page.getByTestId('profile-new-password-input').fill(ROTATED_PASSWORD);
		await page.getByTestId('profile-confirm-password-input').fill(ROTATED_PASSWORD);
		await page.getByTestId('profile-update-password-btn').click();

		// Give the backend a beat to commit the change-password
		// transaction + re-mint OPAQUE under the new passphrase before
		// we log out. The wrapper awaits both the SDK call AND the
		// dynamic `syncOpaqueEnvelope` import; by the time the submit
		// button is re-enabled or the page re-renders, both are done.
		// Loose on which visible signal — assert on the next step
		// (login with new password succeeds) which is the real test.
		await page.waitForTimeout(500);

		// Phase 3 — log out, land on /login.
		await page.getByTestId('appshell-user-menu-btn').click();
		await page.getByTestId('appshell-user-menu-logout-btn').click();
		await page.waitForURL('**/login**', { timeout: 15_000 });
		await expect(page.getByTestId('login-form')).toBeVisible();

		// Phase 4 — the OLD password MUST now fail. If it still works,
		// the backend never committed the new hash (or OPAQUE
		// envelope is pointing at the old password still, which the
		// Phase-2 silent-migration path would fix on next login —
		// still a regression because the explicit change was supposed
		// to be the primary migration lane).
		await page.getByTestId('login-username-input').fill(TEST_ADMIN.username);
		await page.getByTestId('login-password-input').fill(TEST_ADMIN.password);
		await page.getByTestId('login-submit-btn').click();
		await expect(page.locator('.auth-error[role="alert"]')).toBeVisible({ timeout: 10_000 });
		// Still on /login — no redirect into /files.
		await expect(page.getByTestId('login-form')).toBeVisible();

		// Phase 5 — the NEW password MUST succeed. This is the
		// positive side of the invariant.
		await page.getByTestId('login-username-input').fill(TEST_ADMIN.username);
		await page.getByTestId('login-password-input').fill(ROTATED_PASSWORD);
		await page.getByTestId('login-submit-btn').click();
		await page.waitForURL('**/files**', { timeout: 15_000 });
		await expect(page.getByTestId('appshell-logo-link')).toBeVisible();

		// Phase 6 — rotation-back through the UI. Deployment runs with
		// `OXICLOUD_DPOP_MODE=required`, and only requests driven
		// through the page (SW-signed) or through `apiFetch` (page-
		// context signed) carry the DPoP header. `page.request.*` is
		// a Node-side HTTP client that does NOT share the SW's
		// signing — the server 401s a bare `page.request.put` even
		// with the cookies attached. The UI path shares the full
		// auth context (session cookies + CSRF + DPoP proof) and is
		// the ONLY way to drive this from the test side on an
		// OPAQUE+DPoP-required deployment. Adds ~5s to the test
		// budget; Phases 1-5 fit comfortably at ~60s so we have
		// margin under the 90s cap.
		//
		// If THIS step fails, the shared TEST_ADMIN is left in the
		// ROTATED state and the next test run of this file fails at
		// Phase 1's uiLogin. That failure surfaces visibly rather
		// than silently poisoning other specs.
		await page.getByTestId('appshell-user-menu-btn').click();
		await page.getByTestId('appshell-user-menu-profile-item').click();
		await expect(page.getByTestId('profile-password-form')).toBeVisible();
		await page.getByTestId('profile-current-password-input').fill(ROTATED_PASSWORD);
		await page.getByTestId('profile-new-password-input').fill(TEST_ADMIN.password);
		await page.getByTestId('profile-confirm-password-input').fill(TEST_ADMIN.password);
		await page.getByTestId('profile-update-password-btn').click();
		// Belt-and-braces: wait for the OPAQUE re-mint to complete
		// before the test ends. Without this, the test may finish
		// while `syncOpaqueEnvelope` is still mid-flight, and the
		// next test run's seedAdmin + uiLogin could race against
		// a half-written envelope. 500ms matches Phase 2.
		await page.waitForTimeout(500);
	});
});
