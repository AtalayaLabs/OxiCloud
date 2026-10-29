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

  // `/admin/notification`, not `/admin/smtp` — the tab was renamed when
  // the webhook joined it (the page is about every way the instance
  // reaches someone, not one transport). Worth knowing why this bit:
  // `parseTab` falls back to 'dashboard' for an unknown tab, so the old
  // URL rendered the Dashboard and the only symptom was this assertion
  // timing out.
  await page.goto('/admin/notification');
  await expect(page.getByTestId('admin-smtp-send-btn')).toBeVisible();
  // The alerting policy card, which governs both transports below it: a
  // configured channel still delivers nothing when the severity floor
  // excludes what a job found, and this is the only place that says so.
  await expect(page.getByTestId('admin-notify-policy')).toBeVisible();

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

/**
 * Run one row action.
 *
 * The per-row controls used to be six icon buttons; they are now entries
 * in a "⋮" menu, so every action costs one extra click. The menu is
 * rendered inside the row (position: fixed only moves it visually), which
 * is why the item locator can stay row-scoped.
 *
 * Keyed by the action's stable `key`, never its label — labels go through
 * `t()` and would tie these tests to the English locale.
 */
async function rowAction(
  row: ReturnType<import('@playwright/test').Page['locator']>,
  key: string,
): Promise<void> {
  await row.locator('[data-testid^="admin-user-actions-"]').first().click();
  await row.locator(`[data-testid$="-${key}"]`).first().click();
}

test('create and delete a user', async ({ page }) => {
  const uname = uniqUser();
  const row = await createUserRow(page, uname);

  // Delete now goes through a dedicated typed-email confirmation modal
  // (destructive-action guard — see `.btn--danger` gate in +page.svelte).
  // The Delete button stays disabled until the admin re-types the
  // target's email.
  await rowAction(row, 'delete');
  await expect(page.getByTestId('admin-delete-user-form')).toBeVisible({ timeout: 15_000 });
  await page.getByTestId('admin-delete-user-email-input').fill(`${uname}@example.test`);
  await page.getByTestId('admin-delete-user-confirm-btn').click();
  await expect(page.locator('tr').filter({ hasText: uname })).toHaveCount(0, { timeout: 15_000 });
});

test('toggle a user role and active state', async ({ page }) => {
  const uname = uniqUser();
  const row = await createUserRow(page, uname);

  // Each of these confirms through the admin confirm modal.
  await rowAction(row, 'toggle-role');
  await page.getByTestId('admin-confirm-ok-btn').click();
  await rowAction(row, 'toggle-active'); // deactivate
  await page.getByTestId('admin-confirm-ok-btn').click();
  await rowAction(row, 'toggle-active'); // reactivate
  await page.getByTestId('admin-confirm-ok-btn').click();

  await expect(row).toBeVisible();
});

test('reset a user password', async ({ page }) => {
  const uname = uniqUser();
  const row = await createUserRow(page, uname);

  await rowAction(row, 'reset-password');
  await expect(page.getByTestId('admin-reset-password-form')).toBeVisible({ timeout: 15_000 });
  await page.getByTestId('admin-reset-password-input').fill('NewPassword1!');
  await page.getByTestId('admin-reset-password-submit-btn').click();
  await expect(page.getByTestId('admin-reset-password-form')).toHaveCount(0, { timeout: 15_000 });
});

test('save a user quota and deactivate the user', async ({ page }) => {
  const uname = uniqUser();
  const row = await createUserRow(page, uname);

  // The menu entry is `admin-user-actions-<id>-quota`; the QuotaEditor
  // modal it opens uses `admin-user-quota-form` / `-save-btn`. The old
  // prefix collision between the row button and the modal is gone now
  // that the trigger lives under the `admin-user-actions-` prefix.
  await rowAction(row, 'quota');
  await expect(page.getByTestId('admin-user-quota-form')).toBeVisible({ timeout: 15_000 });
  await page.getByTestId('admin-user-quota-save-btn').click();
  await expect(page.getByTestId('admin-user-quota-form')).toHaveCount(0, { timeout: 15_000 });

  // Deactivate (admin's own confirm modal).
  await rowAction(row, 'toggle-active');
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

test('send a test email from the notification tab', async ({ page }) => {
  await page.goto('/admin/notification');
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

/**
 * Jobs tab — the job parameter dialog puts a NON-BOOLEAN parameter on the
 * wire, which no other path in the UI could express.
 *
 * ⚠ `/admin/jobs`, NOT `/admin/maintenance`. They are two different tabs:
 * maintenance owns operator-driven runtime state (the backend write-lock, the
 * re-extract backfills) and jobs owns the registry table this dialog belongs
 * to. Pointed at the wrong one, both these tests found no table, hit their
 * "job not registered" guard and SKIPPED — green output for a test that never
 * ran.
 *
 * Before this dialog the run menu offered one boolean per item and nothing
 * else, so `content_rescan`'s `rederive` (the only mode that applies a FIXED
 * EXTRACTOR — an extractor change does not move the sniffed type, so the
 * default `changed` skips every row it should fix) was reachable only by
 * calling the API by hand.
 *
 * The assertion is on the REQUEST, not on the run's outcome: the job sweeps
 * the whole corpus and its result depends on the test database's contents,
 * while `rederive=always` appearing on the query string is exactly the
 * capability being added.
 */
test('jobs tab: the parameter dialog sends a string parameter', async ({ page }) => {
  await page.goto('/admin/jobs');

  // ⚠ An ASSERTION, not a `test.skip` guard. `content_rescan` registers
  // unconditionally in `di.rs` (only its ffprobe container probe is optional),
  // so "the row isn't there" is always a defect — a tolerant guard here just
  // meant the pin reported green without running. Also note a bare
  // `isVisible()` would be false on every run: the table renders only after
  // `listJobs()` resolves, so the wait is what makes this meaningful.
  const row = page.locator('.jobs-panel__row', { hasText: 'content_rescan' }).first();
  await expect(row).toBeVisible({ timeout: 15_000 });

  // Capture the trigger before it is sent — the dialog's Run button is the
  // dispatch, so there is no second confirm to wait on.
  const triggered = page.waitForRequest(
    (r) => r.url().includes('/api/admin/jobs/content_rescan/trigger') && r.method() === 'POST'
  );

  await row.locator('.jobs-panel__btn--small').first().click();

  const dialogRun = page.getByTestId('job-params-run');
  await expect(dialogRun).toBeVisible();

  // A SELECT, not a text box: `rederive` is declared `JobParam::enumerated`,
  // so the dialog renders its three modes and `alwyas` is not typeable.
  // Choosing marks the row touched, which is what puts it on the wire at all.
  //
  // ⚠ This locator is load-bearing. It read `input[type="text"]` while
  // `rederive` was free text; the enum swap would have left it filling
  // `mime_prefix` instead and asserting on a parameter nobody set.
  await page.locator('select').first().selectOption('always');
  await dialogRun.click();

  const req = await triggered;
  expect(req.url()).toContain('rederive=always');

  // ⚠ And nothing the operator did not choose. `dry_run` was rendered and
  // left alone, so it must be absent — sending it would pin today's default
  // into the run's recorded args as if it had been picked.
  expect(req.url()).not.toContain('dry_run');
});

/**
 * Jobs tab — "Purge old runs" goes through the SHARED parameter dialog, not a
 * form of its own.
 *
 * `job_runs_cleanup` got a parameter before the dialog existed, so it grew a
 * bespoke modal, a bespoke `purgeJobRuns()` wrapper and four locale keys of
 * its own. All three are gone; the button now hands the `JobSummary` to the
 * same dialog every row uses, which is why the retention window can change on
 * the backend with no frontend release.
 *
 * The pin is the dialog's own testid plus the trigger URL: a revert to a
 * hand-built form would still open *something* with a number box in it, and
 * would still send a request — only `job-params-run` tells the two apart.
 */
test('jobs tab: purging old runs reuses the shared parameter dialog', async ({ page }) => {
  await page.goto('/admin/jobs');

  // Asserted, not guarded, for the same reason as the case above:
  // `job_runs_cleanup` registers unconditionally. The button is also the
  // conditional-render pin — it now renders only when the job is in the
  // fetched list, where the old bespoke path hardcoded the name.
  const purge = page.getByRole('button', { name: /purge old runs/i });
  await expect(purge).toBeVisible({ timeout: 15_000 });

  const triggered = page.waitForRequest(
    (r) => r.url().includes('/api/admin/jobs/job_runs_cleanup/trigger') && r.method() === 'POST'
  );

  await purge.click();

  // The shared dialog, identified by ITS testid — the whole point of the
  // change. It also carries the mutating-job warning, since the job declares
  // `mutates: always`, so this is the confirm as well.
  const dialogRun = page.getByTestId('job-params-run');
  await expect(dialogRun).toBeVisible();

  // Declared by the backend as a number with a default of 30, so the dialog
  // seeds it without the frontend knowing the name.
  const days = page.locator('input[type="number"]').first();
  await expect(days).toHaveValue('30');
  await days.fill('7');

  await dialogRun.click();

  const req = await triggered;
  expect(req.url()).toContain('retention_days=7');
});
