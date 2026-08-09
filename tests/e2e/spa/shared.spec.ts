import { test, expect } from './coverage-helpers';
import { apiLogin, apiCreateFolder } from './helpers';

/**
 * Shared route — create a public link via the share dialog, then view it in
 * /shared and open a grant's kebab menu. Exercises the shared page (lanes,
 * rows, menu) plus the grants/shares endpoints.
 */
test.beforeEach(async ({ page }) => {
  await apiLogin(page);
});

function uniq(p: string): string {
  return `${p}-${Date.now()}-${Math.floor(Math.random() * 1e6)}`;
}

test('a created link appears in /shared with an openable menu', async ({ page }) => {
  const name = uniq('Shared');
  // Isolate in a fresh parent (the /files root accumulates 100s of folders
  // across the suite and its virtualised list won't render an arbitrary one).
  const parent = await apiCreateFolder(page, uniq('SharedHost'));
  await apiCreateFolder(page, name, parent.id);

  // Create a public link through the share dialog.
  await page.goto(`/files/${parent.id}`);
  await expect(page.getByTestId(name)).toBeVisible({ timeout: 15_000 });
  await page.getByTestId(name).click({ button: 'right' });
  await page.getByTestId('files-ctx-share-item').click();
  await expect(page.getByTestId('share-dialog')).toBeVisible();
  await page.getByTestId('share-dialog-link-tab').click();
  await page.getByTestId('share-dialog-create-btn').click();
  await expect(page.locator('[data-testid^="share-dialog-link-delete-btn-"]').first()).toBeVisible({
    timeout: 15_000,
  });
  await page.getByTestId('share-dialog-close-btn').click();

  // It now shows up under My shares; open the first grant's kebab menu.
  await page.goto('/shared');
  // Load more grants if the pager is shown (covers cursor pagination).
  await page.getByTestId('shared-load-more-btn').click({ timeout: 2_000 }).catch(() => {});

  const kebab = page.locator('[data-testid^="shared-kebab-"]').first();
  await expect(kebab).toBeVisible({ timeout: 15_000 });
  await kebab.click();
  await expect(page.locator('[data-testid^="shared-menu-"]').first()).toBeVisible();

  // Exercise the link-grant menu actions, then delete the link.
  await page
    .locator('[data-testid^="shared-link-expiry-"]')
    .first()
    .fill('2031-01-01', { timeout: 3_000 })
    .catch(() => {});
  await page
    .locator('[data-testid^="shared-menu-copy-link-"]')
    .first()
    .click({ timeout: 3_000 })
    .catch(() => {});
  await page
    .locator('[data-testid^="shared-delete-link-"]')
    .first()
    .click({ timeout: 3_000 })
    .catch(() => {});
  const confirm = page.getByTestId('dialog-host-confirm-btn');
  if (await confirm.isVisible().catch(() => false)) await confirm.click();
});
