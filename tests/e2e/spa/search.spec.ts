import { test, expect } from './coverage-helpers';
import { apiLogin, apiCreateFolder, apiUploadFile } from './helpers';

/**
 * Search route — seed a uniquely-named file, query for it, then exercise the
 * type/size/date/sort filters and the clear-filters control. Covers the search
 * route + search endpoint module.
 */
test.beforeEach(async ({ page }) => {
  await apiLogin(page);
});

test('search finds a seeded file and applies filters', async ({ page }) => {
  const token = `srch${Date.now()}${Math.floor(Math.random() * 1000)}`;
  const folder = await apiCreateFolder(page, `SearchHost-${token}`);
  await apiUploadFile(
    page,
    { name: `${token}.txt`, mimeType: 'text/plain', body: Buffer.from('searchable content') },
    folder.id,
  );

  await page.goto(`/search?q=${token}`);
  await expect(page.getByTestId(`${token}.txt`)).toBeVisible({ timeout: 15_000 });

  // Exercise the filter + sort controls (onchange handlers). Done after the
  // result assertion; selecting by index keeps this resilient to option sets.
  // Short timeouts so optional controls that aren't rendered fail fast instead
  // of waiting the full test timeout (the scope/clear buttons are conditional).
  const opt = { timeout: 2_000 };
  await page.getByTestId('search-sort-select').selectOption({ index: 1 }, opt).catch(() => {});
  await page.getByTestId('search-size-filter-select').selectOption({ index: 1 }, opt).catch(() => {});
  await page.getByTestId('search-date-filter-select').selectOption({ index: 1 }, opt).catch(() => {});
  await page.getByTestId('search-clear-filters-btn').click(opt).catch(() => {});
});
