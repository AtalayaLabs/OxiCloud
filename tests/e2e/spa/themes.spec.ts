import { test, expect } from './coverage-helpers';
import { readFileSync } from 'node:fs';
import { join } from 'node:path';
import { apiLogin, apiCreateFolder, apiUploadFile, SAMPLE_FILES } from './helpers';

for (const mode of ['light', 'dark'] as const) {
  test(`Slate preserves readable text and unified file icons in ${mode} mode`, async ({ page }) => {
    await page.route('**/themes/active.css', route => route.fulfill({
      contentType: 'text/css',
      body: readFileSync(join(__dirname, '../../../resources/themes/slate/index.css'), 'utf8'),
    }));
    await page.addInitScript(scheme => localStorage.setItem('oxi-theme', scheme), mode);
    await page.goto('/login');
    await expect(page.locator('html')).toHaveAttribute('data-color-scheme', mode);

    const palette = await page.evaluate(() => {
      const probe = document.createElement('span');
      document.body.append(probe);
      const color = (token: string) => {
        probe.style.color = `var(${token})`;
        return getComputedStyle(probe).color;
      };
      const luminance = (rgb: string) => rgb.match(/[\d.]+/g)!.slice(0, 3)
        .map(Number)
        .map(channel => channel / 255)
        .map(channel => channel <= 0.04045 ? channel / 12.92 : ((channel + 0.055) / 1.055) ** 2.4)
        .reduce((sum, channel, index) => sum + channel * [0.2126, 0.7152, 0.0722][index], 0);
      const contrast = (foreground: string, background: string) => {
        const a = luminance(color(foreground));
        const b = luminance(color(background));
        return (Math.max(a, b) + 0.05) / (Math.min(a, b) + 0.05);
      };
      const text = ['text', 'text-heading', 'text-secondary', 'text-muted', 'text-subtle', 'text-faint'];
      const surfaces = ['bg-page', 'bg-surface', 'bg-input', 'bg-hover', 'bg-muted', 'item-selected'];
      const textPairs = text.flatMap(fg => surfaces.map(bg => ({
        fg, bg, ratio: contrast(`--color-${fg}`, `--color-${bg}`),
      })));
      const primary = ['accent', 'accent-hover'].map(bg => contrast('--color-on-accent', `--color-${bg}`));
      const sidebar = contrast('--color-sidebar-text', '--color-sidebar-bg-from');
      const activeSidebar = contrast('--color-sidebar-text-active', '--color-sidebar-active-bg');
      const iconKinds = ['folder', 'doc', 'pdf', 'sheet', 'slides', 'archive', 'code', 'image', 'video', 'audio', 'text', 'generic'];
      const icons = iconKinds.map(kind => color(`--file-kind-${kind}`));
      const iconContrast = contrast('--file-kind-generic', '--color-bg-surface');
      const folderContrast = contrast('--file-kind-folder', '--color-bg-surface');
      const accentText = color('--color-accent-text');
      const sidebarInk = color('--color-sidebar-text');
      probe.remove();
      return { textPairs, primary, sidebar, activeSidebar, icons, iconContrast, folderContrast, accentText, sidebarInk };
    });

    // Regression pin: faint text and file glyphs must stay readable in both
    // modes; use resolved colors so invalid CSS values cannot pass as tokens.
    for (const { fg, bg, ratio } of palette.textPairs) {
      expect(ratio, `${fg} on ${bg}`).toBeGreaterThanOrEqual(4.5);
    }
    for (const ratio of [...palette.primary, palette.sidebar, palette.activeSidebar]) {
      expect(ratio).toBeGreaterThanOrEqual(4.5);
    }
    expect(new Set(palette.icons).size).toBe(1);
    expect(palette.iconContrast).toBeGreaterThanOrEqual(3);
    expect(palette.folderContrast).toBeGreaterThanOrEqual(3);

    await apiLogin(page);
    const folder = await apiCreateFolder(page, `Slate ${mode} ${Date.now()}`);
    await apiCreateFolder(page, 'Reports', folder.id);
    for (const file of [SAMPLE_FILES.text(), SAMPLE_FILES.pdf(), SAMPLE_FILES.csv(), SAMPLE_FILES.json()]) {
      await apiUploadFile(page, file, folder.id);
    }
    await page.goto(`/files/${folder.id}`);
    await expect(page.getByText('notes.txt', { exact: true })).toBeVisible();
    // Regression pin: equal-specificity section colors won when styles were
    // inserted after the theme (e.g. HMR). Replay the real navigation rules
    // last, including the active state; every glyph must keep one neutral ink.
    const sidebarCss = readFileSync(join(__dirname, '../../../frontend/src/lib/styles/ported/sidebar.css'), 'utf8');
    const sectionRules = sidebarCss.match(/\.nav-item(?:\[data-section="[a-z-]+"\]|\.active)[\s\S]*?\}/g);
    expect(sectionRules?.length).toBeGreaterThan(1);
    // Regression pin: the sidebar's base rule also reintroduced its shadow
    // when loaded after the theme. Replay that actual rule in the same order.
    const sidebarBase = sidebarCss.match(/\.sidebar \{[\s\S]*?\n\}/)?.[0];
    expect(sidebarBase).toBeTruthy();
    await page.addStyleTag({ content: [sidebarBase!, ...sectionRules!].join('\n') });
    for (const link of await page.locator('.sidebar .nav-item').all()) {
      await expect(link.locator('.oxi-icon').first()).toHaveCSS('color', palette.sidebarInk);
      await link.hover();
      await expect(link.locator('.oxi-icon').first()).toHaveCSS('color', palette.sidebarInk);
    }
    await expect(page.locator('.sidebar .logo svg')).toHaveCSS('fill', 'rgb(255, 255, 255)');
    await expect(page.locator('.toggle-btn.group-by-btn.active')).toHaveCSS('color', palette.accentText);

    // Regression pin: clearing the logo's shadow token did not remove the
    // sidebar's own shadow, and the preview's inset ring left nested boxes.
    const sidebar = page.locator('.sidebar');
    await expect(sidebar).toHaveCSS('box-shadow', 'none');
    await expect(sidebar).toHaveCSS('background-image', 'none');
    await expect(page.locator('.top-bar')).toHaveCSS('background-color', await sidebar.evaluate(el => getComputedStyle(el).backgroundColor));
    await expect(page.locator('.top-bar')).toHaveCSS('border-bottom-width', '0px');
    for (const name of ['Reports', 'notes.txt']) {
      const card = page.getByTestId(name);
      await expect(card).toHaveCSS('box-shadow', 'none');
      await expect(card.locator('.file-icon')).toHaveCSS('box-shadow', 'none');
      await expect(card.locator('.file-icon')).toHaveCSS('background-color', 'rgba(0, 0, 0, 0)');
      await card.hover();
      await expect(card).toHaveCSS('box-shadow', 'none');
      const checkbox = card.getByRole('checkbox');
      await checkbox.check();
      await expect(card).toHaveClass(/selected/);
      await expect(card).toHaveCSS('box-shadow', 'none');
      // Flat styling must retain a visible selection after the pointer leaves.
      await page.locator('.top-bar').hover();
      await expect(card).toHaveCSS('border-top-style', 'solid');
      await expect(card).not.toHaveCSS('border-top-color', 'rgba(0, 0, 0, 0)');
      await checkbox.uncheck();
      await expect(card).not.toHaveClass(/selected/);
    }
    // Regression pin: enlarging file glyphs left hover chips overlapping them.
    // Check real geometry at desktop and phone widths, for files and folders.
    for (const width of [1280, 390]) {
      await page.setViewportSize({ width, height: 844 });
      for (const name of ['Reports', 'notes.txt']) {
        const card = page.getByTestId(name);
        await card.hover();
        const overlapping = await card.evaluate(el => {
          const glyph = el.querySelector('.file-icon > svg')!.getBoundingClientRect();
          return [...el.querySelectorAll('.checkbox-cell, .action-cell button, .grid-meta .file-actions')]
            .filter(control => {
              const box = control.getBoundingClientRect();
              return box.left < glyph.right && box.right > glyph.left && box.top < glyph.bottom && box.bottom > glyph.top;
            }).map(control => control.className);
        });
        expect(overlapping).toEqual([]);
      }
    }
    await page.setViewportSize({ width: 1280, height: 720 });
    await page.locator('.top-bar').hover();
  });
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
