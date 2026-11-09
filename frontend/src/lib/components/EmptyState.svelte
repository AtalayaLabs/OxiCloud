<script lang="ts">
	import type { Snippet } from 'svelte';
	import Icon from '$lib/icons/Icon.svelte';

	interface Props {
		/** Icon-registry name shown above the title (omit for a text-only state). */
		icon?: string;
		/** Primary line. */
		title?: string;
		/** Secondary explanatory line. */
		hint?: string;
		/** Error styling (danger-coloured icon) + assertive `role="alert"`. */
		error?: boolean;
		/** Extra content (e.g. a call-to-action button) rendered below the hint. */
		children?: Snippet;
	}

	let { icon, title, hint, error = false, children }: Props = $props();
</script>

<!-- `data-testid="empty-state"` is a stable Playwright hook: consumers
     (ResourceList, ShareList, TrashList, …) only render this component
     once the underlying load resolved with no items — so waiting on
     this testid = "the listing definitively finished loading and is
     empty". Used by `tests/e2e/spa/files.spec.ts` to gate a cold-
     navigation upload behind the folder-loaded state (see the guard
     in `routes/files/[...path]/+page.svelte::guardUploadFolderReady`). -->
<div
	class="empty-state"
	class:empty-state--error={error}
	role={error ? 'alert' : undefined}
	data-testid="empty-state"
>
	{#if icon}<Icon name={icon} class="empty-state__icon" />{/if}
	{#if title}<p class="empty-state__title">{title}</p>{/if}
	{#if hint}<p class="empty-state__hint">{hint}</p>{/if}
	{@render children?.()}
</div>

<style>
	/* Layout comes from the global `.empty-state` (styles/ported/content.css);
	   these refine the icon/title/hint elements consistently across views. */
	.empty-state :global(.empty-state__icon) {
		font-size: var(--text-5xl);
		color: var(--color-text-faint);
		margin-bottom: var(--space-2);
	}

	/* `--color-error-text`, NOT `--color-danger-text`. The latter is `#ffffff`
	   in both themes because it is the foreground for text sitting ON a red
	   fill (`.btn-danger`, the batch toolbar, the logout item) — used as a
	   standalone glyph colour it painted white on white, so every error
	   state in the app was invisible in light mode and only legible in dark.
	   `--color-error-text` is the `light-dark()` pair meant for standalone
	   use on the page background: deep red in light mode, soft coral in
	   dark.

	   The same trap was hit and documented once before, on
	   `routes/nextcloud/error/+page.svelte`; `.music-error` renders this
	   component now, so all three error states share one class and one
	   token. `EmptyState.test.ts` fails if this reverts. */
	.empty-state--error :global(.empty-state__icon) {
		color: var(--color-error-text);
	}

	.empty-state__title {
		margin: 0;
		font-size: var(--text-lg);
		font-weight: var(--weight-semibold);
		color: var(--color-text-heading);
	}

	.empty-state__hint {
		margin: 0;
		max-width: 28rem;
		font-size: var(--text-sm);
		color: var(--color-text-muted);
	}
</style>
