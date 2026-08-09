import { test, expect } from './coverage-helpers';
import { apiLogin, apiCreateFolder } from './helpers';

/**
 * MoveDialog coverage — move a folder into another via the context menu.
 * Exercises the move dialog's tree navigation and the folders move endpoint.
 */
test.beforeEach(async ({ page }) => {
  await apiLogin(page);
});

function uniq(p: string): string {
  return `${p}-${Date.now()}-${Math.floor(Math.random() * 1e6)}`;
}

test('move a folder into another via the move dialog', async ({ page }) => {
  const srcName = uniq('MoveSrc');
  const destName = uniq('MoveDest');
  await apiCreateFolder(page, srcName);
  const dest = await apiCreateFolder(page, destName);

  await page.goto('/files');
  await expect(page.getByTestId(srcName)).toBeVisible({ timeout: 15_000 });

  await page.getByTestId(srcName).click({ button: 'right' });
  await page.getByTestId('files-ctx-move-item').click();
  await expect(page.getByTestId('move-dialog')).toBeVisible();

  // Exercise the dialog tree navigation (into a folder, up to parent, home),
  // then settle into the destination and confirm.
  await page.getByTestId(`move-dialog-folder-${dest.id}`).click();
  await page.getByTestId('move-dialog-parent-btn').click({ timeout: 3_000 }).catch(() => {});
  await page.getByTestId('move-dialog-home-btn').click({ timeout: 3_000 }).catch(() => {});
  await page.getByTestId(`move-dialog-folder-${dest.id}`).click();
  await page.getByTestId('move-dialog-confirm-btn').click();

  // Source left the root listing.
  await expect(page.getByTestId(srcName)).toHaveCount(0, { timeout: 15_000 });
  // And now lives inside the destination (route keys on folder id).
  await page.goto(`/files/${dest.id}`);
  await expect(page.getByTestId(srcName)).toBeVisible({ timeout: 15_000 });
});

test('navigate a nested tree in the move dialog', async ({ page }) => {
  const srcName = uniq('NavSrc');
  await apiCreateFolder(page, srcName);
  const dest = await apiCreateFolder(page, uniq('NavDest'));
  const sub = await apiCreateFolder(page, uniq('NavSub'), dest.id);

  await page.goto('/files');
  await expect(page.getByTestId(srcName)).toBeVisible({ timeout: 15_000 });
  await page.getByTestId(srcName).click({ button: 'right' });
  await page.getByTestId('files-ctx-move-item').click();
  await expect(page.getByTestId('move-dialog')).toBeVisible();

  // Descend dest → sub, climb back via parent, jump home, re-enter dest, move.
  await page.getByTestId(`move-dialog-folder-${dest.id}`).click();
  await page.getByTestId(`move-dialog-folder-${sub.id}`).click();
  await page.getByTestId('move-dialog-parent-btn').click();
  await page.getByTestId('move-dialog-home-btn').click();
  await page.getByTestId(`move-dialog-folder-${dest.id}`).click();
  await page.getByTestId('move-dialog-confirm-btn').click();

  await expect(page.getByTestId(srcName)).toHaveCount(0, { timeout: 15_000 });
});
