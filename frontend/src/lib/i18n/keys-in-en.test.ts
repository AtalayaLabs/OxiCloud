import { describe, expect, it } from 'vitest';
import en from '../../../static/locales/en.json';
import { getNestedValue } from './index.svelte';

// #925: 178 keys used as `t('…')` in code were missing from en.json, so no
// locale could translate them and they always rendered the inline English
// fallback. `scripts/check-locales.mjs` only compares the other locales to
// en.json and stayed green. This pin closes that hole from the code side:
// every literal key passed to t() must resolve to a string in en.json (an
// object there — `admin.sessions` — is a miss too). Dynamic keys
// (`t(\`admin.features.${key}\`)`) are out of reach of a static scan.
const sources = import.meta.glob(['/src/**/*.{svelte,ts}', '!/src/**/*.test.ts'], {
	query: '?raw',
	import: 'default',
	eager: true
}) as Record<string, string>;

function literalKeys(code: string): string[] {
	return [...code.matchAll(/(?<![\w.$])t\(\s*(['"`])([\w.-]+)\1/g)].map((m) => m[2]);
}

describe('i18n keys used in code', () => {
	it('scanner picks up literal t() calls only', () => {
		expect(
			literalKeys(`t('a.b') t("c.d", 'x') t(\`e.f\`) at('no') x.t('no') t(\`g.\${k}\`) t(key)`)
		).toEqual(['a.b', 'c.d', 'e.f']);
	});

	it('every literal t() key exists in en.json', () => {
		const missing = new Set<string>();
		for (const code of Object.values(sources))
			for (const key of literalKeys(code))
				if (typeof getNestedValue(en, key) !== 'string') missing.add(key);
		expect([...missing].sort()).toEqual([]);
	});
});
