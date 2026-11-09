<script lang="ts">
	import { BRAND_MARK_PATH, BRAND_MARK_VIEWBOX } from '$lib/icons/brandMark';

	/**
	 * The brand mark with two dashes orbiting it — the app's "working on
	 * it" animation, for any wait big enough to deserve the logo rather
	 * than a bare spinner.
	 *
	 * Inline SVG, not an `<img>` pointing at a static asset. That was the
	 * first implementation and it failed in the one place it matters most:
	 * the server's SPA fallback answers an unmatched path with
	 * `index.html`, so a deployment where the asset does not resolve —
	 * a stale build, a proxy that drops `/logo/*`, a partial deploy —
	 * hands the `<img>` a page of HTML instead of a 404. With `alt=""`
	 * that renders as nothing at all, and the boot screen is the one
	 * screen that must never quietly disappear. Inline markup cannot 404,
	 * and it costs no request on the critical first paint.
	 *
	 * Being inline also means it can read the theme tokens, so it follows
	 * the in-app `data-color-scheme` rather than the OS preference.
	 *
	 * `src/app.html` keeps a hand-written copy of this for its instant
	 * boot splash — it paints before any bundle exists, so it cannot
	 * import a component or read a custom property. `AppSplash.test.ts`
	 * compares the two and fails if they drift.
	 */
	interface Props {
		/** Rendered size in px, square. Default matches the boot splash. */
		size?: number;
		/**
		 * Accessible name. Omitted (the usual case) marks the mark
		 * decorative, because the thing that is loading normally labels
		 * itself — `AppSplash` puts `role="status"` on its container.
		 */
		title?: string;
		class?: string;
	}
	let { size = 64, title, class: className = '' }: Props = $props();
</script>

<svg
	class={`loader-mark ${className}`}
	viewBox={BRAND_MARK_VIEWBOX}
	width={size}
	height={size}
	role={title ? 'img' : undefined}
	aria-hidden={title ? undefined : 'true'}
	aria-label={title}
>
	{#if title}<title>{title}</title>{/if}
	<!--
		Orbit: centred on the viewBox centre (255, 227) with r=150, which
		leaves a 10-unit margin. `rotate(-90)` starts the dashes at twelve
		o'clock — a bare <circle> begins at three.
	-->
	<circle class="loader-mark__track" cx="255" cy="227" r="150" />
	<circle
		class="loader-mark__dash loader-mark__dash--lead"
		cx="255"
		cy="227"
		r="150"
		pathLength="100"
		transform="rotate(-90 255 227)"
	/>
	<circle
		class="loader-mark__dash loader-mark__dash--trail"
		cx="255"
		cy="227"
		r="150"
		pathLength="100"
		transform="rotate(-90 255 227)"
	/>
	<!-- The mark, scaled to 62% about the viewBox centre so it sits inside
	     the orbit. -->
	<path
		class="loader-mark__glyph"
		transform="translate(255 227) scale(0.62) translate(-255 -227)"
		d={BRAND_MARK_PATH}
	/>
</svg>

<style>
	.loader-mark {
		display: block;
	}

	.loader-mark__track {
		fill: none;
		stroke: var(--color-border);
		stroke-width: 7;
	}

	.loader-mark__dash {
		fill: none;
		stroke: var(--color-accent);
		stroke-width: 7;
		stroke-linecap: round;
		/* `pathLength="100"` on the elements above puts dash lengths and
		   offsets in hundredths of the path, so nothing has to measure the
		   geometry with `getTotalLength()` — which is what keeps this
		   animation free of JavaScript. */
		stroke-dasharray: 12 88;
	}

	.loader-mark__dash--lead {
		animation: orbit 0.65s linear infinite;
	}

	/* HALF the lead's speed, on purpose (Ed's call): two dashes at the same
	   rate are a rigid pair, where two at different rates lap each other and
	   the figure keeps changing. The duration carries the difference — 1.3s
	   over the same -50 → -150 lap — rather than a short keyframe range.
	   Getting it the other way (`stroke-dashoffset: -50` plus the lead's
	   keyframes, which end at -100) makes the implicit `from` that -50, so the
	   dash covers half the distance and then TELEPORTS back half a lap on
	   every repeat. Same slowness, with a glitch in it. */
	.loader-mark__dash--trail {
		animation: orbit-trail 1.3s linear infinite;
	}

	@keyframes orbit {
		to {
			stroke-dashoffset: -100;
		}
	}

	@keyframes orbit-trail {
		from {
			stroke-dashoffset: -50;
		}

		to {
			stroke-dashoffset: -150;
		}
	}

	.loader-mark__glyph {
		fill: var(--color-accent);
	}

	/* A boot screen can stay up for seconds, which is long enough to matter
	   for anyone who asked for less motion. The ring and the mark stay; only
	   the dashes stop. */
	@media (prefers-reduced-motion: reduce) {
		.loader-mark__dash--lead,
		.loader-mark__dash--trail {
			animation: none;
		}
	}
</style>
