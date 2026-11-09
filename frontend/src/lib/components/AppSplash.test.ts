import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import { describe, it, expect } from 'vitest';
import { render, screen } from '@testing-library/svelte';
import AppSplash from './AppSplash.svelte';
import { BRAND_MARK_PATH } from '$lib/icons/brandMark';

describe('AppSplash', () => {
	it('is a labelled status region with a decorative loader mark', () => {
		render(AppSplash, { label: 'Signing in', testid: 'login-redirect-loader' });

		const el = screen.getByTestId('login-redirect-loader');
		expect(el.getAttribute('role')).toBe('status');
		// The label describes what is being waited on; the mark is
		// `aria-hidden` so a screen reader announces the wait, not the artwork.
		expect(el.getAttribute('aria-label')).toBe('Signing in');
		const mark = el.querySelector('svg.loader-mark');
		expect(mark?.getAttribute('aria-hidden')).toBe('true');
		// Inline, NOT an <img> at a static asset. That was the first cut and
		// it fails where it matters most: the server answers an unmatched path
		// with index.html (the SPA fallback), so a stale build or a proxy that
		// drops /logo/* hands the <img> a page of HTML instead of a 404 — and
		// with `alt=""` the boot screen renders as nothing at all.
		expect(el.querySelector('img')).toBeNull();
		// It draws the glyph itself, so a missing path can't go unnoticed.
		expect(mark?.querySelector('path')?.getAttribute('d')).toBe(BRAND_MARK_PATH);
	});

	it('defaults the label rather than leaving the region unnamed', () => {
		render(AppSplash, { testid: 'app-boot-loader' });
		expect(screen.getByTestId('app-boot-loader').getAttribute('aria-label')).toBe('Loading');
	});

	// Drift guard, same shape as the `oxi-theme` one in `stores/theme.test.ts`.
	//
	// `src/app.html` paints the first boot screen: on HTML parse, before the
	// CSS bundle exists, so it cannot import this component or read a custom
	// property. A cold boot shows it, then this component while the session
	// probe runs, so a difference at the handover reads as a flicker.
	//
	// The MOVING part can no longer drift — both render the same
	// `logo-loader.svg`, whose animation lives inside the file. What is still
	// two hand-written copies is the frame around it, so that is what this
	// pins. Colours cannot be compared mechanically (tokens on one side,
	// literals on the other) and stay documented in both files.
	it('matches the hand-written boot splash in app.html', () => {
		// Resolved against Vitest's cwd (`frontend/`) — jsdom rewrites
		// `import.meta.url` to an http: URL, so file-URL conversion is out.
		const html = readFileSync(resolve('src/app.html'), 'utf-8');
		const component = readFileSync(resolve('src/lib/components/AppSplash.svelte'), 'utf-8');
		const mark = readFileSync(resolve('src/lib/components/LoaderMark.svelte'), 'utf-8');

		// The frame: same stacking and placement in both, or the second screen
		// paints behind what the first covered, off-centre.
		for (const [name, src] of [
			['app.html', html],
			['AppSplash.svelte', component]
		] as const) {
			expect(src.includes('position: fixed'), `${name}: splash must be viewport-fixed`).toBe(true);
			expect(src.includes('inset: 0'), `${name}: splash must cover the viewport`).toBe(true);
			expect(src.includes('z-index: 9999'), `${name}: splash must sit above the app`).toBe(true);
			expect(src.includes('place-items: center'), `${name}: loader must be centred`).toBe(true);
		}

		// The mark: same geometry and cadence in the shell's copy as in the
		// component, or the handover from one screen to the other visibly
		// jumps. The shell cannot import either the component or the path
		// constant, which is the whole reason this comparison exists.
		expect(html, 'app.html must inline the same glyph path as BRAND_MARK_PATH').toContain(
			BRAND_MARK_PATH
		);
		for (const decl of [
			'r="150"',
			'pathLength="100"',
			'transform="rotate(-90 255 227)"',
			'translate(255 227) scale(0.62) translate(-255 -227)'
		]) {
			expect(html.includes(decl), `app.html's splash mark must carry ${decl}`).toBe(true);
			expect(mark.includes(decl), `LoaderMark must carry ${decl}`).toBe(true);
		}
		// Rendered size: a literal in the shell, a prop at the component's call
		// site. Same 64px either way, or the mark changes size at the handover.
		expect(html, "app.html's splash mark must be 64px").toContain('width="64"');
		expect(component, 'AppSplash must render the mark at 64px').toContain('size={64}');
		for (const decl of ['stroke-width: 7', 'stroke-dasharray: 12 88']) {
			expect(html.includes(decl), `app.html's splash mark must declare \`${decl}\``).toBe(true);
			expect(mark.includes(decl), `LoaderMark must declare \`${decl}\``).toBe(true);
		}

		// The two dashes run at DIFFERENT speeds on purpose — that is what
		// makes the pair read as motion rather than a rigid rotating shape, and
		// it is the property most likely to be "tidied" into one duration by
		// someone who assumes it was a mistake.
		for (const [name, src] of [
			['app.html', html],
			['LoaderMark.svelte', mark]
		] as const) {
			expect(src, `${name}: lead dash must orbit in 0.65s`).toMatch(/orbit 0\.65s linear infinite/);
			expect(src, `${name}: trailing dash must orbit in 1.3s — half speed, deliberately`).toMatch(
				/orbit-trail 1\.3s linear infinite/
			);
			// Over a FULL lap, -50 to -150: a short keyframe range would be slow
			// too, by teleporting the dash back half a lap on every repeat.
			expect(src, `${name}: trailing keyframes must span -50 to -150`).toMatch(
				/orbit-trail\b[\s\S]*?-50[\s\S]*?-150/
			);
			expect(src, `${name}: dashes must stop under reduced motion`).toContain(
				'prefers-reduced-motion'
			);
		}
	});

	// Inline, never an <img> at a static asset. The first cut did exactly that
	// and it broke in the one place that cannot afford it: the server answers
	// an unmatched path with index.html (the SPA fallback in
	// `interfaces/web/embedded.rs`), so a stale build — or a proxy that drops
	// `/logo/*` — hands the image a page of HTML rather than a 404, and with
	// `alt=""` the boot screen renders as nothing at all.
	it('draws the mark inline, with no asset request', () => {
		const html = readFileSync(resolve('src/app.html'), 'utf-8');
		const mark = readFileSync(resolve('src/lib/components/LoaderMark.svelte'), 'utf-8');

		expect(html).not.toContain('logo-loader.svg');
		expect(mark).not.toContain('logo-loader.svg');
		// `<img>` in the shell is fine for the <noscript> layer, which is a
		// static message and not the loader; the splash itself must not have
		// one.
		const splash = /<div id="app-splash"[\s\S]*?<\/div>/.exec(html)?.[0] ?? '';
		expect(splash).toContain('<svg');
		expect(splash).not.toContain('<img');
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
