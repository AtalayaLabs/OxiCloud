#!/usr/bin/env node
// Bulk-translate missing locale keys via Lingva (public Google-Translate
// proxy, no auth required). Pairs with `scripts/check-locales.mjs`:
// check-locales detects the gap, this script fills it.
//
//   node scripts/bulk_translate.mjs
//   node scripts/check-locales.mjs   # verify
//
// Idempotent — only translates keys missing in each locale, AND
// retries any "suspicious" keys where the stored translation equals
// the English source (Lingva passing strings through unchanged).
//
// Design notes:
// - Sentinels: `{{placeholder}}` tokens are frozen as private-use
//   unicode chars (U+E000 + index). MT engines pass them through
//   unchanged because no language model maps them, surviving any
//   word reordering the engine performs.
// - Checkpointing: writes every 50 keys so a network blip loses at
//   most the last batch instead of the whole locale.
// - No English fallback on failure: failed keys are left absent so
//   a re-run picks them up (no silent English bleed).
// - `LINGVA_HOST=https://lingva.example.com node scripts/bulk_translate.mjs`
//   lets you point at a self-hosted mirror if the public one goes
//   down. (Lingva.ml has occasional 502s; the script retries with
//   exponential backoff.)

import { readFileSync, writeFileSync, readdirSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const LOCALES_DIR = join(
    dirname(fileURLToPath(import.meta.url)),
    '..',
    'frontend',
    'static',
    'locales'
);
const LINGVA = process.env.LINGVA_HOST || 'https://lingva.ml';
const THROTTLE_MS = 150;
const RETRIES = 4;
const RETRY_BACKOFF_MS = 800;

const LOCALE_TO_LINGVA = {
    ar: 'ar',
    de: 'de',
    es: 'es',
    fa: 'fa',
    fr: 'fr',
    hi: 'hi',
    it: 'it',
    ja: 'ja',
    ko: 'ko',
    nl: 'nl',
    pl: 'pl',
    pt: 'pt',
    ru: 'ru',
    zh: 'zh',
    'zh-TW': 'zh_HANT'
};
const SKIP = new Set(['en']);

function flat(obj, prefix = '', out = {}) {
    for (const [k, v] of Object.entries(obj)) {
        const key = prefix ? `${prefix}.${k}` : k;
        if (v && typeof v === 'object' && !Array.isArray(v)) flat(v, key, out);
        else out[key] = v;
    }
    return out;
}

function setNested(obj, dottedKey, value) {
    const parts = dottedKey.split('.');
    let cur = obj;
    for (let i = 0; i < parts.length - 1; i++) {
        const k = parts[i];
        if (cur[k] === undefined || typeof cur[k] !== 'object' || Array.isArray(cur[k])) {
            cur[k] = {};
        }
        cur = cur[k];
    }
    cur[parts[parts.length - 1]] = value;
}

// Private-use unicode sentinels: U+E000 + index encoded as code point.
// MT engines pass them through unchanged because they have no glyphs and
// no language model maps them. Each placeholder gets its own char so
// reordering still works (we restore by char, not by position).
function freezePlaceholders(text) {
    const placeholders = [];
    const frozen = text.replace(/\{\{([^}]+)\}\}/g, (_, name) => {
        const idx = placeholders.length;
        placeholders.push(name);
        return String.fromCodePoint(0xe000 + idx);
    });
    return { frozen, placeholders };
}
function thawPlaceholders(text, placeholders) {
    // Iterate over code points so multi-byte chars match correctly.
    let out = '';
    for (const ch of text) {
        const cp = ch.codePointAt(0);
        if (cp >= 0xe000 && cp < 0xe000 + placeholders.length) {
            out += `{{${placeholders[cp - 0xe000]}}}`;
        } else {
            out += ch;
        }
    }
    return out;
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

async function translate(text, targetLang) {
    const { frozen, placeholders } = freezePlaceholders(text);
    const url = `${LINGVA}/api/v1/en/${targetLang}/${encodeURIComponent(frozen)}`;
    let lastErr;
    for (let attempt = 0; attempt < RETRIES; attempt++) {
        try {
            const res = await fetch(url, {
                headers: { 'User-Agent': 'oxicloud-i18n-fill' }
            });
            if (!res.ok) throw new Error(`HTTP ${res.status}`);
            const json = await res.json();
            const out = json.translation;
            if (!out || typeof out !== 'string') throw new Error('no translation field');
            return thawPlaceholders(out, placeholders);
        } catch (e) {
            lastErr = e;
            await sleep(RETRY_BACKOFF_MS * (attempt + 1));
        }
    }
    throw lastErr;
}

// Heuristic: a translation looks suspicious if it equals the source text
// AND the source has at least one letter. Single-word brand terms
// ("OxiCloud") are excluded — those legitimately stay identical.
function looksUntranslated(src, translated) {
    if (typeof src !== 'string' || typeof translated !== 'string') return false;
    if (src !== translated) return false;
    if (!/[a-zA-Z]/.test(src)) return false;
    // Single CamelCase / lowercase token without spaces — likely a brand.
    if (!/\s/.test(src)) return false;
    return true;
}

const en = flat(JSON.parse(readFileSync(join(LOCALES_DIR, 'en.json'), 'utf8')));
const enKeys = Object.keys(en);

const localeFiles = readdirSync(LOCALES_DIR)
    .filter((f) => f.endsWith('.json'))
    .map((f) => f.slice(0, -5))
    .filter((l) => !SKIP.has(l));

const failures = {}; // locale -> array of {key, src, err}

for (const locale of localeFiles) {
    const lingvaCode = LOCALE_TO_LINGVA[locale];
    if (!lingvaCode) {
        console.error(`! no Lingva mapping for ${locale}, skipping`);
        continue;
    }
    const path = join(LOCALES_DIR, `${locale}.json`);
    const dict = JSON.parse(readFileSync(path, 'utf8'));
    const dictFlat = flat(dict);

    // Phase A: fill missing keys.
    const missing = enKeys.filter((k) => !(k in dictFlat));
    // Phase B: retry suspicious (en === locale) translations.
    const suspicious = enKeys.filter((k) => k in dictFlat && looksUntranslated(en[k], dictFlat[k]));

    const todo = [...missing, ...suspicious];
    if (todo.length === 0) {
        console.log(`✓ ${locale}: clean (missing 0, suspicious 0)`);
        continue;
    }
    console.log(
        `▶ ${locale}: ${missing.length} missing + ${suspicious.length} suspicious via Lingva (${lingvaCode})`
    );

    failures[locale] = [];
    let done = 0;
    let writeCheckpoint = 0;
    for (const k of todo) {
        const src = en[k];
        if (typeof src !== 'string') {
            done++;
            continue;
        }
        try {
            const translated = await translate(src, lingvaCode);
            // Re-check: did Lingva just hand back the English unchanged?
            if (looksUntranslated(src, translated)) {
                failures[locale].push({ key: k, src, err: 'identical_to_source' });
            }
            setNested(dict, k, translated);
            done++;
        } catch (e) {
            failures[locale].push({ key: k, src, err: e.message });
            // Don't write English fallback — leave the key absent so the
            // next run picks it up. (For suspicious-retries, the existing
            // suspect value stays in place.)
            done++;
        }

        // Persist progress every 50 keys so a network blip doesn't lose
        // everything translated so far.
        if (done - writeCheckpoint >= 50) {
            writeFileSync(path, JSON.stringify(dict, null, 2) + '\n');
            writeCheckpoint = done;
            console.log(`  ${locale}: ${done}/${todo.length} (checkpoint)`);
        }
        await sleep(THROTTLE_MS);
    }
    writeFileSync(path, JSON.stringify(dict, null, 2) + '\n');
    console.log(
        `✓ ${locale}: ${done}/${todo.length} processed, ${failures[locale].length} unresolved`
    );
}

const totalUnresolved = Object.values(failures).reduce((n, arr) => n + arr.length, 0);
if (totalUnresolved > 0) {
    console.log(`\nUnresolved: ${totalUnresolved}`);
    for (const [loc, arr] of Object.entries(failures)) {
        if (arr.length === 0) continue;
        console.log(`  ${loc}: ${arr.length}`);
        for (const f of arr.slice(0, 5)) {
            console.log(`    ${f.key} (${f.err}): "${f.src.slice(0, 50)}"`);
        }
        if (arr.length > 5) console.log(`    … +${arr.length - 5} more`);
    }
}
console.log('\nDone.');
