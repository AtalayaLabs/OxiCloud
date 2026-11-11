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

/**
 * Reap the playlists this file created, pass OR fail.
 *
 * The happy path deletes its own playlist at the end, which is no cleanup at
 * all for the case that matters: a test that fails mid-way leaves its row
 * behind, and the next run then sees two. That is how one real failure turned
 * into fifteen under `--repeat-each=30` — each cascade failure was an
 * unrelated strict-mode violation on a locator that assumed a single
 * playlist, and the first genuine failure was buried under them.
 *
 * Matches only the `uniq()` shape below, so a playlist a developer made by
 * hand in the same sandbox survives. The character class has to allow digits:
 * the prefixes in use are `PL`, `PLr`, `PLr2`, `PLt` and `PLp`, and `[a-z]*`
 * silently skipped `PLr2`.
 */
test.afterEach(async ({ page }) => {
  await page
    .evaluate(async () => {
      const csrf = document.cookie.match(/(?:^|; )oxicloud_csrf=([^;]+)/)?.[1] ?? '';
      const r = await fetch('/api/playlists', { credentials: 'same-origin' });
      if (!r.ok) return;
      const rows = (await r.json()) as Array<{ id: string; name: string }>;
      await Promise.all(
        rows
          .filter((p) => /^PL[a-z0-9]*-\d{13}-\d+$/.test(p.name))
          .map((p) =>
            fetch(`/api/playlists/${p.id}`, {
              method: 'DELETE',
              credentials: 'same-origin',
              headers: csrf ? { 'x-csrf-token': csrf } : {},
            }),
          ),
      );
    })
    // A test that closed the page or navigated off-origin cannot be cleaned up
    // from here, and failing the cleanup would mask the real failure.
    .catch(() => {});
});

function uniq(p: string): string {
  return `${p}-${Date.now()}-${Math.floor(Math.random() * 1e6)}`;
}

/**
 * The sidebar row for a playlist, located by its visible name.
 *
 * Rows carry a fixed `music-playlist-item` testid rather than their name, so
 * a wrong name reports as a text mismatch instead of a missing element — see
 * the comment on that testid in `routes/music/+page.svelte`.
 */
function playlistRow(page: import('@playwright/test').Page, name: string) {
  return page.getByTestId('music-playlist-item').filter({ hasText: name });
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
  await expect(playlistRow(page, name)).toBeVisible({ timeout: 15_000 });
}

test('playlist lifecycle: create, toggle public, rename, delete', async ({ page }) => {
  const name = uniq('PL');
  const renamed = uniq('PLr');
  await createPlaylist(page, name);

  // Select the playlist → detail panel with its controls.
  await playlistRow(page, name).click();
  await expect(page.getByTestId('music-rename-playlist-btn')).toBeVisible({ timeout: 15_000 });

  await page.getByTestId('music-rename-playlist-btn').click();
  // Assert each field actually holds what was typed before submitting. The
  // dialog's inputs bind on mount, and filling one before that binding
  // settles has produced a submit where the description text ended up inside
  // the NAME — which then surfaced fifteen seconds later as a missing
  // playlist row, naming neither the field nor the value. Checking here
  // fails at the fill, with both strings in the message.
  const nameInput = page.getByTestId('dialog-host-input-name');
  const descInput = page.getByTestId('dialog-host-input-description');
  await nameInput.fill(renamed);
  await descInput.fill('e2e playlist description');
  await expect(nameInput).toHaveValue(renamed);
  await expect(descInput).toHaveValue('e2e playlist description');

  await page.getByTestId('dialog-host-submit-btn').click();
  await expect(playlistRow(page, renamed)).toBeVisible({ timeout: 15_000 });
  // EXACT, where the row filter above is a substring match: a name that
  // merely contains the new one — the failure mode above — has to fail here.
  //
  // Scoped to the row rather than asserted on `page`. Unscoped, this matched
  // EVERY playlist name on the page, so it only held when the account owned
  // exactly one playlist — and under `--repeat-each` it tripped strict mode
  // ("resolved to 15 elements") the moment any run left a row behind, turning
  // one failure into a cascade that failed every later repeat for an unrelated
  // reason. Scoping keeps the exactness that catches a concatenated name while
  // tolerating playlists this test did not create.
  await expect(playlistRow(page, renamed).getByTestId('music-playlist-name')).toHaveText(renamed);
  await expect(page.getByText('e2e playlist description')).toBeVisible();

  await page.getByTestId('music-delete-playlist-btn').click();
  await page.getByTestId('music-delete-playlist-btn').click();
  await expect(playlistRow(page, renamed)).toHaveCount(0, { timeout: 15_000 });
});

test('reorder playlist tracks by dragging', async ({ page }) => {
  const folder = await apiCreateFolder(page, uniq('Audio2'));
  const token = `multi${Date.now()}`;
  await apiUploadFile(page, { name: `${token}-1.wav`, mimeType: 'audio/wav', body: TONE_WAV }, folder.id);
  await apiUploadFile(page, { name: `${token}-2.wav`, mimeType: 'audio/wav', body: TONE_WAV }, folder.id);

  const name = uniq('PLr2');
  await createPlaylist(page, name);
  await playlistRow(page, name).click();
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
  await playlistRow(page, name).click();
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
  await playlistRow(page, name).click();
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

  // Playlist-level actions on the detail panel. Shuffle used to be a second
  // button here; it is a player mode now, toggled in the player panel, so the
  // playlist no longer shuffles a copy of its own list.
  await page.getByTestId('music-play-all-btn').click({ timeout: 3_000 }).catch(() => {});

  // Play the track from its row. The player is in the top bar now, so the
  // queue toggle and close button this test used to click no longer exist —
  // and because every click was wrapped in `.catch(() => {})`, their removal
  // would have gone unnoticed: the test would keep passing while exercising
  // nothing. Assertions below instead.
  await page.locator('[data-testid^="music-track-play-"]').first().click({ timeout: 5_000 });

  const trigger = page.getByTestId('music-player-toggle-btn');
  await trigger.click();
  await expect(page.getByTestId('music-player-title')).toContainText(
    songName.replace('.wav', '')
  );
  // The queue is part of the panel now rather than a drawer behind a toggle.
  await expect(page.getByTestId('music-queue-panel')).toBeVisible();

  // Transport: pause and resume, asserted on the element rather than on the
  // button, so a handler wired to the wrong action cannot pass.
  const paused = () => page.evaluate(() => document.querySelector('audio')?.paused ?? true);
  await page.getByTestId('music-player-play-btn').click();
  await expect.poll(paused).toBe(true);
  await page.getByTestId('music-player-play-btn').click();
  await expect.poll(paused).toBe(false);

  // THE regression this player exists to prevent: leaving Music used to
  // unmount the <audio> element and cut the track off. Navigate away and it
  // must still be there, still playing, still reachable.
  await page.keyboard.press('Escape');
  await page.getByTestId('appshell-nav-files-link').click();
  await expect(page).toHaveURL(/\/files/);
  await expect(trigger).toBeVisible();
  await expect.poll(paused).toBe(false);

  await page.goBack();
  await expect(page.getByTestId('music-add-tracks-btn')).toBeVisible({ timeout: 15_000 });

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
      // 1×1 transparent RGBA PNG with valid CRCs — see the regenerate
      // snippet in spa/helpers.ts under SAMPLE_FILES.png.
      buffer: Buffer.from(
        'iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAAC0lEQVR4nGNgAAIAAAUAAXpeqz8AAAAASUVORK5CYII=',
        'base64',
      ),
    })
    .catch(() => {});
});
