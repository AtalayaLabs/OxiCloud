#!/usr/bin/env node
// Locale completeness + placeholder-integrity check.
//
// Fails (exit 1) if any locale has stray extra keys, or if a translated
// string's {placeholders} differ from the English source. Keys missing from a
// locale are untranslated (t() falls back to English) and only counted.
// Wire into CI / pre-commit alongside the other linters.
//
//   node scripts/check-locales.mjs
import { readFileSync, readdirSync } from 'node:fs';
import { join, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';

const localesDir = join(
    dirname(fileURLToPath(import.meta.url)),
    '..',
    'frontend',
    'static',
    'locales'
);

/** Flatten a nested translation object to dotted keys. */
function flat(obj, prefix = '', out = {}) {
    for (const [k, v] of Object.entries(obj)) {
        const key = prefix ? `${prefix}.${k}` : k;
        if (v && typeof v === 'object' && !Array.isArray(v)) flat(v, key, out);
        else out[key] = v;
    }
    return out;
}

const placeholders = (s) => (typeof s === 'string' ? (s.match(/\{[^}]+\}/g) || []).sort() : []);

const en = flat(JSON.parse(readFileSync(join(localesDir, 'en.json'), 'utf8')));
const enKeys = Object.keys(en);
let problems = 0;

for (const file of readdirSync(localesDir).filter((f) => f.endsWith('.json') && f !== 'en.json')) {
    const loc = flat(JSON.parse(readFileSync(join(localesDir, file), 'utf8')));
    const missing = enKeys.filter((k) => !(k in loc));
    const extra = Object.keys(loc).filter((k) => !(k in en));
    const badPh = enKeys.filter(
        (k) => k in loc && placeholders(en[k]).join() !== placeholders(loc[k]).join()
    );
    if (missing.length) console.log(`${file}: ${missing.length} untranslated`);
    if (extra.length || badPh.length) {
        problems++;
        console.error(`\n${file}:`);
        if (extra.length) console.error(`  extra ${extra.length}: ${extra.slice(0, 42).join(', ')}`);
        if (badPh.length)
            console.error(`  placeholder mismatch ${badPh.length}: ${badPh.slice(0, 42).join(', ')}`);
    }
}

if (problems) {
    console.error(`\n✖ ${problems} locale file(s) have issues.`);
    process.exit(1);
}
console.log('✓ All locales placeholder-consistent, no stray keys.');
