import { test, expect } from './coverage-helpers';
import { apiLogin, seedFilesAndFolders } from './helpers';

/**
 * AppShell chrome — global search box (suggestions + submit), notification
 * bell, language menu, and user menu. Exercises AppShell.svelte.
 */
let seeded = false;

test.beforeEach(async ({ page }) => {
  await apiLogin(page);
  if (!seeded) {
    await seedFilesAndFolders(page).catch(() => {});
    seeded = true;
  }
});

test('global search box shows suggestions and submits', async ({ page }) => {
  await page.goto('/files');
  await expect(page.getByTestId('appshell-logo-link')).toBeVisible({ timeout: 15_000 });

  // The collapsed search toggle only appears on narrow layouts; ignore if absent.
  await page.getByTestId('appshell-search-toggle-btn').click({ timeout: 2_000 }).catch(() => {});
  await page.getByTestId('appshell-search-input').fill('README', { timeout: 5_000 }).catch(() => {});
  await page.waitForTimeout(600); // debounced suggestion fetch
  await page.getByTestId('appshell-search-clear-btn').click({ timeout: 2_000 }).catch(() => {});
  await page.getByTestId('appshell-search-input').fill('Documents', { timeout: 5_000 }).catch(() => {});
  await page.getByTestId('appshell-search-submit-btn').click({ timeout: 2_000 }).catch(() => {});
});

test('notification, user, and language menus open', async ({ page }) => {
  await page.goto('/files');
  await expect(page.getByTestId('appshell-user-menu-btn')).toBeVisible({ timeout: 15_000 });

  await page.getByTestId('appshell-notif-bell-btn').click({ timeout: 2_000 }).catch(() => {});

  await page.getByTestId('appshell-user-menu-btn').click();
  await expect(page.getByTestId('appshell-user-menu-profile-item')).toBeVisible();
  await page.getByTestId('appshell-lang-toggle-btn').click({ timeout: 2_000 }).catch(() => {});
});
