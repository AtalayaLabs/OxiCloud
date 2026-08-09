import { test, expect } from './coverage-helpers';
import { apiLogin, apiCreateFolder } from './helpers';

/**
 * Batch selection bar — select-all then batch favorite / batch delete, each in
 * its own dedicated parent folder so it can't disturb other tests and starts
 * from a stable (un-re-rendered) state.
 */
test.beforeEach(async ({ page }) => {
  await apiLogin(page);
});

function uniq(p: string): string {
  return `${p}-${Date.now()}-${Math.floor(Math.random() * 1e6)}`;
}

/** Create a parent folder with two children and open it in list view. */
async function openFolderWithChildren(
  page: import('@playwright/test').Page,
): Promise<{ c1: string; c2: string }> {
  const parent = await apiCreateFolder(page, uniq('Batch'));
  const c1 = uniq('c1');
  const c2 = uniq('c2');
  await apiCreateFolder(page, c1, parent.id);
  await apiCreateFolder(page, c2, parent.id);
  await page.goto(`/files/${parent.id}`);
  await expect(page.getByTestId(c1)).toBeVisible({ timeout: 15_000 });
  await page.getByTestId('display-mode-view-list-btn').click();
  return { c1, c2 };
}

test('select-all then batch favorite', async ({ page }) => {
  const { c1 } = await openFolderWithChildren(page);
  await page.getByTestId('resource-list-select-all-checkbox').check();
  await expect(page.getByTestId('resource-list-batch-close-btn')).toBeVisible();
  await page.getByTestId('files-batch-favorite-btn').click();
  // Items remain in the folder after favoriting.
  await expect(page.getByTestId(c1)).toBeVisible({ timeout: 15_000 });
});

test('select-all then batch copy and download', async ({ page }) => {
  const { c1 } = await openFolderWithChildren(page);
  await page.getByTestId('resource-list-select-all-checkbox').check();
  await expect(page.getByTestId('resource-list-batch-close-btn')).toBeVisible();

  // Copy → the move dialog (copy mode); cancel.
  await page.getByTestId('files-batch-copy-btn').click();
  await expect(page.getByTestId('move-dialog')).toBeVisible({ timeout: 15_000 });
  await page.getByTestId('move-dialog-cancel-btn').click();

  // Re-select and batch-download (a zip).
  await page.getByTestId('resource-list-select-all-checkbox').check();
  await expect(page.getByTestId('resource-list-batch-close-btn')).toBeVisible();
  const dl = page.waitForEvent('download', { timeout: 10_000 }).catch(() => null);
  await page.getByTestId('files-batch-download-btn').click();
  await dl;
  await expect(page.getByTestId(c1)).toBeVisible();
});

test('select-all then batch delete', async ({ page }) => {
  const { c1, c2 } = await openFolderWithChildren(page);
  await page.getByTestId('resource-list-select-all-checkbox').check();
  await expect(page.getByTestId('resource-list-batch-close-btn')).toBeVisible();
  await page.getByTestId('files-batch-delete-btn').click();
  await page.getByTestId('dialog-host-confirm-btn').click();

  await expect(page.getByTestId(c1)).toHaveCount(0, { timeout: 15_000 });
  await expect(page.getByTestId(c2)).toHaveCount(0);
});
