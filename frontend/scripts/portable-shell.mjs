/**
 * Post-build: make the shell and the PWA manifest prefix-independent.
 *
 * SvelteKit emits root-absolute URLs (`/_app/…`, `/logo/…`) in `index.html`
 * and the manifest ships root-absolute paths of its own. Both are rewritten
 * to relative here, so the `<base href>` the Rust server fills in from
 * `OXICLOUD_BASE_PATH` anchors them — one build serves any deployment prefix.
 *
 * The shell also gets a small script that hands SvelteKit's client runtime the
 * same prefix; its CSP hash is added to the shell's `script-src` here, since
 * SvelteKit computed that list before this script existed.
 *
 * Runs before precompress.mjs so the `.br`/`.gz` siblings carry these bytes.
 */
import { createHash } from 'node:crypto';
import { readFileSync, writeFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';

const dist = (name) => fileURLToPath(new URL(`../../static-dist/${name}`, import.meta.url));

// ── shell ────────────────────────────────────────────────────────────────
const shellPath = dist('index.html');
let shell = readFileSync(shellPath, 'utf-8');

// The <base> tag is the one URL that must stay root-absolute: it is the anchor
// everything else is relative to, and the server rewrites it per deployment.
const BASE_TAG = /<base href="\/"\s*\/?>/;
const baseTag = shell.match(BASE_TAG)?.[0];
if (!baseTag) {
	console.error('portable-shell: no <base href="/"> in index.html — see app.html');
	process.exit(1);
}
shell = shell.replace(BASE_TAG, '@@BASE@@');

const beforeLinks = shell;
shell = shell.replace(/(\s(?:href|src)=")\/(?!\/)/g, '$1');
// The bootstrap's dynamic imports are module specifiers, not URLs: they need
// an explicit `./` to count as relative rather than bare.
shell = shell.replace(/import\("\/(?!\/)/g, 'import("./');
if (shell === beforeLinks) {
	console.error('portable-shell: no root-absolute URLs in index.html — did the build change?');
	process.exit(1);
}

// SvelteKit's client reads its base from this global; the object is created by
// the bootstrap script above us and consumed when the entry module evaluates,
// which happens after this synchronous script has run.
const globalName = shell.match(/__sveltekit_[a-z0-9]+/)?.[0];
if (!globalName) {
	console.error('portable-shell: no __sveltekit_* bootstrap global found in index.html');
	process.exit(1);
}
const baseScript =
	`const p=document.baseURI.replace(location.origin,"").replace(/\\/$/,"");` +
	`globalThis.${globalName}.base=p;globalThis.${globalName}.assets=p;`;
const hash = `sha256-${createHash('sha256').update(baseScript, 'utf-8').digest('base64')}`;

shell = shell.replace('</body>', `\t<script>${baseScript}</script>\n\t</body>`);
if (!shell.includes(baseScript)) {
	console.error('portable-shell: no </body> to append the base script to');
	process.exit(1);
}

// Widen the shell's own CSP meta; the Rust server derives the response header
// from the shell's inline scripts at boot, so that side needs no edit.
const cspBefore = shell;
shell = shell.replace(
	/(<meta http-equiv="content-security-policy" content="[^"]*?script-src [^;"]*)/i,
	`$1 '${hash}'`
);
if (shell === cspBefore) {
	console.error('portable-shell: no script-src in the shell CSP meta to extend');
	process.exit(1);
}

shell = shell.replace('@@BASE@@', baseTag);
writeFileSync(shellPath, shell);

// ── manifest ─────────────────────────────────────────────────────────────
// Manifest members resolve against the manifest's own URL, so relative paths
// follow the deployment prefix without any rewriting at serve time.
const manifestPath = dist('manifest.webmanifest');
const manifest = JSON.parse(readFileSync(manifestPath, 'utf-8'));
const relative = (p) => (p === '/' ? './' : p.replace(/^\//, ''));
manifest.start_url = relative(manifest.start_url ?? '/');
manifest.scope = relative(manifest.scope ?? '/');
for (const icon of manifest.icons ?? []) {
	icon.src = relative(icon.src);
}
writeFileSync(manifestPath, JSON.stringify(manifest, null, '\t') + '\n');

console.log('portable-shell: shell + manifest are prefix-independent');
