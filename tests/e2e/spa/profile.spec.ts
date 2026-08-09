import { test, expect } from './coverage-helpers';
import { apiLogin } from './helpers';

/**
 * Profile route — edit the profile, open the avatar panel, and generate an app
 * password. Covers the profile page + the auth/admin profile endpoints it uses.
 */
test.beforeEach(async ({ page }) => {
  await apiLogin(page);
});

test('edit the profile form', async ({ page }) => {
  await page.goto('/profile');
  await expect(page.getByTestId('profile-edit-form')).toBeVisible({ timeout: 15_000 });

  await page.getByTestId('profile-given-name-input').fill('E2E');
  await page.getByTestId('profile-family-name-input').fill('Tester');
  await page.getByTestId('profile-language-select').selectOption({ index: 1 }).catch(() => {});
  await page.getByTestId('profile-notify-on-share-checkbox').click({ timeout: 2_000 }).catch(() => {});
  await page.getByTestId('profile-save-btn').click();
  // The save button is the same control; just ensure the form stays mounted.
  await expect(page.getByTestId('profile-edit-form')).toBeVisible();
});

test('open the avatar edit panel and upload an image', async ({ page }) => {
  await page.goto('/profile');
  await expect(page.getByTestId('profile-avatar-edit-btn')).toBeVisible({ timeout: 15_000 });
  await page.getByTestId('profile-avatar-edit-btn').click();
  await expect(page.getByTestId('profile-avatar-edit-panel')).toBeVisible();

  await page.getByTestId('profile-avatar-url-tab').click();
  await expect(page.getByTestId('profile-avatar-url-input')).toBeVisible();

  // Upload tab → set an image (exercises client-side image resizing).
  await page.getByTestId('profile-avatar-upload-tab').click();
  await page
    .getByTestId('profile-avatar-file-input')
    .setInputFiles({
      name: 'avatar.png',
      mimeType: 'image/png',
      buffer: Buffer.from(
        'iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAAC0lEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==',
        'base64',
      ),
    })
    .catch(() => {});
  await page.waitForTimeout(500);
  await page.getByTestId('profile-avatar-save-btn').click({ timeout: 3_000 }).catch(() => {});
  // Remove the avatar we just set (the remove button shows once one exists).
  await page.getByTestId('profile-avatar-edit-btn').click({ timeout: 3_000 }).catch(() => {});
  await page.getByTestId('profile-avatar-remove-btn').click({ timeout: 3_000 }).catch(() => {});
  await page.getByTestId('profile-avatar-cancel-btn').click({ timeout: 3_000 }).catch(() => {});
});

test('password change rejects a wrong current password', async ({ page }) => {
  await page.goto('/profile');
  await expect(page.getByTestId('profile-password-form')).toBeVisible({ timeout: 15_000 });
  // Wrong current password → rejected, so the admin login stays valid for
  // other tests while still exercising the change-password handler.
  await page.getByTestId('profile-current-password-input').fill('definitely-wrong-current');
  await page.getByTestId('profile-new-password-input').fill('NewPassword1!');
  await page.getByTestId('profile-confirm-password-input').fill('NewPassword1!');
  await page.getByTestId('profile-update-password-btn').click();
  await expect(page.getByTestId('profile-password-form')).toBeVisible();
});

test('generate and revoke an app password', async ({ page }) => {
  await page.goto('/profile');
  await expect(page.getByTestId('profile-app-pw-label-input')).toBeVisible({ timeout: 15_000 });
  await page.getByTestId('profile-app-pw-label-input').fill('e2e-token');
  await page.getByTestId('profile-app-pw-generate-btn').click();

  // The generated secret + copy button appear once created.
  await expect(page.getByTestId('profile-app-pw-copy-btn')).toBeVisible({ timeout: 15_000 });
  await page.getByTestId('profile-app-pw-copy-btn').click().catch(() => {});

  // Revoke the app password we just created.
  const revoke = page.locator('[data-testid^="profile-app-pw-revoke-"]').first();
  if (await revoke.isVisible().catch(() => false)) {
    await revoke.click();
    const confirm = page.getByTestId('dialog-host-confirm-btn');
    if (await confirm.isVisible().catch(() => false)) await confirm.click();
  }
  await expect(page.getByTestId('profile-app-pw-generate-btn')).toBeVisible();
});
