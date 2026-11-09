import type { Locator } from '@playwright/test';
import { test, expect } from './coverage-helpers';
import { apiLogin } from './helpers';

/**
 * Regression: on a short window the last nav links were unreachable.
 *
 * `body` is `height: 100dvh; overflow: hidden` (reset.css) and `.sidebar` is
 * 100% of it, so whatever did not fit was simply clipped — the page could not
 * scroll and neither could the sidebar. With every feature flag on (and the
 * drive picker adding rows under Files) that is an ordinary laptop window in
 * landscape, not an edge case.
 *
 * The scroll sits on `.sidebar`, with two things pinned to its edges: the
 * header always (the logo is the way home from anywhere), and the storage
 * read-out only above 512px of height — worth a strip when there is room,
 * worth nothing when every pixel belongs to the links.
 *
 * Playwright because this needs real layout: jsdom has none, so a component
 * test cannot tell a scrollable sidebar from a clipped one, nor a pinned
 * element from one that scrolled past.
 */

/** Scrolls the sidebar to its end and reports how far it moved. */
async function scrollSidebarToEnd(sidebar: Locator): Promise<number> {
  await sidebar.evaluate((el) => {
    el.scrollTop = el.scrollHeight;
  });
  return sidebar.evaluate((el) => el.scrollTop);
}

test('short viewport: the sidebar scrolls to its last items', async ({ page }) => {
  // Short enough that the sidebar cannot fit, wide enough to stay in the
  // desktop layout (the mobile breakpoint turns it into a drawer). Also below
  // the 513px storage threshold, so nothing is pinned at the bottom here.
  await page.setViewportSize({ width: 1280, height: 420 });

  await apiLogin(page);
  await page.goto('/files');
  await expect(page.getByTestId('appshell-logo-link')).toBeVisible({ timeout: 15_000 });

  const sidebar = page.locator('aside.sidebar');

  // The sidebar really is overflowing — otherwise this viewport proves
  // nothing and the assertions below would pass on a sidebar that just fits.
  const overflowing = await sidebar.evaluate((el) => el.scrollHeight > el.clientHeight + 1);
  expect(
    overflowing,
    'viewport is too tall to exercise the regression — lower it until the sidebar overflows'
  ).toBe(true);

  // And it scrolls, which is the fix. Before it, `scrollTop` stayed 0 because
  // the overflow was clipped rather than scrollable.
  expect(await scrollSidebarToEnd(sidebar)).toBeGreaterThan(0);

  // The logo stays put while everything else moves.
  await expect(page.getByTestId('appshell-logo-link')).toBeInViewport();

  // The last link is reachable: Playwright scrolls it into view before
  // clicking, which is exactly what a user cannot do when nothing scrolls.
  const lastLink = sidebar.locator('a.nav-item').last();
  const href = await lastLink.getAttribute('href');
  await lastLink.click();
  await expect(page).toHaveURL(new RegExp(`${href}$`));
});

test('above 512px tall: the storage read-out stays pinned to the bottom', async ({ page }) => {
  // Over the threshold, but still short enough that the sidebar overflows.
  await page.setViewportSize({ width: 1280, height: 560 });

  await apiLogin(page);
  await page.goto('/files');
  await expect(page.getByTestId('appshell-logo-link')).toBeVisible({ timeout: 15_000 });

  const sidebar = page.locator('aside.sidebar');
  const overflowing = await sidebar.evaluate((el) => el.scrollHeight > el.clientHeight + 1);
  expect(
    overflowing,
    'viewport is too tall to exercise the pin — lower it until the sidebar overflows'
  ).toBe(true);

  // Scrolled to the TOP, the read-out's natural place is below the fold; the
  // sticky rule pulls it up to the bottom edge. Without it this is where the
  // element would be invisible.
  await sidebar.evaluate((el) => {
    el.scrollTop = 0;
  });
  const storage = page.locator('.storage-container');
  await expect(storage).toBeInViewport();

  const [storageBox, sidebarBox] = await Promise.all([
    storage.boundingBox(),
    sidebar.boundingBox()
  ]);
  expect(storageBox).not.toBeNull();
  expect(sidebarBox).not.toBeNull();
  // Sitting ON the bottom edge, not merely somewhere on screen. A couple of
  // pixels of tolerance for sub-pixel layout rounding; the failure this
  // guards against is off by hundreds.
  const gap = Math.abs(
    storageBox!.y + storageBox!.height - (sidebarBox!.y + sidebarBox!.height)
  );
  expect(gap, 'storage read-out must sit on the sidebar bottom edge').toBeLessThanOrEqual(2);

  // Still there at the other end of the scroll, where it settles into its own
  // place in the column rather than overlapping the last link.
  expect(await scrollSidebarToEnd(sidebar)).toBeGreaterThan(0);
  await expect(storage).toBeInViewport();
  await expect(page.getByTestId('appshell-logo-link')).toBeInViewport();
});
