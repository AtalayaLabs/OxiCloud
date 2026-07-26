<script lang="ts">
	import EmptyState from '$lib/components/EmptyState.svelte';
	import VirtualList from '$lib/components/VirtualList.svelte';
	import { errorMessage } from '$lib/utils/errors';
	import { goto } from '$app/navigation';
	import { resolve } from '$app/paths';
	import { page } from '$app/state';
	import { searchFiles } from '$lib/api/endpoints/search';
	import { fileInlineUrl } from '$lib/api/endpoints/files';
	import type { FileItem, FolderItem, SearchResults, SortBy } from '$lib/api/types';
	import Icon from '$lib/icons/Icon.svelte';
	import { t } from '$lib/i18n/index.svelte';
	import { files as filesStore } from '$lib/stores/files.svelte';
	import { ui } from '$lib/stores/ui.svelte';
	import { formatBytes } from '$lib/utils/format';
	import { formatDate, iconNameFromClass, fileIconKindClass } from '$lib/utils/display';

	const query = $derived(page.url.searchParams.get('q') ?? '');

	let results = $state<SearchResults | null>(null);
	let loading = $state(false);
	let error = $state<string | null>(null);
	let sortBy = $state<SortBy>('relevance');
	// Scope: search everywhere, or within the folder last open in the files view.
	// Default to the current folder when one is set (and we're not in the trash
	// section), mirroring the legacy searchView behaviour.
	let scope = $state<'all' | 'folder'>(
		filesStore.currentFolder && filesStore.section !== 'trash' ? 'folder' : 'all'
	);

	// Filters
	type TypeKey = 'all' | 'image' | 'video' | 'document' | 'audio' | 'archive';
	type SizeKey = 'all' | 'small' | 'medium' | 'large';
	type DateKey = 'all' | 'day' | 'week' | 'month' | 'year';
	let typeFilter = $state<TypeKey>('all');
	let sizeFilter = $state<SizeKey>('all');
	let dateFilter = $state<DateKey>('all');

	const TYPE_EXT: Record<Exclude<TypeKey, 'all'>, string[]> = {
		image: ['jpg', 'jpeg', 'png', 'gif', 'webp', 'svg', 'bmp', 'heic', 'avif', 'tiff'],
		video: ['mp4', 'mov', 'mkv', 'avi', 'webm', 'm4v', 'wmv', 'flv'],
		document: [
			'pdf',
			'doc',
			'docx',
			'xls',
			'xlsx',
			'ppt',
			'pptx',
			'txt',
			'md',
			'odt',
			'rtf',
			'csv'
		],
		audio: ['mp3', 'wav', 'flac', 'aac', 'ogg', 'm4a', 'opus'],
		archive: ['zip', 'rar', '7z', 'tar', 'gz', 'bz2', 'xz']
	};
	const TYPES: { v: TypeKey; l: string }[] = [
		{ v: 'all', l: t('search.type.all', 'All types') },
		{ v: 'image', l: t('search.type.image', 'Images') },
		{ v: 'video', l: t('search.type.video', 'Videos') },
		{ v: 'document', l: t('search.type.document', 'Documents') },
		{ v: 'audio', l: t('search.type.audio', 'Audio') },
		{ v: 'archive', l: t('search.type.archive', 'Archives') }
	];
	const SIZES: { v: SizeKey; l: string }[] = [
		{ v: 'all', l: t('search.size.all', 'Any size') },
		{ v: 'small', l: t('search.size.small', '< 1 MB') },
		{ v: 'medium', l: t('search.size.medium', '1–100 MB') },
		{ v: 'large', l: t('search.size.large', '> 100 MB') }
	];
	const DATES: { v: DateKey; l: string }[] = [
		{ v: 'all', l: t('search.date.all', 'Any time') },
		{ v: 'day', l: t('search.date.day', 'Past 24 hours') },
		{ v: 'week', l: t('search.date.week', 'Past week') },
		{ v: 'month', l: t('search.date.month', 'Past month') },
		{ v: 'year', l: t('search.date.year', 'Past year') }
	];

	const MB = 1024 * 1024;
	function sizeBounds(k: SizeKey): { minSize?: number; maxSize?: number } {
		switch (k) {
			case 'small':
				return { maxSize: MB };
			case 'medium':
				return { minSize: MB, maxSize: 100 * MB };
			case 'large':
				return { minSize: 100 * MB };
			default:
				return {};
		}
	}
	function dateBound(k: DateKey): number | undefined {
		const day = 86400;
		const now = Math.floor(Date.now() / 1000);
		switch (k) {
			case 'day':
				return now - day;
			case 'week':
				return now - 7 * day;
			case 'month':
				return now - 30 * day;
			case 'year':
				return now - 365 * day;
			default:
				return undefined;
		}
	}

	const hasFilters = $derived(typeFilter !== 'all' || sizeFilter !== 'all' || dateFilter !== 'all');
	function clearFilters() {
		typeFilter = 'all';
		sizeFilter = 'all';
		dateFilter = 'all';
	}

	const SORTS: { v: SortBy; l: string }[] = [
		{ v: 'relevance', l: t('search.sort.relevance', 'Relevance') },
		{ v: 'name', l: t('search.sort.name_asc', 'Name A-Z') },
		{ v: 'name_desc', l: t('search.sort.name_desc', 'Name Z-A') },
		{ v: 'date_desc', l: t('search.sort.newest', 'Newest') },
		{ v: 'date', l: t('search.sort.oldest', 'Oldest') },
		{ v: 'size_desc', l: t('search.sort.largest', 'Largest') },
		{ v: 'size', l: t('search.sort.smallest', 'Smallest') }
	];

	// Stale-response guard: rapid-fire query/filter/sort changes each start a
	// full recursive backend search; without the token a SLOW earlier response
	// could resolve after (and clobber) a newer one, and the superseded server
	// work ran to completion. The seq token keeps only the latest result; the
	// AbortController cancels the superseded request outright.
	let runSeq = 0;
	let inflight: AbortController | null = null;

	async function run(q: string) {
		const seq = ++runSeq;
		inflight?.abort();
		inflight = null;
		if (!q) {
			results = null;
			loading = false;
			return;
		}
		const ctl = new AbortController();
		inflight = ctl;
		loading = true;
		error = null;
		try {
			// Trash section searches are always global — there is no folder to scope to.
			const folderId =
				scope === 'folder' && filesStore.section !== 'trash'
					? (filesStore.currentFolder ?? undefined)
					: undefined;
			const fresh = await searchFiles(q, {
				recursive: true,
				sortBy,
				folderId,
				fileTypes: typeFilter === 'all' ? undefined : TYPE_EXT[typeFilter],
				...sizeBounds(sizeFilter),
				modifiedAfter: dateBound(dateFilter),
				signal: ctl.signal
			});
			if (seq !== runSeq) return; // superseded while awaiting
			results = fresh;
		} catch (e) {
			// An aborted request is not an error — a newer run owns the UI.
			if (seq !== runSeq || ctl.signal.aborted) return;
			error = errorMessage(e);
		} finally {
			if (seq === runSeq) loading = false;
		}
	}

	function openFolder(folder: FolderItem) {
		goto(resolve(`/files/${folder.id}`));
	}

	function openFile(file: FileItem) {
		window.open(fileInlineUrl(file.id), '_blank', 'noopener');
	}

	const isEmpty = $derived(!!results && results.files.length === 0 && results.folders.length === 0);

	// Flatten folders + files into one list so the results render through a single
	// windowed list (only the visible rows hit the DOM, even for 100s of hits).
	type SearchEntry = { kind: 'folder'; folder: FolderItem } | { kind: 'file'; file: FileItem };
	const entries = $derived<SearchEntry[]>(
		results
			? [
					...results.folders.map((folder) => ({ kind: 'folder' as const, folder })),
					...results.files.map((file) => ({ kind: 'file' as const, file }))
				]
			: []
	);

	$effect(() => {
		// re-run when query, sort, scope, or any filter changes
		void sortBy;
		void scope;
		void typeFilter;
		void sizeFilter;
		void dateFilter;
		void run(query);
	});

	/**
	 * Page-wide OS drag/drop guard.
	 *
	 * `/search` has no upload path, but without capturing the drop the
	 * browser's default handler navigates to the dropped file — the tab
	 * REPLACES the app with the file itself, which is data-loss-esque
	 * (user loses their in-progress search + any unsaved UI state).
	 * The `ResourceList`-based views (`/photos`, `/shared`, `/trash`, …)
	 * already fire a "wrong drop zone" toast via their `.rl-root`
	 * wrapper when `enableSystemDrop` is false — this handler brings
	 * `/search` to the same contract, and covers the whole viewport
	 * (not just a list surface) so drops on the sticky header /
	 * result-card margins are caught too. Same toast copy + "Go to
	 * Files" action as `ResourceList` for a consistent recovery UX.
	 */
	function onWindowDragOver(e: DragEvent) {
		if (!e.dataTransfer?.types?.includes('Files')) return;
		if (e.defaultPrevented) return;
		e.preventDefault();
		// `dropEffect = 'none'` would tell the browser to REJECT the
		// drop before `drop` fires — the toast path below never runs.
		// Accept at the pointer level and let `onWindowDrop` decide.
		if (e.dataTransfer) e.dataTransfer.dropEffect = 'copy';
	}
	function onWindowDrop(e: DragEvent) {
		if (!e.dataTransfer?.types?.includes('Files')) return;
		if (e.defaultPrevented) return;
		e.preventDefault();
		ui.notify(
			t(
				'resource_list.wrong_drop_zone_msg',
				'Uploads only work in Files — open the Files section and drop there.'
			),
			'warning',
			6000,
			true,
			{
				action: {
					label: t('resource_list.wrong_drop_zone_action', 'Go to Files'),
					// Best-effort recovery — the drop's `DataTransfer` is
					// discarded once this handler returns (browsers don't
					// persist it across a navigation), so we land the
					// user in `/files` where they can re-drag from the OS.
					onClick: () => goto(resolve('/files'))
				}
			}
		);
	}
</script>

<svelte:head><title>{t('search.title', 'Search')} · OxiCloud</title></svelte:head>

<svelte:window ondragover={onWindowDragOver} ondrop={onWindowDrop} />

<div class="page-sticky-header search-head">
	<h1 class="page-title">
		{#if query}{t('search.results_for', { q: query }, 'Results for “{{q}}”')}{:else}{t(
				'search.title',
				'Search'
			)}{/if}
		{#if results?.query_time_ms != null}
			<span class="search-time">({results.query_time_ms} ms)</span>
		{/if}
	</h1>
	{#if query}
		<div class="search-controls">
			{#if filesStore.currentFolder}
				<div class="seg" role="group" aria-label={t('search.scope', 'Scope')}>
					<button
						class="seg__btn"
						class:active={scope === 'all'}
						data-testid="search-scope-all-btn"
						onclick={() => (scope = 'all')}
					>
						{t('search.everywhere', 'Everywhere')}
					</button>
					<button
						class="seg__btn"
						class:active={scope === 'folder'}
						data-testid="search-scope-folder-btn"
						onclick={() => (scope = 'folder')}
					>
						{t('search.this_folder', 'This folder')}
					</button>
				</div>
			{/if}
			<select
				class="sort-select"
				bind:value={typeFilter}
				aria-label={t('search.type_label', 'Type')}
				data-testid="search-type-filter-select"
			>
				{#each TYPES as o (o.v)}<option value={o.v} data-testid={`search-type-${o.v}`}>{o.l}</option
					>{/each}
			</select>
			<select
				class="sort-select"
				bind:value={sizeFilter}
				aria-label={t('search.size_label', 'Size')}
				data-testid="search-size-filter-select"
			>
				{#each SIZES as o (o.v)}<option value={o.v} data-testid={`search-size-${o.v}`}>{o.l}</option
					>{/each}
			</select>
			<select
				class="sort-select"
				bind:value={dateFilter}
				aria-label={t('search.date_label', 'Date')}
				data-testid="search-date-filter-select"
			>
				{#each DATES as o (o.v)}<option value={o.v} data-testid={`search-date-${o.v}`}>{o.l}</option
					>{/each}
			</select>
			<select
				class="sort-select"
				bind:value={sortBy}
				aria-label={t('search.sort_by', 'Sort by')}
				data-testid="search-sort-select"
			>
				{#each SORTS as s (s.v)}<option value={s.v} data-testid={`search-sort-${s.v}`}>{s.l}</option
					>{/each}
			</select>
			{#if hasFilters}
				<button class="clear-filters" data-testid="search-clear-filters-btn" onclick={clearFilters}>
					<Icon name="times" />
					{t('search.clear_filters', 'Clear filters')}
				</button>
			{/if}
		</div>
	{/if}
</div>

{#if loading}
	<div class="search-loading">
		<Icon name="spinner" class="search-loading__spinner" />
		<h2 class="search-loading__text">
			{t('search.searching_for', { q: query }, 'Searching for “{{q}}”…')}
		</h2>
	</div>
{:else if error}
	<EmptyState title={error} error />
{:else if !query}
	<EmptyState title={t('search.prompt', 'Type a query in the search bar above.')} />
{:else if isEmpty}
	<EmptyState icon="search" title={t('search.no_results', 'No results found for this search')} />
{:else if results}
	<div class="files-container">
		<div class="files-list-view" style="--files-list-columns: minmax(200px, 2fr) 1fr 110px 140px">
			<div class="list-header">
				<div>{t('files.col_name', 'Name')}</div>
				<div>{t('files.col_path', 'Path')}</div>
				<div>{t('files.col_size', 'Size')}</div>
				<div>{t('files.col_modified', 'Modified')}</div>
			</div>

			<VirtualList
				items={entries}
				rowHeight={56}
				key={(e) => (e.kind === 'folder' ? e.folder.id : e.file.id)}
			>
				{#snippet row(e)}
					{#if e.kind === 'folder'}
						<div
							class="file-item"
							role="button"
							tabindex="0"
							aria-label={e.folder.name}
							data-testid={e.folder.name}
							onclick={() => openFolder(e.folder)}
							onkeydown={(ev) => ev.key === 'Enter' && openFolder(e.folder)}
						>
							<div class="name-cell">
								<span class="file-icon file-icon--folder"><Icon name="folder" /></span>
								<span>{e.folder.name}</span>
							</div>
							<div class="path-cell">{e.folder.path}</div>
							<div class="size-cell">—</div>
							<div class="date-cell">{formatDate(e.folder.modified_at)}</div>
						</div>
					{:else}
						<div
							class="file-item"
							role="button"
							tabindex="0"
							aria-label={e.file.name}
							data-testid={e.file.name}
							onclick={() => openFile(e.file)}
							onkeydown={(ev) => ev.key === 'Enter' && openFile(e.file)}
						>
							<div class="name-cell">
								<span class="file-icon {fileIconKindClass(iconNameFromClass(e.file.icon_class))}"
									><Icon name={iconNameFromClass(e.file.icon_class)} /></span
								>
								<span>{e.file.name}</span>
							</div>
							<div class="path-cell">{e.file.path}</div>
							<div class="size-cell">{e.file.size != null ? formatBytes(e.file.size) : ''}</div>
							<div class="date-cell">{formatDate(e.file.modified_at)}</div>
						</div>
					{/if}
				{/snippet}
			</VirtualList>
		</div>
	</div>
{/if}

<style>
	.search-head {
		display: flex;
		align-items: center;
		justify-content: space-between;
		gap: var(--space-3);
		flex-wrap: wrap;
	}

	.search-controls {
		display: flex;
		align-items: center;
		gap: var(--space-2);
	}

	.sort-select {
		padding: var(--space-2) var(--space-2-5);
		border: 1px solid var(--color-border);
		border-radius: var(--radius-md);
		background: var(--color-bg-input);
		color: var(--color-text);
	}

	.seg {
		display: flex;
		border: 1px solid var(--color-border);
		border-radius: var(--radius-md);
		overflow: hidden;
	}

	.seg__btn {
		padding: var(--space-2) var(--space-3);
		border: none;
		background: var(--color-bg-surface);
		color: var(--color-text-muted);
		cursor: pointer;
	}

	.seg__btn.active {
		background: var(--color-accent);
		color: var(--color-on-accent);
	}

	.search-time {
		font-size: var(--text-sm);
		font-weight: var(--weight-normal);
		color: var(--color-text-muted);
	}

	.clear-filters {
		display: inline-flex;
		align-items: center;
		gap: 0.35rem;
		padding: var(--space-2) var(--space-3);
		border: 1px solid var(--color-border);
		border-radius: var(--radius-md);
		background: var(--color-bg-surface);
		color: var(--color-text-muted);
		cursor: pointer;
	}

	.clear-filters:hover {
		background: var(--color-bg-hover);
	}

	.search-loading {
		display: flex;
		align-items: center;
		gap: var(--space-3);
		padding: var(--space-4) 0;
		color: var(--color-text-muted);
	}

	.search-loading :global(.search-loading__spinner) {
		font-size: var(--text-xl);
		color: var(--color-accent);
		animation: spin var(--spin-duration) linear infinite;
	}

	.search-loading__text {
		margin: 0;
		font-size: var(--text-lg);
		font-weight: var(--weight-medium);
		color: var(--color-text);
	}
</style>
