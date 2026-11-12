import * as path from 'path';
import { readFileSync } from 'fs';
import { Page } from '@playwright/test';
import { test, expect } from './coverage-helpers';
import { apiLogin, apiCreateFolder, apiUploadFile } from './helpers';

const PDF = readFileSync(path.join(__dirname, '..', '..', 'fixtures', 'doc-date.pdf'));

/**
 * PDF preview, both branches — regression cover for issue #780.
 *
 * The bug: the viewer fetched the PDF, wrapped it in a `blob:` URL and framed
 * it in an `<iframe>`, relying on the browser's own PDF viewer. Desktop Chrome
 * rendered it; Chrome on Android showed a broken-document icon and logged
 * `Framing '' violates ... "frame-src 'self' blob:"`. The split is
 * `navigator.pdfViewerEnabled`: measured `true` on the desktop and `false` on
 * the phone, on the same build. So the viewer branches — frame a blob where a
 * native viewer exists, render with the vendored pdf.js where it does not.
 *
 * ## Why the capability is injected rather than inherited
 *
 * Playwright drives Chromium, not Chrome, and Chromium ships no PDF viewer
 * extension — so its `pdfViewerEnabled` describes the test browser, not the
 * browser class we care about. Device emulation does not help either:
 * `devices['Pixel 7']` sets user agent, viewport and touch, and says nothing
 * about PDF capability. Injecting the flag is the only way to exercise BOTH
 * branches deterministically, and it pins what actually regressed — our branch
 * on the flag, and each path's ability to run under the app's CSP.
 *
 * What this cannot prove, and nothing in CI can: that a real Chrome on real
 * Android renders the result. That was verified by hand on the device, and by
 * the reporter of #780.
 *
 * ## The CSP assertion is the point
 *
 * Both paths sit one directive away from breaking, in opposite directions: the
 * native path needs `frame-src blob:`, and the pdf.js path needs
 * `worker-src 'self'` for the ~1 MB worker under `/vendors` plus
 * `isEvalSupported: false` so no `unsafe-eval` is required. So each test fails
 * on any CSP refusal in the console — which is exactly how #780 announced
 * itself.
 */

function uniq(p: string): string {
  return `${p}-${Date.now()}-${Math.floor(Math.random() * 1e6)}`;
}

/** Make `navigator.pdfViewerEnabled` report `enabled` before the app boots. */
async function fakePdfViewerSupport(page: Page, enabled: boolean): Promise<void> {
  await page.addInitScript((v) => {
    // Chromium defines it as a getter on the prototype, so redefine it there
    // rather than shadowing it on the instance.
    Object.defineProperty(Navigator.prototype, 'pdfViewerEnabled', {
      get: () => v,
      configurable: true,
    });
  }, enabled);
}

/** Upload a PDF into a fresh folder and open it in the file viewer. */
async function openPdfInViewer(page: Page): Promise<void> {
  const folderName = uniq('PdfPreview');
  const folder = await apiCreateFolder(page, folderName);
  const file = { name: `${uniq('doc')}.pdf`, mimeType: 'application/pdf', body: PDF };
  await apiUploadFile(page, file, folder.id);

  await page.goto('/files');
  await page.getByTestId(folderName).click();
  await expect(page.getByTestId(file.name)).toBeVisible({ timeout: 15_000 });

  await page.getByTestId(file.name).click({ button: 'right' });
  await page.getByTestId('files-ctx-file-open-item').click();
  await expect(page.getByTestId('file-viewer-dialog')).toBeVisible({ timeout: 15_000 });
}

/** Console messages naming a CSP refusal, collected as they arrive. */
function watchForCspViolations(page: Page): string[] {
  const hits: string[] = [];
  page.on('console', (msg) => {
    const text = msg.text();
    if (/Content Security Policy|violates the following/i.test(text)) hits.push(text);
  });
  return hits;
}

test('a browser WITH a native PDF viewer frames the blob (desktop Chrome)', async ({ page }) => {
  const csp = watchForCspViolations(page);
  await fakePdfViewerSupport(page, true);
  await apiLogin(page);
  await openPdfInViewer(page);

  // The native path: an iframe pointed at a same-origin blob. The blob is what
  // dodges the API response's `X-Frame-Options: DENY` + `frame-ancestors
  // 'none'`, so the scheme is load-bearing rather than incidental.
  const frame = page.locator('iframe.fv__pdf');
  await expect(frame).toHaveCount(1, { timeout: 15_000 });
  expect(await frame.getAttribute('src')).toMatch(/^blob:/);

  // pdf.js is NOT used here, which is the whole point of branching: a browser
  // that renders PDFs itself never downloads the ~1 MB worker.
  await expect(page.locator('[data-testid="pdf-preview-pages"]')).toHaveCount(0);

  expect(csp, `CSP refusals in the console: ${csp.join(' | ')}`).toEqual([]);
});

test('a browser WITHOUT one renders with pdf.js (Chrome on Android)', async ({ page }) => {
  const csp = watchForCspViolations(page);
  await fakePdfViewerSupport(page, false);
  await apiLogin(page);
  await openPdfInViewer(page);

  // No iframe at all: framing a PDF where the browser has no viewer is what
  // produced the broken-document icon, and no header change fixes that.
  await expect(page.locator('iframe.fv__pdf')).toHaveCount(0);

  // A canvas is appended before rendering finishes. Wait for the preview
  // status to clear too, so a blank canvas followed by a render failure
  // cannot pass as a successful paint under the live CSP.
  const pages = page.locator('[data-testid="pdf-preview-pages"] canvas');
  await expect(pages.first()).toBeVisible({ timeout: 30_000 });
  await expect(page.locator('.pdfp__status')).toHaveCount(0, { timeout: 30_000 });
  const box = await pages.first().boundingBox();
  expect(box?.width ?? 0).toBeGreaterThan(0);
  expect(box?.height ?? 0).toBeGreaterThan(0);

  expect(csp, `CSP refusals in the console: ${csp.join(' | ')}`).toEqual([]);
});
