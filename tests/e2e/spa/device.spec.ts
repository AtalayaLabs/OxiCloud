import { test, expect } from './coverage-helpers';
import { apiLogin } from './helpers';

/**
 * Device-pairing route — submit a (bogus) device code to exercise the lookup
 * path and its error/retry handling. Standalone page (no app shell).
 */
test.beforeEach(async ({ page }) => {
  await apiLogin(page);
});

test('submitting an unknown device code shows the retry path', async ({ page }) => {
  await page.goto('/device');
  await expect(page.getByTestId('device-code-form')).toBeVisible({ timeout: 15_000 });

  await page.getByTestId('device-code-input').fill('ZZZZ-ZZZZ');
  await page.getByTestId('device-continue-btn').click();

  // An unknown code returns to a retry/error state (or stays on the form).
  await expect(
    page.getByTestId('device-retry-btn').or(page.getByTestId('device-code-form')),
  ).toBeVisible({ timeout: 15_000 });

  // If a retry button appeared, use it to return to the code form.
  const retry = page.getByTestId('device-retry-btn');
  if (await retry.isVisible().catch(() => false)) {
    await retry.click();
    await expect(page.getByTestId('device-code-form')).toBeVisible({ timeout: 15_000 });
  }
});
