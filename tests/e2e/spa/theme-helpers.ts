import { existsSync, readFileSync, readdirSync } from 'node:fs';
import { join } from 'node:path';
import type { Locator, Page } from '@playwright/test';
import { expect } from './coverage-helpers';

const themeDir = join(__dirname, '../../../resources/themes');
export const colorModes = ['light', 'dark'] as const;
type ColorMode = (typeof colorModes)[number];

// The built-in appearance is the same app with no operator overrides.
export const themes = [
  { name: 'default', css: '' },
  ...readdirSync(themeDir, { withFileTypes: true })
    .filter((entry) => entry.isDirectory() && existsSync(join(themeDir, entry.name, 'index.css')))
    .map((entry) => entry.name)
    .sort()
    .map((name) => ({
      name,
      css: readFileSync(join(themeDir, name, 'index.css'), 'utf8'),
    })),
];

export async function applyTheme(
  page: Page,
  theme: (typeof themes)[number],
  mode: ColorMode,
): Promise<void> {
  await page.addInitScript((scheme) => {
    localStorage.setItem('oxi-theme', scheme);
    localStorage.setItem('oxi-view-mode', 'grid');
  }, mode);
  // Intercept only this browser's stylesheet: never change instance appearance.
  await page.route('**/themes/active.css', (route) =>
    route.fulfill({
      contentType: 'text/css',
      body: theme.css,
    }),
  );
}

/** Verify the intercepted stylesheet was loaded and parsed by the browser. */
export async function expectThemeLoaded(page: Page, theme: (typeof themes)[number]): Promise<void> {
  await expect
    .poll(() =>
      page.locator('#active-theme').evaluate((el, hasOverrides) => {
        const sheet = (el as HTMLLinkElement).sheet;
        return sheet !== null && sheet.cssRules.length > 0 === hasOverrides;
      }, theme.css.length > 0),
    )
    .toBe(true);
}

/** Check rendered text against its background, including translucent overlay
 * controls. Do not prescribe a palette or compare unused token combinations. */
export async function expectReadable(locator: Locator): Promise<void> {
  await expect(locator).toBeVisible();
  await expect
    .poll(
      () =>
        locator.evaluate((el) => {
          // Normalize any browser-supported CSS color (including oklch and
          // display-p3) to sRGB before computing relative luminance.
          const canvas = document.createElement('canvas');
          canvas.width = canvas.height = 1;
          const context = canvas.getContext('2d', { willReadFrequently: true })!;
          const rgba = (css: string) => {
            context.clearRect(0, 0, 1, 1);
            context.fillStyle = css;
            context.fillRect(0, 0, 1, 1);
            const [r, g, b, a] = context.getImageData(0, 0, 1, 1).data;
            return [r, g, b, a / 255];
          };
          const over = (fg: number[], bg: number[]) => {
            const alpha = fg[3] + bg[3] * (1 - fg[3]);
            return [0, 1, 2]
              .map((i) => (alpha ? (fg[i] * fg[3] + bg[i] * bg[3] * (1 - fg[3])) / alpha : 0))
              .concat(alpha);
          };
          let background = [0, 0, 0, 0];
          for (let node: Element | null = el; node; node = node.parentElement) {
            background = over(background, rgba(getComputedStyle(node).backgroundColor));
          }
          background = over(background, [255, 255, 255, 1]);
          const foreground = over(rgba(getComputedStyle(el).color), background);
          const luminance = (color: number[]) =>
            color
              .slice(0, 3)
              .map((v) => v / 255)
              .map((v) => (v <= 0.04045 ? v / 12.92 : ((v + 0.055) / 1.055) ** 2.4))
              .reduce((sum, v, i) => sum + v * [0.2126, 0.7152, 0.0722][i], 0);
          const a = luminance(foreground),
            b = luminance(background);
          return (Math.max(a, b) + 0.05) / (Math.min(a, b) + 0.05);
        }),
      'rendered text contrast',
    )
    .toBeGreaterThanOrEqual(4.5);
}
