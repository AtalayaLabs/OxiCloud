<script lang="ts" module>
	/** A group-by dimension shown in the toolbar's popup menu. */
	export interface GroupOption {
		key: string;
		label: string;
		/** Optional glyph for the menu option (defaults to the group glyph). */
		icon?: string;
	}
</script>

<script lang="ts">
	import type { Snippet } from 'svelte';
	import Icon from '$lib/icons/Icon.svelte';
	import { t } from '$lib/i18n/index.svelte';
	import { files as filesStore } from '$lib/stores/files.svelte';
	import { preferences } from '$lib/stores/preferences.svelte';

	interface Props {
		/** Group-by dimensions; omit/empty to hide the group-by control. */
		groups?: GroupOption[];
		/** Active group-by key (controlled by the parent). */
		groupBy?: string;
		/** Whether the sort direction is reversed (controlled by the parent). */
		reversed?: boolean;
		/** Fired when a group-by dimension is chosen. */
		ongroup?: (key: string) => void;
		/** Fired when the sort-direction toggle is clicked. */
		ondirection?: () => void;
		/** Show the grid/list view toggle (default true). */
		showViewToggle?: boolean;
		/** Left-hand actions (upload/new-folder/empty-trash/batch bar, …). */
		start?: Snippet;
		/** Right-hand extras rendered inside `.view-toggle`, immediately
		 * before the group-by button. Use for page-local dropdown
		 * controls (e.g. Shares' kind filter) that should sit as siblings
		 * of the group-by dropdown and reuse `.toggle-btn`/`.group-by-*`
		 * classes for a consistent look. */
		beforeGroupBy?: Snippet;
		/** Show the dotfile-visibility eye toggle. Opt-in per page so
		 * surfaces that don't filter dotfiles (favorites, trash) don't
		 * get a control that appears to do nothing. When enabled the
		 * button lands at the RIGHT end of `.view-toggle` — same row as
		 * grid/list — and its aria-pressed state mirrors
		 * `preferences.hideDotfiles`. */
		showDotfileToggle?: boolean;
	}

	let {
		groups,
		groupBy = '',
		reversed = false,
		ongroup,
		ondirection,
		showViewToggle = true,
		start,
		beforeGroupBy,
		showDotfileToggle = false
	}: Props = $props();

	// The group-by button always reflects the active dimension (default = first).
	const active = $derived(groups?.find((g) => g.key === groupBy) ?? groups?.[0]);
	let menuOpen = $state(false);

	// Close the popup on outside click.
	$effect(() => {
		if (!menuOpen) return;
		const onDown = (e: MouseEvent) => {
			if (!(e.target as HTMLElement).closest('.group-by-selector')) menuOpen = false;
		};
		window.addEventListener('pointerdown', onDown);
		return () => window.removeEventListener('pointerdown', onDown);
	});

	function pick(key: string) {
		menuOpen = false;
		ongroup?.(key);
	}
</script>

<div class="actions-bar">
	{#if start}{@render start()}{:else}<div class="action-buttons"></div>{/if}

	{#if groups?.length || showViewToggle || beforeGroupBy || showDotfileToggle}
		<div class="view-toggle" role="group" aria-label={t('view.label', 'View options')}>
			{#if groups?.length}
				<button
					class="toggle-btn sort-dir-btn"
					class:active={reversed}
					title={t('sortdir.title', 'Sort direction')}
					aria-label={t('sort.direction', 'Sort direction')}
					data-testid="list-toolbar-sort-direction-btn"
					onclick={() => ondirection?.()}
				>
					<Icon name="arrow-up" />
				</button>
			{/if}
			{#if beforeGroupBy}{@render beforeGroupBy()}{/if}
			{#if groups?.length}
				<div class="group-by-selector" data-testid="list-toolbar-groupby-menu">
					<button
						class="toggle-btn group-by-btn active"
						title={t('groupby.title', 'Group by')}
						aria-haspopup="true"
						aria-expanded={menuOpen}
						data-testid="list-toolbar-groupby-btn"
						onclick={() => (menuOpen = !menuOpen)}
					>
						<Icon name={active?.icon ?? 'layer-group'} />
						<span class="group-by-label">{active?.label ?? ''}</span>
					</button>
					{#if menuOpen}
						<div class="group-by-menu">
							{#each groups as g (g.key)}
								<button
									class="group-by-option"
									class:active={groupBy === g.key}
									data-testid={`list-toolbar-groupby-${g.key}-item`}
									onclick={() => pick(g.key)}
								>
									<Icon name={g.icon ?? 'layer-group'} />
									{g.label}
								</button>
							{/each}
						</div>
					{/if}
				</div>
				{#if showViewToggle}<span class="view-toggle-separator"></span>{/if}
			{/if}
			{#if showDotfileToggle}
				<!--
					Right-most utility toggle: flip dotfile visibility for
					the current view without opening the profile page.
					`aria-pressed` reflects the persisted state (across
					sessions), matching how `preferences.hideDotfiles`
					participates in ARIA-toggle-button semantics. The
					title flips between "hide" / "show" so screen-reader
					users get an action label, not a state label.
				-->
				<button
					class="toggle-btn"
					class:active={preferences.hideDotfiles}
					title={preferences.hideDotfiles
						? t('view.show_dotfiles', 'Show hidden files')
						: t('view.hide_dotfiles', 'Hide hidden files')}
					aria-pressed={preferences.hideDotfiles}
					data-testid="list-toolbar-dotfile-toggle-btn"
					onclick={() => preferences.toggleHideDotfiles()}
					><Icon name={preferences.hideDotfiles ? 'eye-slash' : 'eye'} /></button
				>
			{/if}
			{#if showViewToggle && showDotfileToggle}
				<span class="view-toggle-separator"></span>
			{/if}
			{#if showViewToggle}
				<button
					class="toggle-btn"
					class:active={filesStore.viewMode === 'grid'}
					title={t('view.grid', 'Grid view')}
					aria-pressed={filesStore.viewMode === 'grid'}
					data-testid="list-toolbar-view-grid-btn"
					onclick={() => filesStore.setViewMode('grid')}><Icon name="th" /></button
				>
				<button
					class="toggle-btn"
					class:active={filesStore.viewMode === 'list'}
					title={t('view.list', 'List view')}
					aria-pressed={filesStore.viewMode === 'list'}
					data-testid="list-toolbar-view-list-btn"
					onclick={() => filesStore.setViewMode('list')}><Icon name="list" /></button
				>
			{/if}
		</div>
	{/if}
</div>
