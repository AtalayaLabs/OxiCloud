<script lang="ts">
	import EmptyState from '$lib/components/EmptyState.svelte';
	import ResourceList, {
		isFile,
		type ContextAction,
		type GroupByDef
	} from '$lib/components/ResourceList.svelte';
	import { errorMessage, errorToast } from '$lib/utils/errors';
	import { goto } from '$app/navigation';
	import { resolve } from '$app/paths';
	import { page } from '$app/state';
	import { searchResources } from '$lib/api/endpoints/search';
	import { fileDownloadUrl, renameFile, deleteFile } from '$lib/api/endpoints/files';
	import { renameFolder, deleteFolder } from '$lib/api/endpoints/folders';
	import {
		addFavorite,
		removeFavorite,
		dateBucket,
		sizeBucket
	} from '$lib/api/endpoints/favorites';
	import type { FileItem, FolderItem, SearchResourceItem, SortBy } from '$lib/api/types';
	import { lazyComponent } from '$lib/composables/lazyComponent.svelte';
	import { folderAccessCached, probeFolderAccess } from '$lib/utils/folderAccess';
	import Icon from '$lib/icons/Icon.svelte';
	import { t } from '$lib/i18n/index.svelte';
	import { confirmDialog, promptDialog } from '$lib/stores/dialogs.svelte';
	import { files as filesStore } from '$lib/stores/files.svelte';
	import { ui } from '$lib/stores/ui.svelte';

	const query = $derived(page.url.searchParams.get('q') ?? '');

	// Folder scope encoded in the URL as `?in=<uuid>` so refresh survives —
	// pre-refactor the scope lived only in `filesStore.currentFolder`, which
	// resets to null on a hard reload. Ed hit this 2026-07-26: refresh
	// silently switched to "Everywhere" and disabled "This folder".
	// `AppShell` also encodes this param when the user searches from a
	// specific `/files/<uuid>`, so the round-trip is symmetric.
	//
	// `?in=` is *sticky* — it stays in the URL even after the user picks
	// "Everywhere" — so they can toggle back to "This folder" without
	// losing the reference. The active-scope flip is signalled separately
	// by `?scope=all` (absent = default to folder when `in` is present).
	const scopeFolderId = $derived(page.url.searchParams.get('in') ?? null);
	const scopeOverride = $derived(page.url.searchParams.get('scope'));
	// A concrete folder for the scope: prefer the URL param (durable across
	// refresh), fall back to whatever folder the Files view has open in
	// this session (the pre-URL-param behaviour).
	const effectiveFolder = $derived(scopeFolderId ?? filesStore.currentFolder ?? null);

	// Rendered as `<h1 class="page-title">` inside ResourceList. Bakes the
	// query time / result count into the title string because ResourceList
	// doesn't (yet) expose a subtitle slot, and lifting the "Xms" affordance
	// into the title is small enough not to warrant one. The bare "Search"
	// label is the empty-query fallback for the browser-tab title path
	// (`<svelte:head>`); when there IS a query, ResourceList never mounts
	// with this string — the `else` branch below owns the render.
	const resultsTitle = $derived.by(() => {
		const base = query
			? t('search.results_for', { q: query }, 'Results for “{{q}}”')
			: t('search.title', 'Search');
		if (queryTimeMs == null) return base;
		const suffix =
			total != null
				? t('search.found_n_in_ms', { n: total, ms: queryTimeMs }, '{{n}} results in {{ms}} ms')
				: `${queryTimeMs} ms`;
		return `${base} · ${suffix}`;
	});

	// Accumulated pages of results. Cursor pagination — a filter/sort/query
	// change resets to page 1, infinite scroll appends via `loadMore()`.
	let raw = $state<SearchResourceItem[]>([]);
	let cursor = $state<string | undefined>(undefined);
	let queryTimeMs = $state<number | null>(null);
	let total = $state<number | undefined>(undefined);
	let loading = $state(false);
	let error = $state<string | null>(null);
	// Sort dimension + direction are surfaced through ResourceList's
	// built-in group-by selector (DisplayModeControls) rather than a
	// standalone sort `<select>`, so /search matches /favorites /
	// /recent / /trash. `groupBy` = active group-by key from the
	// `groupBys` list below; `reversed` = the asc/desc toggle.
	let groupBy = $state('');
	let reversed = $state(false);
	// Derived from the URL — URL is the single source of truth so refresh,
	// bookmarks and shared links all restore the same scope. Rules:
	//   `?scope=all`               → 'all' (explicit "Everywhere" toggle;
	//                                       `in=` may still be present as
	//                                       a sticky fallback for the
	//                                       "This folder" button)
	//   `?in=<uuid>` (no override) → 'folder'
	//   neither                    → 'all'
	const scope = $derived<'all' | 'folder'>(
		scopeOverride === 'all' ? 'all' : effectiveFolder && scopeFolderId ? 'folder' : 'all'
	);
	function setScope(next: 'all' | 'folder') {
		// Build the query string by hand — Svelte's lint flags mutating a
		// stdlib `URLSearchParams`, and we don't need reactivity here.
		//
		// Key point (Ed's 2026-07-26 UX ask): the `in=` param is preserved
		// even when switching to "Everywhere" so "This folder" stays
		// clickable and remembers WHICH folder. The active-scope flip
		// rides on `scope=all` instead.
		const parts: string[] = [];
		if (query) parts.push(`q=${encodeURIComponent(query)}`);
		// Sticky `in=`: keep whatever's already in the URL, or seed it
		// from filesStore when the user first pins "This folder" from a
		// fresh /search visit.
		const stickyFolder = scopeFolderId ?? (next === 'folder' ? filesStore.currentFolder : null);
		if (stickyFolder) {
			parts.push(`in=${encodeURIComponent(stickyFolder)}`);
		}
		if (next === 'all' && stickyFolder) {
			// Only meaningful when there's a folder to override — otherwise
			// the URL is "everywhere by default" and the flag would be noise.
			parts.push('scope=all');
		}
		const target = resolve(parts.length ? `/search?${parts.join('&')}` : '/search');
		// `replaceState: true` keeps the browser back-button meaningful —
		// scope changes are UI state, not navigation. `keepFocus: true`
		// keeps focus on whatever button the user just clicked.
		void goto(target, { replaceState: true, keepFocus: true, noScroll: true });
	}

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

	// ── Group / sort dimensions (shown in the DisplayModeControls dropdown) ──
	// Ed's 2026-07-26 spec: 4 options total —
	//   • Relevance (default, flat)  — search's native ranking
	//   • Name       (flat)           — A-Z with the asc/desc toggle for Z-A
	//   • Size       (grouped)        — bucketed via the shared `sizeBucket`
	//   • Modified   (grouped)        — bucketed via the shared `dateBucket`
	// Omitting `bucketOf` = flat list (see the ResourceList interface).
	// `orderBy` values map 1:1 to the backend `SearchResourcesQuery.order_by`.
	// The asc/desc button binds to `reversed` and passes through to the
	// backend `reverse` flag.
	const groupBys: GroupByDef[] = [
		{
			key: '',
			label: t('search.sort.relevance', 'Relevance'),
			orderBy: 'relevance',
			icon: 'ranking-star'
		},
		{ key: 'name', label: t('files.name', 'Name'), orderBy: 'name', icon: 'arrow-up-a-z' },
		{
			key: 'size',
			label: t('groupby.size', 'Size'),
			orderBy: 'size',
			bucketOf: (item) => sizeBucket(isFile(item) ? item.size : null)
		},
		{
			key: 'modifiedAt',
			label: t('groupby.modifiedAt', 'Modified date'),
			orderBy: 'updated_at',
			bucketOf: (item) => dateBucket(item.modified_at)
		},
		{
			key: 'createdAt',
			label: t('groupby.createdAt', 'Created date'),
			orderBy: 'created_at',
			bucketOf: (item) => dateBucket(item.created_at)
		}
	];

	function orderByForGroup(): string {
		return groupBys.find((g) => g.key === groupBy)?.orderBy ?? 'relevance';
	}

	// Stale-response guard: rapid-fire query/filter/sort changes each start a
	// full recursive backend search; without the token a SLOW earlier response
	// could resolve after (and clobber) a newer one, and the superseded server
	// work ran to completion. The seq token keeps only the latest result; the
	// AbortController cancels the superseded request outright. The same token
	// invalidates any in-flight `loadMore()` when the query/filter changes so
	// its rows never append to a fresh result set.
	let runSeq = 0;
	let inflight: AbortController | null = null;

	function currentQueryParams() {
		// Trash section searches are always global — there is no folder to
		// scope to. Otherwise the folder comes from the URL (`?in=<uuid>`)
		// via `scopeFolderId`, which survives a hard refresh and shared
		// links unlike `filesStore.currentFolder` (session-only, resets on
		// reload).
		const folderId =
			scope === 'folder' && filesStore.section !== 'trash'
				? (effectiveFolder ?? undefined)
				: undefined;
		return {
			recursive: true,
			sortBy: orderByForGroup() as SortBy,
			reverse: reversed,
			folderId,
			fileTypes: typeFilter === 'all' ? undefined : TYPE_EXT[typeFilter],
			...sizeBounds(sizeFilter),
			modifiedAfter: dateBound(dateFilter)
		};
	}

	async function run(q: string) {
		const seq = ++runSeq;
		inflight?.abort();
		inflight = null;
		if (!q) {
			raw = [];
			cursor = undefined;
			queryTimeMs = null;
			total = undefined;
			loading = false;
			error = null;
			return;
		}
		const ctl = new AbortController();
		inflight = ctl;
		loading = true;
		error = null;
		try {
			const fresh = await searchResources(q, { ...currentQueryParams(), signal: ctl.signal });
			if (seq !== runSeq) return; // superseded while awaiting
			raw = fresh.items;
			cursor = fresh.next_cursor;
			queryTimeMs = fresh.query_time_ms;
			total = fresh.total;
		} catch (e) {
			// An aborted request is not an error — a newer run owns the UI.
			if (seq !== runSeq || ctl.signal.aborted) return;
			error = errorMessage(e);
		} finally {
			if (seq === runSeq) loading = false;
		}
	}

	async function loadMore() {
		if (!cursor || loading) return;
		// Snapshot the current seq — if a filter/sort/query change bumps
		// `runSeq` while we're awaiting, we drop this page on the floor
		// (its rows belong to a stale filter set).
		const seq = runSeq;
		const ctl = new AbortController();
		// Don't overwrite `inflight` — that belongs to `run()` and lets a
		// query change abort a page-1 fetch. A concurrent loadMore is
		// harmless: at most one appends because of the seq guard.
		loading = true;
		try {
			const nextPage = await searchResources(query, {
				...currentQueryParams(),
				cursor,
				signal: ctl.signal
			});
			if (seq !== runSeq) return;
			raw = [...raw, ...nextPage.items];
			cursor = nextPage.next_cursor;
			// total/query_time refresh — the server recomputes both per page.
			total = nextPage.total ?? total;
		} catch (e) {
			if (seq !== runSeq || ctl.signal.aborted) return;
			error = errorMessage(e);
		} finally {
			if (seq === runSeq) loading = false;
		}
	}

	// Feed ResourceList the raw resource objects — that's the shared
	// `{FileItem | FolderItem}[]` shape every other /*/resources page
	// uses. Search-specific meta (score/snippet/via) is not surfaced
	// today; a follow-up commit will extend ResourceList with an
	// optional per-row meta slot for the snippet + a "matched: content"
	// chip. Score isn't worth surfacing to end users.
	const items = $derived(raw.map((it) => it.resource as FileItem | FolderItem));

	// ── Per-row actions ──────────────────────────────────────────────────
	// Match the shape of `/favorites` + `/recent`: files open inline in
	// the shared FileViewer (was new-tab pre-refactor — a discontinuity
	// with the rest of the app), folders navigate. Share + move + delete
	// all reuse the same lazy dialogs.
	let viewerOpen = $state(false);
	let viewerFile = $state<FileItem | null>(null);
	let moveOpen = $state(false);
	let moveTarget = $state<{ id: string; name: string; kind: 'file' | 'folder' } | null>(null);
	let shareOpen = $state(false);
	let shareTarget = $state<{ id: string; name: string; kind: 'file' | 'folder' } | null>(null);
	const fileViewer = lazyComponent(() => import('$lib/components/FileViewer.svelte'));
	const moveDialog = lazyComponent(() => import('$lib/components/MoveDialog.svelte'));
	const shareDialog = lazyComponent(() => import('$lib/components/ShareDialog.svelte'));
	$effect(() => {
		if (viewerOpen) void fileViewer.load();
		if (moveOpen) void moveDialog.load();
		if (shareOpen) void shareDialog.load();
	});

	function kindOf(item: FileItem | FolderItem): 'file' | 'folder' {
		return isFile(item) ? 'file' : 'folder';
	}

	function open(item: FileItem | FolderItem) {
		if (!isFile(item)) {
			goto(resolve(`/files/${item.id}`));
			return;
		}
		viewerFile = item;
		viewerOpen = true;
	}

	// Files carry `folder_id`, folders carry `parent_id`; both are nullable
	// at drive roots. Null → no meaningful parent to open. Mirrors the
	// same helper in /favorites and /recent.
	function parentFolderId(item: FileItem | FolderItem): string | null {
		return isFile(item) ? item.folder_id : item.parent_id;
	}

	async function toggleFavorite(item: FileItem | FolderItem) {
		const kind = kindOf(item);
		try {
			if (item.is_favorite) {
				await removeFavorite(kind, item.id);
			} else {
				await addFavorite(kind, item.id);
			}
			// Optimistic in-place update — a full reload would jump the
			// user out of their scroll position on an infinite-scroll
			// page. Mutating the raw envelope entry updates the derived
			// `items` array and ResourceList's star widget flips.
			const idx = raw.findIndex((it) => it.resource.id === item.id);
			if (idx !== -1) {
				raw[idx] = {
					...raw[idx],
					resource: { ...raw[idx].resource, is_favorite: !item.is_favorite }
				};
			}
		} catch (e) {
			errorToast(e);
		}
	}

	function openShareDialog(item: FileItem | FolderItem) {
		shareTarget = { id: item.id, name: item.name, kind: kindOf(item) };
		shareOpen = true;
	}

	function openMoveDialog(item: FileItem | FolderItem) {
		moveTarget = { id: item.id, name: item.name, kind: kindOf(item) };
		moveOpen = true;
	}

	function downloadItem(item: FileItem | FolderItem) {
		if (!isFile(item)) return;
		const a = document.createElement('a');
		a.href = fileDownloadUrl(item.id);
		a.download = item.name;
		document.body.appendChild(a);
		a.click();
		a.remove();
	}

	async function rename(item: FileItem | FolderItem) {
		const name = await promptDialog({
			title: t('common.rename', 'Rename'),
			defaultValue: item.name,
			confirmText: t('common.rename', 'Rename')
		});
		if (!name || name === item.name) return;
		try {
			if (isFile(item)) await renameFile(item.id, name);
			else await renameFolder(item.id, name);
			// Update the row in place so the user keeps their scroll
			// position instead of jumping back to page 1.
			const idx = raw.findIndex((it) => it.resource.id === item.id);
			if (idx !== -1) {
				raw[idx] = { ...raw[idx], resource: { ...raw[idx].resource, name } };
			}
		} catch (e) {
			errorToast(e);
		}
	}

	async function remove(item: FileItem | FolderItem) {
		const ok = await confirmDialog({
			title: t('common.delete', 'Delete'),
			message: t('files.confirm_delete', { name: item.name }, 'Delete "{{name}}"?'),
			confirmText: t('common.delete', 'Delete'),
			danger: true
		});
		if (!ok) return;
		try {
			if (isFile(item)) await deleteFile(item.id);
			else await deleteFolder(item.id);
			raw = raw.filter((it) => it.resource.id !== item.id);
		} catch (e) {
			errorToast(e);
		}
	}

	function openParent(item: FileItem | FolderItem) {
		const pid = parentFolderId(item);
		if (pid) goto(resolve(`/files/${pid}`));
	}

	// Canonical context-menu order — matches `/favorites` and `/files`
	// (Open parent, Download, Share, Move, Rename, Delete) so users
	// don't have to relearn the menu when switching sections.
	const contextActions: ContextAction[] = [
		{
			key: 'open_parent',
			label: t('files.open_parent', 'Open parent folder'),
			icon: 'folder-open',
			// Hidden only when there is literally no parent (drive-root
			// folders where `parent_id === null`). Otherwise stay visible
			// and disable when the caller lacks Read on the parent — a
			// grey entry reads as "you can't do this here" rather than
			// "the option is missing." `folderAccessCached` returns
			// true/false/undefined; disable only on explicit `false`.
			visible: (item) => parentFolderId(item) !== null,
			disabled: (item) => {
				const pid = parentFolderId(item);
				return pid === null || folderAccessCached(pid) === false;
			},
			run: openParent
		},
		{
			key: 'download',
			label: t('common.download', 'Download'),
			icon: 'download',
			visible: isFile,
			run: downloadItem
		},
		{
			key: 'share',
			label: t('files.share', 'Share'),
			icon: 'share-alt',
			run: openShareDialog
		},
		{
			key: 'move',
			label: t('files.move', 'Move'),
			icon: 'arrows-alt',
			run: openMoveDialog
		},
		{ key: 'rename', label: t('common.rename', 'Rename'), icon: 'pen', run: rename },
		{
			key: 'delete',
			label: t('common.delete', 'Delete'),
			icon: 'trash',
			danger: true,
			run: remove
		}
	];

	$effect(() => {
		// re-run when query, sort/direction, scope, or any filter changes
		void groupBy;
		void reversed;
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
	 * ResourceList already fires a "wrong drop zone" toast via its
	 * `.rl-root` wrapper when `enableSystemDrop` is false, but that
	 * covers only the list area — this handler covers the whole
	 * viewport (sticky header, empty-state screen, gaps around the
	 * list). Same toast copy + "Go to Files" action as ResourceList
	 * for a consistent recovery UX.
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

{#if !query}
	<EmptyState title={t('search.prompt', 'Type a query in the search bar above.')} />
{:else}
	<ResourceList
		title={resultsTitle}
		{items}
		{loading}
		{error}
		emptyIcon="search"
		emptyText={t('search.no_results', 'No results found for this search')}
		hasMore={!!cursor}
		onloadmore={loadMore}
		showPath
		showViewToggle
		onopen={open}
		onfavorite={toggleFavorite}
		onshared={openShareDialog}
		{contextActions}
		{groupBys}
		bind:groupBy
		bind:reversed
		onreload={() => {
			// Group-by or asc/desc changed — reset pagination and let the
			// `$effect` above pick up the new state on its next tick (it's
			// already reactive on `groupBy` + `reversed`). Explicit
			// `cursor = undefined` here just guarantees the in-flight
			// `next_cursor` from the OLD sort can't feed a stale page 2.
			cursor = undefined;
		}}
		menuPrepare={async (item) => {
			// Lazy folder-access probe — fires only when the user opens
			// the context menu on a row, not proactively for every row on
			// load. Cached in the LRU (see `folderAccess.ts`) so
			// subsequent right-clicks on the same folder are instant.
			// Mirrors /favorites + /recent so the "Open parent folder"
			// entry lands enabled/disabled without a "flash of enabled"
			// on first right-click.
			const pid = parentFolderId(item);
			if (pid) await probeFolderAccess(pid);
		}}
	>
		{#snippet actions()}
			<!--
				Scope segment is always visible so the affordance is
				discoverable even from a cold `/search?q=…` load; when
				there's no `filesStore.currentFolder` (user typed the URL
				directly, or search bar navigation didn't carry a folder),
				"This folder" disables — clicking it wouldn't have a
				folder to scope to. Pre-refactor the whole segment was
				hidden in that case, which read as "the option was
				removed" (2026-07-26 UX feedback).
			-->
			<div class="seg" role="group" aria-label={t('search.scope', 'Scope')}>
				<button
					class="seg__btn"
					class:active={scope === 'all'}
					data-testid="search-scope-all-btn"
					onclick={() => setScope('all')}
				>
					{t('search.everywhere', 'Everywhere')}
				</button>
				<button
					class="seg__btn"
					class:active={scope === 'folder'}
					data-testid="search-scope-folder-btn"
					disabled={!effectiveFolder}
					title={effectiveFolder
						? undefined
						: t('search.this_folder_disabled', 'Open a folder in Files to scope search to it')}
					onclick={() => setScope('folder')}
				>
					{t('search.this_folder', 'This folder')}
				</button>
			</div>
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
			<!--
				NOTE: sort dimension + asc/desc live in ResourceList's
				built-in DisplayModeControls now (fed by `groupBys` +
				`bind:groupBy` + `bind:reversed` below), matching
				/favorites / /recent / /trash. The old
				`<select bind:value={sortBy}>` was removed with the
				`SORTS` array.
			-->
			{#if hasFilters}
				<button class="clear-filters" data-testid="search-clear-filters-btn" onclick={clearFilters}>
					<Icon name="times" />
					{t('search.clear_filters', 'Clear filters')}
				</button>
			{/if}
		{/snippet}
		{#snippet itemActions(item)}
			<!--
				Per-row "Open parent folder" quick-action — search results
				are context-poor by nature (the path column shows WHERE the
				match is, but jumping there takes an extra right-click on
				every other section). Surface it as a direct button so a
				single click navigates. Same `.btn-action` treatment as
				trash's Restore / Delete and recent's broom, so the row's
				action-cell keeps the visual rhythm shared across sections.
				Hidden when there's no meaningful parent (drive-root
				folders where `parent_id === null`).
			-->
			{#if parentFolderId(item) !== null}
				<button
					class="btn-action btn-action--hover"
					data-testid={`search-open-parent-btn-${item.id}`}
					title={t('files.open_parent', 'Open parent folder')}
					aria-label={t('files.open_parent', 'Open parent folder')}
					onclick={(e) => {
						e.stopPropagation();
						openParent(item);
					}}
				>
					<Icon name="folder-open" />
				</button>
			{/if}
		{/snippet}
	</ResourceList>
{/if}

{#if fileViewer.component}
	{@const FileViewer = fileViewer.component}
	<FileViewer bind:open={viewerOpen} file={viewerFile} />
{/if}
{#if moveDialog.component}
	{@const MoveDialog = moveDialog.component}
	<MoveDialog
		bind:open={moveOpen}
		item={moveTarget}
		onmoved={() => {
			// A move can shift the row out of the current scope (`?in=<uuid>`)
			// or into it, and the SQL name-match count may change. Reload
			// page 1 rather than trying to patch state in place — search
			// state is already reactive on query/scope so a fresh `run()`
			// is cheap and correct.
			void run(query);
		}}
	/>
{/if}
{#if shareDialog.component}
	{@const ShareDialog = shareDialog.component}
	<ShareDialog bind:open={shareOpen} item={shareTarget} />
{/if}

<style>
	/* Filter cluster lives inside ResourceList's action-bar snippet now,
	   but the actual DOM is scoped to THIS component's <style> block —
	   Svelte's scoped selectors still apply because these are declared
	   with the elements they style below.

	   Every color/border here uses tokens; no raw values (Stylelint gate). */
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

	.seg__btn:disabled {
		opacity: 0.5;
		cursor: not-allowed;
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
</style>
