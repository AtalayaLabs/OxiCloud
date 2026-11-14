import test from 'node:test';
import assert from 'node:assert/strict';
import { copyFileSync, mkdirSync, mkdtempSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { spawnSync } from 'node:child_process';

const repo = join(dirname(fileURLToPath(import.meta.url)), '..');
const gradient = 'linear-gradient(135deg, #ff5e3a 0%, #ff8a5c 100%)';

function fixture(run) {
    const root = mkdtempSync(join(tmpdir(), 'oxicloud-theme-tools-'));
    const write = (name, contents) => {
        mkdirSync(dirname(join(root, name)), { recursive: true });
        writeFileSync(join(root, name), contents);
    };
    try {
        write('frontend/src/lib/styles/base/variables.css',
            ':root { --structural-used: 1px; --structural-unused: 2px; }');
        write('frontend/src/app.css', '.example { gap: var(--structural-used); }');
        write('resources/themes/tokens.css', `:root {
            --visual-unused: blue;
            --visual-used: red;
            --visual-alias: var(--visual-used);
            --color-logo-gradient: ${gradient};
        }`);
        write('resources/themes/example/index.css', '.example { color: var(--visual-alias); }');
        mkdirSync(join(root, 'frontend/static/logo'), { recursive: true });
        copyFileSync(join(repo, 'frontend/static/logo/logo-plain.svg'),
            join(root, 'frontend/static/logo/logo-plain.svg'));
        mkdirSync(join(root, 'scripts'));
        for (const name of ['check-brand-drift.mjs', 'check-dead-tokens.mjs']) {
            copyFileSync(join(repo, 'scripts', name), join(root, 'scripts', name));
        }
        run({ root, write, invoke: (name) => spawnSync(process.execPath,
            [join(root, 'scripts', name)], { cwd: root, encoding: 'utf8' }) });
    } finally {
        rmSync(root, { recursive: true, force: true });
    }
}

test('brand guard follows extracted defaults and still rejects a changed gradient', () => {
    // Regression pin: moving the visual tokens made the guard report a missing
    // gradient even though the locked brand was unchanged. Keep both outcomes.
    fixture(({ write, invoke }) => {
        const unchanged = invoke('check-brand-drift.mjs');
        assert.ifError(unchanged.error);
        assert.equal(unchanged.status, 0, unchanged.stderr);
        write('resources/themes/tokens.css', ':root { --color-logo-gradient: none; }');
        const changed = invoke('check-brand-drift.mjs');
        assert.ifError(changed.error);
        assert.equal(changed.status, 1);
        assert.match(changed.stderr, /Logo gradient drift/);
    });
});

test('dead-token report includes visual defaults and references from installed themes', () => {
    // Regression pin: after extraction the report silently omitted visual tokens
    // and their aliases. Synthetic tokens keep the check independent of palettes.
    fixture(({ invoke }) => {
        const result = invoke('check-dead-tokens.mjs');
        assert.ifError(result.error);
        assert.equal(result.status, 0, result.stderr);
        const reported = result.stdout.split('\n').filter((line) => line.startsWith('  --'));
        assert.ok(reported.includes('  --visual-unused'));
        assert.ok(reported.includes('  --structural-unused'));
        assert.ok(!reported.includes('  --visual-used'));
        assert.ok(!reported.includes('  --visual-alias'));
        assert.ok(!reported.includes('  --structural-used'));
        assert.equal(new Set(reported).size, reported.length);
    });
});
