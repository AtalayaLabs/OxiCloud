// @vitest-environment node
import { expect, it } from 'vitest';

// Regression pin: the Places tab sat on an infinite spinner because this
// policy reverted to `worker-src 'self'` and MapLibre spawns its worker from
// a blob: URL, so the map never reached `load`. The server CSP header cannot
// grant blob: back to the document — only this meta policy can.
it('SPA CSP allows the blob worker MapLibre spawns', async () => {
	// Runtime-only specifier keeps svelte.config.js out of the svelte-check
	// program — it and strip-testid.js are unchecked plain-JS build files.
	const { default: config } = await import(
		/* @vite-ignore */ new URL('../svelte.config.js', import.meta.url).href
	);
	expect(config.kit.csp.directives['worker-src']).toContain('blob:');
});
