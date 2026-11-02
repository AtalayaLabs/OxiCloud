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

test('creating a duplicate folder surfaces an inline error in the dialog', async ({ page }) => {
  const parent = await apiCreateFolder(page, uniq('DupHost'));
  const name = uniq('Dup');
  await apiCreateFolder(page, name, parent.id); // pre-create the clashing name

  await page.goto(`/files/${parent.id}`);
  await expect(page.getByTestId(name)).toBeVisible({ timeout: 15_000 });

  await page.getByTestId('files-add-btn').click();
  await page.getByTestId('files-new-folder-btn').click();
  await page.getByTestId('dialog-host-prompt-input').fill(name);
  await page.getByTestId('dialog-host-submit-btn').click();

  // The 409 is caught by `promptDialog`'s `action` callback and surfaces
  // INLINE in the modal (with `role="alert"` + `aria-invalid="true"` on
  // the input) — not a toast anymore. Pre-feat/modal-error-handling the
  // dialog closed and a corner toast appeared; the new UX keeps the modal
  // open so the user can edit the typed value and retry without losing
  // it. See `frontend/AGENTS.md § Dialog error UX`.
  //
  // Three invariants pinned together so a regression in any ONE of them
  // trips here:
  //   1. The modal stays open (dialog-host-prompt-input is still visible).
  //   2. The inline-error paragraph renders inside the modal.
  //   3. No toast appeared (would signal a drift back to the old pattern).
  const modal = page.getByTestId('modal');
  await expect(modal).toBeVisible({ timeout: 15_000 });
  await expect(modal.getByRole('alert')).toBeVisible({ timeout: 15_000 });
  await expect(page.getByTestId('dialog-host-prompt-input')).toBeVisible();
  await expect(page.getByTestId('dialog-host-prompt-input')).toHaveAttribute(
    'aria-invalid',
    'true',
  );
  // Toast must NOT appear — regression guard.
  await expect(page.locator('[data-testid^="toaster-toast-"]')).toHaveCount(0);
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
  await expect(page.getByTestId('files-add-btn')).toBeVisible();
});
