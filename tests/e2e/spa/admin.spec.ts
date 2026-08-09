import * as path from 'path';
import { test, expect } from './coverage-helpers';
import { apiLogin, apiAdminCreateUser } from './helpers';

const PLUGIN_ZIP = path.join(__dirname, '..', 'fixtures', 'plugin-hello.zip');

/**
 * Admin panel — walk every tab and open a couple of forms. The admin route is
 * one of the largest source files; exercising each tab pulls in a lot of it
 * plus the admin endpoint module.
 */
test.beforeEach(async ({ page }) => {
  await apiLogin(page);
});

test('walk every admin tab', async ({ page }) => {
  await page.goto('/admin');
  // Dashboard is the default section on `/admin` (bare path).
  // Assert on the sidebar entry (now the source of admin navigation)
  // and on content that only renders when Dashboard is active.
  await expect(page.getByTestId('appshell-nav-admin-dashboard-link')).toBeVisible({
    timeout: 15_000
  });
  await expect(page.getByTestId('admin-dashboard-registration-checkbox')).toBeVisible();

  await page.goto('/admin/users');
  await expect(page.getByTestId('admin-users-create-btn')).toBeVisible();

  await page.goto('/admin/oidc');
  await expect(page.getByTestId('admin-oidc-form')).toBeVisible();

  await page.goto('/admin/storage');
  // Storage panel is now a card-per-entry list (multi-entry config
  // landed with `feat(storage): add multi entry in config`). The old
  // single `admin-storage-form` was retired along with the in-UI save
  // flow — backend is env-configured per entry, and the panel exposes
  // Test / Blob-consistency / Migrate actions per card.
  await expect(page.getByTestId('admin-storage-entries-list')).toBeVisible();

  await page.goto('/admin/smtp');
  await expect(page.getByTestId('admin-smtp-send-btn')).toBeVisible();

  await page.goto('/admin/plugins');
  // Plugins panel content is conditional; assert the URL landed
  // on the plugins section (proves routing worked; no content
  // guarantee).
  await expect(page).toHaveURL(/\/admin\/plugins$/);
});

test('open the create-user form', async ({ page }) => {
  await page.goto('/admin/users');
  await page.getByTestId('admin-users-create-btn').click();
  await expect(page.getByTestId('admin-create-user-form')).toBeVisible({ timeout: 15_000 });
});

test('storage tab: test button probes the active entry', async ({ page }) => {
  await page.goto('/admin/storage');
  await expect(page.getByTestId('admin-storage-entries-list')).toBeVisible({ timeout: 15_000 });
  // Test button is safe — the handler does a read/write/delete probe on the
  // backend without mutating any user data. There's always at least one entry
  // in the E2E env (the default `local` storage entry seeded from example.env).
  const testBtn = page.locator('[data-testid^="admin-storage-test-"]').first();
  await testBtn.click();
  // Result footer renders (either status--ok or status--error) once the probe
  // completes; either outcome exercises the endpoint + render path.
  await expect(page.locator('.entry-card__test-result').first()).toBeVisible({ timeout: 10_000 });
});

test('oidc tab: toggle enabled and fill issuer', async ({ page }) => {
  await page.goto('/admin/oidc');
  await expect(page.getByTestId('admin-oidc-form')).toBeVisible({ timeout: 15_000 });
  await page.getByTestId('admin-oidc-enabled-checkbox').check().catch(() => {});
  await page
    .getByTestId('admin-oidc-issuer-input')
    .fill('https://example.test/issuer', { timeout: 2_000 })
    .catch(() => {});
});

function uniqUser(): string {
  return `u${Date.now()}${Math.floor(Math.random() * 1e6)}`;
}

/**
 * Create a user via the admin form, dismiss the (modal-backdrop) create dialog,
 * and return a locator for that user's table row.
 */
async function createUserRow(
  page: import('@playwright/test').Page,
  uname: string,
): Promise<ReturnType<import('@playwright/test').Page['locator']>> {
  await page.goto('/admin/users');
  await page.getByTestId('admin-users-create-btn').click();
  await expect(page.getByTestId('admin-create-user-form')).toBeVisible({ timeout: 15_000 });
  await page.getByTestId('admin-create-user-username-input').fill(uname);
  await page.getByTestId('admin-create-user-email-input').fill(`${uname}@example.test`);
  await page.getByTestId('admin-create-user-password-input').fill('TestPassword1!');
  await page.getByTestId('admin-create-user-submit-btn').click();

  const row = page.locator('tr').filter({ hasText: uname });
  await expect(row).toBeVisible({ timeout: 15_000 });

  // The create flow can leave a modal open whose backdrop blocks row actions.
  await page.keyboard.press('Escape');
  const cancel = page.getByTestId('admin-create-user-cancel-btn');
  if (await cancel.isVisible().catch(() => false)) await cancel.click();
  await expect(page.locator('.modal__backdrop')).toHaveCount(0, { timeout: 10_000 });
  return row;
}

test('create and delete a user', async ({ page }) => {
  const uname = uniqUser();
  const row = await createUserRow(page, uname);

  // Delete now goes through a dedicated typed-email confirmation modal
  // (destructive-action guard — see `.btn--danger` gate in +page.svelte).
  // The Delete button stays disabled until the admin re-types the
  // target's email.
  await row.locator('[data-testid^="admin-user-delete-"]').first().click();
  await expect(page.getByTestId('admin-delete-user-form')).toBeVisible({ timeout: 15_000 });
  await page.getByTestId('admin-delete-user-email-input').fill(`${uname}@example.test`);
  await page.getByTestId('admin-delete-user-confirm-btn').click();
  await expect(page.locator('tr').filter({ hasText: uname })).toHaveCount(0, { timeout: 15_000 });
});

test('toggle a user role and active state', async ({ page }) => {
  const uname = uniqUser();
  const row = await createUserRow(page, uname);

  // Each of these confirms through the admin confirm modal.
  await row.locator('[data-testid^="admin-user-toggle-role-"]').first().click();
  await page.getByTestId('admin-confirm-ok-btn').click();
  await row.locator('[data-testid^="admin-user-toggle-active-"]').first().click(); // deactivate
  await page.getByTestId('admin-confirm-ok-btn').click();
  await row.locator('[data-testid^="admin-user-toggle-active-"]').first().click(); // reactivate
  await page.getByTestId('admin-confirm-ok-btn').click();

  await expect(row).toBeVisible();
});

test('reset a user password', async ({ page }) => {
  const uname = uniqUser();
  const row = await createUserRow(page, uname);

  await row.locator('[data-testid^="admin-user-reset-password-"]').first().click();
  await expect(page.getByTestId('admin-reset-password-form')).toBeVisible({ timeout: 15_000 });
  await page.getByTestId('admin-reset-password-input').fill('NewPassword1!');
  await page.getByTestId('admin-reset-password-submit-btn').click();
  await expect(page.getByTestId('admin-reset-password-form')).toHaveCount(0, { timeout: 15_000 });
});

test('save a user quota and deactivate the user', async ({ page }) => {
  const uname = uniqUser();
  const row = await createUserRow(page, uname);

  // The quota row-button and the QuotaEditor modal now share the
  // `admin-user-quota-` prefix — the button is `admin-user-quota-<id>`
  // and the modal's form / save button are `admin-user-quota-form` /
  // `admin-user-quota-save-btn`. Scope the row lookup to the button
  // shape (trailing UUID) so the modal's siblings don't match first.
  await row.locator('button[data-testid^="admin-user-quota-"]').first().click();
  await expect(page.getByTestId('admin-user-quota-form')).toBeVisible({ timeout: 15_000 });
  await page.getByTestId('admin-user-quota-save-btn').click();
  await expect(page.getByTestId('admin-user-quota-form')).toHaveCount(0, { timeout: 15_000 });

  // Deactivate (admin's own confirm modal).
  await row.locator('[data-testid^="admin-user-toggle-active-"]').first().click();
  await page.getByTestId('admin-confirm-ok-btn').click();
});

test('save oidc settings', async ({ page }) => {
  await page.goto('/admin/oidc');
  await expect(page.getByTestId('admin-oidc-form')).toBeVisible({ timeout: 15_000 });
  await page.getByTestId('admin-oidc-issuer-input').fill('https://example.test/issuer').catch(() => {});
  await page.getByTestId('admin-oidc-client-id-input').fill('client-123').catch(() => {});
  await page.getByTestId('admin-oidc-save-btn').click().catch(() => {});
  await expect(page.getByTestId('admin-oidc-form')).toBeVisible();
});

test('install, toggle, and delete a plugin', async ({ page }) => {
  await page.goto('/admin/plugins');
  await expect(page).toHaveURL(/\/admin\/plugins$/);

  // Install the example hello plugin (plugins are enabled in the coverage env).
  await page.getByTestId('admin-plugins-install-input').setInputFiles(PLUGIN_ZIP);

  // The installed plugin shows up with toggle/delete controls.
  const toggle = page.locator('[data-testid^="admin-plugin-toggle-"]').first();
  await expect(toggle).toBeVisible({ timeout: 20_000 });
  await toggle.click(); // disable
  await page.waitForTimeout(400);
  await toggle.click(); // re-enable
  await page.waitForTimeout(400);

  // Delete it (admin confirm modal).
  await page.locator('[data-testid^="admin-plugin-delete-"]').first().click();
  const ok = page.getByTestId('admin-confirm-ok-btn');
  if (await ok.isVisible().catch(() => false)) await ok.click();
  await expect(page.locator('[data-testid^="admin-plugin-toggle-"]')).toHaveCount(0, {
    timeout: 15_000,
  });
});

test('view plugin logs and details', async ({ page }) => {
  await page.goto('/admin/plugins');
  await page.getByTestId('admin-plugins-install-input').setInputFiles(PLUGIN_ZIP);
  await expect(page.locator('[data-testid^="admin-plugin-details-"]').first()).toBeVisible({
    timeout: 20_000,
  });

  // Open the logs/details view and exercise its controls, then close.
  await page.locator('[data-testid^="admin-plugin-details-"]').first().click();
  await page.getByTestId('admin-plugin-logs-level-select').selectOption({ index: 1 }).catch(() => {});
  await page.getByTestId('admin-plugin-logs-search-input').fill('hello').catch(() => {});
  await page.getByTestId('admin-plugin-logs-search-btn').click({ timeout: 2_000 }).catch(() => {});
  // Note: the live-tail checkbox opens an SSE stream that prevents the page from
  // settling, so it's left untested here.
  await page.getByTestId('admin-plugin-logs-close-btn').click({ timeout: 3_000 }).catch(() => {});

  // Clean up: delete the plugin.
  await page.locator('[data-testid^="admin-plugin-delete-"]').first().click();
  const ok = page.getByTestId('admin-confirm-ok-btn');
  if (await ok.isVisible().catch(() => false)) await ok.click();
});

test('plugins tab: save retention settings', async ({ page }) => {
  await page.goto('/admin/plugins');
  await expect(page).toHaveURL(/\/admin\/plugins$/);
  // The retention form is conditional; fill + save it when present.
  const retention = page.getByTestId('admin-plugin-retention-form');
  if (await retention.isVisible().catch(() => false)) {
    await page.getByTestId('admin-plugin-retention-days-input').fill('30').catch(() => {});
    await page.getByTestId('admin-plugin-retention-save-btn').click().catch(() => {});
  }
});

test('toggle the dashboard registration setting', async ({ page }) => {
  await page.goto('/admin');
  await expect(page.getByTestId('admin-dashboard-registration-checkbox')).toBeVisible({
    timeout: 15_000,
  });
  await page.getByTestId('admin-dashboard-registration-checkbox').click();
});

test('send a test email from the smtp tab', async ({ page }) => {
  await page.goto('/admin/smtp');
  await expect(page.getByTestId('admin-smtp-to-input')).toBeVisible({ timeout: 15_000 });
  await page.getByTestId('admin-smtp-to-input').fill('test@example.test');
  await page.getByTestId('admin-smtp-send-btn').click();
  await expect(page.getByTestId('admin-smtp-send-btn')).toBeVisible();
});

test('oidc tab: run discovery against a bogus issuer', async ({ page }) => {
  await page.goto('/admin/oidc');
  await expect(page.getByTestId('admin-oidc-form')).toBeVisible({ timeout: 15_000 });
  await page.getByTestId('admin-oidc-issuer-input').fill('https://idp.example.test').catch(() => {});
  // Discovery fails (no real IdP) — exercises the discover + error path.
  await page.getByTestId('admin-oidc-discover-btn').click().catch(() => {});
  await page.waitForTimeout(800);
  await expect(page.getByTestId('admin-oidc-form')).toBeVisible();
});

test('users tab: paginate the user list', async ({ page }) => {
  // The list paginates at PAGE_SIZE (25); create enough to get a second page.
  await apiLogin(page);
  for (let i = 0; i < 26; i++) {
    await apiAdminCreateUser(page, `pageu${Date.now()}${i}`);
  }
  await page.goto('/admin/users');
  await expect(page.getByTestId('admin-users-pager-next-btn')).toBeVisible({ timeout: 15_000 });
  await page.getByTestId('admin-users-pager-next-btn').click();
  await page.waitForTimeout(400);
  await page.getByTestId('admin-users-pager-prev-btn').click().catch(() => {});
});

test('plugins tab: install then save retention settings', async ({ page }) => {
  await page.goto('/admin/plugins');
  await page.getByTestId('admin-plugins-install-input').setInputFiles(PLUGIN_ZIP);
  await expect(page.locator('[data-testid^="admin-plugin-delete-"]').first()).toBeVisible({
    timeout: 20_000,
  });

  // Retention form is shown once a plugin exists.
  const retention = page.getByTestId('admin-plugin-retention-form');
  if (await retention.isVisible().catch(() => false)) {
    await page.getByTestId('admin-plugin-retention-days-input').fill('30').catch(() => {});
    await page.getByTestId('admin-plugin-retention-max-input').fill('100').catch(() => {});
    await page.getByTestId('admin-plugin-retention-save-btn').click().catch(() => {});
  }

  // Clean up.
  await page.locator('[data-testid^="admin-plugin-delete-"]').first().click();
  const ok = page.getByTestId('admin-confirm-ok-btn');
  if (await ok.isVisible().catch(() => false)) await ok.click();
});
