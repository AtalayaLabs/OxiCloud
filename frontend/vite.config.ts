import { sveltekit } from '@sveltejs/kit/vite';
import { defineConfig } from 'vitest/config';
import istanbul from 'vite-plugin-istanbul';
import { svelteTesting } from '@testing-library/svelte/vite';

// `static-dist/askama-common.css` is emitted by `scripts/emit-askama-common.mjs`,
// wired into `package.json` as a `postbuild` step. That runs AFTER
// `@sveltejs/adapter-static` finalises `static-dist/`, avoiding the
// wipe-and-copy race that would eat any file a `writeBundle` hook wrote
// during the Vite build phase. See the script header for the pipeline
// rationale and the single-source-of-truth invariant it preserves.

// Backend dev server (cargo run) — the Vite dev server proxies API/protocol
// traffic here so cookies, CSRF, and the auth-refresh flow are same-origin.
const BACKEND = process.env.OXICLOUD_BACKEND ?? 'http://localhost:8086';

// When COVERAGE=1, instrument the app source with Istanbul so Playwright e2e
// runs can read `window.__coverage__` and report SvelteKit code coverage. Off
// by default so normal dev/release builds carry no instrumentation overhead.
const COVERAGE = process.env.COVERAGE === '1';

// `changeOrigin: true` rewrites the `Host` header to match the backend's
// authority (localhost:8086) so the backend answers as if the request
// arrived natively. But DPoP's `htu` claim is bound to what the browser
// sees (localhost:5173) — a bare rewrite makes the server compute
// `htu = http://localhost:8086/api/…` and fire `dpop.verify_failed
// reason=wrong_htu` on every request. Set `X-Forwarded-*` so the DPoP
// middleware (which mirrors production reverse-proxy behaviour) can
// reconstruct the browser-visible URL. Same reasoning that applies to
// nginx/Cloudflare in front of the deployment applies to Vite in dev.
const DEV_ORIGIN_HEADERS = {
	'X-Forwarded-Proto': 'http',
	'X-Forwarded-Host': 'localhost:5173'
};
const p = (target: string) => ({ target, changeOrigin: true, headers: DEV_ORIGIN_HEADERS });

const proxy = {
	'/api': p(BACKEND),
	'/locales': p(BACKEND),
	'/.well-known': p(BACKEND),
	'/remote.php': p(BACKEND),
	'/ocs': p(BACKEND),
	'/status.php': p(BACKEND),
	'/webdav': p(BACKEND),
	'/caldav': p(BACKEND),
	'/carddav': p(BACKEND),
	'/wopi': p(BACKEND),
	'/magic': p(BACKEND)
};

export default defineConfig({
	plugins: [
		sveltekit(),
		// Compiles Svelte components in client mode for Vitest component tests
		// (so onMount etc. run); a no-op outside the test runner.
		svelteTesting(),
		...(COVERAGE
			? [
					istanbul({
						include: 'src/**/*.{ts,svelte}',
						exclude: ['node_modules', 'src/**/*.{test,spec}.{js,ts}'],
						extension: ['.ts', '.svelte'],
						requireEnv: false,
						forceBuildInstrument: true
					})
				]
			: [])
	],
	server: {
		port: 5173,
		proxy
	},
	test: {
		environment: 'jsdom',
		// vitest-coverage.ts is a no-op unless COVERAGE=1; it collects Istanbul
		// coverage into tests/e2e/.nyc_output_unit for the combined report.
		setupFiles: ['./vitest-setup.ts', './vitest-coverage.ts'],
		include: ['src/**/*.{test,spec}.{js,ts}'],
		globals: true
	}
});
