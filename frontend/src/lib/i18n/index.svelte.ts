/**
 * Reactive i18n — ported from static/js/core/i18n.js.
 *
 * Kept as a bespoke module (rather than svelte-i18n) so the 16 existing locale
 * JSON files work byte-for-byte: they use `{{param}}` interpolation, dot-notation
 * nested keys, and a prefix_suffix underscore-fallback heuristic that ICU-based
 * libraries don't model. `t()` reads module-level runes, so any component that
 * calls it re-renders when the locale changes.
 *
 * Storage key `oxicloud-locale` and the server round-trip via
 * PATCH /api/auth/me/profile are preserved for cross-device/email parity.
 */
import { base } from '$app/paths';
import { apiFetch } from '$lib/api/client';
import { getCsrfHeaders } from '$lib/api/csrf';

// Keep in sync with the locale files in static/locales (and, post-cutover,
// frontend/static/locales). Mirrors AVAILABLE_LOCALES in the language selector.
export const SUPPORTED_LOCALES = [
	'en',
	'es',
	'zh',
	'zh-TW',
	'fa',
	'fr',
	'de',
	'pt',
	'nl',
	'it',
	'hi',
	'ar',
	'ru',
	'ja',
	'ko',
	'pl'
] as const;

export type Locale = (typeof SUPPORTED_LOCALES)[number];

/** Locales that render right-to-left. */
const RTL_LOCALES: readonly Locale[] = ['fa', 'ar'];

export interface LanguageMeta {
	code: Locale;
	/** Endonym (native language name). */
	name: string;
	/** Flag emoji. */
	flag: string;
}

/**
 * Display metadata for the language selector — native names + flags, ported
 * from ALL_LANGUAGES in static/js/features/auth/auth.js. Order matches
 * SUPPORTED_LOCALES so the rich dropdown lists the same set as `t()` resolves.
 */
export const LANGUAGES: readonly LanguageMeta[] = [
	{ code: 'en', name: 'English', flag: '🇬🇧' },
	{ code: 'es', name: 'Español', flag: '🇪🇸' },
	{ code: 'zh', name: '简体中文', flag: '🇨🇳' },
	{ code: 'zh-TW', name: '繁體中文', flag: '🇹🇼' },
	{ code: 'fa', name: 'فارسی', flag: '🇮🇷' },
	{ code: 'fr', name: 'Français', flag: '🇫🇷' },
	{ code: 'de', name: 'Deutsch', flag: '🇩🇪' },
	{ code: 'pt', name: 'Português', flag: '🇧🇷' },
	{ code: 'nl', name: 'Nederlands', flag: '🇳🇱' },
	{ code: 'it', name: 'Italiano', flag: '🇮🇹' },
	{ code: 'hi', name: 'हिन्दी', flag: '🇮🇳' },
	{ code: 'ar', name: 'العربية', flag: '🇸🇦' },
	{ code: 'ru', name: 'Русский', flag: '🇷🇺' },
	{ code: 'ja', name: '日本語', flag: '🇯🇵' },
	{ code: 'ko', name: '한국어', flag: '🇰🇷' },
	{ code: 'pl', name: 'Polski', flag: '🇵🇱' }
];

const STORAGE_KEY = 'oxi-locale';

/**
 * Reflect the active locale on `<html>`: sets `lang` and flips `dir` to `rtl`
 * for Farsi/Arabic (and `ltr` otherwise) so the ported [dir="rtl"] CSS engages.
 */
function applyHtmlLang(locale: string): void {
	if (typeof document === 'undefined') return;
	const html = document.documentElement;
	html.setAttribute('lang', locale);
	html.setAttribute('dir', (RTL_LOCALES as readonly string[]).includes(locale) ? 'rtl' : 'ltr');
}

type Dict = Record<string, unknown>;

/**
 * Resolve the best supported locale from a browser language list.
 * Priority: exact full-tag > Chinese script/region heuristics > primary subtag.
 */
export function resolveBrowserLocale(
	langs: readonly string[] = typeof navigator !== 'undefined'
		? (navigator.languages ?? [navigator.language || 'en'])
		: ['en']
): Locale {
	const lowerSupported = SUPPORTED_LOCALES.map((l) => l.toLowerCase());

	for (const bl of langs) {
		const idx = lowerSupported.indexOf(bl.toLowerCase());
		if (idx !== -1) return SUPPORTED_LOCALES[idx];
	}
	for (const bl of langs) {
		const tag = bl.toLowerCase();
		if (!tag.startsWith('zh')) continue;
		const isTraditional = tag.includes('hant') || /\b(tw|hk|mo)\b/.test(tag);
		const target: Locale = isTraditional ? 'zh-TW' : 'zh';
		if (SUPPORTED_LOCALES.includes(target)) return target;
	}
	for (const bl of langs) {
		const primary = bl.substring(0, 2).toLowerCase();
		const match = SUPPORTED_LOCALES.find((l) => l === primary);
		if (match) return match;
	}
	return 'en';
}

// Resolved-value cache, one map per dict object: `t()` runs ~10× per rendered
// list row over the app's finite static key set, so the nested split + tree
// walk runs once per (locale, key) instead of on every call. Dicts are
// assigned once in `loadDict` and never mutated, so entries can't go stale;
// the cap only guards against a pathological dynamic-key caller.
const RESOLVED_CACHE_MAX = 4000;
const resolvedCache = new WeakMap<Dict, Map<string, string | null>>();

/** Resolve a dot-notation key with a prefix_suffix underscore fallback. */
export function getNestedValue(obj: Dict | undefined, path: string): string | null {
	if (!obj || typeof obj !== 'object') return resolveNestedValue(obj, path);
	let cache = resolvedCache.get(obj);
	if (cache === undefined) {
		// eslint-disable-next-line svelte/prefer-svelte-reactivity -- deliberately non-reactive: a memo written during render must not create/notify signals
		cache = new Map();
		resolvedCache.set(obj, cache);
	}
	const hit = cache.get(path);
	if (hit !== undefined) return hit;
	const value = resolveNestedValue(obj, path);
	if (cache.size >= RESOLVED_CACHE_MAX) cache.clear();
	cache.set(path, value);
	return value;
}

/** The uncached lookup: flat-key fast path, dotted walk, underscore fallback. */
function resolveNestedValue(obj: Dict | undefined, path: string): string | null {
	if (obj && typeof obj === 'object' && path in obj) {
		const value = obj[path];
		return typeof value === 'string' ? value : null;
	}

	const keys = path.split('.');
	let current: unknown = obj;
	for (const key of keys) {
		if (current && typeof current === 'object' && key in (current as Dict)) {
			current = (current as Dict)[key];
		} else {
			if (path.includes('_') && !path.includes('.')) {
				const [prefix, ...parts] = path.split('_');
				const suffix = parts.join('_');
				const branch = obj?.[prefix];
				if (branch && typeof branch === 'object' && suffix in (branch as Dict)) {
					const v = (branch as Dict)[suffix];
					return typeof v === 'string' ? v : null;
				}
			}
			return null;
		}
	}
	return typeof current === 'string' ? current : null;
}

/** Replace `{{param}}` placeholders; leaves unknown placeholders intact. */
export function interpolate(text: string, params: Record<string, unknown>): string {
	// The vast majority of UI strings carry no placeholder — skip the regex
	// scan (and its per-call machinery) for them.
	if (!text.includes('{{')) return text;
	return text.replace(/{{\s*([^}]+)\s*}}/g, (_, key: string) => {
		const k = key.trim();
		return params[k] !== undefined ? String(params[k]) : `{{${key}}}`;
	});
}

// ── Reactive state ─────────────────────────────────────────────────────────

const dicts = $state<Record<string, Dict>>({});
const store = $state<{ locale: string; loaded: boolean }>({
	locale: resolveBrowserLocale(),
	loaded: false
});

async function loadDict(locale: string): Promise<Dict> {
	if (dicts[locale]) return dicts[locale];
	try {
		const res = await fetch(`${base}/locales/${locale}.json`);
		if (!res.ok) throw new Error(`locale ${locale} ${res.status}`);
		dicts[locale] = (await res.json()) as Dict;
	} catch (err) {
		console.error('i18n: failed to load locale', locale, err);
		dicts[locale] = {};
	}
	return dicts[locale];
}

/** Shared frozen empty params for the no-interpolation call forms, so the
 * ubiquitous `t(key)` / `t(key, 'fallback')` don't each allocate a throwaway
 * `{}` (t() is the hottest UI function — ~10× per row). Never mutated, so a
 * single shared instance is safe. See benches/ROUND14.md §F1. */
const EMPTY_PARAMS: Record<string, unknown> = Object.freeze({});

/**
 * Translate a key.
 *  - `t(key)` / `t(key, params)` — interpolation params object.
 *  - `t(key, fallback)` — string fallback used when the key is missing.
 *  - `t(key, params, fallback)` — both; the fallback is also interpolated.
 */
export function t(
	key: string,
	paramsOrFallback: string | Record<string, unknown> = EMPTY_PARAMS,
	fallbackArg?: string
): string {
	const isStringForm = typeof paramsOrFallback === 'string';
	const params = isStringForm ? EMPTY_PARAMS : paramsOrFallback;
	const fallback = isStringForm ? paramsOrFallback : (fallbackArg ?? null);

	const localeData = dicts[store.locale];
	if (!localeData) {
		return fallback ? interpolate(fallback, params) : (key.split('.').pop() ?? key);
	}

	let value = getNestedValue(localeData, key);
	if (!value && store.locale !== 'en' && dicts.en) {
		value = getNestedValue(dicts.en, key);
	}
	if (!value) return fallback ? interpolate(fallback, params) : key;
	return interpolate(value, params);
}

export async function initI18n(): Promise<void> {
	const saved = typeof localStorage !== 'undefined' ? localStorage.getItem(STORAGE_KEY) : null;
	if (saved && (SUPPORTED_LOCALES as readonly string[]).includes(saved)) {
		store.locale = saved;
	}
	await loadDict(store.locale);
	applyHtmlLang(store.locale);
	store.loaded = true;
	// Warm the English fallback in the background. `t()` only consults it for
	// keys the active (complete) locale is missing — and most call sites already
	// pass an inline English fallback — so it must not block first paint. When it
	// arrives, `dicts.en` is reactive, so any key that fell through re-renders.
	if (store.locale !== 'en') void loadDict('en');
}

export async function setLocale(locale: Locale): Promise<boolean> {
	if (!(SUPPORTED_LOCALES as readonly string[]).includes(locale)) {
		console.error(`Locale not supported: ${locale}`);
		return false;
	}
	await loadDict(locale);
	store.locale = locale;
	applyHtmlLang(locale);
	if (typeof localStorage !== 'undefined') localStorage.setItem(STORAGE_KEY, locale);
	persistLocaleToServer(locale);
	return true;
}

/** Fire-and-forget server persistence; anonymous callers 401 and that's fine. */
function persistLocaleToServer(locale: string): void {
	apiFetch('/api/auth/me/profile', {
		method: 'PATCH',
		headers: { 'Content-Type': 'application/json', ...getCsrfHeaders() },
		credentials: 'same-origin',
		body: JSON.stringify({ preferred_locale: locale })
	}).catch((err: unknown) => {
		console.debug('locale: server persistence skipped', err);
	});
}

export const i18n = {
	t,
	setLocale,
	get locale() {
		return store.locale;
	},
	get loaded() {
		return store.loaded;
	},
	supported: SUPPORTED_LOCALES
};
