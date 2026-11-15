import { test, expect } from './coverage-helpers';
import { apiLogin, apiCreateFolder, apiUploadFile, SAMPLE_FILES } from './helpers';
import { themes, colorModes, applyTheme, expectThemeLoaded, expectReadable } from './theme-helpers';

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

for (const theme of themes) {
  for (const mode of colorModes) {
    test(`${theme.name} keeps lightbox controls readable in ${mode} mode`, async ({ page }) => {
      await applyTheme(page, theme, mode);
      await seedPhotos(page, 2);
      await page.goto('/photos');
      await expectThemeLoaded(page, theme);
      await expect(page.locator('html')).toHaveAttribute('data-color-scheme', mode);
      await page.locator('[data-testid^="photo-tile-"]').first().click();

      const lightbox = page.getByTestId('photo-lightbox');
      await expect(lightbox).toBeVisible();
      // Regression pin: Lightbox controls previously shared the on-accent token;
      // lightbox captions and controls must retain their own light foreground
      // on the dark overlay. Assert browser-computed colors, not token strings.
      for (const selector of ['.lb__info', '.lb__close', '.lb__nav', '.lb__tool', '.lb__counter']) {
        for (const element of await lightbox.locator(selector).all()) {
          await expectReadable(element);
        }
      }
      const favorite = lightbox.locator('.lb__tool[title="Favorite"]');
      const remove = page.getByTestId('photo-lightbox-delete-btn');
      await remove.click();
      await expect(remove).toContainText(/Really\?|Permanently\?/);
      await expect(lightbox).toBeVisible();
      await favorite.focus();
      await expect(remove).not.toHaveClass(/confirm-btn--armed/);
      // Regression pin: the generic scrim becomes nearly white in light mode,
      // hiding the white glyph. The lightbox needs its own translucent dark-overlay control.
      await expectReadable(favorite);
      const inactiveFill = await favorite.evaluate((el) => getComputedStyle(el).backgroundColor);
      await favorite.click();
      await expect(favorite).toHaveClass(/active/);
      // Regression pin: an accent fill hid the active glyph on the overlay.
      // Every theme must keep it readable and distinguish the active state.
      await expectReadable(favorite);
      await expect(favorite).not.toHaveCSS('background-color', inactiveFill);
      await favorite.click();
      await expect(favorite).not.toHaveClass(/active/);
      await page.getByTestId('photo-lightbox-close-btn').click();
      await expect(lightbox).toHaveCount(0);
    });
  }
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
