/**
 * Theme store — light / dark / auto.
 *
 * Persists to the `oxi-theme` localStorage key (part of the normalised
 * `oxi-*` prefs namespace — see `$lib/utils/localStoragePrefs`) and
 * reflects the choice on `<html data-color-scheme>`. `auto` removes the
 * attribute so the OS `prefers-color-scheme` takes over. The anti-FOUC
 * inline script in app.html applies the stored value before first paint;
 * this store owns runtime changes from the UI.
 */
export type Theme = 'light' | 'dark' | 'auto';

/**
 * localStorage key holding the active theme.
 *
 * Exported (not just module-private) because `src/app.html`'s anti-FOUC
 * inline script also reads it — that script runs before any JS bundle
 * loads, so it can't `import` here. The `app.html` copy is a hardcoded
 * string kept in sync by `theme.test.ts::app.html theme key matches
 * THEME_STORAGE_KEY`, which fails CI on any drift.
 */
export const THEME_STORAGE_KEY = 'oxi-theme';

function readInitial(): Theme {
	if (typeof localStorage === 'undefined') return 'auto';
	const v = localStorage.getItem(THEME_STORAGE_KEY);
	return v === 'light' || v === 'dark' ? v : 'auto';
}

const store = $state<{ theme: Theme }>({ theme: readInitial() });

function applyChrome(): void {
	if (typeof document === 'undefined') return;
	const meta = document.querySelector<HTMLMetaElement>('meta[name="theme-color"]');
	if (!meta) return;
	// A custom property's text may still contain light-dark(). Resolve it as
	// an actual color so browser chrome follows the active theme and mode.
	const probe = document.createElement('span');
	probe.style.color = 'var(--color-bg-page)';
	probe.hidden = true;
	document.documentElement.append(probe);
	const color = getComputedStyle(probe).color;
	probe.remove();
	if (color) meta.content = color;
}

function apply(theme: Theme): void {
	if (typeof document === 'undefined') return;
	const html = document.documentElement;
	if (theme === 'light' || theme === 'dark') html.setAttribute('data-color-scheme', theme);
	else html.removeAttribute('data-color-scheme');
	applyChrome();
}

if (typeof window !== 'undefined') {
	window.matchMedia?.('(prefers-color-scheme: dark)').addEventListener?.('change', () => {
		if (store.theme === 'auto') applyChrome();
	});
	document.getElementById('active-theme')?.addEventListener('load', applyChrome);
	applyChrome();
}

export function setTheme(theme: Theme): void {
	store.theme = theme;
	if (typeof localStorage !== 'undefined') {
		if (theme === 'auto') localStorage.removeItem(THEME_STORAGE_KEY);
		else localStorage.setItem(THEME_STORAGE_KEY, theme);
	}
	apply(theme);
}

export const theme = {
	get current() {
		return store.theme;
	},
	set: setTheme
};
