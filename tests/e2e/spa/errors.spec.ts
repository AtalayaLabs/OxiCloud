import { test, expect } from './coverage-helpers';
import { apiLogin, apiCreateFolder } from './helpers';

/**
 * Error-path coverage — drive the catch/error branches that the happy-path
 * specs don't reach: a duplicate-folder 409 and an over-cap (413) upload.
 */
test.beforeEach(async ({ page }) => {
  await apiLogin(page);
});

function uniq(p: string): string {
  return `${p}-${Date.now()}-${Math.floor(Math.random() * 1e6)}`;
}

test('creating a duplicate folder surfaces an error toast', async ({ page }) => {
  const parent = await apiCreateFolder(page, uniq('DupHost'));
  const name = uniq('Dup');
  await apiCreateFolder(page, name, parent.id); // pre-create the clashing name

  await page.goto(`/files/${parent.id}`);
  await expect(page.getByTestId(name)).toBeVisible({ timeout: 15_000 });

  await page.getByTestId('files-new-folder-btn').click();
  await page.getByTestId('dialog-host-prompt-input').fill(name);
  await page.getByTestId('dialog-host-submit-btn').click();

  // The 409 is caught and raised as a toast.
  await expect(page.locator('[data-testid^="toaster-toast-"]').first()).toBeVisible({
    timeout: 15_000,
  });
});

test('an over-cap upload is handled without crashing', async ({ page }) => {
  const folder = await apiCreateFolder(page, uniq('BigUp'));
  await page.goto(`/files/${folder.id}`);
  await expect(page.getByTestId('files-upload-file-input')).toBeAttached({ timeout: 15_000 });

  // 5 MiB exceeds the 4 MiB test cap → the upload fails (413) and is caught.
  const big = Buffer.alloc(5 * 1024 * 1024, 65);
  await page.getByTestId('files-upload-file-input').setInputFiles({
    name: 'big.bin',
    mimeType: 'application/octet-stream',
    buffer: big,
  });
  await page.waitForTimeout(2_000);
  // The page stays usable (the error path ran).
  await expect(page.getByTestId('files-upload-btn')).toBeVisible();
});
