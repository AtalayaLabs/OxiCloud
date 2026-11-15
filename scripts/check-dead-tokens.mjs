#!/usr/bin/env node
// Dead-token report (informational, exit 0): design tokens defined in
// the built-in token sources but never referenced via var() in the app or themes.
//
// NOTE: a cleanup AID, not a hard gate — some tokens (file-type / calendar
// colours) are referenced by JS string construction, so excluded prefixes are
// skipped to avoid false positives. Verify before pruning.
//
//   node scripts/check-dead-tokens.mjs
import { readFileSync, readdirSync, statSync } from 'node:fs';
import { join, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = join(dirname(fileURLToPath(import.meta.url)), '..', 'frontend', 'src');
const EXCLUDE_PREFIX = ['--color-ft-', '--color-cal-']; // referenced dynamically from JS

const themes = join(root, '..', '..', 'resources', 'themes');
// Theme extraction must not hide visual defaults or their alias references
// from this report. Structural tokens still live in variables.css.
const varsSrc = [join(root, 'lib', 'styles', 'base', 'variables.css'), join(themes, 'tokens.css')]
    .map((file) => readFileSync(file, 'utf8')).join('\n');
const defined = [...new Set([...varsSrc.matchAll(/(--[a-z0-9-]+)\s*:/gi)].map((m) => m[1]))];

function walk(dir, files = []) {
    for (const name of readdirSync(dir)) {
        const p = join(dir, name);
        if (statSync(p).isDirectory()) walk(p, files);
        else if (/\.(css|svelte|ts|js|html|webmanifest)$/.test(name)) files.push(p);
    }
    return files;
}
let corpus = '';
for (const f of [...walk(root), ...walk(themes)]) corpus += readFileSync(f, 'utf8');
const used = new Set([...corpus.matchAll(/var\(\s*(--[a-z0-9-]+)/gi)].map((m) => m[1]));

const dead = defined
    .filter((t) => !used.has(t))
    .filter((t) => !EXCLUDE_PREFIX.some((p) => t.startsWith(p)))
    .sort();

if (dead.length) {
    console.log(`Dead-token candidates (defined, never referenced via var()): ${dead.length}`);
    console.log('  ' + dead.join('\n  '));
    console.log('\n(Verify each before pruning — some may be referenced dynamically.)');
} else {
    console.log('✓ No dead-token candidates.');
}
