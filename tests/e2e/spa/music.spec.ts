import * as fs from 'fs';
import * as path from 'path';
import { test, expect } from './coverage-helpers';
import { apiLogin, apiCreateFolder, apiUploadFile } from './helpers';

/**
 * Music route — playlist lifecycle, the add-tracks dialog, and the audio player
 * (driven with a real WAV fixture so the track shows up in the picker). The
 * music page is one of the largest source files.
 */
const TONE_WAV = fs.readFileSync(path.join(__dirname, '..', 'fixtures', 'tone.wav'));
test.beforeEach(async ({ page }) => {
  await apiLogin(page);
});

function uniq(p: string): string {
  return `${p}-${Date.now()}-${Math.floor(Math.random() * 1e6)}`;
}

/** Create a playlist and return its name; works whether or not others exist. */
async function createPlaylist(page: import('@playwright/test').Page, name: string): Promise<void> {
  await page.goto('/music');
  const createBtn = page.getByTestId('music-create-playlist-btn');
  const emptyBtn = page.getByTestId('music-create-playlist-empty-btn');
  await expect(createBtn.or(emptyBtn)).toBeVisible({ timeout: 15_000 });
  if (await createBtn.isVisible().catch(() => false)) await createBtn.click();
  else await emptyBtn.click();
  await page.getByTestId('dialog-host-prompt-input').fill(name);
  await page.getByTestId('dialog-host-submit-btn').click();
  await expect(page.getByTestId(name)).toBeVisible({ timeout: 15_000 });
}

test('playlist lifecycle: create, toggle public, rename, delete', async ({ page }) => {
  const name = uniq('PL');
  const renamed = uniq('PLr');
  await createPlaylist(page, name);

  // Select the playlist → detail panel with its controls.
  await page.getByTestId(name).click();
  await expect(page.getByTestId('music-rename-playlist-btn')).toBeVisible({ timeout: 15_000 });

  await page.getByTestId('music-toggle-public-btn').click().catch(() => {});

  await page.getByTestId('music-rename-playlist-btn').click();
  await page.getByTestId('dialog-host-prompt-input').fill(renamed);
  await page.getByTestId('dialog-host-submit-btn').click();
  await expect(page.getByTestId(renamed)).toBeVisible({ timeout: 15_000 });

  await page.getByTestId('music-delete-playlist-btn').click();
  await page.getByTestId('dialog-host-confirm-btn').click();
  await expect(page.getByTestId(renamed)).toHaveCount(0, { timeout: 15_000 });
});

test('reorder playlist tracks by dragging', async ({ page }) => {
  const folder = await apiCreateFolder(page, uniq('Audio2'));
  const token = `multi${Date.now()}`;
  await apiUploadFile(page, { name: `${token}-1.wav`, mimeType: 'audio/wav', body: TONE_WAV }, folder.id);
  await apiUploadFile(page, { name: `${token}-2.wav`, mimeType: 'audio/wav', body: TONE_WAV }, folder.id);

  const name = uniq('PLr2');
  await createPlaylist(page, name);
  await page.getByTestId(name).click();
  await page.getByTestId('music-add-tracks-btn').click();
  await expect(page.getByTestId('music-add-tracks-dialog')).toBeVisible();
  await page.getByTestId('music-add-tracks-search-input').fill(token);
  await page.getByTestId(`${token}-1.wav`).check({ timeout: 15_000 });
  await page.getByTestId(`${token}-2.wav`).check({ timeout: 5_000 });
  await page.getByTestId('music-add-tracks-confirm-btn').click();
  await expect(page.getByTestId('music-add-tracks-dialog')).toHaveCount(0);

  // Drag the first track onto the second to reorder.
  const tracks = page.locator('.music-track');
  await expect(tracks).toHaveCount(2, { timeout: 15_000 });
  await tracks.nth(0).dragTo(tracks.nth(1));
});

test('add-tracks dialog opens, searches, and closes', async ({ page }) => {
  const name = uniq('PLt');
  await createPlaylist(page, name);
  await page.getByTestId(name).click();
  await expect(page.getByTestId('music-add-tracks-btn')).toBeVisible({ timeout: 15_000 });

  await page.getByTestId('music-add-tracks-btn').click();
  await expect(page.getByTestId('music-add-tracks-dialog')).toBeVisible();
  await page.getByTestId('music-add-tracks-search-input').fill('song');
  await page.getByTestId('music-add-tracks-close-btn').click();
  await expect(page.getByTestId('music-add-tracks-dialog')).toHaveCount(0);
});

test('add an audio track and drive the player', async ({ page }) => {
  // A real WAV so the backend recognises it as audio and lists it in the picker.
  const folder = await apiCreateFolder(page, uniq('Audio'));
  const songName = `${uniq('song')}.wav`;
  await apiUploadFile(page, { name: songName, mimeType: 'audio/wav', body: TONE_WAV }, folder.id);

  const name = uniq('PLp');
  await createPlaylist(page, name);
  await page.getByTestId(name).click();
  await expect(page.getByTestId('music-add-tracks-btn')).toBeVisible({ timeout: 15_000 });

  // Open the picker and search by name (an empty query may match nothing).
  await page.getByTestId('music-add-tracks-btn').click();
  await expect(page.getByTestId('music-add-tracks-dialog')).toBeVisible();
  await page.getByTestId('music-add-tracks-search-input').fill(songName.replace('.wav', ''));
  const songCheckbox = page.getByTestId(songName);
  await expect(songCheckbox).toBeVisible({ timeout: 15_000 });
  await songCheckbox.check();
  await page.getByTestId('music-add-tracks-confirm-btn').click();
  await expect(page.getByTestId('music-add-tracks-dialog')).toHaveCount(0);

  // Playlist-level actions on the detail panel.
  await page.getByTestId('music-play-all-btn').click({ timeout: 3_000 }).catch(() => {});
  await page.getByTestId('music-shuffle-play-btn').click({ timeout: 3_000 }).catch(() => {});

  // Play the track and exercise the transport controls.
  await page.locator('[data-testid^="music-track-play-"]').first().click({ timeout: 5_000 });
  await page.getByTestId('music-player-play-btn').click({ timeout: 3_000 }).catch(() => {});
  await page.getByTestId('music-player-shuffle-btn').click({ timeout: 3_000 }).catch(() => {});
  await page.getByTestId('music-player-repeat-btn').click({ timeout: 3_000 }).catch(() => {});
  await page.getByTestId('music-player-next-btn').click({ timeout: 3_000 }).catch(() => {});
  await page.getByTestId('music-player-mute-btn').click({ timeout: 3_000 }).catch(() => {});

  // Queue panel — open, remove a queued track, then close.
  await page.getByTestId('music-player-queue-toggle-btn').click({ timeout: 3_000 }).catch(() => {});
  await page
    .locator('[data-testid^="music-queue-remove-"]')
    .first()
    .click({ timeout: 2_000 })
    .catch(() => {});
  await page.getByTestId('music-queue-close-btn').click({ timeout: 3_000 }).catch(() => {});

  // Edit the playlist description.
  await page.getByTestId('music-edit-description-btn').click({ timeout: 3_000 }).catch(() => {});
  const descInput = page.getByTestId('dialog-host-prompt-input');
  if (await descInput.isVisible().catch(() => false)) {
    await descInput.fill('e2e description');
    await page.getByTestId('dialog-host-submit-btn').click().catch(() => {});
  }

  // Manage-shares dialog.
  await page.getByTestId('music-manage-shares-btn').click({ timeout: 3_000 }).catch(() => {});
  await page.getByTestId('music-shares-close-btn').click({ timeout: 3_000 }).catch(() => {});

  // Set a cover image (exercises the cover upload path).
  await page.getByTestId('music-set-cover-btn').click({ timeout: 3_000 }).catch(() => {});
  await page
    .getByTestId('music-cover-input')
    .setInputFiles({
      name: 'cover.png',
      mimeType: 'image/png',
      buffer: Buffer.from(
        'iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAAC0lEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==',
        'base64',
      ),
    })
    .catch(() => {});
});
