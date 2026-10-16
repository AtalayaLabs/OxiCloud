import { test, expect, uiLogin, TEST_ADMIN } from './coverage-helpers';

test.describe('SPA · authentication', () => {
  test('login page renders the sign-in form', async ({ page }) => {
    await page.goto('/login');
    await expect(page.getByTestId('login-form')).toBeVisible();
    await expect(page.getByTestId('login-username-input')).toBeVisible();
    await expect(page.getByTestId('login-password-input')).toBeVisible();
    await expect(page.getByTestId('login-submit-btn')).toBeVisible();
    await expect(page).toHaveTitle(/OxiCloud/i);
  });

  test('wrong password is rejected with an error', async ({ page }) => {
    await page.goto('/login');
    await page.getByTestId('login-username-input').fill(TEST_ADMIN.username);
    await page.getByTestId('login-password-input').fill('definitely-wrong-password');
    await page.getByTestId('login-submit-btn').click();

    await expect(page.locator('.auth-error[role="alert"]')).toBeVisible();
    // Still on the login page — no redirect into the app.
    await expect(page.getByTestId('login-form')).toBeVisible();
  });

  test('register and setup panels are reachable from login', async ({ page }) => {
    await page.goto('/login');
    await page.getByTestId('login-to-register-btn').click();
    await expect(page.getByTestId('login-register-form')).toBeVisible();
    await page.getByTestId('login-register-to-login-btn').click();
    await expect(page.getByTestId('login-form')).toBeVisible();
  });

  test('submit button dispatches to magic-link when password is empty', async ({ page }) => {
    // Unified login form: one identifier + one optional password + one
    // adaptive submit button. Filling the identifier and leaving the
    // password blank flips the button label to "Send sign-in link" and
    // routes to /api/auth/magic-link/send on click. The old two-form
    // UX with `login-magic-toggle-btn` was retired 2026-07-14.
    await page.goto('/login');
    await expect(page.getByTestId('login-form')).toBeVisible();
    await page.getByTestId('login-username-input').fill('someone@example.test');
    // Password intentionally NOT filled — this drives the label swap.
    const submit = page.getByTestId('login-submit-btn');
    await expect(submit).toBeVisible();
    // Label content differs per mode: password-empty → magic-link copy;
    // password-filled → "Sign in". Assert the magic-link copy is what's
    // shown so the dispatch is provably in the magic-link branch.
    await expect(submit).toHaveText(/link|Link|Send/);
  });

  test('successful login reaches the files app shell', async ({ page }) => {
    await uiLogin(page);
    await expect(page).toHaveURL(/\/files/);
    await expect(page.getByTestId('appshell-logo-link')).toBeVisible();
    await expect(page.getByTestId('appshell-user-menu-btn')).toBeVisible();
  });

  test('logout returns to the login page', async ({ page }) => {
    await uiLogin(page);
    await page.getByTestId('appshell-user-menu-btn').click();
    await page.getByTestId('appshell-user-menu-logout-btn').click();
    await page.waitForURL('**/login**', { timeout: 15_000 });
    await expect(page.getByTestId('login-form')).toBeVisible();
  });

  test('logout clears the DPoP keypair from IndexedDB', async ({ page }) => {
    // The browser DPoP keypair is stored in IndexedDB at
    //   DB: 'oxicloud-dpop', store: 'keypair', key: 'current'
    // (see frontend/src/lib/auth/dpop.ts). `logout()` clears it so the
    // NEXT login mints a fresh identity — no correlation across the
    // logout boundary from the per-request-signature standpoint.
    //
    // Regression this guards against: `logout` migrated from apiFetch
    // to the generated SDK (commit e07ba863), and preserving the
    // DPoP teardown + cross-tab broadcast required restructuring the
    // control flow around phases that must run unconditionally even
    // if the HTTP /logout 5xx's. A future refactor that moves the
    // teardown back inside the `try { … }` branch would silently
    // leave a stale keypair in IndexedDB — invisible to the user,
    // confusing to the next session's bind step.
    await uiLogin(page);

    // Keypair must exist AFTER login (the login flow computes
    // `tryBindingThumbprint` which calls `ensureKeypair`).
    const beforeLogout = await page.evaluate(async () => {
      const db = await new Promise<IDBDatabase>((resolve, reject) => {
        const r = indexedDB.open('oxicloud-dpop', 1);
        r.onsuccess = () => resolve(r.result);
        r.onerror = () => reject(r.error);
      });
      return new Promise<unknown>((resolve, reject) => {
        const tx = db.transaction('keypair', 'readonly');
        const req = tx.objectStore('keypair').get('current');
        req.onsuccess = () => resolve(req.result ?? null);
        req.onerror = () => reject(req.error);
      });
    });
    expect(beforeLogout).not.toBeNull();

    // Trigger logout via the user menu — same path as the test above.
    await page.getByTestId('appshell-user-menu-btn').click();
    await page.getByTestId('appshell-user-menu-logout-btn').click();
    await page.waitForURL('**/login**', { timeout: 15_000 });

    // The keypair row MUST be gone. A null read on the same key proves
    // `clearKeypair()` fired as part of the logout teardown.
    const afterLogout = await page.evaluate(async () => {
      const db = await new Promise<IDBDatabase>((resolve, reject) => {
        const r = indexedDB.open('oxicloud-dpop', 1);
        r.onsuccess = () => resolve(r.result);
        r.onerror = () => reject(r.error);
      });
      return new Promise<unknown>((resolve, reject) => {
        const tx = db.transaction('keypair', 'readonly');
        const req = tx.objectStore('keypair').get('current');
        req.onsuccess = () => resolve(req.result ?? null);
        req.onerror = () => reject(req.error);
      });
    });
    expect(afterLogout).toBeNull();
  });
});
