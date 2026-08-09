import { test, expect } from './coverage-helpers';
import { apiLogin } from './helpers';

/**
 * Command palette — opens on Ctrl/Cmd+K, filters on typed input, and closes on
 * Escape. Covers CommandPalette.svelte.
 */
test.beforeEach(async ({ page }) => {
  await apiLogin(page);
});

test('command palette opens, filters, and closes', async ({ page }) => {
  await page.goto('/files');
  await expect(page.getByTestId('appshell-logo-link')).toBeVisible({ timeout: 15_000 });

  await page.keyboard.press('Control+k');
  await expect(page.getByTestId('command-palette-panel')).toBeVisible({ timeout: 5_000 });

  await page.getByTestId('command-palette-input').fill('trash');
  // At least one command item should match.
  await expect(page.locator('[data-testid^="command-palette-"][data-testid$="-item"]').first()).toBeVisible();

  await page.keyboard.press('Escape');
  await expect(page.getByTestId('command-palette-panel')).toHaveCount(0);
});

test('command palette navigates via a command', async ({ page }) => {
  await page.goto('/files');
  await expect(page.getByTestId('appshell-logo-link')).toBeVisible({ timeout: 15_000 });

  await page.keyboard.press('Control+k');
  await expect(page.getByTestId('command-palette-panel')).toBeVisible({ timeout: 5_000 });
  // Pick the first matching command via keyboard (covers the run path).
  await page.getByTestId('command-palette-input').fill('trash');
  await page.keyboard.press('Enter');
  await expect(page.getByTestId('command-palette-panel')).toHaveCount(0);
});
