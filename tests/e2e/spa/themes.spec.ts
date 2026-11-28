import { test, expect } from './coverage-helpers';
import type { Page } from '@playwright/test';
import { themes, colorModes, applyTheme, expectThemeLoaded, expectReadable } from './theme-helpers';
import { apiLogin } from './helpers';
import * as fs from 'node:fs';
import * as nodePath from 'node:path';

const rootId = '00000000-0000-4000-8000-000000000002';
const userId = '00000000-0000-4000-8000-000000000001';
const stamp = '2026-11-01T12:00:00Z';
const fileName = (i: number) => `Review ${String(i + 1).padStart(3, '0')}.pdf`;

// pdf.js fetches `/api/files/{id}` for every PDF-mime row in the list so it
// can render a client-side thumbnail. The browser-side mocks give the SPA a
// fake 120-row list but no bodies, so the pdf.js worker fetches out to the
// real server with no real session and gets 401. The 401 surfaces as a
// pdf.js `UnexpectedResponseException` on the page, which the global JS-error
// collector in `helpers.ts` turns into a test failure. Serving a real
// (tiny) PDF body from the fixture keeps pdf.js happy — thumbnail generation
// succeeds in-browser, no outbound request, no unhandled rejection.
const STUB_PDF = fs.readFileSync(
  nodePath.join(__dirname, '..', '..', 'fixtures', 'doc-date.pdf'),
);

/** CSS checks use the real SPA with browser-only API fixtures. No instance
 * appearance setting, account or stored document is changed by these tests. */
async function library(page: Page) {
  await page.addInitScript((id) => localStorage.setItem('oxi-active-user-id', id), userId);
  const origin = new URL(
    page.url() === 'about:blank' ? test.info().project.use.baseURL! : page.url(),
  ).origin;
  await page.context().addCookies([{ name: 'oxicloud_csrf', value: 'theme-fixture', url: origin }]);
  const user = {
    full: {
      user: {
        id: userId,
        username: 'camille',
        email: 'camille@example.test',
        role: 'admin',
        is_external: false,
      },
      active: true,
      has_password: true,
      opaque_registered: false,
      opaque_migrated: false,
      storage_used_bytes: 0,
      storage_quota_bytes: 10737418240,
      created_at: stamp,
      updated_at: stamp,
    },
    can_edit_image: true,
    force_password_change: false,
    is_dpop_bound: true,
    is_upgradable: false,
    notify_on_share: false,
    ui_preferences: {},
  };
  const drive = {
    id: 'library-drive',
    name: 'Library',
    kind: 'personal',
    root_folder_id: rootId,
    default_for_user: userId,
    policies: {},
    used_bytes: 0,
    quota_bytes: null,
    created_at: stamp,
    updated_at: stamp,
  };
  await page.route(
    (url) => url.pathname.startsWith('/api/'),
    async (route) => {
      const path = new URL(route.request().url()).pathname;
      if (path === '/api/config') {
        const response = await route.fetch();
        const config = (await response.json()) as {
          features: Record<string, boolean>;
        };
        config.features.message_bus = false;
        return route.fulfill({ json: config });
      }
      if (path === '/api/auth/me') return route.fulfill({ json: user });
      if (path === '/api/drives') return route.fulfill({ json: [drive] });
      if (path === `/api/folders/${rootId}/ancestors`)
        return route.fulfill({
          json: {
            ancestors: [{ id: rootId, name: 'Library', drive_id: drive.id }],
            access_source: { kind: 'drive', drive },
          },
        });
      if (path === `/api/folders/${rootId}/resources`)
        return route.fulfill({
          json: {
            items: Array.from({ length: 120 }, (_, i) => ({
              resource_type: 'file',
              resource: {
                id: `10000000-0000-4000-8000-${String(i).padStart(12, '0')}`,
                name: fileName(i),
                path: `/${fileName(i)}`,
                folder_id: rootId,
                created_by: userId,
                updated_by: userId,
                size: 283100 + i,
                // Not `application/pdf` on purpose. The SPA's
                // client-side thumbnail generator (`lib/utils/thumbnail.ts`)
                // spawns the pdf.js worker for every PDF-mime row, and
                // dedicated-worker fetches bypass Playwright's
                // `page.route` — so each row's `/api/files/{id}` fetch
                // leaks out to the real server (no real session) and
                // 401s, and pdf.js surfaces an `UnexpectedResponseException`
                // the JS-error collector promotes to a test failure.
                // An unrecognised mime type makes `generate()` throw
                // "unsupported mime type" before any fetch; the caller
                // (`queueGenerate`) catches it and warns to console. The
                // theme suite only exercises CSS over the row names and
                // controls — no thumbnail is painted either way.
                mime_type: 'application/octet-stream',
                category: 'Document',
                created_at: stamp,
                modified_at: stamp,
                content_hash: '',
                etag: '',
                is_favorite: false,
                is_shared: false,
              },
            })),
          },
        });
      // Preference writes stay inside the fixture, like the document reads.
      if (route.request().method() !== 'GET') return route.fulfill({ json: {} });
      if (path === '/api/notifications')
        return route.fulfill({ json: { items: [], unread_count: 0 } });
      if (path === '/api/status') return route.fallback();
      // Any GET /api/files/{id} — the thumbnail generator's pdf.js call.
      // Serve a real tiny PDF so the parse succeeds in-browser; without
      // this the fetch leaks out to the server (no real session), 401s,
      // and pdf.js surfaces an `UnexpectedResponseException` that the
      // global JS-error collector flags as a test failure.
      if (/^\/api\/files\/[0-9a-f-]+$/i.test(path))
        return route.fulfill({
          status: 200,
          contentType: 'application/pdf',
          body: STUB_PDF,
        });
      return route.fulfill({
        status: 404,
        json: { error: 'NotFound', message: 'Layout fixture' },
      });
    },
  );
  await page.goto(`/files/${rootId}`);
  await expect(page.getByTestId(fileName(0))).toBeVisible();
}

for (const theme of themes) {
  for (const mode of colorModes) {
    test.describe(`${theme.name} / ${mode}`, () => {
      test.beforeEach(async ({ page }) => {
        await applyTheme(page, theme, mode);
      });

      test('menus remain reachable over the scrolling content', async ({ page }) => {
        await library(page);
        await expectThemeLoaded(page, theme);

        // Issue #894: exercise real hit targets above and inside the scroller,
        // including after scrolling. This checks interactions, but does not
        // reproduce the reported platform-specific painting artifacts.
        for (const width of [1440, 390]) {
          await page.setViewportSize({ width, height: 844 });
          for (const index of [0, 119]) {
            await page.locator('.content-area').evaluate((el, end) => {
              el.scrollTop = end ? el.scrollHeight : 0;
            }, index > 0);
            const item = page.getByTestId(fileName(index));
            await expect(item).toBeVisible();
            await item.click({ button: 'right' });
            const menu = page.getByRole('menu');
            await expect(menu).toBeVisible();
            const actions = menu.getByRole('menuitem');
            for (const action of await actions.all()) {
              if (await action.isEnabled()) await action.click({ trial: true });
            }
            await expectReadable(actions.first());
            await page.keyboard.press('ArrowDown');
            await expect(actions.nth(1)).toBeFocused();
            await page.keyboard.press('Escape');
            await expect(menu).toBeHidden();

            if (width > 768) {
              await page.getByTestId('appshell-user-menu-btn').click();
              const profile = page.getByTestId('appshell-user-menu-profile-item');
              await expectReadable(profile);
              await profile.click({ trial: true });
              await page.getByTestId('appshell-user-menu-btn').click();
              await expect(profile).toBeHidden();
            }
          }
        }
      });

      test('library keeps selection, controls and virtual scrolling usable', async ({ page }) => {
        await page.setViewportSize({ width: 1440, height: 1000 });
        await library(page);
        await expectThemeLoaded(page, theme);
        await expectReadable(page.getByTestId(fileName(0)).locator('.name-cell__text'));

        for (const width of [1440, 768, 390]) {
          await page.setViewportSize({ width, height: 844 });
          const first = page.getByTestId(fileName(0));
          await expect(first).toBeVisible();
          await first.hover();
          const checkbox = first.getByRole('checkbox');
          // Regression pin: flat card styling hid selection after hover ended.
          // Compare rendered styles instead of requiring a theme-specific border.
          const appearance = () =>
            first.evaluate((el) => {
              const s = getComputedStyle(el);
              return [s.backgroundColor, s.borderColor, s.boxShadow, s.outline].join('|');
            });
          await page.locator('.top-bar').hover();
          const unselected = await appearance();
          await first.hover();
          await checkbox.check();
          await page.locator('.top-bar').hover();
          await expect(first).toHaveClass(/selected/);
          await expect.poll(appearance).not.toBe(unselected);
          await first.hover();
          await checkbox.uncheck();
          await expect(first).not.toHaveClass(/selected/);

          // Regression pin: card restyling obscured hover commands. Check
          // hit targets rather than forbidding overlays on decorative glyphs.
          for (const control of await first.locator('button, input[type="checkbox"]').all()) {
            if (!(await control.isVisible())) continue;
            await control.click({ trial: true });
            expect(
              await control.evaluate((el) => {
                const box = el.getBoundingClientRect();
                const card = el.closest('.file-item')!.getBoundingClientRect();
                return (
                  box.left >= card.left - 1 &&
                  box.right <= card.right + 1 &&
                  box.top >= card.top - 1 &&
                  box.bottom <= card.bottom + 1
                );
              }),
            ).toBe(true);
          }

          const share = page.getByTestId(`folder-breadcrumb-share-${rootId}`);
          await share.focus();
          await page.keyboard.press('Tab');
          await page.keyboard.press('Shift+Tab');
          await expect(share).toBeFocused();
          await expect(share).toBeInViewport();
          await expect(share).not.toHaveCSS('outline-style', 'none');

          // Regression pin: mismatched CSS grid sizing silently skipped files
          // in virtual windows. Exercise both ends of the rendered collection.
          await page.locator('.content-area').evaluate((el) => {
            el.scrollTop = el.scrollHeight;
          });
          await expect(page.getByTestId(fileName(119))).toBeVisible();
          await page.locator('.content-area').evaluate((el) => {
            el.scrollTop = 0;
          });
          await expect(first).toBeVisible();
          expect(
            await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth),
          ).toBe(true);
        }

        await page.setViewportSize({ width: 1440, height: 1000 });
        await page.getByTestId('appshell-sidebar-collapse-btn').click();
        await expect(page.locator('.sidebar')).toHaveClass(/sidebar--collapsed/);
        await expect(page.locator('.drive-picker')).toBeHidden();
        // Regression pin: a collapsed sidebar must not cover the content panel.
        expect(
          await page
            .locator('.main-content')
            .evaluate(
              (el) =>
                el.getBoundingClientRect().left >=
                document.querySelector('.sidebar')!.getBoundingClientRect().right - 1,
            ),
        ).toBe(true);

        await page.getByTestId('display-mode-view-toggle-btn').click();
        await expect(page.locator('.files-list-view')).toBeVisible();
        await page.locator('.content-area').evaluate((el) => {
          el.scrollTop = el.scrollHeight;
        });
        await expect(page.getByTestId(fileName(119))).toBeVisible();
        await page.locator('.content-area').evaluate((el) => {
          el.scrollTop = 0;
        });
        await page.getByTestId('display-mode-groupby-btn').click();
        await page.getByTestId('display-mode-groupby-type-item').click();
        await expect(page.locator('.lane__header--sticky').first()).toBeVisible();

        await page.evaluate(() => (document.documentElement.dir = 'rtl'));
        await page.setViewportSize({ width: 390, height: 400 });
        await page.getByTestId('appshell-sidebar-toggle-btn').click();
        await expect(page.locator('.sidebar')).toHaveClass(/open/);
        await expect
          .poll(() =>
            page.locator('.sidebar').evaluate((el) => {
              const box = el.getBoundingClientRect();
              return box.left >= -1 && box.right <= innerWidth + 1;
            }),
          )
          .toBe(true);
      });

      test('sign-in controls stay reachable in short viewports', async ({ page }) => {
        await page.setViewportSize({ width: 390, height: 400 });
        await page.goto('/login');
        await expectThemeLoaded(page, theme);
        await expect(page.locator('html')).toHaveAttribute('data-color-scheme', mode);
        await expectReadable(page.locator('.auth-title'));
        // Regression pin: vertically centred login panels lost their controls
        // in short windows. Test reachability without prescribing a scroll owner.
        for (const id of ['login-submit-btn', 'login-username-input']) {
          const control = page.getByTestId(id);
          await control.scrollIntoViewIfNeeded();
          await expect(control).toBeInViewport();
          await expect(control).toBeEnabled();
        }
      });
    });
  }
}

// Browser regression pins: custom properties accept invalid text until consumed.
// Assert real colors/backgrounds rather than the presence of a token declaration.
for (const mode of ['light', 'dark'] as const) {
  test(`global theme follows ${mode} mode and keeps contrast overrides`, async ({ page }) => {
    await page.addInitScript((scheme) => localStorage.setItem('oxi-theme', scheme), mode);
    await page.route('**/themes/active.css', route => route.fulfill({
      contentType: 'text/css',
      body: ':root { --color-bg-page: light-dark(#123456, #abcdef); --color-border: #010101; }'
    }));
    await page.goto('/login');
    await expect(page.locator('html')).toHaveAttribute('data-color-scheme', mode);
    const background = () => page.evaluate(() => {
      const probe = document.createElement('div');
      probe.style.backgroundColor = 'var(--color-bg-page)';
      document.body.append(probe);
      const color = getComputedStyle(probe).backgroundColor;
      probe.remove();
      return color;
    });
    await expect.poll(background).toBe(mode === 'light' ? 'rgb(18, 52, 86)' : 'rgb(171, 205, 239)');
    // Browser chrome needs a resolved color, not an unevaluated light-dark().
    await expect(page.locator('meta[name="theme-color"]')).toHaveAttribute(
      'content', mode === 'light' ? 'rgb(18, 52, 86)' : 'rgb(171, 205, 239)',
    );
    await page.emulateMedia({ contrast: 'more' });
    const border = await page.evaluate(() => {
      const probe = document.createElement('div');
      probe.style.borderColor = 'var(--color-border)';
      document.body.append(probe);
      const color = getComputedStyle(probe).borderTopColor;
      probe.remove();
      return color;
    });
    expect(border).not.toBe('rgb(1, 1, 1)');
  });
}

test('default colors remain usable when the theme request fails', async ({ page }) => {
  await page.route('**/themes/active.css', route => route.abort());
  await page.goto('/login');
  await expect(page.locator('body')).toBeVisible();
  const accent = await page.evaluate(() => {
    const probe = document.createElement('div');
    probe.style.color = 'var(--color-accent)';
    document.body.append(probe);
    const color = getComputedStyle(probe).color;
    probe.remove();
    return color;
  });
  expect(accent).toBe('rgb(255, 94, 58)');
});

test('menu gradient is a usable background in both color schemes', async ({ page }) => {
  await page.goto('/login');
  for (const mode of ['light', 'dark']) {
    const image = await page.evaluate((scheme) => {
      document.documentElement.setAttribute('data-color-scheme', scheme);
      const probe = document.createElement('div');
      probe.style.backgroundImage = 'var(--color-user-menu-header-bg)';
      document.body.append(probe);
      const image = getComputedStyle(probe).backgroundImage;
      probe.remove();
      return image;
    }, mode);
    // light-dark(gradient, gradient) made the declaration invalid at use time.
    expect(image).toContain('linear-gradient(');
    expect(image).not.toBe('none');
  }
});

test('system preference and Askama pages consume the same active CSS', async ({ page }) => {
  await page.emulateMedia({ colorScheme: 'dark' });
  await page.route('**/themes/active.css', route => route.fulfill({
    contentType: 'text/css', body: ':root { --color-bg-page: light-dark(#123456, #abcdef); }'
  }));
  await page.goto('/magic/v1/theme-test-invalid-token');
  await expect(page.locator('link[rel="stylesheet"][href$="/themes/active.css"]')).toHaveCount(1);
  await expect(page.locator('body')).toHaveCSS('background-color', 'rgb(171, 205, 239)');
});

test('initial splash uses the active background before the application mounts', async ({ page }) => {
  let release!: () => void;
  const gate = new Promise<void>(resolve => { release = resolve; });
  await page.addInitScript(() => localStorage.setItem('oxi-theme', 'light'));
  await page.route('**/themes/active.css', route => route.fulfill({
    contentType: 'text/css', body: ':root { --color-bg-page: #123456; }'
  }));
  await page.route('**/_app/immutable/**/*.js', async route => { await gate; await route.continue(); });
  const navigation = page.goto('/login');
  try {
    await expect(page.locator('#app-splash')).toHaveCSS('background-color', 'rgb(18, 52, 86)');
  } finally {
    release();
    await navigation;
  }
});

// Regression pin: loading operator CSS before bundle styles let the global
// body reset overwrite component rules despite a successful theme selection.
test('operator component rules remain applied after the bundle loads', async ({ page }) => {
  await page.route('**/themes/active.css', route => route.fulfill({
    contentType: 'text/css', body: 'body { background-color: #123456; }'
  }));
  await page.goto('/login');
  await expect(page.locator('body')).toHaveCSS('background-color', 'rgb(18, 52, 86)');
});

// The layout tests intercept CSS. This integration pin must cross the real
// admin UI, persistence service and public stylesheet route without interception.
test('saved appearance reaches an anonymous browser without changing its color mode', async ({
  page,
  browser,
}) => {
  await apiLogin(page);
  await page.goto('/admin/appearance');
  const picker = page.getByLabel('Global theme');
  await expect(picker).toBeVisible();
  const previous = await picker.inputValue();
  const installed = themes.find((theme) => theme.name === 'slate')!;
  const anonymous = await browser.newContext({ baseURL: new URL(page.url()).origin });
  const saveTheme = async (id: string) => {
    await picker.selectOption(id);
    await page.getByRole('button', { name: 'Save', exact: true }).click();
    await expect(page.getByRole('status')).toContainText('next full page load');
  };
  try {
    await saveTheme(installed.name);
    await page.reload();
    await expect(page.getByText(`Active theme: ${installed.name}`, { exact: true })).toBeVisible();

    const visitor = await anonymous.newPage();
    await visitor.addInitScript(() => localStorage.setItem('oxi-theme', 'dark'));
    await visitor.goto('/login');
    await expectThemeLoaded(visitor, installed);
    await expect(visitor.locator('html')).toHaveAttribute('data-color-scheme', 'dark');
    const css = await visitor.evaluate(async () => {
      const link = document.querySelector<HTMLLinkElement>('#active-theme')!;
      const response = await fetch(link.href);
      return { status: response.status, body: await response.text() };
    });
    expect(css.status).toBe(200);
    expect(css.body).toBe(installed.css);
  } finally {
    // Other specs share this server: always restore its original selection.
    await anonymous.close();
    await page.goto('/admin/appearance');
    await saveTheme(previous);
  }
});
