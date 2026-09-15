<script lang="ts">
	/**
	 * The OxiCloud wordmark: cloud glyph + product name.
	 *
	 * Extracted from `AppShell`'s sidebar header so the public-share page can
	 * show the same brand without copying the SVG path — a duplicated glyph is
	 * the kind of thing that silently diverges the next time the mark changes.
	 *
	 * Styling comes from the global `.logo-container` / `.logo` / `.app-name`
	 * rules in `styles/ported/sidebar.css`, unchanged.
	 */
	interface Props {
		/**
		 * Where the mark links, ALREADY RESOLVED by the caller (i.e. pass
		 * `resolve('/files')`, not `'/files'`). Resolution stays with the
		 * caller because that is where the route is statically known —
		 * SvelteKit's `resolve()` is typed against the route table, which a
		 * component taking an arbitrary destination cannot satisfy.
		 *
		 * Omitted renders a plain `<div>` instead of an anchor: the
		 * public-share page has nowhere to send a visitor, and pointing at
		 * `/files` would only bounce them to the login screen.
		 */
		href?: string;
		/** Forwarded to the outer element so existing selectors keep working. */
		testId?: string;
	}
	let { href, testId }: Props = $props();
</script>

{#snippet mark()}
	<div class="logo">
		<svg viewBox="95 67 320 320" aria-hidden="true">
			<path
				d="M345 310c32 0 58-26 58-58s-26-58-58-58c-6.2 0-12 0.9-17.5 2.7C318 166 289 143 255 143c-34.3 0-63.1 22.6-73 53.7C176.9 195.7 171 195 165 195c-32 0-58 26-58 58s26 58 58 58h180z"
			/>
		</svg>
	</div>
	<div class="app-name">OxiCloud</div>
{/snippet}

{#if href}
	<!--
		`href` arrives pre-resolved (see the prop doc). The rule cannot follow
		it across the component boundary to the caller's `resolve()`.
	-->
	<!-- eslint-disable-next-line svelte/no-navigation-without-resolve -->
	<a {href} class="logo-container" data-testid={testId}>
		{@render mark()}
	</a>
{:else}
	<div class="logo-container" data-testid={testId}>
		{@render mark()}
	</div>
{/if}
