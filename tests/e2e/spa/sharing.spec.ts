import { test, expect } from './coverage-helpers';
import { apiLogin, apiCreateFolder } from './helpers';

/**
 * ShareDialog coverage — opened from the files context menu. Exercises the
 * link tab (create a public link) and the people tab search, which pulls in
 * the share / shares / grants / recipients endpoint modules.
 */
test.beforeEach(async ({ page }) => {
  await apiLogin(page);
});

function uniq(p: string): string {
  return `${p}-${Date.now()}-${Math.floor(Math.random() * 1e6)}`;
}

/**
 * Create the named folder inside a *fresh* parent and open it, so the folder is
 * the only row (the shared `/files` root accumulates hundreds of folders across
 * the suite and its virtualised list won't render an arbitrary one), then open
 * its share dialog from the context menu.
 */
async function openShareDialog(page: import('@playwright/test').Page, name: string): Promise<void> {
  const parent = await apiCreateFolder(page, uniq('ShareHost'));
  await apiCreateFolder(page, name, parent.id);
  await page.goto(`/files/${parent.id}`);
  await expect(page.getByTestId(name)).toBeVisible({ timeout: 15_000 });
  await page.getByTestId(name).click({ button: 'right' });
  await page.getByTestId('files-ctx-share-item').click();
  await expect(page.getByTestId('share-dialog')).toBeVisible();
}

test('create a public link via the share dialog', async ({ page }) => {
  const name = uniq('Share');
  await openShareDialog(page, name);

  // Link tab → fill name/password/expiry, then create a link.
  await page.getByTestId('share-dialog-link-tab').click();
  await page.getByTestId('share-dialog-link-name-input').fill('e2e link');
  await page.getByTestId('share-dialog-link-password-input').fill('secret123', { timeout: 2_000 }).catch(() => {});
  await page.getByTestId('share-dialog-link-expires-input').fill('2031-01-01', { timeout: 2_000 }).catch(() => {});
  await page.getByTestId('share-dialog-create-btn').click();

  // A created link exposes copy/delete buttons keyed by its id.
  const del = page.locator('[data-testid^="share-dialog-link-delete-btn-"]').first();
  await expect(del).toBeVisible({ timeout: 15_000 });
  await page.locator('[data-testid^="share-dialog-link-copy-btn-"]').first().click({ timeout: 2_000 }).catch(() => {});

  // Delete the link we just made.
  await del.click();
  const confirm = page.getByTestId('dialog-host-confirm-btn');
  if (await confirm.isVisible().catch(() => false)) await confirm.click();

  await page.getByTestId('share-dialog-close-btn').click();
  await expect(page.getByTestId('share-dialog')).toHaveCount(0);
});

test('people tab search runs in the share dialog', async ({ page }) => {
  const name = uniq('ShareP');
  await openShareDialog(page, name);

  await page.getByTestId('share-dialog-people-tab').click();
  await page.getByTestId('share-dialog-search-input').fill('admin');
  // Let the debounced search fire (covers the recipients endpoint).
  await page.waitForTimeout(800);

  await page.getByTestId('share-dialog-close-btn').click();
  await expect(page.getByTestId('share-dialog')).toHaveCount(0);
});

test('invite an external user by email from the share dialog', async ({ page }) => {
  const name = uniq('ShareE');
  await openShareDialog(page, name);
  await page.getByTestId('share-dialog-people-tab').click();

  // Typing an email surfaces a synthetic "invite by email" result.
  const email = `invitee-${Date.now()}@example.test`;
  await page.getByTestId('share-dialog-search-input').fill(email);
  const inviteRow = page.locator('[data-testid^="share-dialog-result-email-"]').first();
  await expect(inviteRow).toBeVisible({ timeout: 15_000 });
  await inviteRow.click();

  // The invitee becomes a pending member.
  await expect(page.locator('[data-testid^="share-dialog-member-"]').first()).toBeVisible({
    timeout: 15_000,
  });
  await page.getByTestId('share-dialog-close-btn').click();
  await expect(page.getByTestId('share-dialog')).toHaveCount(0);
});
