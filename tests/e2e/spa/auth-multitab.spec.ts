// =============================================================
// SPA · cross-tab logout — Gate 8 broadcast behaviour
// =============================================================
// When tab A logs out, every OTHER tab of the same origin that
// shares the browser profile must observe the session is gone
// and route itself to /login — the UX plan calls this Gate 8.
//
// The mechanism:
//   * logout() in auth.ts fires `broadcastSessionCleared()`
//     (`$lib/auth/session-broadcast`) on a BroadcastChannel.
//   * +layout.svelte's `onSessionCleared(…)` subscription in
//     onMount wipes the session store and calls `goto('/login')`.
//
// Why Playwright is the right layer:
//   * Vitest cannot exercise cross-tab/BroadcastChannel reliably —
//     jsdom's BroadcastChannel is a no-op shim.
//   * Hurl is wire-only and can't reason about multiple SPA
//     instances.
//   * Only a real browser with two Pages in one BrowserContext
//     actually shares the channel.
//
// Lives in its own file so a single-tab regression in auth.spec.ts
// can't poison this suite's state and vice-versa.

import { test, expect, uiLogin, TEST_ADMIN } from './coverage-helpers';

test.describe('SPA · cross-tab logout', () => {
	test('logout in one tab routes the other to /login', async ({ browser }) => {
		// Two pages in the SAME context so they share cookies, IndexedDB,
		// localStorage, AND the BroadcastChannel. `browser.newContext()`
		// would isolate them from each other — different browser profile
		// semantics, channel does not propagate.
		const context = await browser.newContext();
		const tabA = await context.newPage();
		const tabB = await context.newPage();

		// Log in tab A — tab B sees the shared cookie jar immediately on
		// its first navigation; both tabs land on /files.
		await uiLogin(tabA, TEST_ADMIN);
		await tabB.goto('/files');
		await expect(tabB).toHaveURL(/\/files/);
		await expect(tabB.getByTestId('appshell-user-menu-btn')).toBeVisible();

		// Log out from tab A via the user menu — triggers
		// `broadcastSessionCleared()` after the HTTP /logout POST
		// resolves (success OR failure — unconditional per logout()'s
		// three-phase design).
		await tabA.getByTestId('appshell-user-menu-btn').click();
		await tabA.getByTestId('appshell-user-menu-logout-btn').click();
		await tabA.waitForURL('**/login**', { timeout: 15_000 });

		// Tab B must observe the broadcast and route itself to /login
		// WITHOUT any user interaction. 10s is generous — the handoff
		// is normally sub-500ms (BroadcastChannel is in-process, then
		// `goto('/login')` fires on the next tick).
		await tabB.waitForURL('**/login**', { timeout: 10_000 });
		await expect(tabB.getByTestId('login-form')).toBeVisible();

		await context.close();
	});
});
