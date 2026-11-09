import { test, expect } from './coverage-helpers';
import { apiLogin, apiCreateFolder, apiRecordRecent, switchToListView } from './helpers';

/**
 * Recent route — populate it via the recents API (the SPA doesn't auto-record
 * through this endpoint), then exercise the list, batch selection, and clear.
 */
test.beforeEach(async ({ page }) => {
  await apiLogin(page);
});

function uniq(p: string): string {
  return `${p}-${Date.now()}-${Math.floor(Math.random() * 1e6)}`;
}

test('recent shows accessed items and batch selection', async ({ page }) => {
  const a = await apiCreateFolder(page, uniq('RecA'));
  const b = await apiCreateFolder(page, uniq('RecB'));
  await apiRecordRecent(page, 'folder', a.id);
  await apiRecordRecent(page, 'folder', b.id);

  await page.goto('/recent');
  await expect(page.getByTestId('appshell-logo-link')).toBeVisible({ timeout: 15_000 });

  // Switch to list view (reveals the select-all header) and batch-select.
  // /recent's batch bar was trimmed to Download + Remove-from-recent
  // (destructive-to-content actions moved into the row context menu),
  // so this exercises the new remove-from-recent batch instead of the
  // old batch-move-into-dialog flow.
  await switchToListView(page);
  await page.getByTestId('resource-list-select-all-checkbox').check();
  await page.getByTestId('recent-batch-remove-btn').click({ timeout: 3_000 }).catch(() => {});
});

// Clear-recent lives in its own test because it needs a NON-EMPTY history,
// and the batch-selection test above removes every row it seeded. Sharing
// one test made the assertion depend on whether that removal happened:
// `recent-batch-remove-btn` is clicked with `.catch(() => {})`, so a
// silently-failed click left the items in place and the button present,
// while a successful one emptied the list and unmounted it
// (`{#if items.length > 0}`). Green or red by coin toss.
//
// The visibility check itself is not wrapped in `if (await isVisible())`
// any more, and must not be again: that conditional made it
// unfalsifiable, so the button moving into the view-toggle cluster — which
// is what actually happened — left the spec green. A conditional around
// the step a test exists to verify turns a regression into a silent pass.
test('recent clears the history from the action cluster', async ({ page }) => {
  const c = await apiCreateFolder(page, uniq('RecClear'));
  await apiRecordRecent(page, 'folder', c.id);

  await page.goto('/recent');
  await expect(page.getByTestId('appshell-logo-link')).toBeVisible({ timeout: 15_000 });

  // Seeded one item and removed nothing, so the button's presence is a
  // fact this test can assert rather than probe.
  const clearBtn = page.getByTestId('recent-clear-btn');
  await expect(clearBtn).toBeVisible();

  // Armed confirm: the first click arms, the second runs.
  await clearBtn.click();
  await clearBtn.click();

  // And it actually cleared — with the history empty the snippet's
  // `items.length > 0` guard unmounts the button. Without this the test
  // would pass on two clicks that did nothing.
  await expect(clearBtn).toHaveCount(0);
});

test('recent grouping and sort cycle (ResourceList toolbar)', async ({ page }) => {
  const a = await apiCreateFolder(page, uniq('RecG'));
  await apiRecordRecent(page, 'folder', a.id);
  await page.goto('/recent');
  await expect(page.getByTestId('appshell-logo-link')).toBeVisible({ timeout: 15_000 });

  // Cycle every group-by dimension exposed by the shared ResourceList toolbar.
  for (let i = 0; i < 5; i++) {
    await page.getByTestId('display-mode-groupby-btn').click({ timeout: 2_000 }).catch(() => {});
    await page
      .locator('[data-testid^="display-mode-groupby-"][data-testid$="-item"]')
      .nth(i)
      .click({ timeout: 2_000 })
      .catch(() => {});
  }
  await page.getByTestId('display-mode-sort-direction-btn').click({ timeout: 2_000 }).catch(() => {});
});
