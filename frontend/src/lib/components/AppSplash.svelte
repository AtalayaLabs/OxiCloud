<script lang="ts">
	/**
	 * Full-viewport centred spinner — the app's one "nothing to show yet"
	 * screen.
	 *
	 * Three sites paint this, and they must be indistinguishable because a
	 * cold boot crosses all of them in sequence: the inline
	 * `#app-splash` in `app.html` (paints on HTML parse, before any
	 * bundle), then this component while the session probe is in flight,
	 * then — on an OIDC callback — again while the code exchange runs.
	 * Any difference in size, colour or cadence reads as a flicker at each
	 * handover.
	 *
	 * Two of those three were hand-written copies carrying a comment
	 * telling the next person to "update BOTH sites at once"; this
	 * component is that instruction made structural. The third,
	 * `app.html`, cannot import anything — it has to paint before the CSS
	 * bundle exists — so it keeps a hand-written copy of both the frame and
	 * the mark, with literals in place of tokens.
	 * `AppSplash.test.ts` compares the two and fails if they drift, down to
	 * the glyph path, the dash geometry and the animation cadence.
	 *
	 * Mount it conditionally; it has no internal "is loading" state.
	 */
	import LoaderMark from '$lib/components/LoaderMark.svelte';

	interface Props {
		/**
		 * Accessible name, announced to screen readers. Describe what is
		 * being waited on ("Signing in"), not the widget.
		 */
		label?: string;
		testid?: string;
	}
	let { label = 'Loading', testid }: Props = $props();
</script>

<div class="app-splash" role="status" aria-label={label} data-testid={testid}>
	<!-- Decorative: the container above carries the accessible name. -->
	<LoaderMark size={64} />
</div>

<style>
	/* Tokens, not literals — but they must RESOLVE to what the inline
	   splash in `app.html` hard-codes, since that one runs before this
	   stylesheet exists and cannot read a custom property:
	   `--color-bg-page` ↔ `light-dark(#f5f7fa, #0f172a)`, plus
	   `--color-border` and `--color-accent` (#ff5e3a). Retoning the
	   splash means editing both. */
	.app-splash {
		position: fixed;
		inset: 0;
		/* Above everything, including the sticky header (`--z-sticky`,
		   100) and the context menu (1001) — this covers the app, it does
		   not sit inside it. */
		z-index: 9999;
		display: grid;
		place-items: center;
		background: var(--color-bg-page);
	}

	/* The mark and its animation live in `LoaderMark`; this component owns
	   only the full-viewport frame around it. */
</style>
