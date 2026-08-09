import * as fs from 'fs';
import * as path from 'path';
import { Page, expect } from '@playwright/test';
import { test as base, TEST_ADMIN } from './helpers';

/**
 * `test` for the SvelteKit SPA coverage suite. Extends the shared `test`
 * (JS-error guard + container/stack support) with an auto fixture that, after
 * every test, reads the Istanbul `window.__coverage__` accumulated by the
 * instrumented build and appends it to `.nyc_output/` for an nyc report.
 *
 * Import `test`/`expect` from here in every `spa/*.spec.ts`.
 */
const NYC_DIR = path.join(__dirname, '..', '.nyc_output');

export const test = base.extend<{ collectCoverage: void }>({
  collectCoverage: [
    async ({ page }, use, testInfo) => {
      await use();
      // The instrumented bundle accumulates into window.__coverage__ across
      // SPA navigations; read it once at test end. May be undefined if the
      // page never loaded the app (pure-API test) — skip those.
      const coverage = await page
        .evaluate(() => (window as unknown as { __coverage__?: unknown }).__coverage__)
        .catch(() => undefined);
      if (coverage) {
        const file = path.join(
          NYC_DIR,
          `coverage-${testInfo.workerIndex}-${testInfo.testId}.json`,
        );
        fs.writeFileSync(file, JSON.stringify(coverage));
      }
    },
    { auto: true },
  ],
});

export { expect, TEST_ADMIN };

/**
 * Sign in through the SvelteKit login UI and wait until the app shell is
 * mounted. Uses stable `data-testid` selectors (kept in VITE_E2E builds).
 */
export async function uiLogin(page: Page, admin = TEST_ADMIN): Promise<void> {
  await page.goto('/login');
  await page.getByTestId('login-username-input').fill(admin.username);
  await page.getByTestId('login-password-input').fill(admin.password);
  await page.getByTestId('login-submit-btn').click();
  await page.waitForURL('**/files**', { timeout: 15_000 });
  await expect(page.getByTestId('appshell-logo-link')).toBeVisible({ timeout: 15_000 });
}
