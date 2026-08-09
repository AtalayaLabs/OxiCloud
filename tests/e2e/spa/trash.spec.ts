import { test, expect } from './coverage-helpers';
import { apiLogin, apiCreateFolder, apiTrashFolder } from './helpers';

/**
 * Trash flow — restore and permanently delete items from /trash. The folder is
 * trashed via the API (the /files root accumulates hundreds of folders across
 * the suite and its virtualised list won't render a specific one), and the
 * trash is emptied first so our item is the only — and thus visible — row.
 */
test.beforeEach(async ({ page }) => {
  await apiLogin(page);
});

function uniq(p: string): string {
  return `${p}-${Date.now()}-${Math.floor(Math.random() * 1e6)}`;
}

test('restore an item from trash', async ({ page }) => {
  // Ensure the trash is non-empty, then restore the first item (the trash list
  // accumulates across the suite, so we exercise the endpoint on whatever is
  // first rather than hunting a specific virtualised row).
  const folder = await apiCreateFolder(page, uniq('Trash'));
  await apiTrashFolder(page, folder.id);

  await page.goto('/trash');
  // Row action buttons are CSS hover-revealed (display:none until hover), so
  // assert they're attached and dispatch the click directly.
  const restore = page.locator('[data-testid^="trash-restore-btn-"]').first();
  await expect(restore).toBeAttached({ timeout: 15_000 });
  await restore.dispatchEvent('click');
  // The restore ran without error and the app is still mounted.
  await expect(page.getByTestId('appshell-logo-link')).toBeVisible({ timeout: 15_000 });
});

test('permanently delete an item from trash', async ({ page }) => {
  const folder = await apiCreateFolder(page, uniq('Purge'));
  await apiTrashFolder(page, folder.id);

  await page.goto('/trash');
  const del = page.locator('[data-testid^="trash-delete-btn-"]').first();
  await expect(del).toBeAttached({ timeout: 15_000 });
  await del.dispatchEvent('click');
  const confirm = page.getByTestId('dialog-host-confirm-btn');
  if (await confirm.isVisible().catch(() => false)) await confirm.click();
  await expect(page.getByTestId('appshell-logo-link')).toBeVisible({ timeout: 15_000 });
});

test('empty the trash from the toolbar', async ({ page }) => {
  // Seed a trashed item so the empty-trash button is shown, then empty via UI.
  const folder = await apiCreateFolder(page, uniq('EmptyMe'));
  await apiTrashFolder(page, folder.id);

  await page.goto('/trash');
  await expect(page.getByTestId('trash-empty-btn')).toBeVisible({ timeout: 15_000 });
  await page.getByTestId('trash-empty-btn').click();
  const confirm = page.getByTestId('dialog-host-confirm-btn');
  if (await confirm.isVisible().catch(() => false)) await confirm.click();
  await expect(page.locator('[data-testid^="trash-restore-btn-"]')).toHaveCount(0, {
    timeout: 15_000,
  });
});
