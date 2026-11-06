<script lang="ts" module>
	/** Reuses the existing GroupOption shape from `<ListToolbar>` so
	 *  callers can pass the same `groupBys` arrays their pages already
	 *  define. Duplicated here so consumers can import a coherent set
	 *  without pulling in the legacy toolbar. */
	export interface GroupOption {
		key: string;
		label: string;
		icon?: string;
	}
</script>

<script lang="ts">
	/**
	 * Right-hand cluster of display-mode controls: group-by menu,
	 * sort-direction toggle, grid/list view toggle, hide-dotfiles eye.
	 *
	 * Every control is opt-in via its own `show…` prop so a section
	 * without one (e.g. `/trash` has no dotfile toggle by design) can
	 * omit the prop rather than pass an empty array or a no-op
	 * callback. State bindings pass through — the parent still owns
	 * `groupBy`, `reversed`, `viewMode`, etc.
	 *
	 * Style-wise this reuses the ported `.view-toggle` block + child
	 * classes (buttons.css) so it renders identically to the
	 * `<ListToolbar>` right cluster. That keeps every page's look
	 * consistent across the ResourceList migration.
	 */
	import type { Snippet } from 'svelte';
	import Icon from '$lib/icons/Icon.svelte';
	import { t } from '$lib/i18n/index.svelte';
	import { files as filesStore } from '$lib/stores/files.svelte';
	import { preferences } from '$lib/stores/preferences.svelte';

	interface Props {
		// ── Group-by ─────────────────────────────────────────────
		/** Group-by dimensions; omit or empty array = hide the control. */
		groups?: GroupOption[];
		/** Active group-by key (controlled by the parent). */
		groupBy?: string;
		/** Fired when a group-by dimension is chosen. */
		ongroup?: (key: string) => void;

		// ── Sort direction ───────────────────────────────────────
		/** Whether the sort direction is reversed. */
		reversed?: boolean;
		/** Fired when the sort-direction toggle is clicked. */
		ondirection?: () => void;
		/** Show the sort-direction toggle. Defaults to `true` when
		 *  `groups` is non-empty (there's nothing to reverse otherwise). */
		showSort?: boolean;

		// ── View mode (grid / list) ─────────────────────────────
		/** Show the grid/list view toggle. */
		showViewMode?: boolean;

		// ── Dotfile visibility ──────────────────────────────────
		/** Show the hide-dotfiles eye toggle. Only makes sense on
		 *  algorithmic listings (files, recent); off by default. */
		showDotfileToggle?: boolean;

		// ── Extension slot ──────────────────────────────────────
		/** Rendered immediately before the group-by button, still
		 *  inside `.view-toggle`. Kind-filter dropdowns and other
		 *  page-local controls that want to sit alongside the
		 *  built-ins land here. */
		beforeGroupBy?: Snippet;

		/** Rendered right after the dotfile eye toggle, inside
		 *  `.view-toggle`. Page actions that belong with the view
		 *  controls rather than the action bar's left cluster land
		 *  here. */
		afterDotfileToggle?: Snippet;
	}

	let {
		groups,
		groupBy = '',
		ongroup,
		reversed = false,
		ondirection,
		showSort,
		showViewMode = false,
		showDotfileToggle = false,
		beforeGroupBy,
		afterDotfileToggle
	}: Props = $props();

	// Sort toggle defaults ON when a group-by list is provided —
	// there's nothing to reverse without it.
	const sortVisible = $derived(showSort ?? (groups?.length ?? 0) > 0);

	const active = $derived(groups?.find((g) => g.key === groupBy) ?? groups?.[0]);
	let menuOpen = $state(false);

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

	// Hide the whole cluster if nothing is enabled — the ActionBar
	// then collapses to its start-only layout without an empty
	// right block occupying space.
	const anyVisible = $derived(
		(groups?.length ?? 0) > 0 ||
			showViewMode ||
			showDotfileToggle ||
			!!beforeGroupBy ||
			!!afterDotfileToggle
	);
</script>

{#if anyVisible}
	<div class="view-toggle" role="group" aria-label={t('view.label', 'View options')}>
		{#if groups?.length && sortVisible}
			<button
				class="toggle-btn sort-dir-btn"
				class:active={reversed}
				title={t('sortdir.title', 'Sort direction')}
				aria-label={t('sort.direction', 'Sort direction')}
				data-testid="display-mode-sort-direction-btn"
				onclick={() => ondirection?.()}
			>
				<Icon name="arrow-up" />
			</button>
		{/if}
		{#if beforeGroupBy}{@render beforeGroupBy()}{/if}
		{#if groups?.length}
			<div class="group-by-selector" data-testid="display-mode-groupby-menu">
				<button
					class="toggle-btn group-by-btn active"
					title={t('groupby.title', 'Group by')}
					aria-haspopup="true"
					aria-expanded={menuOpen}
					data-testid="display-mode-groupby-btn"
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
								data-testid={`display-mode-groupby-${g.key}-item`}
								onclick={() => pick(g.key)}
							>
								<Icon name={g.icon ?? 'layer-group'} />
								{g.label}
							</button>
						{/each}
					</div>
				{/if}
			</div>
			<span class="view-toggle-separator"></span>
		{/if}
		{#if showDotfileToggle}
			<button
				class="toggle-btn"
				class:active={preferences.hideDotfiles}
				title={preferences.hideDotfiles
					? t('view.show_dotfiles', 'Show hidden files')
					: t('view.hide_dotfiles', 'Hide hidden files')}
				aria-pressed={preferences.hideDotfiles}
				data-testid="display-mode-dotfile-toggle-btn"
				onclick={() => preferences.toggleHideDotfiles()}
			>
				<Icon name={preferences.hideDotfiles ? 'eye-slash' : 'eye'} />
			</button>
		{/if}
		{#if afterDotfileToggle}{@render afterDotfileToggle()}{/if}
		{#if showViewMode && showDotfileToggle}
			<span class="view-toggle-separator"></span>
		{/if}
		{#if showViewMode}
			<button
				class="toggle-btn"
				title={filesStore.viewMode === 'grid'
					? t('view.list', 'List view')
					: t('view.grid', 'Grid view')}
				aria-label={filesStore.viewMode === 'grid'
					? t('view.list', 'List view')
					: t('view.grid', 'Grid view')}
				data-testid="display-mode-view-toggle-btn"
				onclick={() => filesStore.setViewMode(filesStore.viewMode === 'grid' ? 'list' : 'grid')}
			>
				<Icon name={filesStore.viewMode === 'grid' ? 'list' : 'th'} />
			</button>
		{/if}
	</div>
{/if}
