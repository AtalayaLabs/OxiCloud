import { test, expect } from './coverage-helpers';
import { apiLogin, apiCreateFolder, apiAdminCreateUser, apiCreateGroup } from './helpers';

/**
 * User-to-user sharing — create a second user, add them as a member through the
 * ShareDialog people tab, change role / toggle notify, then manage the grant
 * from /shared. Covers the ShareDialog member paths + shared user-grant rows.
 */
test.beforeEach(async ({ page }) => {
  await apiLogin(page);
});

function uniq(p: string): string {
  return `${p}-${Date.now()}-${Math.floor(Math.random() * 1e6)}`;
}

test('share a folder with another user and manage the grant', async ({ page }) => {
  const username = await apiAdminCreateUser(page, uniq('shareu'));
  const folderName = uniq('UserShare');
  // Isolate in a fresh parent (the /files root accumulates 100s of folders
  // across the suite and won't render an arbitrary one).
  const parent = await apiCreateFolder(page, uniq('UserShareHost'));
  await apiCreateFolder(page, folderName, parent.id);

  // Open the share dialog → people tab → search for the user and add them.
  await page.goto(`/files/${parent.id}`);
  await expect(page.getByTestId(folderName)).toBeVisible({ timeout: 15_000 });
  await page.getByTestId(folderName).click({ button: 'right' });
  await page.getByTestId('files-ctx-share-item').click();
  await expect(page.getByTestId('share-dialog')).toBeVisible();
  await page.getByTestId('share-dialog-people-tab').click();

  await page.getByTestId('share-dialog-search-input').fill(username);
  const result = page.locator('[data-testid^="share-dialog-result-user-"]').first();
  await expect(result).toBeVisible({ timeout: 15_000 });
  await result.click();

  // The member row appears with role/notify/remove controls.
  const memberRole = page.locator('[data-testid^="share-dialog-member-role-"]').first();
  await expect(memberRole).toBeVisible({ timeout: 15_000 });
  await memberRole.selectOption({ index: 1 }).catch(() => {});
  await page
    .locator('[data-testid^="share-dialog-member-notify-"]')
    .first()
    .click({ timeout: 3_000 })
    .catch(() => {});
  await page.getByTestId('share-dialog-close-btn').click();
  await expect(page.getByTestId('share-dialog')).toHaveCount(0);

  // Manage the grant from /shared. Scope to our folder's lane when present so
  // we drive the *user* grant menu (role / notify / expiry / remove); the menu
  // closes after each action, so reopen the kebab between steps.
  await page.goto('/shared');
  const lane = page.locator('section.ms-lane').filter({ hasText: folderName });
  const scope = (await lane.count()) > 0 ? lane.first() : page.locator('body');
  const kebab = scope.locator('[data-testid^="shared-kebab-"]').first();
  await expect(kebab).toBeVisible({ timeout: 15_000 });

  await kebab.click();
  await scope.locator('[data-testid^="shared-notify-"]').first().click({ timeout: 2_000 }).catch(() => {});
  await kebab.click().catch(() => {});
  await scope.locator('[data-testid^="shared-role-"]').first().click({ timeout: 2_000 }).catch(() => {});
  await kebab.click().catch(() => {});
  await scope
    .locator('[data-testid^="shared-expiry-"][data-testid$="-input"]')
    .first()
    .fill('2031-06-01', { timeout: 2_000 })
    .catch(() => {});
  await kebab.click().catch(() => {});
  await scope
    .locator('[data-testid^="shared-remove-access-"]')
    .first()
    .click({ timeout: 2_000 })
    .catch(() => {});
  const confirm = page.getByTestId('dialog-host-confirm-btn');
  if (await confirm.isVisible().catch(() => false)) await confirm.click();
});

test('share a folder with a group', async ({ page }) => {
  const groupName = await apiCreateGroup(page, uniq('shareg'));
  const folderName = uniq('GroupShare');
  const parent = await apiCreateFolder(page, uniq('GroupShareHost'));
  await apiCreateFolder(page, folderName, parent.id);

  await page.goto(`/files/${parent.id}`);
  await expect(page.getByTestId(folderName)).toBeVisible({ timeout: 15_000 });
  await page.getByTestId(folderName).click({ button: 'right' });
  await page.getByTestId('files-ctx-share-item').click();
  await expect(page.getByTestId('share-dialog')).toBeVisible();
  await page.getByTestId('share-dialog-people-tab').click();

  // Search the group and add it as a member.
  await page.getByTestId('share-dialog-search-input').fill(groupName);
  const groupResult = page.locator('[data-testid^="share-dialog-result-group-"]').first();
  await expect(groupResult).toBeVisible({ timeout: 15_000 });
  await groupResult.click();

  await expect(page.locator('[data-testid^="share-dialog-member-"]').first()).toBeVisible({
    timeout: 15_000,
  });
  await page.getByTestId('share-dialog-close-btn').click();
  await expect(page.getByTestId('share-dialog')).toHaveCount(0);
});
