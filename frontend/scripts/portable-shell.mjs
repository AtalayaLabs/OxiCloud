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

/**
 * The document as the browser will parse it: the server drops HTML comments
 * from the shell when it serves it, and one of `app.html`'s comments *mentions*
 * `<script>` — scanning the raw file pairs that mention with the next
 * `</script>` and hashes the wrong span.
 */
function withoutComments(html) {
	let out = '';
	let rest = html;
	for (;;) {
		const comment = rest.indexOf('<!--');
		const verbatim = ['<script', '<style']
			.map((tag) => rest.indexOf(tag))
			.filter((i) => i !== -1)
			.sort((a, b) => a - b)[0];
		if (comment !== -1 && (verbatim === undefined || comment < verbatim)) {
			out += rest.slice(0, comment);
			const end = rest.indexOf('-->', comment);
			if (end === -1) return out + rest.slice(comment);
			rest = rest.slice(end + 3);
		} else if (verbatim !== undefined) {
			const close = rest.startsWith('<script', verbatim) ? '</script>' : '</style>';
			const end = rest.indexOf(close, verbatim);
			if (end === -1) return out + rest;
			out += rest.slice(0, end + close.length);
			rest = rest.slice(end + close.length);
		} else {
			return out + rest;
		}
	}
}

const INLINE_SCRIPT = /<script(?![^>]*\ssrc=)[^>]*>([\s\S]*?)<\/script>/g;

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

shell = shell.replace('</body>', `\t<script>${baseScript}</script>\n\t</body>`);
if (!shell.includes(baseScript)) {
	console.error('portable-shell: no </body> to append the base script to');
	process.exit(1);
}

// Recompute the CSP hash list from the finished document. SvelteKit hashed the
// bootstrap at build time, and the rewrites above changed its body — appending
// only the new script's hash would leave that stale hash in place and the
// browser would block the bootstrap.
const inlineScripts = [...withoutComments(shell).matchAll(INLINE_SCRIPT)];
if (inlineScripts.length === 0) {
	console.error('portable-shell: no inline scripts found to hash');
	process.exit(1);
}
const hashes = inlineScripts
	.map((m) => `'sha256-${createHash('sha256').update(m[1], 'utf-8').digest('base64')}'`)
	.join(' ');

const cspBefore = shell;
shell = shell.replace(
	/(<meta http-equiv="content-security-policy" content="[^"]*?script-src )([^;"]*)/i,
	(_full, head, sources) =>
		`${head}${sources
			.replace(/'sha256-[^']*'/g, '')
			.replace(/\s+/g, ' ')
			.trim()} ${hashes}`
);
if (shell === cspBefore) {
	console.error('portable-shell: no script-src in the shell CSP meta to rewrite');
	process.exit(1);
}

// Self-check: every inline script the shell ships must be allowed by the
// policy it ships with, or the SPA boots into a blank page behind the splash.
for (const [, body] of withoutComments(shell).matchAll(INLINE_SCRIPT)) {
	const hash = createHash('sha256').update(body, 'utf-8').digest('base64');
	if (!shell.includes(`'sha256-${hash}'`)) {
		console.error(`portable-shell: inline script not covered by the CSP meta (sha256-${hash})`);
		process.exit(1);
	}
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
