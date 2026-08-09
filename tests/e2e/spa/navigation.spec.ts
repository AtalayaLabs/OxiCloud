import { test, expect } from './coverage-helpers';
import { apiLogin, seedFilesAndFolders } from './helpers';

/**
 * Loads every primary route while authenticated. Each route is its own test
 * (own page/document) so its `window.__coverage__` is captured before the next
 * navigation resets it — a `goto` loop in one test would only keep the last
 * route's data. Visiting a route mounts its `+page.svelte` and the API
 * endpoint modules it imports, which is the bulk of first-load coverage.
 */
let seeded = false;

test.beforeEach(async ({ page }) => {
  await apiLogin(page);
  if (!seeded) {
    // Seed once per worker. Tolerant of a re-seed (e.g. after a worker reset)
    // hitting "already exists" so it never fails an unrelated route test.
    await seedFilesAndFolders(page).catch(() => {});
    seeded = true;
  }
});

// Routes that render inside the standard app shell (assert the shell logo).
const SHELL_ROUTES: { path: string; name: string }[] = [
  { path: '/files', name: 'files' },
  { path: '/shared', name: 'shared' },
  { path: '/shared-with-me', name: 'shared-with-me' },
  { path: '/recent', name: 'recent' },
  { path: '/favorites', name: 'favorites' },
  { path: '/photos', name: 'photos' },
  { path: '/music', name: 'music' },
  { path: '/trash', name: 'trash' },
  { path: '/search?q=README', name: 'search' },
  { path: '/profile', name: 'profile' },
  { path: '/groups', name: 'groups' },
  { path: '/admin', name: 'admin' },
];

for (const route of SHELL_ROUTES) {
  test(`route ${route.name} mounts inside the app shell`, async ({ page }) => {
    await page.goto(route.path);
    await expect(page.getByTestId('appshell-logo-link')).toBeVisible({ timeout: 15_000 });
    // Let lazy chunks + onMount data fetches settle so their code is exercised.
    await page.waitForLoadState('networkidle').catch(() => {});
  });
}

// The device-pairing route is a standalone page (OAuth device-code flow); it
// does NOT use the app shell, so assert its own form instead.
test('route device renders the pairing form', async ({ page }) => {
  await page.goto('/device');
  await expect(page.getByTestId('device-code-form')).toBeVisible({ timeout: 15_000 });
  await expect(page.getByTestId('device-code-input')).toBeVisible();
});
