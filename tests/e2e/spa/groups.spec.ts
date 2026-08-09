import { test, expect } from './coverage-helpers';
import { apiLogin, apiAdminCreateUser } from './helpers';

/**
 * Groups route — create a group, expand it, search members, rename, and delete
 * (delete is a type-the-name confirm). Group rows are id-keyed, so we scope to
 * our own `li.group` by its unique name and prefix-match the buttons.
 */
test.beforeEach(async ({ page }) => {
  await apiLogin(page);
});

function uniq(p: string): string {
  return `${p}-${Date.now()}-${Math.floor(Math.random() * 1e6)}`;
}

test('group lifecycle: create, expand, rename, delete', async ({ page }) => {
  const name = uniq('Grp');
  const renamed = uniq('Grpr');
  await page.goto('/groups');
  await expect(page.getByTestId('groups-create-btn')).toBeVisible({ timeout: 15_000 });

  await page.getByTestId('groups-create-btn').click();
  await page.getByTestId('dialog-host-prompt-input').fill(name);
  await page.getByTestId('dialog-host-submit-btn').click();

  const row = page.locator('li.group').filter({ hasText: name });
  await expect(row).toBeVisible({ timeout: 15_000 });

  // Expand → members panel, then run a member search (covers recipients).
  await row.locator('[data-testid^="groups-expand-"]').click();
  await expect(row.locator('[data-testid^="groups-members-panel-"]')).toBeVisible();
  await row.getByTestId('groups-member-add-input').fill('admin').catch(() => {});
  await page.waitForTimeout(600);

  // Rename.
  await row.locator('[data-testid^="groups-rename-"]').click();
  await page.getByTestId('dialog-host-prompt-input').fill(renamed);
  await page.getByTestId('dialog-host-submit-btn').click();
  const renamedRow = page.locator('li.group').filter({ hasText: renamed });
  await expect(renamedRow).toBeVisible({ timeout: 15_000 });

  // Delete — the confirm prompt requires typing the group name.
  await renamedRow.locator('[data-testid^="groups-delete-"]').click();
  await page.getByTestId('dialog-host-prompt-input').fill(renamed);
  await page.getByTestId('dialog-host-submit-btn').click();
  await expect(page.locator('li.group').filter({ hasText: renamed })).toHaveCount(0, {
    timeout: 15_000,
  });
});

test('add and remove a group member', async ({ page }) => {
  const username = await apiAdminCreateUser(page, uniq('grpmem'));
  const groupName = uniq('GrpM');

  await page.goto('/groups');
  await page.getByTestId('groups-create-btn').click();
  await page.getByTestId('dialog-host-prompt-input').fill(groupName);
  await page.getByTestId('dialog-host-submit-btn').click();

  const row = page.locator('li.group').filter({ hasText: groupName });
  await expect(row).toBeVisible({ timeout: 15_000 });
  await row.locator('[data-testid^="groups-expand-"]').click();
  await expect(row.locator('[data-testid^="groups-members-panel-"]')).toBeVisible();

  // Search the new user and add them.
  await row.getByTestId('groups-member-add-input').fill(username);
  const opt = row.locator('[data-testid^="groups-member-add-opt-user-"]').first();
  await expect(opt).toBeVisible({ timeout: 15_000 });
  await opt.click();

  // Remove the member we just added.
  const remove = row.locator('[data-testid^="groups-member-remove-"]').first();
  await expect(remove).toBeVisible({ timeout: 15_000 });
  await remove.click();
});
