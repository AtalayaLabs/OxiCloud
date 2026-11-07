import { test, expect } from './coverage-helpers';
import { apiLogin, apiCreateFolder, favoriteRow, switchToListView } from './helpers';

/**
 * Favorites route — favorite a folder from its row star, see it in
 * /favorites, then unfavorite it via the same star. Covers the favorites page +
 * favorites endpoint.
 */
test.beforeEach(async ({ page }) => {
  await apiLogin(page);
});

function uniq(p: string): string {
  return `${p}-${Date.now()}-${Math.floor(Math.random() * 1e6)}`;
}

test('favorite, view, and unfavorite a folder', async ({ page }) => {
  const name = uniq('Fav');
  await apiCreateFolder(page, name);

  await page.goto('/files');
  await expect(page.getByTestId(name)).toBeVisible({ timeout: 15_000 });
  // The star's handler is fire-and-forget (`void toggleFavorite`), so the
  // test's next navigation can race the write. Wait for the actual POST to
  // land before going to /favorites so the list-fetch there sees the new row
  // committed. The batch test doesn't need this because it queues 2 POSTs
  // sequentially, which naturally gives the first one time to commit.
  const favorited = page.waitForResponse(
    (r) => r.url().includes('/api/favorites') && r.request().method() === 'POST' && r.ok()
  );
  await favoriteRow(page, name);
  await favorited;

  await page.goto('/favorites');
  const row = page.getByTestId(name);
  await expect(row).toBeVisible({ timeout: 15_000 });

  // The favorite star is hover-revealed; dispatch the click to toggle it off.
  await row.locator('[data-testid^="resource-list-favorite-"]').dispatchEvent('click');
  await expect(page.getByTestId(name)).toHaveCount(0, { timeout: 15_000 });
});

test('favorites batch select-all then move dialog', async ({ page }) => {
  const f1 = uniq('FavB1');
  const f2 = uniq('FavB2');
  await apiCreateFolder(page, f1);
  await apiCreateFolder(page, f2);

  // Favorite both from the row star.
  await page.goto('/files');
  for (const n of [f1, f2]) {
    await expect(page.getByTestId(n)).toBeVisible({ timeout: 15_000 });
    await favoriteRow(page, n);
  }

  await page.goto('/favorites');
  await expect(page.getByTestId(f1)).toBeVisible({ timeout: 15_000 });
  // The select-all checkbox lives in the list-view header.
  await switchToListView(page);
  await page.getByTestId('resource-list-select-all-checkbox').check();
  await expect(page.getByTestId('resource-list-batch-close-btn')).toBeVisible();

  // Batch-remove-from-favorite un-stars every selected row without
  // touching the underlying file — the /favorites batch bar was
  // trimmed to Download + Remove-from-favorite (destructive-to-content
  // actions moved into the row context menu). Verify the two folders
  // vanish from the list after the click.
  await page.getByTestId('favorites-batch-remove-btn').click();
  await expect(page.getByTestId(f1)).toHaveCount(0, { timeout: 15_000 });
  await expect(page.getByTestId(f2)).toHaveCount(0);
});
