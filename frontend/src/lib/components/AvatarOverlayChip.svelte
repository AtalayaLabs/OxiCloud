<script lang="ts">
	/**
	 * A user avatar anchored to the bottom-right of a grid card's preview
	 * tile — "who shared this" on `/shared-with-me`, "who changed this" on
	 * `/changes`. Render it from `ResourceList`'s `cardOverlay` snippet, which
	 * emits inside `.file-icon` and only in grid view, so there is nothing to
	 * gate here.
	 *
	 * Extracted from `/shared-with-me`, which positioned the chip with a
	 * `:global(.files-grid-view .file-item .file-icon …)` selector — needed
	 * there because the page's scoped CSS could not reach a span the snippet
	 * emitted. Owning the span here removes both the duplication and the
	 * global selector: the styles are scoped to this component and no longer
	 * depend on `ResourceList`'s class names holding still.
	 *
	 * The ring is `--color-bg-surface` so the chip stays legible over any
	 * thumbnail — photo, type-tinted tile, light or dark.
	 */
	import UserAvatar from './UserAvatar.svelte';

	interface Props {
		userId: string;
		/** Falls back to this while the profile resolves, or if it fails. */
		fallbackLabel?: string;
		/** Pixel size — 24 matches the grid-card scale. */
		size?: number;
		/** Tooltip override; pass '' to suppress the avatar's own. */
		title?: string;
	}
	let { userId, fallbackLabel, size = 24, title }: Props = $props();
</script>

<span class="chip">
	<UserAvatar {userId} {fallbackLabel} {size} {title} />
</span>

<style>
	.chip {
		position: absolute;
		right: 5px;
		bottom: 5px;
		z-index: 10;
		display: inline-flex;
		border-radius: var(--radius-full);
		box-shadow:
			0 0 0 2px var(--color-bg-surface),
			0 1px 3px var(--color-shadow-sm);
	}
</style>
