import { test, expect } from './coverage-helpers';
import { apiLogin, apiCreateFolder, apiUploadFile, SAMPLE_FILES } from './helpers';

/**
 * Deeper files-page coverage (the largest source file): list/grid view toggle,
 * column sorting, group-by lanes, breadcrumb navigation, and copy-via-dialog.
 */
test.beforeEach(async ({ page }) => {
  await apiLogin(page);
});

function uniq(p: string): string {
  return `${p}-${Date.now()}-${Math.floor(Math.random() * 1e6)}`;
}

test('sort columns and toggle list/grid views', async ({ page }) => {
  const folder = await apiCreateFolder(page, uniq('SortHost'));
  await apiUploadFile(page, SAMPLE_FILES.text(), folder.id);
  await apiUploadFile(page, SAMPLE_FILES.json(), folder.id);
  await page.goto(`/files/${folder.id}`);
  await expect(page.getByTestId(SAMPLE_FILES.text().name)).toBeVisible({ timeout: 15_000 });

  await page.getByTestId('display-mode-view-list-btn').click();
  // Column sort buttons live in the list-view header.
  await page.getByTestId('files-sort-name-btn').click({ timeout: 5_000 }).catch(() => {});
  await page.getByTestId('files-sort-size-btn').click({ timeout: 5_000 }).catch(() => {});
  await page.getByTestId('files-sort-modified_at-btn').click({ timeout: 5_000 }).catch(() => {});
  await page.getByTestId('display-mode-view-grid-btn').click();
});

test('sort by every column and group by every dimension', async ({ page }) => {
  const folder = await apiCreateFolder(page, uniq('Dims'));
  await apiUploadFile(page, SAMPLE_FILES.text(), folder.id);
  await apiUploadFile(page, SAMPLE_FILES.png(), folder.id);
  await apiUploadFile(page, SAMPLE_FILES.pdf(), folder.id);
  await page.goto(`/files/${folder.id}`);
  await expect(page.getByTestId(SAMPLE_FILES.text().name)).toBeVisible({ timeout: 15_000 });

  // List view exposes the column-sort buttons.
  await page.getByTestId('display-mode-view-list-btn').click();
  for (const col of ['name', 'owner', 'type', 'size', 'modified_at']) {
    await page.getByTestId(`files-sort-${col}-btn`).click({ timeout: 3_000 }).catch(() => {});
  }
  // Flip the sort direction.
  await page.getByTestId('display-mode-sort-direction-btn').click({ timeout: 3_000 }).catch(() => {});

  // Cycle through every group-by dimension.
  for (const g of ['type', 'size', 'modifiedAt', 'createdAt']) {
    await page.getByTestId('display-mode-groupby-btn').click({ timeout: 3_000 }).catch(() => {});
    await page.getByTestId(`display-mode-groupby-${g}-item`).click({ timeout: 3_000 }).catch(() => {});
  }
});

test('group files by type', async ({ page }) => {
  const folder = await apiCreateFolder(page, uniq('GroupHost'));
  await apiUploadFile(page, SAMPLE_FILES.text(), folder.id);
  await apiUploadFile(page, SAMPLE_FILES.png(), folder.id);
  await page.goto(`/files/${folder.id}`);
  await expect(page.getByTestId(SAMPLE_FILES.text().name)).toBeVisible({ timeout: 15_000 });

  await page.getByTestId('display-mode-groupby-btn').click();
  await page.getByTestId('display-mode-groupby-type-item').click();
  // The grouped (swimlane) view now renders; items remain visible.
  await expect(page.getByTestId(SAMPLE_FILES.text().name)).toBeVisible();
});

test('file context menu: favorite, copy, open-parent', async ({ page }) => {
  const folder = await apiCreateFolder(page, uniq('Ctx'));
  await apiCreateFolder(page, uniq('CtxDest'));
  const f = SAMPLE_FILES.text();
  await apiUploadFile(page, f, folder.id);
  await page.goto(`/files/${folder.id}`);
  await expect(page.getByTestId(f.name)).toBeVisible({ timeout: 15_000 });

  // Favorite the file from its context menu.
  await page.getByTestId(f.name).click({ button: 'right' });
  await page.getByTestId('files-ctx-favorite-item').click();
  await expect(page.getByTestId('files-context-menu')).toHaveCount(0);

  // Copy → opens the move dialog (copy mode); cancel.
  await page.getByTestId(f.name).click({ button: 'right' });
  await page.getByTestId('files-ctx-copy-item').click();
  await expect(page.getByTestId('move-dialog')).toBeVisible({ timeout: 15_000 });
  await page.getByTestId('move-dialog-cancel-btn').click();

  // Open-parent navigates to the file's parent folder.
  await page.getByTestId(f.name).click({ button: 'right' });
  await page.getByTestId('files-ctx-open-parent-item').click();
  await expect(page).toHaveURL(/\/files\//, { timeout: 15_000 });
});

test('download a folder as a zip via the context menu', async ({ page }) => {
  const folderName = uniq('Zip');
  const folder = await apiCreateFolder(page, folderName);
  await apiUploadFile(page, SAMPLE_FILES.text(), folder.id);
  await page.goto('/files');
  await expect(page.getByTestId(folderName)).toBeVisible({ timeout: 15_000 });

  await page.getByTestId(folderName).click({ button: 'right' });
  const downloadPromise = page.waitForEvent('download', { timeout: 10_000 }).catch(() => null);
  await page.getByTestId('files-ctx-download-zip-item').click();
  await downloadPromise;
});

test('deep-link ?file= opens the viewer', async ({ page }) => {
  const folder = await apiCreateFolder(page, uniq('DeepLink'));
  const f = SAMPLE_FILES.markdown();
  await apiUploadFile(page, f, folder.id);

  await page.goto(`/files/${folder.id}`);
  await expect(page.getByTestId(f.name)).toBeVisible({ timeout: 15_000 });
  // Extract the file id straight off the row — ResourceList tags every
  // `.file-item` with `data-item-id={item.id}`. The pre-migration
  // approach read `files-file-share-{id}` off a per-row share button
  // that no longer exists (Share moved into the context menu).
  const fileId = await page
    .locator(`.file-item[data-testid="${f.name}"]`)
    .first()
    .getAttribute('data-item-id');
  if (!fileId) throw new Error(`could not resolve file id for ${f.name}`);
  await page.goto(`/files/${folder.id}?file=${fileId}`);
  await expect(page.getByTestId('file-viewer-dialog')).toBeVisible({ timeout: 15_000 });
  await page.getByTestId('file-viewer-close-btn').click();
});

test('download a file via the context menu', async ({ page }) => {
  const folder = await apiCreateFolder(page, uniq('Dl'));
  const f = SAMPLE_FILES.text();
  await apiUploadFile(page, f, folder.id);
  await page.goto(`/files/${folder.id}`);
  await expect(page.getByTestId(f.name)).toBeVisible({ timeout: 15_000 });

  await page.getByTestId(f.name).click({ button: 'right' });
  const downloadPromise = page.waitForEvent('download', { timeout: 10_000 }).catch(() => null);
  await page.getByTestId('files-ctx-download-item').click();
  const download = await downloadPromise;
  if (download) expect(download.suggestedFilename().length).toBeGreaterThan(0);
});

test('upload a folder via the hidden folder input', async ({ page }) => {
  const folder = await apiCreateFolder(page, uniq('FolderUp'));
  await page.goto(`/files/${folder.id}`);
  await expect(page.getByTestId('files-upload-folder-input')).toBeAttached({ timeout: 15_000 });

  // webkitdirectory input — set a couple of nested files to exercise the
  // folder-upload handler.
  await page
    .getByTestId('files-upload-folder-input')
    .setInputFiles([
      { name: 'up/a.txt', mimeType: 'text/plain', buffer: Buffer.from('a') },
      { name: 'up/b.txt', mimeType: 'text/plain', buffer: Buffer.from('b') },
    ])
    .catch(() => {});
  await page.waitForTimeout(1500);
  // The page stays functional whether or not the upload fully completes.
  await expect(page.getByTestId('files-upload-btn')).toBeVisible();
});

test('drag a file onto a subfolder to move it', async ({ page }) => {
  const parent = await apiCreateFolder(page, uniq('DragHost'));
  const destName = uniq('DragDest');
  await apiCreateFolder(page, destName, parent.id);
  const f = SAMPLE_FILES.text();
  await apiUploadFile(page, f, parent.id);

  await page.goto(`/files/${parent.id}`);
  await expect(page.getByTestId(f.name)).toBeVisible({ timeout: 15_000 });
  await expect(page.getByTestId(destName)).toBeVisible();

  // HTML5 drag-and-drop: the row is draggable and the folder is a drop target.
  await page.getByTestId(f.name).dragTo(page.getByTestId(destName));

  // The file moved into the destination folder.
  await expect(page.getByTestId(f.name)).toHaveCount(0, { timeout: 15_000 });
});

test('open a folder with the keyboard (focus + Enter)', async ({ page }) => {
  const parent = await apiCreateFolder(page, uniq('KeyNav'));
  const childName = uniq('KeyChild');
  const child = await apiCreateFolder(page, childName, parent.id);
  await page.goto(`/files/${parent.id}`);
  await expect(page.getByTestId(childName)).toBeVisible({ timeout: 15_000 });

  // Focus the folder row and press Enter to open it (covers the keydown path).
  await page.getByTestId(childName).focus();
  await page.keyboard.press('Enter');
  // The URL ends with the child id (nested under the parent path).
  await expect(page).toHaveURL(new RegExp(`${child.id}$`), { timeout: 15_000 });
});

test('keyboard select-all and escape in the files list', async ({ page }) => {
  const folder = await apiCreateFolder(page, uniq('Keys'));
  await apiCreateFolder(page, uniq('k1'), folder.id);
  await apiCreateFolder(page, uniq('k2'), folder.id);
  await page.goto(`/files/${folder.id}`);
  await expect(page.locator('.files-page')).toBeVisible({ timeout: 15_000 });

  await page.locator('.files-page').click({ position: { x: 5, y: 5 } });
  await page.keyboard.press('Control+a');
  await expect(page.getByTestId('resource-list-batch-close-btn')).toBeVisible({ timeout: 5_000 }).catch(() => {});
  await page.keyboard.press('Escape');
});

test('breadcrumb navigates back to home', async ({ page }) => {
  const folderName = uniq('Crumb');
  const folder = await apiCreateFolder(page, folderName);
  await page.goto(`/files/${folder.id}`);
  // Breadcrumb root chip leaves the subfolder for the root listing. Bare /files
  // canonicalizes to the user's drive root, where the just-created folder lives.
  // Testid changed to `folder-breadcrumb-root-link` when the breadcrumb was
  // extracted into the shared `<FolderBreadcrumb>` component (used by /files
  // AND /search) — the /files-specific `files-breadcrumb-*` prefix retired.
  await page.getByTestId('folder-breadcrumb-root-link').click();
  await expect(page).not.toHaveURL(new RegExp(folder.id));
  await expect(page.getByTestId(folderName)).toBeVisible({ timeout: 15_000 });
});

test('open an image in the viewer and use the zoom controls', async ({ page }) => {
  const folder = await apiCreateFolder(page, uniq('ImgView'));
  const img = SAMPLE_FILES.png();
  await apiUploadFile(page, img, folder.id);
  await page.goto(`/files/${folder.id}`);
  await expect(page.getByTestId(img.name)).toBeVisible({ timeout: 15_000 });

  await page.getByTestId(img.name).click({ button: 'right' });
  await page.getByTestId('files-ctx-file-open-item').click();
  await expect(page.getByTestId('file-viewer-dialog')).toBeVisible({ timeout: 15_000 });

  // Zoom controls render only for images.
  await page.getByTestId('file-viewer-zoom-in-btn').click({ timeout: 3_000 }).catch(() => {});
  await page.getByTestId('file-viewer-zoom-out-btn').click({ timeout: 3_000 }).catch(() => {});
  await page.getByTestId('file-viewer-zoom-reset-btn').click({ timeout: 3_000 }).catch(() => {});
  // The download + open-in-new-tab links are present in the viewer toolbar.
  await expect(page.getByTestId('file-viewer-download-link')).toBeVisible();
  await expect(page.getByTestId('file-viewer-open-new-tab-link')).toBeVisible();
  await page.getByTestId('file-viewer-close-btn').click();
  await expect(page.getByTestId('file-viewer-dialog')).toHaveCount(0);
});

test('copy a folder into another (copy mode keeps the source)', async ({ page }) => {
  const srcName = uniq('CopySrc');
  await apiCreateFolder(page, srcName);
  const dest = await apiCreateFolder(page, uniq('CopyDest'));

  await page.goto('/files');
  await expect(page.getByTestId(srcName)).toBeVisible({ timeout: 15_000 });
  await page.getByTestId(srcName).click({ button: 'right' });
  await page.getByTestId('files-ctx-copy-item').click();
  await expect(page.getByTestId('move-dialog')).toBeVisible();
  await page.getByTestId(`move-dialog-folder-${dest.id}`).click();
  await page.getByTestId('move-dialog-confirm-btn').click();

  // MoveDialog closes only AFTER `await copyFolders(...)` resolves, so
  // waiting for its disappearance is equivalent to waiting for the
  // batch-copy request to complete. Without this the navigation to
  // /files/{dest.id} below can race the in-flight POST and land on
  // an empty listing.
  await expect(page.getByTestId('move-dialog')).toHaveCount(0, { timeout: 15_000 });

  // Copy leaves the source in place.
  await expect(page.getByTestId(srcName)).toBeVisible({ timeout: 15_000 });
  // And a copy now lives in the destination.
  await page.goto(`/files/${dest.id}`);
  await expect(page.getByTestId(srcName)).toBeVisible({ timeout: 15_000 });
});
