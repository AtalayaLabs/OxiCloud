<script lang="ts">
	/**
	 * A toolbar button that opens a small panel of filter controls.
	 *
	 * For filters that are worth having but rarely touched: inline they cost
	 * permanent toolbar width and read as chrome, while behind one button they
	 * cost a click and disappear until wanted.
	 *
	 * Deliberately NOT `ActionMenu`, which is a list of items that each `run`
	 * something. These are stateful controls — checkboxes and radio groups
	 * that stay open while you change them — so the host supplies the panel's
	 * contents and this owns only the parts that are the same everywhere: the
	 * trigger, the open/close state, dismissal on Escape or an outside click,
	 * and the `aria-expanded` / `aria-haspopup` pairing.
	 *
	 * `active` marks the trigger when the filters differ from their defaults,
	 * so a hidden non-default filter cannot silently explain a short list.
	 */
	import type { Snippet } from 'svelte';
	import Icon from '$lib/icons/Icon.svelte';

	interface Props {
		/** Tooltip and accessible name for the trigger. */
		label: string;
		/** Panel contents — the host's own controls. */
		children: Snippet;
		/** Highlight the trigger: a filter is set. */
		active?: boolean;
		icon?: string;
		testId?: string;
	}
	let { label, children, active = false, icon = 'sliders-h', testId }: Props = $props();

	let open = $state(false);
	let root = $state<HTMLElement | null>(null);

	// Dismissal lives here rather than in each host: a panel that cannot be
	// closed by clicking away is the bug every hand-rolled dropdown ships
	// first. `pointerdown`, not `click`, so it fires before a control inside
	// another menu swallows the event.
	$effect(() => {
		if (!open) return;
		const away = (e: PointerEvent) => {
			if (root && !root.contains(e.target as Node)) open = false;
		};
		const esc = (e: KeyboardEvent) => {
			if (e.key === 'Escape') open = false;
		};
		document.addEventListener('pointerdown', away);
		document.addEventListener('keydown', esc);
		return () => {
			document.removeEventListener('pointerdown', away);
			document.removeEventListener('keydown', esc);
		};
	});
</script>

<div class="filter-menu" bind:this={root}>
	<button
		class="toggle-btn"
		class:active
		title={label}
		aria-label={label}
		aria-haspopup="true"
		aria-expanded={open}
		data-testid={testId}
		onclick={() => (open = !open)}
	>
		<Icon name={icon} />
	</button>
	{#if open}
		<div class="filter-menu__panel" role="group" aria-label={label}>
			{@render children()}
		</div>
	{/if}
</div>

<style>
	.filter-menu {
		position: relative;
		display: inline-flex;
	}

	.filter-menu__panel {
		position: absolute;
		top: calc(100% + 4px);
		right: 0;
		z-index: 30;
		display: flex;
		min-width: 190px;
		flex-direction: column;
		gap: 0.35rem;
		padding: 0.6rem;
		border: 1px solid var(--color-border);
		border-radius: var(--radius-md);
		background: var(--color-bg-surface);
		box-shadow: 0 4px 12px var(--color-shadow-sm);
	}
</style>
