import { test, expect } from './coverage-helpers';
import { apiLogin } from './helpers';

/**
 * Regression: DrivePicker's scoped `display: flex` tied the
 * `.sidebar--collapsed .drive-picker` hide rule on specificity and won on
 * source order, so the drive list stayed visible beside the icon rail after
 * the sidebar was collapsed.
 */
test('collapsed sidebar hides the drive picker below Files', async ({ page }) => {
  const stamp = '2026-01-01T00:00:00Z';
  const drive = (over: Record<string, unknown>) => ({
    id: 'drive',
    name: 'Drive',
    kind: 'personal',
    root_folder_id: 'root',
    used_bytes: 0,
    quota_bytes: null,
    policies: {},
    created_at: stamp,
    updated_at: stamp,
    ...over
  });

  // Stubbed because the picker only renders when there is more than one
  // drive; a fresh test account has exactly one.
  await page.route('**/api/drives', (route) =>
    route.fulfill({
      json: [
        drive({ id: 'personal', name: 'Personal', default_for_user: 'admin' }),
        drive({ id: 'team', name: 'Team', kind: 'shared', root_folder_id: 'team-root' })
      ]
    })
  );

  await apiLogin(page);
  await page.goto('/files');
  await expect(page.getByTestId('appshell-logo-link')).toBeVisible({ timeout: 15_000 });
  await expect(page.locator('.drive-picker')).toBeVisible();

  await page.getByTestId('appshell-sidebar-collapse-btn').click();

  await expect(page.locator('aside.sidebar')).toHaveClass(/sidebar--collapsed/);
  await expect(page.locator('.drive-picker')).toBeHidden();
});
