import { test, expect } from './coverage-helpers';
import type { Page } from '@playwright/test';
import { apiLogin, apiCreateFolder, apiAdminCreateUser, apiCreateGroup } from './helpers';

/**
 * Grant modal — the critical paths, in a browser.
 *
 * `share-users.spec.ts` covers one user and one group, end to end through
 * /shared. This file covers what the add form itself does, which is where the
 * recent work landed:
 *
 *   - the queue: one recipient, TWO in a single [Add], a group, and a pick
 *     dropped before committing;
 *   - the role segments that replaced a `<select>`;
 *   - removing a member again from the same dialog;
 *   - the row's shared chip following the dialog without a reload.
 *
 * Why in a browser as well as in Vitest: the Vitest pins mock the API, so they
 * prove the component's wiring and nothing about the round trip. Three of
 * these paths cross it — two grants in one press, a revoke, and the chip
 * broadcast arriving at a list the dialog does not own.
 */
test.beforeEach(async ({ page }) => {
  await apiLogin(page);
});

function uniq(p: string): string {
  return `${p}-${Date.now()}-${Math.floor(Math.random() * 1e6)}`;
}

/**
 * A folder of our own inside a fresh parent, with its share dialog open on
 * the people tab. The parent matters: the /files root accumulates hundreds of
 * folders across the suite and will not render an arbitrary one.
 */
async function openShareDialog(page: Page): Promise<{ parentId: string; folderName: string }> {
  const folderName = uniq('GrantModal');
  const parent = await apiCreateFolder(page, uniq('GrantModalHost'));
  await apiCreateFolder(page, folderName, parent.id);

  await page.goto(`/files/${parent.id}`);
  await expect(page.getByTestId(folderName)).toBeVisible({ timeout: 15_000 });
  await page.getByTestId(folderName).click({ button: 'right' });
  await page.getByTestId('files-ctx-share-item').click();
  await expect(page.getByTestId('share-dialog')).toBeVisible();
  await page.getByTestId('share-dialog-people-tab').click();
  return { parentId: parent.id, folderName };
}

/** Type a name and queue the first matching result of `kind`. */
async function queue(page: Page, name: string, kind: 'user' | 'group'): Promise<void> {
  await page.getByTestId('share-dialog-search-input').fill(name);
  const result = page.locator(`[data-testid^="share-dialog-result-${kind}-"]`).first();
  await expect(result).toBeVisible({ timeout: 15_000 });
  await result.click();
}

test('adds one user, with the role chosen on the segments', async ({ page }) => {
  const username = await apiAdminCreateUser(page, uniq('gm1'));
  await openShareDialog(page);

  // At rest the form is the name field alone — the role segments and [Add]
  // arrive with the first pick, because until then they are decisions about
  // nobody.
  await expect(page.getByTestId('share-dialog-add-btn')).toHaveCount(0);
  await queue(page, username, 'user');
  await expect(page.getByTestId('share-dialog-selected')).toBeVisible();

  // The segmented picker replaced a `<select>`; pressing "Can edit" is what
  // the grant must carry.
  await page.getByTestId('share-dialog-role-editor-btn').click();
  await expect(page.getByTestId('share-dialog-role-editor-btn')).toHaveAttribute(
    'aria-checked',
    'true',
  );
  await page.getByTestId('share-dialog-add-btn').click();

  // Granted: the member row appears, and its own role control reads Editor —
  // this is the assertion that the pressed segment reached the server rather
  // than the form's default.
  const memberRole = page.locator('[data-testid^="share-dialog-member-role-"]').first();
  await expect(memberRole).toBeVisible({ timeout: 15_000 });
  await expect(memberRole).toHaveValue('editor');
  // The queue is empty again, so a second press cannot re-grant.
  await expect(page.getByTestId('share-dialog-add-btn')).toHaveCount(0);
});

test('adds two users in a single Add', async ({ page }) => {
  const first = await apiAdminCreateUser(page, uniq('gm2a'));
  const second = await apiAdminCreateUser(page, uniq('gm2b'));
  await openShareDialog(page);

  // The field survives the first pick — that is the whole point of the queue.
  await queue(page, first, 'user');
  await queue(page, second, 'user');
  await expect(page.locator('[data-testid^="share-dialog-pick-user-"]')).toHaveCount(2);
  // The count is in the label, or [Add] over two chips reads as "add the one
  // I just typed".
  await expect(page.getByTestId('share-dialog-add-btn')).toContainText('2');

  await page.getByTestId('share-dialog-add-btn').click();

  // Two member rows, one press, one role.
  await expect(page.locator('[data-testid^="share-dialog-member-role-"]')).toHaveCount(2, {
    timeout: 15_000,
  });
  await expect(page.locator('[data-testid^="share-dialog-pick-user-"]')).toHaveCount(0);
});

test('drops a queued recipient before committing', async ({ page }) => {
  const keep = await apiAdminCreateUser(page, uniq('gm3keep'));
  const drop = await apiAdminCreateUser(page, uniq('gm3drop'));
  await openShareDialog(page);

  await queue(page, keep, 'user');
  await queue(page, drop, 'user');
  await expect(page.locator('[data-testid^="share-dialog-pick-user-"]')).toHaveCount(2);

  // Remove the second chip, then commit. Only the kept one may be granted —
  // a queue that ignores its own ✕ grants access nobody asked for.
  await page.locator('[data-testid^="share-dialog-unpick-user-"]').last().click();
  await expect(page.locator('[data-testid^="share-dialog-pick-user-"]')).toHaveCount(1);
  await page.getByTestId('share-dialog-add-btn').click();

  await expect(page.locator('[data-testid^="share-dialog-member-role-"]')).toHaveCount(1, {
    timeout: 15_000,
  });
});

test('adds a group alongside a user in one press', async ({ page }) => {
  const username = await apiAdminCreateUser(page, uniq('gm4u'));
  const groupName = await apiCreateGroup(page, uniq('gm4g'));
  await openShareDialog(page);

  // A group and a user queue the same way and commit together; the subject
  // kinds take different branches server-side (`subject.type`).
  await queue(page, username, 'user');
  await queue(page, groupName, 'group');
  await expect(page.locator('[data-testid^="share-dialog-pick-group-"]')).toHaveCount(1);

  await page.getByTestId('share-dialog-add-btn').click();

  await expect(page.locator('[data-testid^="share-dialog-member-role-"]')).toHaveCount(2, {
    timeout: 15_000,
  });
});

test('removes a member again from the same dialog', async ({ page }) => {
  const username = await apiAdminCreateUser(page, uniq('gm5'));
  await openShareDialog(page);

  await queue(page, username, 'user');
  await page.getByTestId('share-dialog-add-btn').click();
  const remove = page.locator('[data-testid^="share-dialog-member-remove-"]').first();
  await expect(remove).toBeVisible({ timeout: 15_000 });

  await remove.click();

  // Gone from the list, and the dialog is back to its resting state: no
  // member rows, no commit controls.
  await expect(page.locator('[data-testid^="share-dialog-member-role-"]')).toHaveCount(0, {
    timeout: 15_000,
  });
  await expect(page.getByTestId('share-dialog-add-btn')).toHaveCount(0);
});

test('lights the row shared chip without a reload, and clears it on revoke', async ({ page }) => {
  const username = await apiAdminCreateUser(page, uniq('gm6'));
  const { folderName } = await openShareDialog(page);

  // The chip belongs to the LIST behind the dialog, which the dialog does not
  // own — it follows a window broadcast. This is the regression: sharing used
  // to leave the chip dark until the page was refreshed, and unsharing left
  // it lit.
  const chip = page.locator(`[data-testid^="resource-list-shared-"]`).first();

  await queue(page, username, 'user');
  await page.getByTestId('share-dialog-add-btn').click();
  await expect(page.locator('[data-testid^="share-dialog-member-role-"]')).toHaveCount(1, {
    timeout: 15_000,
  });
  await expect(chip).toHaveAttribute('aria-pressed', 'true', { timeout: 15_000 });

  // And back: revoking the only grant must put it out, again with no reload.
  await page.locator('[data-testid^="share-dialog-member-remove-"]').first().click();
  await expect(page.locator('[data-testid^="share-dialog-member-role-"]')).toHaveCount(0, {
    timeout: 15_000,
  });
  await expect(chip).toHaveAttribute('aria-pressed', 'false', { timeout: 15_000 });

  // Survives the reload too, which is the server half of the same fix: the
  // listing ETag now folds `is_shared`, so the refetch is a 200 with the
  // current flag rather than a 304 with the pre-revoke body.
  await page.getByTestId('share-dialog-close-btn').click();
  await page.reload();
  await expect(page.getByTestId(folderName)).toBeVisible({ timeout: 15_000 });
  await expect(chip).toHaveAttribute('aria-pressed', 'false', { timeout: 15_000 });
});
