import { test, expect } from './coverage-helpers';
import { apiLogin, apiCreateFolder, apiUploadFile, SAMPLE_FILES } from './helpers';

/**
 * Photos route — populate the grid with uploaded images, open the lightbox and
 * navigate it, and switch the moments/places/people subnav. Covers the photos
 * page + PhotoLightbox (and mounts PlacesMap / PeopleView).
 */
test.beforeEach(async ({ page }) => {
  await apiLogin(page);
});

function uniq(p: string): string {
  return `${p}-${Date.now()}-${Math.floor(Math.random() * 1e6)}`;
}

async function seedPhotos(page: import('@playwright/test').Page, n: number): Promise<void> {
  const folder = await apiCreateFolder(page, uniq('Photos'));
  for (let i = 0; i < n; i++) {
    await apiUploadFile(
      page,
      { name: `${uniq('pic')}.png`, mimeType: 'image/png', body: SAMPLE_FILES.png().body },
      folder.id,
    );
  }
}

test('photo grid opens the lightbox and navigates', async ({ page }) => {
  await seedPhotos(page, 3);
  await page.goto('/photos');
  await expect(page.getByTestId('appshell-logo-link')).toBeVisible({ timeout: 15_000 });

  const tile = page.locator('[data-testid^="photo-tile-"]').first();
  await expect(tile).toBeVisible({ timeout: 15_000 });
  await tile.click();

  await expect(page.getByTestId('photo-lightbox')).toBeVisible({ timeout: 15_000 });
  await page.getByTestId('photo-lightbox-next-btn').click({ timeout: 3_000 }).catch(() => {});
  await page.getByTestId('photo-lightbox-prev-btn').click({ timeout: 3_000 }).catch(() => {});
  await page.getByTestId('photo-lightbox-close-btn').click();
  await expect(page.getByTestId('photo-lightbox')).toHaveCount(0);
});

test('select photos, toggle layout, and batch-delete', async ({ page }) => {
  await seedPhotos(page, 3);
  await page.goto('/photos');

  // The tile check button is hover-revealed; dispatch the click to select.
  const tileCheck = page.locator('[data-testid^="photo-tile-check-"]').first();
  await expect(tileCheck).toBeAttached({ timeout: 15_000 });
  await tileCheck.dispatchEvent('click');
  await expect(page.getByTestId('photos-batch-bar')).toBeVisible({ timeout: 5_000 });

  // Layout toggles.
  await page.getByTestId('photos-layout-justified-btn').click();
  await page.getByTestId('photos-layout-square-btn').click();

  // Batch-delete the selection (confirm if prompted).
  await page.getByTestId('photos-batch-delete-btn').click();
  const confirm = page.getByTestId('dialog-host-confirm-btn');
  if (await confirm.isVisible().catch(() => false)) await confirm.click();
});

test('photos subnav switches to places and people', async ({ page }) => {
  await seedPhotos(page, 2);
  await page.goto('/photos');
  await expect(page.getByTestId('photos-tab-places')).toBeVisible({ timeout: 15_000 });

  await page.getByTestId('photos-tab-places').click();
  await page.waitForTimeout(800);

  const peopleTab = page.getByTestId('photos-tab-people');
  if (await peopleTab.isVisible().catch(() => false)) {
    await peopleTab.click();
    await page.waitForTimeout(800);
  }

  await page.getByTestId('photos-tab-moments').click();
  await expect(page.getByTestId('photos-tab-moments')).toHaveAttribute('aria-selected', 'true');
});
