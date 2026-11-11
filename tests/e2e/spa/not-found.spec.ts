import { test, expect } from './coverage-helpers';
import { apiLogin } from './helpers';

/**
 * The 404 page (`frontend/src/routes/+error.svelte`) and the wire contract
 * underneath it.
 *
 * Why Playwright and not Hurl, since most of this is response headers: the
 * api-test runner has no built frontend. `static-dist/` is gitignored, CI's
 * `build` job uploads only the binary, and that binary is built
 * `--features plugins` rather than `bundled-assets` — so there is no SPA
 * shell there and every assertion below would be green locally and red in
 * CI. This job builds the SPA first, which makes it the only harness where
 * the static layer is populated. The one case that survives without a
 * frontend is a miss under `/_app/immutable/`, and that is pinned at the
 * wire in `tests/api/web_static_cache_headers.hurl`.
 */

test('an unmatched route keeps the app shell and answers 200 + no-store', async ({ page }) => {
  await apiLogin(page);

  const res = await page.goto('/this-route-does-not-exist');

  // The server answers 200 with the SPA shell and the CLIENT router raises
  // the 404 — `web_routes_filesystem` uses `ServeDir::fallback`, not
  // `not_found_service`, deliberately (see the doc comment on
  // `spa_service`: "a client route like /files/<id> is a page, not a
  // miss"). Pinning 200 here is pinning that choice: switching to
  // `not_found_service` to make the status "honest" would break every deep
  // link in the app, and this is the test that would say so.
  expect(res?.status()).toBe(200);

  // `no-store`, not merely `no-cache`. `security_headers_layer` upgrades it
  // for any `text/html` response because `no-store` is the explicit
  // back-forward-cache opt-out in Chrome/Firefox/Safari and `no-cache`
  // alone is not — without it a shell loaded before a deploy can be
  // resurrected byte-for-byte on a back-navigation, stale CSP hash
  // included, leaving the app stuck behind the boot spinner.
  expect(res?.headers()['cache-control']).toContain('no-store');

  // Nothing in a private file store wants indexing. Applied to the whole
  // web router, so the shell carries it too — which is the case that
  // matters, since `/s/<token>` is also an unmatched client route served
  // this same shell.
  expect(res?.headers()['x-robots-tag']).toBe('noindex, nofollow');

  await expect(page.getByTestId('errpage')).toBeVisible();

  // Signed in, the 404 renders INSIDE `AppShell` — the root layout's
  // `{:else if ready && session.isAuthenticated}` branch. That is the point
  // of keeping the chrome: the sidebar is the recovery path, so a mistyped
  // URL costs one click rather than a dead end.
  await expect(page.locator('.sidebar')).toBeVisible();

  await page.getByTestId('errpage-home').click();
  // Asserting we LEFT the error page rather than naming a destination —
  // `/` is free to redirect wherever the app's landing route goes next.
  await expect(page.getByTestId('errpage')).toHaveCount(0);
});

test('a 404 under a public prefix renders full-width with no shell', async ({ page }) => {
  // No `apiLogin`: there is no global `storageState`, so a fresh context is
  // signed out. `/login/...` is in the root layout's `PUBLIC_PREFIXES`, so
  // the guard effect does NOT bounce to `/login` and the error page renders
  // bare — which is the only way to reach the 404 while signed out. (A
  // non-public bad path redirects to the login form instead, so it never
  // shows a 404 at all.)
  const res = await page.goto('/login/does-not-exist');
  expect(res?.status()).toBe(200);

  await expect(page.getByTestId('errpage')).toBeVisible();

  // No chrome for an anonymous visitor: a sidebar full of links that each
  // bounce back to the login form is worse than no sidebar.
  await expect(page.locator('.sidebar')).toHaveCount(0);

  // Regression pin for the centring bug, and a width check is the honest
  // proxy for it. `body` is `display: flex` (row), so outside `AppShell`
  // the page div is a flex ITEM and sizes to its content — it shrink-wrapped
  // and hugged the left edge however well it centred its own children,
  // exactly as the old boot loader did (see the note at the foot of
  // `routes/+layout.svelte`). `width: 100%` in `.errpage` is the fix, and
  // dropping it would halve this box.
  const box = await page.getByTestId('errpage').boundingBox();
  const viewport = page.viewportSize();
  expect(box).not.toBeNull();
  expect(viewport).not.toBeNull();
  expect(box!.width).toBeGreaterThan(viewport!.width * 0.9);
});
