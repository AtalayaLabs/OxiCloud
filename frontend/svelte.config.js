import adapter from '@sveltejs/adapter-static';
import { vitePreprocess } from '@sveltejs/vite-plugin-svelte';
import { stripTestId } from './strip-testid.js';
import { createHash } from 'node:crypto';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { parse } from 'parse5';

// SvelteKit's `kit.csp` hash mode only hashes the inline scripts SvelteKit
// itself generates (its bootstrap) — NOT inline scripts authored in app.html.
// So we compute the SHA-256 of our anti-FOUC theme-init <script> here, at
// config load, and feed it into `script-src`. This keeps the script INLINE
// (zero extra request) while auto-managing its hash: edit the script and the
// hash regenerates on the next build — the CSP can't drift out of step.
//
// We parse app.html with parse5 (WHATWG-compliant) and target the <script> by
// id, so the lookup is exact. parse5 returns a raw-text element's content
// byte-for-byte, and SvelteKit emits app.html verbatim (only %sveltekit.*%
// substitution, no minification of the shell), so the bytes we hash equal what
// the browser parses. Throws if the id is missing — failing the build loudly
// rather than shipping a CSP that silently blocks the script.
function inlineScriptHash(htmlPath, id) {
	const stack = [parse(readFileSync(htmlPath, 'utf-8'))];
	while (stack.length) {
		const node = stack.pop();
		for (const child of node.childNodes ?? []) stack.push(child);
		if (node.tagName === 'script' && node.attrs?.some((a) => a.name === 'id' && a.value === id)) {
			const body = (node.childNodes ?? []).map((c) => c.value ?? '').join('');
			return `sha256-${createHash('sha256').update(body, 'utf8').digest('base64')}`;
		}
	}
	throw new Error(`svelte.config.js: no inline <script id="${id}"> found in ${htmlPath}`);
}

const themeInitHash = inlineScriptHash(
	fileURLToPath(new URL('./src/app.html', import.meta.url)),
	'theme-init'
);

/**
 * SvelteKit config — pure SPA via adapter-static.
 *
 * Phase 0: output to the local `build/` dir so the existing `static-dist/`
 * (still produced by build.rs) is untouched. At cutover (Phase 5) the
 * `pages`/`assets` targets switch to `../static-dist` and build.rs stops
 * generating assets.
 *
 * `fallback: 'index.html'` makes every unmatched client route serve the SPA
 * shell, which the Rust web layer will mirror with a ServeFile fallback.
 *
 * @type {import('@sveltejs/kit').Config}
 */
const config = {
	// `stripTestId` removes `data-testid` attributes from production builds; it
	// runs after vitePreprocess and only scans markup outside <script>/<style>.
	preprocess: [vitePreprocess(), stripTestId()],
	kit: {
		adapter: adapter({
			// Cutover: emit the SPA into the repo-root `static-dist/` that the Rust
			// web layer serves in release. build.rs no longer generates this dir
			// (gated behind OXICLOUD_LEGACY_ASSETS for rollback).
			pages: '../static-dist',
			assets: '../static-dist',
			fallback: 'index.html',
			precompress: false,
			strict: true
		}),
		// All routes are client-rendered; SSR/prerender are disabled in +layout.ts.
		alias: {
			$lib: './src/lib'
		},
		// The build carries no deployment prefix. `relative: true` keeps the
		// lazy-chunk URLs inside the bundles relative to the importing module,
		// so they resolve at any prefix; the shell's own URLs are made relative
		// by scripts/portable-shell.mjs and anchored by the <base href> the
		// server fills in from OXICLOUD_BASE_PATH (see app.html).
		paths: {
			base: '',
			relative: true
		},
		// The app registers the worker itself (see +layout.svelte): it needs the
		// runtime prefix for both the script URL and the scope, which SvelteKit's
		// own root-absolute `/service-worker.js` registration cannot carry.
		serviceWorker: {
			register: false
		},
		// Poll `_app/version.json` so an open tab notices a fresh deploy; the root
		// layout reloads itself when it does, instead of silently running stale
		// code after a rebuild (the classic "my fix isn't applied" trap). Five
		// minutes — the SvelteKit default is 60 s, which is noisy on access logs
		// for the "99% of tabs will never see a deploy during their lifetime"
		// case; a 5-minute lag before an open tab notices a rebuild is fine
		// for an operator-owned self-hosted deployment.
		version: {
			pollInterval: 300000
		},
		// Content-Security-Policy for the SPA document.
		//
		// The Rust server deliberately does NOT send a CSP *header* on text/html
		// responses (see `content_security_policy` in src/main.rs); this <meta>
		// policy is the sole, strict authority for the app shell. `mode: 'hash'`
		// auto-emits the SHA-256 of SvelteKit's inline bootstrap; `themeInitHash`
		// (computed above from app.html) covers the inline theme-init script. Net
		// result: strict script-src with no `'unsafe-inline'`. Directives mirror
		// the server's header policy for every other response. (The shell has no
		// inline <style>, so `style-src 'unsafe-inline'` stays effective for
		// runtime element.style.)
		csp: {
			mode: 'hash',
			directives: {
				'default-src': ['self'],
				// `'wasm-unsafe-eval'` (CSP Level 3) permits WebAssembly.compile()
				// without allowing eval() for JavaScript. Required for the OPAQUE
				// aPAKE client (`@serenity-kit/opaque`), lazy-loaded by the login
				// path only when `OXICLOUD_OPAQUE_MODE != off`. Mirrors the Rust
				// server's `content_security_policy` in `src/interfaces/web/mod.rs`
				// so headers + this meta tag agree on the same posture.
				'script-src': ['self', 'wasm-unsafe-eval', themeInitHash],
				'worker-src': ['self', 'blob:'],
				'style-src': ['self', 'unsafe-inline'],
				'img-src': ['self', 'data:', 'blob:', 'https:'],
				'media-src': ['self', 'blob:'],
				'connect-src': ['self'],
				'font-src': ['self', 'data:'],
				// `'self'` for the WOPI host page (`/wopi/edit/{id}`) and `blob:` for
				// the PDF viewer's object URL — the only two things the SPA frames.
				//
				// This was `'*'`, i.e. any origin, because the WOPI modal used to
				// iframe the admin-configured editor host directly. The editor now
				// loads in a nested iframe INSIDE the host page, governed by that
				// page's own CSP (which names the one origin, resolved from the
				// discovery document's `urlsrc` at request time). So the SPA no
				// longer frames anything cross-origin and this is build-time
				// knowable. Do not re-add a wildcard: a cross-origin editor belongs
				// in the host page's policy, not this one.
				'frame-src': ['self', 'blob:'],
				'frame-ancestors': ['none'],
				'base-uri': ['self'],
				// `'self'` only. This used to carry `https:` so the WOPI modal could
				// POST its hidden token form straight to the editor's action URL — a
				// cross-origin, admin-configured Collabora/OnlyOffice host that this
				// build cannot know, so a whole SCHEME was the narrowest expressible
				// rule. Two costs: ANY https origin could receive a token-bearing
				// form, and a CLEARTEXT editor (loopback, private network) was
				// excluded outright, with nothing but a console CSP error to say why.
				//
				// The handshake now happens in `GET /wopi/edit/{id}` — a
				// server-rendered page that emits its own CSP naming the one real
				// editor origin, resolved from OXICLOUD_WOPI_DISCOVERY_URL at request
				// time. Nothing in the SPA posts cross-origin any more, so this is
				// back to `'self'`. Do not re-add a scheme here: if an editor fails to
				// load, the origin belongs in the host page's policy, not this one.
				'form-action': ['self']
			}
		}
	}
};

export default config;
