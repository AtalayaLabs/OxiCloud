<!--
	A "⋮" trigger and the menu it opens.

	Built for table rows whose action column had grown past the point
	where icons carry meaning — the admin user table reached six, several
	sharing the same glyph for different things. A labelled menu item
	needs no icon to disambiguate, so the whole problem dissolves.

	Two things a row of icon buttons cannot do, which this gets for free:

	- **Unavailable actions can explain themselves.** An icon button that
	  is merely greyed relies on a tooltip nobody hovers. A menu row has
	  space for a `hint` under the label, so "cannot be deleted" arrives
	  with its reason attached.
	- **Destructive actions can be set apart.** `danger` items sort to the
	  bottom behind a separator, instead of sitting flush against a benign
	  neighbour where a mis-click costs an account.

	Deliberately NOT wired into `ResourceList`, which has its own inline
	pointer-anchored context menu for files and folders. Adopting this
	there is a reasonable follow-up; doing it in the same change as the
	admin table would put the core file list in a UI refactor that has
	nothing to do with it.
-->
<script lang="ts">
	import Icon from '$lib/icons/Icon.svelte';

	export interface ActionMenuItem {
		/** Stable key — also the `data-testid` suffix, so tests do not
		 *  depend on label text (which is translated). */
		key: string;
		label: string;
		icon?: string;
		/** Sorts to the bottom, behind a separator, and styled as danger. */
		danger?: boolean;
		disabled?: boolean;
		/** Why it is disabled. Rendered under the label — the reason is
		 *  worth more than the greyed-out state on its own. */
		hint?: string;
		run: () => void;
	}

	let {
		items,
		label,
		testId
	}: {
		items: ActionMenuItem[];
		/** Accessible name for the trigger, e.g. "Actions for alice". */
		label: string;
		/** Trigger gets `{testId}`; each item gets `{testId}-{key}`. */
		testId: string;
	} = $props();

	let open = $state(false);
	let trigger = $state<HTMLButtonElement | null>(null);
	let x = $state(0);
	let y = $state(0);

	// Benign items keep author order; destructive ones sink. Computed
	// rather than asked of the caller so every menu in the app agrees
	// about where "Delete" lives.
	const ordered = $derived([...items.filter((i) => !i.danger), ...items.filter((i) => i.danger)]);
	const firstDangerKey = $derived(items.find((i) => i.danger)?.key);

	const MENU_WIDTH = 240;

	function toggle() {
		if (open) {
			open = false;
			return;
		}
		const rect = trigger?.getBoundingClientRect();
		if (rect) {
			// Right-aligned under the trigger, clamped so a row near the
			// viewport edge does not open the menu off-screen.
			x = Math.max(8, Math.min(rect.right - MENU_WIDTH, window.innerWidth - MENU_WIDTH - 8));
			y = Math.min(rect.bottom + 4, window.innerHeight - (ordered.length * 44 + 24));
		}
		open = true;
	}

	function close() {
		open = false;
		trigger?.focus();
	}

	function onKeydown(e: KeyboardEvent) {
		if (e.key === 'Escape' && open) {
			e.stopPropagation();
			close();
		}
	}
</script>

<svelte:window onkeydown={onKeydown} />

<button
	bind:this={trigger}
	class="icon-btn"
	data-testid={testId}
	title={label}
	aria-label={label}
	aria-haspopup="menu"
	aria-expanded={open}
	onclick={toggle}
>
	<Icon name="ellipsis-v" />
</button>

{#if open}
	<!-- Scrim closes on any outside click. `role="presentation"` because
	     it is a dismissal surface, not a control. -->
	<div class="am-scrim" role="presentation" onclick={close}></div>
	<div
		class="am-menu"
		style:left="{x}px"
		style:top="{y}px"
		role="menu"
		data-testid={`${testId}-menu`}
	>
		{#each ordered as item (item.key)}
			{#if item.danger && item.key === firstDangerKey}
				<div class="am-sep" role="separator"></div>
			{/if}
			<button
				class="am-item"
				class:am-item--danger={item.danger}
				role="menuitem"
				disabled={item.disabled}
				aria-disabled={item.disabled}
				data-testid={`${testId}-${item.key}`}
				onclick={() => {
					if (item.disabled) return;
					close();
					item.run();
				}}
			>
				{#if item.icon}<Icon name={item.icon} />{/if}
				<span class="am-item__text">
					{item.label}
					{#if item.disabled && item.hint}
						<span class="am-item__hint">{item.hint}</span>
					{/if}
				</span>
			</button>
		{/each}
	</div>
{/if}

<style>
	/* Covers the viewport so any outside click dismisses. Below the menu,
	   above everything else. */
	.am-scrim {
		position: fixed;
		inset: 0;
		z-index: 900;
	}

	.am-menu {
		position: fixed;
		z-index: 901;
		min-width: 240px;
		padding: var(--space-1) 0;
		background: var(--color-bg-surface);
		border: 1px solid var(--color-border);
		border-radius: var(--radius-md);
		box-shadow: var(--shadow-lg);
	}

	.am-item {
		display: flex;
		align-items: flex-start;
		gap: var(--space-2);
		width: 100%;
		padding: var(--space-2) var(--space-3);
		background: none;
		border: none;
		text-align: left;
		font-size: 0.875rem;
		color: var(--color-text);
		cursor: pointer;
	}

	.am-item:hover:not(:disabled) {
		background: var(--color-bg-muted);
	}

	.am-item:disabled {
		color: var(--color-text-muted);
		cursor: not-allowed;
	}

	.am-item--danger:not(:disabled) {
		color: var(--color-danger-text);
	}

	.am-item--danger:hover:not(:disabled) {
		background: var(--color-danger-bg);
	}

	.am-item__text {
		display: flex;
		flex-direction: column;
	}

	/* The reason an action is unavailable. Smaller and muted so it reads
	   as explanation rather than as a second action. */
	.am-item__hint {
		font-size: 0.75rem;
		color: var(--color-text-muted);
	}

	.am-sep {
		height: 1px;
		margin: var(--space-1) 0;
		background: var(--color-border);
	}
</style>
