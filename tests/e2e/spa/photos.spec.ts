import { test, expect } from './coverage-helpers';
import { apiLogin, apiCreateFolder, apiUploadFile, SAMPLE_FILES } from './helpers';
import { readFileSync } from 'node:fs';
import { join } from 'node:path';

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

for (const mode of ['light', 'dark'] as const) {
  test(`Modern Pro keeps lightbox controls readable in ${mode} mode`, async ({ page }) => {
    const theme = readFileSync(
      join(__dirname, '../../../resources/themes/installed/modern-pro/theme.css'),
      'utf8',
    );
    await page.route('**/themes/active.css', route => route.fulfill({
      contentType: 'text/css',
      body: theme,
    }));
    await page.addInitScript(scheme => localStorage.setItem('oxi-theme', scheme), mode);
    await seedPhotos(page, 2);
    await page.goto('/photos');
    await expect(page.locator('html')).toHaveAttribute('data-color-scheme', mode);
    await page.locator('[data-testid^="photo-tile-"]').first().click();

    const lightbox = page.getByTestId('photo-lightbox');
    await expect(lightbox).toBeVisible();
    // Regression pin: Modern Pro makes on-accent text dark in dark mode;
    // lightbox captions and controls must retain their own light foreground
    // on the dark overlay. Assert browser-computed colors, not token strings.
    await expect(lightbox).toHaveCSS('background-color', 'rgba(0, 0, 0, 0.92)');
    for (const selector of ['.lb__info', '.lb__close', '.lb__nav', '.lb__tool', '.lb__counter']) {
      for (const element of await lightbox.locator(selector).all()) {
        await expect(element).toHaveCSS('color', 'rgb(255, 255, 255)');
      }
    }
    const favorite = lightbox.locator('.lb__tool[title="Favorite"]');
    // Regression pin: larger action targets must include the ConfirmButton
    // child, whose old icon variant stayed a small square beside round tools.
    for (const button of await lightbox.locator('.lb__toolbar button, .lb__close, .lb__nav').all()) {
      const bounds = await button.boundingBox();
      expect(bounds!.width).toBeGreaterThanOrEqual(48);
      expect(bounds!.height).toBeGreaterThanOrEqual(48);
    }
    const remove = page.getByTestId('photo-lightbox-delete-btn');
    await expect(remove).toHaveCSS('border-radius', await favorite.evaluate(el => getComputedStyle(el).borderRadius));
    await expect(remove).toHaveCSS('background-color', 'rgba(255, 255, 255, 0.12)');
    await remove.click();
    await expect(remove).toContainText(/Really\?|Permanently\?/);
    await expect(lightbox).toBeVisible();
    await expect(remove).toHaveCSS('height', '48px');
    await favorite.focus();
    await expect(remove).not.toHaveClass(/confirm-btn--armed/);
    await expect(favorite).toHaveCSS('background-color', 'rgba(255, 255, 255, 0.12)');
    await favorite.click();
    await expect(favorite).toHaveClass(/active/);
    await expect(favorite).toHaveCSS('color', 'rgb(255, 255, 255)');
    await expect(favorite).toHaveCSS('background-color', 'rgba(255, 255, 255, 0.25)');
    await favorite.click();
    await expect(favorite).not.toHaveClass(/active/);
    await page.getByTestId('photo-lightbox-close-btn').click();
    await expect(lightbox).toHaveCount(0);
  });
}

// The retired `[ Grid | Justified ]` layout toggle originally
// shipped in 75ee9b7c (`feat(photos): justified
// (aspect-preserving) layout option`, Jun 2026). Removed in
// ebb15488 to free the toolbar for the §6 / §6b / §6c filter
// axes. The pre-rename test name was "select photos, toggle
// layout, and batch-delete"; the layout-flip step was always
// decorative on a batch-delete flow, so this test now exercises
// just the real property. Git-show either commit to recover the
// layout buttons or their original test IDs.
test('select photos and batch-delete', async ({ page }) => {
  await seedPhotos(page, 3);
  await page.goto('/photos');

  // The tile check button is hover-revealed; dispatch the click to select.
  const tileCheck = page.locator('[data-testid^="photo-tile-check-"]').first();
  await expect(tileCheck).toBeAttached({ timeout: 15_000 });
  await tileCheck.dispatchEvent('click');
  await expect(page.getByTestId('photos-batch-bar')).toBeVisible({ timeout: 5_000 });

  // Batch-delete the selection: first click arms the button, second confirms.
  await page.getByTestId('photos-batch-delete-btn').click();
  await page.getByTestId('photos-batch-delete-btn').click();
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
