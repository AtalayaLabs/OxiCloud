import { test, expect } from './coverage-helpers';
import {
  apiCreateFolder,
  apiLogin,
  apiSetHideDotfiles,
  apiTrashFolder,
} from './helpers';

/**
 * Dotfile-hide filter — end-to-end coverage of the UI-only, per-user
 * `hide_dotfiles` preference (JSONB `auth.users.ui_preferences`).
 *
 * Deliberately narrow scope:
 *
 * 1. Toggle: a `.hidden` folder in `/files` disappears when the
 *    toolbar eye button is pressed and reappears when it's pressed
 *    again. This is the "does the filter actually filter" test.
 *
 * 2. Empty state: a folder that contains ONLY dotfiles renders the
 *    "N hidden items — Show hidden files" affordance rather than the
 *    generic "This folder is empty" copy. Clicking the affordance
 *    flips the preference back off and the rows reappear. Guards
 *    against a mystery-empty-folder regression.
 *
 * 3. Trash safety: the hide preference is deliberately IGNORED on
 *    `/trash`, so a dotfile-named item still shows up for recovery.
 *    Pins the "safety-net surface always shows everything" rule
 *    against a future refactor that might extend the filter to
 *    trash by accident.
 *
 * Other surfaces (favorites, recent, photos, public share) all
 * derive from the same `filterDotfiles` helper and the same
 * `preferences.hideDotfiles` reactive read; unit tests cover the
 * predicate, so we don't burn browser cycles verifying each list
 * page renders one more filtered row correctly. The three tests
 * above hit the three DIFFERENT semantics (filter, empty-state,
 * exemption), which is what actually needs regression coverage.
 *
 * Isolation. `hide_dotfiles` is per-user and persists on the server,
 * so it survives the login-per-test that other specs rely on for
 * isolation. `beforeEach` explicitly resets it to `false` and
 * `afterEach` restores it, otherwise a failed test would leave the
 * whole suite running with the filter on.
 */

// Test-created folders. Tests push here in-flight; afterEach reaps.
// Keeps /files root clean so unrelated specs' virtualised listings
// don't lose their own fixtures to overflow.
const scratchFolderIds: string[] = [];

test.beforeEach(async ({ page }) => {
  await apiLogin(page);
  await apiSetHideDotfiles(page, false);
});

test.afterEach(async ({ page }) => {
  // Belt-and-braces: even if a test forgot to reset, restore the
  // default so the next spec file starts from a known state.
  await apiSetHideDotfiles(page, false).catch(() => {});
  // Reap this test's fixtures. `catch` per id so a stale reference
  // (already trashed by the test body, e.g. Test 3) doesn't cascade
  // a teardown error onto a real assertion failure.
  while (scratchFolderIds.length) {
    const id = scratchFolderIds.pop()!;
    await apiTrashFolder(page, id).catch(() => {});
  }
});

function uniq(prefix: string): string {
  return `${prefix}-${Date.now()}-${Math.floor(Math.random() * 1e6)}`;
}

test('toolbar eye toggle hides and re-shows dotfiles in /files', async ({ page }) => {
  // Scratch parent so we don't dump siblings into /files root — the
  // root's virtualised list is shared with the rest of the suite and
  // its DOM size caps out around a few dozen rows; every persistent
  // fixture we leave there risks pushing an unrelated test's own
  // folder out of view (see `files-extra.spec.ts` regressions).
  // Trashing the parent in afterEach cascades to the children.
  const parent = await apiCreateFolder(page, uniq('DotfileToggleScratch'));
  scratchFolderIds.push(parent.id);
  const visible = uniq('Visible');
  const hidden = `.${uniq('hidden')}`;
  await apiCreateFolder(page, visible, parent.id);
  await apiCreateFolder(page, hidden, parent.id);

  await page.goto(`/files/${parent.id}`);

  // Baseline: both rows render. Row test-id = folder name (see
  // ResourceList / +page.svelte's data-testid pattern used by the
  // sibling files.spec.ts).
  await expect(page.getByTestId(visible)).toBeVisible({ timeout: 15_000 });
  await expect(page.getByTestId(hidden)).toBeVisible();

  // Flip the filter on via the eye toggle in the ListToolbar. The
  // click routes through `preferences.toggleHideDotfiles()` which
  // does an optimistic local mutation, so the row update should be
  // visible before the debounced PATCH lands.
  await page.getByTestId('display-mode-dotfile-toggle-btn').click();

  // Visible row stays; hidden row vanishes.
  await expect(page.getByTestId(visible)).toBeVisible();
  await expect(page.getByTestId(hidden)).toHaveCount(0);

  // Flip it back off — the hidden row must reappear. Same button;
  // its state flips atomically with `preferences.hideDotfiles`.
  await page.getByTestId('display-mode-dotfile-toggle-btn').click();
  await expect(page.getByTestId(hidden)).toBeVisible();
});

test('empty-state hint appears when a folder holds only dotfiles', async ({ page }) => {
  // Isolate the folder: nest inside a fresh parent so the only
  // children are our dotfiles. Root has accumulated cruft from the
  // suite and would drown the empty-state case.
  const parent = await apiCreateFolder(page, uniq('OnlyDotfilesParent'));
  scratchFolderIds.push(parent.id);
  const dot1 = `.${uniq('a')}`;
  const dot2 = `.${uniq('b')}`;
  await apiCreateFolder(page, dot1, parent.id);
  await apiCreateFolder(page, dot2, parent.id);

  // Navigate into the parent. `/files/[...path]` treats the path
  // segments as folder ids in the deep-link form.
  await page.goto(`/files/${parent.id}`);

  // Baseline: both dotfiles are visible with hide off.
  await expect(page.getByTestId(dot1)).toBeVisible({ timeout: 15_000 });
  await expect(page.getByTestId(dot2)).toBeVisible();

  // Turn hide on. Folder becomes visually empty — but not the
  // generic empty state; the "N hidden items" affordance appears
  // instead, offering a one-click "Show hidden files" escape.
  await page.getByTestId('display-mode-dotfile-toggle-btn').click();

  const showHiddenBtn = page.getByTestId('files-show-hidden-btn');
  await expect(showHiddenBtn).toBeVisible({ timeout: 15_000 });
  // Regression pin: the generic "This folder is empty" hint MUST NOT
  // show — that would hide the fact that content exists.
  await expect(page.getByText('This folder is empty')).toHaveCount(0);

  // Click the "Show hidden files" button. It calls
  // `preferences.setHideDotfiles(false)` and both dotfiles must
  // reappear in the same view without a reload.
  await showHiddenBtn.click();
  await expect(page.getByTestId(dot1)).toBeVisible();
  await expect(page.getByTestId(dot2)).toBeVisible();
});

test('trash always shows dotfiles even when hide is on', async ({ page }) => {
  // Create a `.`-prefixed folder, trash it, then flip the hide
  // preference on. Trash MUST still show the row: hiding a
  // trashed dotfile would let it ride the retention timer to
  // permanent deletion without being reviewable — a
  // safety-net-defeating footgun.
  const dotname = `.${uniq('TrashedHidden')}`;
  const folder = await apiCreateFolder(page, dotname);
  await apiTrashFolder(page, folder.id);

  // Turn hide on server-side so the client picks it up on next
  // session load (rather than driving it through the UI toggle
  // and then navigating — same end state, one fewer moving part).
  await apiSetHideDotfiles(page, true);

  await page.goto('/trash');

  // Row must be present. Trash entries render the resource name
  // as plain text (no per-row test-id keyed by name in the current
  // template); text lookup is the reliable selector.
  //
  // `exact: true` narrows to the name cell — the path cell (which
  // renders as "Personal/{name}") would otherwise also match under
  // Playwright's default substring semantics and trip strict mode.
  await expect(page.getByText(dotname, { exact: true })).toBeVisible({ timeout: 15_000 });

  // Belt-and-braces: also verify the hide preference IS on in the
  // background — otherwise the assertion above passes trivially
  // because nothing was being hidden in the first place. We check
  // by visiting /files (where the filter IS supposed to apply) and
  // asserting the OTHER dotfile from the earlier test class would
  // be hidden. Actually — because tests are ordered arbitrarily,
  // we just verify the toolbar toggle reflects the current server
  // state via aria-pressed on /files.
  await page.goto('/files');
  const toggle = page.getByTestId('display-mode-dotfile-toggle-btn');
  await expect(toggle).toHaveAttribute('aria-pressed', 'true');
});
