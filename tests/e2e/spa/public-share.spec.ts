import { test, expect } from './coverage-helpers';
import {
  apiCreateFolder,
  apiCreateShare,
  apiLogin,
  apiUploadFile,
  SAMPLE_FILES,
} from './helpers';

/**
 * Public share page — the browser-level guard for issue #721.
 *
 * #721 was: a public share grid loaded full-resolution originals where the
 * authenticated grid asks for thumbnails. The cause was duplication — the
 * share page was a second implementation of `ResourceList` and had drifted
 * from it. The rewrite deleted that copy, so the behaviour is now correct by
 * construction rather than by matching two code paths.
 *
 * Which is exactly why this test exists at the network layer and not the DOM.
 * "The page renders" would pass whatever the grid fetched. What must stay true
 * is a property of the REQUESTS a visitor's browser makes, and it can regress
 * from far away: a component default flipping, a thumbnail fallback firing, a
 * future contributor reaching for `fileDownloadUrl` in a tile. The API tests
 * pin that `/thumbnail/` works anonymously; only a browser pins that it is
 * what the page actually uses.
 */
test.describe('public share', () => {
  test('the grid fetches thumbnails, never full originals', async ({
    page,
    browser,
    baseURL,
  }) => {
    // ── Owner side: a folder with an image, and a link to it ──────────
    await apiLogin(page);
    const folder = await apiCreateFolder(page, `share-e2e-${Date.now()}`);
    await apiUploadFile(page, SAMPLE_FILES.png(), folder.id);
    const share = await apiCreateShare(page, folder.id, 'folder');

    // ── Visitor side ──────────────────────────────────────────────────
    // A FRESH context, so no session cookie rides along. Reusing `page`
    // would authenticate as the owner and the test would prove nothing
    // about visitors — the same trap that made the first version of the
    // API test pass for the wrong reason.
    const visitor = await browser.newContext({ baseURL });
    const visitorPage = await visitor.newPage();

    // The shared `page` fixture installs this guard, but only on `page` —
    // the visitor page is the one under test, so it needs its own.
    const jsErrors: Error[] = [];
    visitorPage.on('pageerror', (err) => jsErrors.push(err));

    const fileRequests: string[] = [];
    visitorPage.on('request', (req) => {
      const path = new URL(req.url()).pathname;
      if (path.startsWith('/api/files/')) fileRequests.push(path);
    });

    try {
      await visitorPage.goto(`/s/${share.token}`);
      await expect(visitorPage.getByText(SAMPLE_FILES.png().name)).toBeVisible();

      // Settle the network before reading the request list. The filename
      // appears as soon as the tile is in the DOM, but the thumbnail `<img>`
      // carries `loading="lazy"`, so its fetch is issued a tick later —
      // sampling at the visibility assertion saw zero requests and failed.
      await visitorPage.waitForLoadState('networkidle');

      expect(jsErrors.map((e) => e.message)).toEqual([]);

      const thumbnails = fileRequests.filter((p) => p.includes('/thumbnail/'));
      const originals = fileRequests.filter((p) => !p.includes('/thumbnail/'));

      expect(
        thumbnails.length,
        `expected at least one /thumbnail/ request, saw: ${JSON.stringify(fileRequests)}`,
      ).toBeGreaterThan(0);

      // The #721 assertion. A bare `/api/files/{id}` here is a
      // full-resolution fetch to paint a tile — which is the bug, whether
      // it comes back as a grid regression or as the client-side thumbnail
      // fallback rasterising an original it just downloaded.
      expect(
        originals,
        'a share grid must not fetch full-resolution originals (issue #721)',
      ).toEqual([]);
    } finally {
      await visitor.close();
    }
  });
});
