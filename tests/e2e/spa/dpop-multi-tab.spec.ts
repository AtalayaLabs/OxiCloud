import { test, expect, uiLogin } from './coverage-helpers';

/**
 * SPA · DPoP multi-tab coverage — Gate 8 follow-up.
 *
 * IndexedDB, cookies, and `BroadcastChannel` are shared across every
 * tab of a single Playwright `BrowserContext`. That's the correct
 * shape for testing the multi-tab DPoP invariants:
 *
 *   * shared keypair — a second tab opened after login already sees
 *     the first tab's persisted keypair via IndexedDB, so both tabs
 *     sign requests with the same JWK thumbprint (`dpop_jkt`) →
 *     server accepts both under a single bound session.
 *   * `BroadcastChannel('oxicloud-session-cleared')` — logout on
 *     one tab must cause the other tab's root layout to reset the
 *     session store and redirect to `/login` synchronously, without
 *     waiting for a network round trip to 401. See
 *     `frontend/src/lib/auth/session-broadcast.ts`.
 *
 * Runs under `OXICLOUD_AUTH_OPAQUE_MODE=migrate` +
 * `OXICLOUD_DPOP_MODE=required` inherited from
 * `tests/common/server.env` — so the actual OPAQUE login handshake
 * fires (WASM client → KE1 → KE3) and every subsequent request
 * carries a DPoP proof the middleware verifies.
 */
test.describe('SPA · DPoP multi-tab', () => {
  test('a second tab shares the first tab\'s DPoP keypair (IndexedDB)', async ({ context }) => {
    const tabA = await context.newPage();
    await uiLogin(tabA);
    // Sanity: tab A landed on an authenticated view.
    await expect(tabA.getByTestId('appshell-logo-link')).toBeVisible();

    // Second tab in the same context — cookies + IndexedDB shared.
    const tabB = await context.newPage();
    // Deep-link straight into an authenticated route. If the session
    // cookie is shared (it is — cookies are per-context) AND the
    // DPoP keypair is shared (it is — IndexedDB is per-origin per-
    // context), tab B loads without redirecting to /login.
    await tabB.goto('/files');
    await expect(tabB.getByTestId('appshell-logo-link')).toBeVisible({ timeout: 15_000 });

    // Both tabs' auth store agrees on the same user id — proves the
    // shared cookie + shared keypair combination actually authorised
    // an API call under DPoP=required against a bound session.
    const [uidA, uidB] = await Promise.all([
      tabA.evaluate(async () => {
        const res = await fetch('/api/auth/me', { credentials: 'same-origin' });
        return res.ok ? ((await res.json()) as { id: string }).id : null;
      }),
      tabB.evaluate(async () => {
        const res = await fetch('/api/auth/me', { credentials: 'same-origin' });
        return res.ok ? ((await res.json()) as { id: string }).id : null;
      })
    ]);
    expect(uidA).not.toBeNull();
    expect(uidB).toBe(uidA);
  });

  test('logging out on one tab redirects the other via BroadcastChannel', async ({ context }) => {
    const tabA = await context.newPage();
    await uiLogin(tabA);

    const tabB = await context.newPage();
    await tabB.goto('/files');
    await expect(tabB.getByTestId('appshell-logo-link')).toBeVisible({ timeout: 15_000 });

    // Log out from tab A. Bypass the user-menu UI (which drifts as
    // the shell markup evolves) — call `/api/auth/logout` directly
    // then post to the BroadcastChannel by hand. Same shape as
    // `endpoints/auth.ts::logout()` — the two side-effects the SPA
    // does after a successful server logout are (a) wipe DPoP
    // state (moot here since tab A is about to close/redirect) and
    // (b) broadcast, which is exactly what we simulate.
    await tabA.evaluate(async () => {
      const csrf =
        document.cookie
          .split(';')
          .map((c) => c.trim())
          .find((c) => c.startsWith('oxicloud_csrf='))
          ?.slice('oxicloud_csrf='.length) ?? '';
      const res = await fetch('/api/auth/logout', {
        method: 'POST',
        credentials: 'same-origin',
        headers: { 'Content-Type': 'application/json', 'x-csrf-token': csrf },
        body: '{}'
      });
      if (!res.ok) throw new Error(`logout returned ${res.status}`);
      new BroadcastChannel('oxicloud-session-cleared').postMessage({
        kind: 'session_cleared',
        at: Date.now()
      });
    });

    // Tab B should navigate to /login on its own. No API call
    // needed — the BroadcastChannel handler in the root layout
    // does session.reset() + goto('/login').
    await tabB.waitForURL('**/login**', { timeout: 5_000 });
  });
});
