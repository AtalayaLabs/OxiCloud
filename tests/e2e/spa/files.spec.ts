import { test, expect } from './coverage-helpers';
import { apiLogin, apiCreateFolder, apiUploadFile, SAMPLE_FILES } from './helpers';

/**
 * File-browser CRUD against the SvelteKit `/files` route — the single largest
 * source file. Drives the toolbar, the right-click context menu, and the
 * prompt/confirm dialogs, which also exercises the files/folders/favorites
 * endpoint modules.
 */
test.beforeEach(async ({ page }) => {
  await apiLogin(page);
});

function uniq(prefix: string): string {
  return `${prefix}-${Date.now()}-${Math.floor(Math.random() * 1e6)}`;
}

test('create a folder via the toolbar', async ({ page }) => {
  const name = uniq('Created');
  await page.goto('/files');
  await page.getByTestId('files-new-folder-btn').click();
  await page.getByTestId('dialog-host-prompt-input').fill(name);
  await page.getByTestId('dialog-host-submit-btn').click();
  await expect(page.getByTestId(name)).toBeVisible({ timeout: 15_000 });
});

test('rename a folder via the context menu', async ({ page }) => {
  const before = uniq('Rename');
  const after = uniq('Renamed');
  await apiCreateFolder(page, before);
  await page.goto('/files');
  await expect(page.getByTestId(before)).toBeVisible({ timeout: 15_000 });

  await page.getByTestId(before).click({ button: 'right' });
  await page.getByTestId('files-ctx-rename-item').click();
  const input = page.getByTestId('dialog-host-prompt-input');
  await input.fill(after);
  await page.getByTestId('dialog-host-submit-btn').click();

  await expect(page.getByTestId(after)).toBeVisible({ timeout: 15_000 });
  await expect(page.getByTestId(before)).toHaveCount(0);
});

test('favorite a folder via the context menu', async ({ page }) => {
  const name = uniq('Fav');
  await apiCreateFolder(page, name);
  await page.goto('/files');
  await expect(page.getByTestId(name)).toBeVisible({ timeout: 15_000 });

  await page.getByTestId(name).click({ button: 'right' });
  await page.getByTestId('files-ctx-favorite-item').click();

  // Action completed — the row is still present and the menu has closed.
  await expect(page.getByTestId('files-context-menu')).toHaveCount(0);
  await expect(page.getByTestId(name)).toBeVisible();
});

test('delete a folder via the context menu', async ({ page }) => {
  const name = uniq('Delete');
  await apiCreateFolder(page, name);
  await page.goto('/files');
  await expect(page.getByTestId(name)).toBeVisible({ timeout: 15_000 });

  await page.getByTestId(name).click({ button: 'right' });
  await page.getByTestId('files-ctx-delete-item').click();
  await page.getByTestId('dialog-host-confirm-btn').click();

  await expect(page.getByTestId(name)).toHaveCount(0, { timeout: 15_000 });
});

test('upload a file via the hidden file input', async ({ page }) => {
  const folder = uniq('Uploads');
  const created = await apiCreateFolder(page, folder);
  // Navigate straight into the (empty) folder by ID (the route keys on folder
  // id, not name) — avoids the crowded root listing and click ambiguity.
  await page.goto(`/files/${created.id}`);
  await expect(page.getByTestId('files-upload-file-input')).toBeAttached({ timeout: 15_000 });
  // Now inside the folder; upload a text file by setting the hidden input.
  const f = SAMPLE_FILES.text();
  await page.getByTestId('files-upload-file-input').setInputFiles({
    name: f.name,
    mimeType: f.mimeType,
    buffer: f.body,
  });
  await expect(page.getByTestId(f.name)).toBeVisible({ timeout: 15_000 });
});

test('open a text file in the viewer', async ({ page }) => {
  const folderName = uniq('Viewer');
  const folder = await apiCreateFolder(page, folderName);
  const f = SAMPLE_FILES.markdown();
  await apiUploadFile(page, f, folder.id);

  await page.goto('/files');
  await page.getByTestId(folderName).click();
  await expect(page.getByTestId(f.name)).toBeVisible({ timeout: 15_000 });

  // Open via the context menu, then assert the inline viewer dialog appears.
  await page.getByTestId(f.name).click({ button: 'right' });
  await page.getByTestId('files-ctx-file-open-item').click();
  await expect(page.getByTestId('file-viewer-dialog')).toBeVisible({ timeout: 15_000 });
  await page.getByTestId('file-viewer-close-btn').click();
  await expect(page.getByTestId('file-viewer-dialog')).toHaveCount(0);
});
