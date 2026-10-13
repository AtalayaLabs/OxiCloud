import type { Page } from '@playwright/test';
import { test, expect } from './coverage-helpers';
import {
  SAMPLE_FILES,
  apiCreateFolder,
  apiLogin,
  apiRecordRecent,
  apiTrashFolder,
  apiUploadFile,
} from './helpers';

/**
 * Timestamp wire-shape pivot — rendering smoke.
 *
 * After the ISO 8601 pivot (commits a6c4ca16 + def8ea49 + c0740d41) every
 * server-emitted timestamp is an RFC 3339 UTC string. The Rust
 * `wire_shape_tests` freeze the server emission; this file is the FE-side
 * companion that asserts each timestamp-bearing surface renders a MEANINGFUL
 * date on the DOM — not any of the three canonical silent-failure markers:
 *
 *   - "1970"       — `new Date(epoch_seconds)` misread as milliseconds-since-
 *                     epoch (year 1970 ≈ what a 1_700_000_000 reads as when
 *                     the `* 1000` scale is forgotten)
 *   - "NaN"        — arithmetic on a string timestamp
 *   - "Invalid Date" — `.toString()` on `new Date("not-a-date")`
 *
 * Each test seeds the minimum state needed for the surface to have at least
 * one row, navigates to the route, waits for the row to render, then scopes a
 * `textContent()` scan to the main content area so a legitimate `1970` in a
 * footer link or docs page can't false-trip.
 *
 * The regex uses `\b` boundaries so an id prefix like `uuid-1970…` won't
 * false-match — only the standalone 4-digit year does.
 */

const BROKEN = /\b(1970|NaN|Invalid Date)\b/;

/** Scan a DOM region for any of the silent-failure markers. */
async function expectNoBrokenDates(page: Page, selector: string, label: string): Promise<void> {
  const text = (await page.locator(selector).textContent()) ?? '';
  expect(text, `broken timestamp in ${label} (${selector})`).not.toMatch(BROKEN);
}

test.beforeEach(async ({ page }) => {
  await apiLogin(page);
});

test('files list renders real dates after upload', async ({ page }) => {
  // Fresh unique folder + file per run so the test is idempotent against
  // the shared e2e DB (other specs may have left `Documents/README.md`
  // behind; `seedFilesAndFolders` would 409 on the "Documents" conflict).
  const folderName = `Dates-${Date.now()}`;
  const folder = await apiCreateFolder(page, folderName);
  await apiUploadFile(page, SAMPLE_FILES.markdown(), folder.id);
  await page.goto('/files');
  await expect(page.getByTestId(folderName)).toBeVisible({ timeout: 15_000 });
  await page.getByTestId(folderName).click();
  await expect(page.getByTestId('README.md')).toBeVisible({ timeout: 15_000 });
  await expectNoBrokenDates(page, '.main-content', `/files/${folderName}`);
});

test('favorites list renders real dates', async ({ page }) => {
  // Favorite a folder from /files so the favorites list has at least one row.
  const name = `Fav-${Date.now()}`;
  await apiCreateFolder(page, name);
  await page.goto('/files');
  await expect(page.getByTestId(name)).toBeVisible({ timeout: 15_000 });
  await page.getByTestId(name).click({ button: 'right' });
  // The context-menu `favorite` click is fire-and-forget — wait for the POST
  // to land before leaving the page (same pattern as favorites.spec.ts).
  const favBtn = page.getByRole('menuitem', { name: /favorite/i });
  const favPost = page.waitForResponse(
    (r) => r.url().includes('/api/favorites/') && r.request().method() === 'POST' && r.ok(),
  );
  await favBtn.click();
  await favPost;
  await page.goto('/favorites');
  await expect(page.getByTestId(name)).toBeVisible({ timeout: 15_000 });
  await expectNoBrokenDates(page, '.main-content', '/favorites');
});

test('recent list renders real dates', async ({ page }) => {
  const name = `Rec-${Date.now()}`;
  const folder = await apiCreateFolder(page, name);
  await apiRecordRecent(page, 'folder', folder.id);
  await page.goto('/recent');
  await expect(page.getByTestId(name)).toBeVisible({ timeout: 15_000 });
  await expectNoBrokenDates(page, '.main-content', '/recent');
});

test('trash list renders real dates', async ({ page }) => {
  const name = `Tra-${Date.now()}`;
  const folder = await apiCreateFolder(page, name);
  await apiTrashFolder(page, folder.id);
  await page.goto('/trash');
  await expect(page.getByTestId(name)).toBeVisible({ timeout: 15_000 });
  await expectNoBrokenDates(page, '.main-content', '/trash');
});

test('admin sessions table renders real dates', async ({ page }) => {
  await page.goto('/admin/sessions');
  // The caller's own session is always present post-login.
  await expect(page.getByTestId('admin-sessions-table')).toBeVisible({ timeout: 15_000 });
  // At least one row — the caller's own session is self-sourced on login,
  // so no explicit seed step is needed.
  await expect(page.locator('[data-testid^="admin-sessions-row-"]').first()).toBeVisible({
    timeout: 15_000,
  });
  await expectNoBrokenDates(page, '[data-testid="admin-sessions-table"]', '/admin/sessions');
});

test('admin users table renders real dates', async ({ page }) => {
  await page.goto('/admin/users');
  // The caller themselves is always listed; wait for the table to render.
  await expect(page.getByTestId('admin-users-create-btn')).toBeVisible({ timeout: 15_000 });
  // The user list is sibling to the create button — scope to main-content.
  await expectNoBrokenDates(page, '.main-content', '/admin/users');
});
