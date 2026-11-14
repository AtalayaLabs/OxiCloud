#!/usr/bin/env node
// check-brand-drift.mjs — guard the brand mark against silent change.
//
// Fails (exit 1) if the canonical logo SVG or the logo gradient token drift
// from the locked baseline below. The brand is an ownable asset; an accidental
// recolour, a stretched glyph, or a "tweaked" gradient should never sneak in
// via an unrelated PR. If a change IS intentional, update LOCK deliberately
// (with design sign-off) — that diff is the audit trail.
//
//   node scripts/check-brand-drift.mjs
import { readFileSync } from 'node:fs';
import { createHash } from 'node:crypto';
import { join, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';

const repo = join(dirname(fileURLToPath(import.meta.url)), '..');
const sha16 = (s) => createHash('sha256').update(s).digest('hex').slice(0, 16);

// ── Locked baseline ─────────────────────────────────────────────────────────
// Update INTENTIONALLY when the brand changes (and only then).
const LOCK = {
    logoHash: '7fbd2016e9caeac1', // sha256(frontend/static/logo/logo-plain.svg)[:16]
    gradient: 'linear-gradient(135deg, #ff5e3a 0%, #ff8a5c 100%)' // --color-logo-gradient
};

const logo = readFileSync(join(repo, 'frontend/static/logo/logo-plain.svg'), 'utf8');
// Regression pin: extracting themes moved the gradient out of variables.css.
// Read the canonical defaults so the existing lock still checks the same brand.
const vars = readFileSync(join(repo, 'resources/themes/tokens.css'), 'utf8');
const gradMatch = vars.match(/--color-logo-gradient:\s*([^;]+);/);
const gradient = gradMatch ? gradMatch[1].trim().replace(/\s+/g, ' ') : '(token missing!)';

let failed = false;
const actualLogoHash = sha16(logo);
if (actualLogoHash !== LOCK.logoHash) {
    console.error(`✖ Brand mark drift: logo-plain.svg hash ${actualLogoHash} ≠ locked ${LOCK.logoHash}`);
    failed = true;
}
if (gradient !== LOCK.gradient) {
    console.error(`✖ Logo gradient drift:\n    actual: ${gradient}\n    locked: ${LOCK.gradient}`);
    failed = true;
}

if (failed) {
    console.error(
        '\nThe brand mark or logo gradient changed. If this is intentional, update' +
            '\nLOCK in scripts/check-brand-drift.mjs (with design sign-off).'
    );
    process.exit(1);
}
console.log('✓ Brand mark + logo gradient match the locked baseline.');
