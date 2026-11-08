import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import { describe, it, expect } from 'vitest';
import { render, screen } from '@testing-library/svelte';
import AppSplash from './AppSplash.svelte';

describe('AppSplash', () => {
	it('is a labelled status region with a decorative spinner', () => {
		render(AppSplash, { label: 'Signing in', testid: 'login-redirect-loader' });

		const el = screen.getByTestId('login-redirect-loader');
		expect(el.getAttribute('role')).toBe('status');
		// The label describes what is being waited on; the ring itself is
		// `aria-hidden` so a screen reader announces the wait, not a div.
		expect(el.getAttribute('aria-label')).toBe('Signing in');
		const ring = el.querySelector('.app-splash__spinner');
		expect(ring?.getAttribute('aria-hidden')).toBe('true');
	});

	it('defaults the label rather than leaving the region unnamed', () => {
		render(AppSplash, { testid: 'app-boot-loader' });
		expect(screen.getByTestId('app-boot-loader').getAttribute('aria-label')).toBe('Loading');
	});

	// Drift guard, same shape as the `oxi-theme` one in `stores/theme.test.ts`.
	//
	// `src/app.html` inlines a copy of this spinner: it has to paint on HTML
	// parse, before the CSS bundle exists, so it cannot import this component
	// or read a custom property — its values are literals. A cold boot shows
	// the inline copy, then this component while the session probe runs, so
	// any difference in size or cadence reads as a flicker exactly at the
	// handover. These are the numbers that have to agree; the colours cannot
	// be compared mechanically (tokens on one side, literals on the other)
	// and are covered by the comments in both files.
	it('matches the inline boot splash in app.html', () => {
		// Resolved against Vitest's cwd (`frontend/`) — jsdom rewrites
		// `import.meta.url` to an http: URL, so file-URL conversion is out.
		const html = readFileSync(resolve('src/app.html'), 'utf-8');
		const component = readFileSync(resolve('src/lib/components/AppSplash.svelte'), 'utf-8');

		for (const decl of [
			'width: 38px',
			'height: 38px',
			'border-radius: 50%',
			'animation: spin 0.7s linear infinite'
		]) {
			// app.html names its keyframe `app-splash-spin`; the bundled copy
			// uses the shared `spin` from `base/animations.css`.
			const inHtml = decl.replace('spin 0.7s', 'app-splash-spin 0.7s');
			expect(
				html.includes(inHtml),
				`app.html's #app-splash__spinner must declare \`${inHtml}\` — ` +
					`keep it in step with AppSplash.svelte.`
			).toBe(true);
			expect(
				component.includes(decl),
				`AppSplash.svelte must declare \`${decl}\` — keep it in step with app.html.`
			).toBe(true);
		}

		// Both sit at the same stacking level and cover the viewport, or the
		// second one would paint behind what the first covered.
		for (const [name, src] of [
			['app.html', html],
			['AppSplash.svelte', component]
		] as const) {
			expect(src.includes('position: fixed'), `${name}: splash must be viewport-fixed`).toBe(true);
			expect(src.includes('inset: 0'), `${name}: splash must cover the viewport`).toBe(true);
			expect(src.includes('z-index: 9999'), `${name}: splash must sit above the app`).toBe(true);
			expect(src.includes('place-items: center'), `${name}: spinner must be centred`).toBe(true);
		}

		// And both stop turning for a viewer who asked for less motion — a
		// boot screen can stay up for seconds.
		expect(html.includes('prefers-reduced-motion')).toBe(true);
		expect(component.includes('prefers-reduced-motion')).toBe(true);
	});
});

// The no-JS twin of the splash. With scripting off, nothing ever clears
// `#app-splash` — the app mounting is the only thing that does — so the
// shell paints an explanation over it instead of spinning forever.
describe('app.html noscript fallback', () => {
	const html = readFileSync(resolve('src/app.html'), 'utf-8');

	// THE load-bearing property: the message is inside <noscript>, so it does
	// not exist in the DOM when scripting works. Moved out of that element —
	// or gated on `@media (scripting: none)` alone, which not every supported
	// browser honours — and a merely slow boot would accuse the visitor of
	// having JavaScript disabled.
	it('keeps the enable-JavaScript message inside <noscript>', () => {
		expect(html).toMatch(/<noscript>[\s\S]*id="app-noscript"[\s\S]*<\/noscript>/);
	});

	// `both` is the part that matters: it holds opacity 0 through the delay.
	// Without it the message paints immediately, then fades in again — the
	// animation's own start value does not apply before it begins. The 0.5s
	// delay itself is only a beat so the spinner registers before the swap;
	// the <noscript> element, not the timing, is what keeps this away from a
	// visitor whose JavaScript is merely still loading.
	it('holds the message hidden through its entry delay', () => {
		expect(html).toMatch(/#app-noscript \{[\s\S]*?animation: app-noscript-in [^;]*\bboth;/);
		// Reduced motion shows it at once rather than animating.
		expect(html).toMatch(/prefers-reduced-motion[\s\S]{0,120}#app-noscript \{\s*animation: none;/);
	});

	// The logo path is inlined at a dozen sites across the SPA and the Askama
	// templates, and those copies have ALREADY drifted from the asset they
	// depict (different viewBox; the asset carries a background circle). This
	// layer renders only when scripting is off, so it can afford the one
	// cached request and reference the asset instead of becoming copy
	// thirteen.
	it('references the logo asset rather than inlining its path', () => {
		expect(html).toMatch(/id="app-noscript__logo"[\s\S]{0,200}logo\/logo-plain\.svg/);
		// `%sveltekit.assets%` so it still resolves under a deployment prefix,
		// the same way the <link rel="icon"> tags above it do.
		expect(html).toContain('src="%sveltekit.assets%/logo/logo-plain.svg"');
	});
});
