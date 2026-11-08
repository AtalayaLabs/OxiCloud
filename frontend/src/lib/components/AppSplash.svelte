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
	 * bundle exists — so it stays inline, and
	 * `AppSplash.test.ts` fails if its geometry drifts from this file's.
	 *
	 * Mount it conditionally; it has no internal "is loading" state.
	 */
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
	<div class="app-splash__spinner" aria-hidden="true"></div>
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

	.app-splash__spinner {
		width: 38px;
		height: 38px;
		border: 3px solid var(--color-border);
		border-top-color: var(--color-accent);
		border-radius: 50%;
		/* `spin` is the shared keyframe in `base/animations.css` — the one
		   that replaced six local copies. */
		animation: spin 0.7s linear infinite;
	}

	/* A spinner is the one thing that may NOT be the exception here: a
	   boot screen can stay up for seconds, and that is long enough to
	   matter for anyone who asked for less motion. The ring stays
	   visible, it just stops turning — same as `#app-splash` does. */
	@media (prefers-reduced-motion: reduce) {
		.app-splash__spinner {
			animation: none;
		}
	}
</style>
