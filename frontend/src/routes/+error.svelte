<script lang="ts">
	import { page } from '$app/state';
	import { resolve } from '$app/paths';
	import { t } from '$lib/i18n/index.svelte';

	// 404 gets the shy cloud; every other status keeps the same layout with a
	// straight face. A 500 illustrated by a bashful mascot reads as flippant
	// when someone's upload has just failed.
	const isNotFound = $derived(page.status === 404);
</script>

<div class="errpage" data-testid="errpage">
	{#if isNotFound}
		<!--
			The cloud is the SAME path as `routes/device/+page.svelte` — the
			product's own mark, so this reads as OxiCloud rather than as a
			mascot drawn for the occasion. Replaces the three-div
			border-radius cloud of the mockup: one path is fewer elements,
			scales without the circles drifting apart, and stays identical to
			the logo if the logo is ever redrawn.

			Every fill and stroke is a token. That is what answers light/dark:
			`variables.css` defines these with `light-dark()`, so the
			illustration re-themes with the app and there is no second palette
			to keep in sync — no `@media (prefers-color-scheme)` here on
			purpose, since the app themes off `data-color-scheme` and a media
			query would disagree with an explicit user choice.
		-->
		<div class="errpage__art">
			<svg viewBox="95 125 320 200" role="img" aria-labelledby="errpage-art-title">
				<title id="errpage-art-title"
					>{t('error.not_found.art_alt', 'A small cloud that has drifted too far')}</title
				>
				<path
					class="errpage__cloud"
					d="M345 310c32 0 58-26 58-58s-26-58-58-58c-6.2 0-12 0.9-17.5 2.7C318 166 289 143 255 143c-34.3 0-63.1 22.6-73 53.7C176.9 195.7 171 195 165 195c-32 0-58 26-58 58s26 58 58 58h180z"
				/>
				<!-- Shy eyes, low and close together as in the mockup: that
				     spacing is what makes it read as bashful rather than
				     surprised. Accent-coloured so the one warm note in the
				     page is the brand's. -->
				<g class="errpage__eyes">
					<circle cx="243" cy="258" r="8" />
					<circle cx="273" cy="258" r="8" />
				</g>
			</svg>
		</div>
	{/if}

	<p class="errpage__code" aria-hidden="true">{page.status}</p>

	<h1 class="errpage__title">
		{#if isNotFound}
			{t('error.not_found.title', 'Head in the clouds')}
		{:else}
			{t('error.generic.title', 'Something went wrong')}
		{/if}
	</h1>

	<p class="errpage__body">
		{#if isNotFound}
			{t(
				'error.not_found.body',
				'This little cloud drifted a bit too far. The page you asked for is nowhere to be found.'
			)}
		{:else}
			{page.error?.message ?? t('error.generic.body', 'Please try again in a moment.')}
		{/if}
	</p>

	<div class="errpage__actions">
		<!-- One way out, deliberately. A "Go back" button alongside this was
		     dropped: the browser's own back button already does that, better
		     (it's where everyone's hand already is, and it works when there
		     is no history to go back to — ours would have been a dead
		     control). Home is the one action the chrome can't offer, so it
		     takes the primary style the pair used to split. -->
		<a class="btn btn-primary" href={resolve('/')} data-testid="errpage-home">
			{t('error.go_home', 'Go home')}
		</a>
	</div>
</div>

<style>
	/* `width: 100%` is load-bearing, not belt-and-braces. Signed out, the
	   root layout renders this page BARE — no `AppShell` — so it becomes a
	   direct child of `body`, which is `display: flex` (row). A flex item
	   sizes to its content, so without this the whole column shrink-wraps
	   and hugs the left edge however well it centres its own children.
	   Same trap the old boot loader hit (see the note at the foot of
	   `routes/+layout.svelte`). Inside `AppShell` it's simply the pane
	   width, so one rule covers both. `box-sizing: border-box` is global,
	   so the padding stays inside the 100%. */
	.errpage {
		display: flex;
		flex-direction: column;
		align-items: center;
		justify-content: center;
		width: 100%;
		min-height: 70vh;
		padding: var(--space-6) var(--space-4);
		text-align: center;
	}

	/* The float is the whole charm of the mockup, kept at its 3s / -15px. */
	.errpage__art {
		width: min(200px, 48vw);
		margin-bottom: var(--space-2);
		animation: errpage-float 3s ease-in-out infinite;
	}

	.errpage__art svg {
		display: block;
		width: 100%;
		height: auto;
	}

	/* The logo in its own colour. `--color-accent` is the one accent token
	   with no `light-dark()` in it (#ff5e3a flat), which is what the mark
	   needs: the brand orange is the brand orange on either ground. */
	.errpage__cloud {
		fill: var(--color-accent);
	}

	/* Blink on the eye GROUP, not each eye: per-eye animations drift out of
	   phase and the result reads as a twitch rather than a blink.
	   `--color-on-accent` rather than a literal white: it is the token that
	   means "drawn on top of the accent", so the eyes stay readable if the
	   accent is ever retuned, and it is flat #ffffff in both themes — a
	   `light-dark()` token would turn them dark-on-orange at night. */
	.errpage__eyes {
		fill: var(--color-on-accent);
		transform-box: fill-box;
		transform-origin: center;
		animation: errpage-blink 4s infinite;
	}

	/* The numerals sit behind the message rather than above it: the headline
	   carries the meaning and "404" is confirmation for whoever wants it.
	   Accent at low alpha instead of the mockup's white-on-orange, which only
	   worked because that page owned the whole background. */
	.errpage__code {
		margin: 0;
		font-size: clamp(3.5rem, 14vw, 6rem);
		font-weight: 900;
		line-height: 1;
		color: var(--color-accent);
		opacity: 0.18;
		font-variant-numeric: tabular-nums;
		letter-spacing: -0.02em;
	}

	.errpage__title {
		margin: var(--space-2) 0 0;
		font-size: var(--text-2xl);
		color: var(--color-text-heading);
		text-wrap: balance;
	}

	.errpage__body {
		max-width: 42ch;
		margin: var(--space-3) 0 0;
		color: var(--color-text-secondary);
		text-wrap: pretty;
	}

	.errpage__actions {
		display: flex;
		flex-wrap: wrap;
		gap: var(--space-3);
		justify-content: center;
		margin-top: var(--space-6);
	}

	@keyframes errpage-float {
		0%,
		100% {
			transform: translateY(0);
		}

		50% {
			transform: translateY(-15px);
		}
	}

	@keyframes errpage-blink {
		0%,
		90%,
		100% {
			transform: scaleY(1);
		}

		95% {
			transform: scaleY(0.1);
		}
	}

	@media (prefers-reduced-motion: reduce) {
		.errpage__art,
		.errpage__eyes {
			animation: none;
		}
	}
</style>
