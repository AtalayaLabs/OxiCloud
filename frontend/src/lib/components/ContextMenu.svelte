<script lang="ts" module>
	/**
	 * One entry in a context menu. Everything is already resolved by the
	 * caller — no per-item predicates, no target object. Callers that have
	 * a richer action model (`ResourceList`'s `ContextAction`, with its
	 * `visible?` / `disabled?` gates) evaluate it against their target and
	 * hand the result down as a flat list.
	 *
	 * That split is the point: this component owns the menu's SHAPE
	 * (scrim, placement, keyboard, styling) and knows nothing about files,
	 * folders, drives or breadcrumbs.
	 */
	export interface ContextMenuEntry {
		key: string;
		label: string;
		icon: string;
		/** Forwarded verbatim so each call site keeps its own e2e hooks. */
		testid?: string;
		/**
		 * Renders the entry dimmed and inert. Prefer it over dropping the
		 * entry when the action *could* apply but the caller lacks the
		 * permission — a greyed row answers "this option exists" instead
		 * of leaving a hole that reads as a missing feature.
		 *
		 * Ignored for an `href` entry: an anchor has no disabled state, and
		 * no call site has needed one.
		 */
		disabled?: boolean;
		/**
		 * Renders an `<a download>` instead of a `<button>` — for entries
		 * whose action IS a navigation the browser performs natively (the
		 * file-download entry on `/files`). Mutually exclusive with
		 * `danger`; `onselect` still fires, so the menu can close itself.
		 */
		href?: string;
		/**
		 * Destructive: renders a `ConfirmButton` that arms on the first
		 * click and runs on the second, so `onselect` only ever fires
		 * from a deliberate second click.
		 */
		danger?: boolean;
		/** Wording for the armed state — see `ConfirmButton.confirmLabel`. */
		confirmLabel?: string;
		/**
		 * Runs on activation. The component does NOT close itself first:
		 * callers close in their own handler, because some need to read
		 * their target state before it is cleared.
		 */
		onselect?: () => void;
	}
</script>

<script lang="ts">
	import ConfirmButton from '$lib/components/ConfirmButton.svelte';
	import Icon from '$lib/icons/Icon.svelte';
	import {
		clampMenuToViewport,
		focusFirstMenuItem,
		handleMenuKeydown
	} from '$lib/utils/menuKeyboard';

	/**
	 * The app's one right-click menu: a fixed-position panel over a
	 * full-viewport scrim, with arrow-key navigation, Escape-to-close and
	 * viewport clamping.
	 *
	 * Mount it conditionally (`{#if open}`) rather than toggling a prop —
	 * the focus-and-clamp pass runs on mount, and callers already have an
	 * `open` flag because they need somewhere to keep the target.
	 *
	 * Shared by `ResourceList` (its built-in row menu), `/files` (the
	 * override menu with WOPI/audio entries) and `FolderBreadcrumb`
	 * (per-crumb folder actions). Three copies of this markup existed
	 * before, and they had already drifted: two different `min-width`
	 * values, one scrim with a `data-testid` and one without.
	 */
	interface Props {
		/** Viewport coords of the click that opened the menu. */
		x: number;
		y: number;
		entries: ContextMenuEntry[];
		/** Called on scrim click, Escape and Tab. */
		onclose: () => void;
		/** `data-testid` for the menu panel. */
		testid?: string;
		/** `data-testid` for the scrim. */
		scrimTestid?: string;
	}
	let { x, y, entries, onclose, testid, scrimTestid }: Props = $props();

	let menuEl = $state<HTMLElement | null>(null);

	// Clamped position — null until the panel has been measured, so the
	// first paint uses the raw click coords and the pass below pulls it
	// back inside the viewport. Callers used to pre-clamp with a
	// hard-coded guess at the menu's height (`entries.length * 44 + 24`
	// in one, a flat `320` in the other); they no longer need to.
	let placed = $state<{ x: number; y: number } | null>(null);

	// Focus lands on the first item once, when the panel mounts. Deliberately
	// NOT re-run when the entries change: `/files` probes WOPI editability
	// after the menu is already open, and pulling focus back to the top
	// mid-arrow-key would be worse than the stale position it fixes below.
	$effect(() => {
		if (menuEl) focusFirstMenuItem(menuEl);
	});

	$effect(() => {
		// Dependencies, read before the early return: the anchor (a caller
		// may reposition an open menu) and the entry count (the panel's
		// height changes when entries appear — again, `/files`'s async WOPI
		// probe, which can push the bottom of the menu off-screen).
		const ax = x;
		const ay = y;
		void entries.length;
		if (!menuEl) return;
		placed = clampMenuToViewport(menuEl, ax, ay);
	});
</script>

<!--
	Scrim: closes on click, and swallows a second right-click so the
	browser's native menu never appears over ours. Sits above
	`.page-sticky-header` (`--z-sticky` = 100) so a click on the
	breadcrumb or action bar closes the menu instead of being eaten by
	the sticky strip.
-->
<div
	class="ctx-scrim"
	role="presentation"
	data-testid={scrimTestid}
	onclick={onclose}
	oncontextmenu={(e) => e.preventDefault()}
></div>
<div
	class="ctx-menu"
	bind:this={menuEl}
	onkeydown={(e) => handleMenuKeydown(e, menuEl, onclose)}
	style:left="{placed?.x ?? x}px"
	style:top="{placed?.y ?? y}px"
	role="menu"
	tabindex="-1"
	data-testid={testid}
>
	{#each entries as entry (entry.key)}
		{#if entry.danger}
			<ConfirmButton
				icon={entry.icon}
				class="menu-item menu-item--danger"
				role="menuitem"
				disabled={entry.disabled}
				title={entry.label}
				confirmLabel={entry.confirmLabel}
				data-testid={entry.testid}
				onconfirm={() => entry.onselect?.()}
			>
				{entry.label}
			</ConfirmButton>
		{:else if entry.href}
			<!--
				`rel="external"` keeps SvelteKit's client router out of it:
				this href is a download endpoint, not an app route.
			-->
			<a
				class="menu-item"
				role="menuitem"
				href={entry.href}
				rel="external"
				download
				data-testid={entry.testid}
				onclick={() => entry.onselect?.()}
			>
				<Icon name={entry.icon} />
				{entry.label}
			</a>
		{:else}
			<button
				class="menu-item"
				role="menuitem"
				disabled={entry.disabled}
				data-testid={entry.testid}
				onclick={() => {
					// The attribute alone is what a browser enforces; the
					// guard is here so "disabled" is a property of this
					// component rather than of the host that happens to
					// deliver the click (jsdom, for one, delivers it).
					if (!entry.disabled) entry.onselect?.();
				}}
			>
				<Icon name={entry.icon} />
				{entry.label}
			</button>
		{/if}
	{/each}
</div>

<style>
	.ctx-scrim {
		position: fixed;
		inset: 0;
		z-index: 1000;
	}

	.ctx-menu {
		position: fixed;
		z-index: 1001;
		min-width: 200px;
		max-height: calc(100dvh - 16px);
		overflow-y: auto;
		padding: var(--space-1);
		background: var(--color-bg-surface);
		border: 1px solid var(--color-border);
		border-radius: var(--radius-md);
		box-shadow: var(--shadow-lg);
	}
</style>
