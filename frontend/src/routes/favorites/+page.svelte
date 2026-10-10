<script lang="ts">
	import { SvelteMap } from 'svelte/reactivity';
	import { primeContextPage } from '$lib/utils/listContext';
	import Button from '$lib/components/Button.svelte';
	import { useOwnerCache } from '$lib/composables/useOwnerCache.svelte';
	import { errorToast } from '$lib/utils/errors';
	import { goto } from '$app/navigation';
	import { resolve } from '$app/paths';
	import { onMount } from 'svelte';
	import {
		dateBucket,
		fetchFavoritesPage,
		removeFavorite,
		resolveOwnerName,
		sizeBucket,
		typeLabel,
		type FavoritesResourceItem
	} from '$lib/api/endpoints/favorites';
	import { fileDownloadUrl } from '$lib/api/endpoints/files';
	import { renameFile, deleteFile } from '$lib/api/endpoints/files';
	import { renameFolder, deleteFolder } from '$lib/api/endpoints/folders';
	import type { FileItem, FolderItem } from '$lib/api/types';
	import { lazyComponent } from '$lib/composables/lazyComponent.svelte';
	import ResourceList, {
		isFile,
		type ContextAction,
		type GroupByDef,
		type ItemContext
	} from '$lib/components/ResourceList.svelte';
	import { confirmDialog, promptDialog } from '$lib/stores/dialogs.svelte';
	import { folderAccessCached, probeFolderAccess } from '$lib/utils/folderAccess';
	import { t } from '$lib/i18n/index.svelte';

	let raw = $state<FavoritesResourceItem[]>([]);
	let cursor = $state<string | undefined>(undefined);
	let loading = $state(false);
	let error = $state<string | null>(null);
	let groupBy = $state('');
	let reversed = $state(false);
	const owners = useOwnerCache(resolveOwnerName);

	// Favorites view DELIBERATELY doesn't set `showDotfileToggle` on
	// the ResourceList below — favoriting is an explicit "I want to
	// keep an eye on this" action by the user, and hiding a starred
	// dotfile here would contradict that intent. The
	// `preferences.hideDotfiles` toggle is for reducing incidental
	// clutter in algorithmic listings (files / recent / photos), not
	// for overriding user-intentional pins. Trash follows the same
	// principle for a safety-net reason; the general rule: explicit-
	// action surfaces don't filter, algorithmic surfaces do.
	//
	// ResourceList consumes raw `FileItem | FolderItem`; the favorites
	// envelope contributes `favorited_at` via `date` in contextMap.
	// Every item on this page has `is_favorite: true` by construction
	// (the listing SQL hardcodes `TRUE AS is_favorite`), so the star
	// widget lights up universally without any shadow set here.
	const items = $derived(raw.map((it) => it.resource as FileItem | FolderItem));
	// Persistent reactive map, primed per page in `load()` (benches/ROUND16.md §F2)
	// instead of rebuilding a fresh Map that re-hashes the whole accumulated list
	// on every infinite-scroll page.
	const contextMap = new SvelteMap<string, ItemContext>();

	const groupBys: GroupByDef[] = [
		{ key: '', label: t('files.name', 'Name'), orderBy: 'name', icon: 'arrow-up-a-z' },
		{
			key: 'owner',
			label: t('groupby.owner', 'Owner'),
			orderBy: 'owner',
			bucketOf: (item) => item.created_by ?? null,
			labelOf: (id) => owners.label(id)
		},
		{
			key: 'type',
			label: t('groupby.type', 'Type'),
			orderBy: 'type',
			bucketOf: (item) => item.category ?? 'other',
			labelOf: (k) => typeLabel(k)
		},
		{
			key: 'size',
			label: t('groupby.size', 'Size'),
			orderBy: 'size',
			bucketOf: (item) => sizeBucket(isFile(item) ? item.size : null)
		},
		{
			key: 'favoriteDate',
			label: t('groupby.favoriteDate', 'Favorite date'),
			orderBy: 'favorited_at',
			bucketOf: (_item, ctx) => dateBucket(ctx?.date)
		},
		{
			key: 'modifiedAt',
			label: t('groupby.modifiedAt', 'Modified date'),
			orderBy: 'modified_at',
			bucketOf: (item) => dateBucket(item.modified_at)
		}
	];

	async function load(reset = false, orderBy = 'name', rev = reversed) {
		loading = true;
		error = null;
		try {
			const page = await fetchFavoritesPage({
				cursor: reset ? undefined : cursor,
				orderBy,
				reverse: rev,
				resourceTypes: ['file', 'folder']
			});
			raw = reset ? page.items : [...raw, ...page.items];
			primeContextPage(contextMap, reset, page.items, (it) => [
				it.resource.id,
				{ date: it.favorited_at }
			]);
			cursor = page.next_cursor;
			void owners.resolve(page.items.map((i) => i.resource.created_by));
		} catch (e) {
			console.error('favorites: load error', e);
			error = t('errors_loadFailed', 'Failed to load items');
		} finally {
			loading = false;
		}
	}

	function orderByForGroup(): string {
		return groupBys.find((g) => g.key === groupBy)?.orderBy ?? 'name';
	}

	let viewerOpen = $state(false);
	let viewerFile = $state<FileItem | null>(null);

	// The file preview is loaded the first time a file is opened, keeping its
	// module out of this route's initial chunk.
	const fileViewer = lazyComponent(() => import('$lib/components/FileViewer.svelte'));
	const moveDialog = lazyComponent(() => import('$lib/components/MoveDialog.svelte'));
	const shareDialog = lazyComponent(() => import('$lib/components/ShareDialog.svelte'));
	$effect(() => {
		if (viewerOpen) void fileViewer.load();
		if (moveOpen) void moveDialog.load();
		if (shareOpen) void shareDialog.load();
	});

	function open(item: FileItem | FolderItem) {
		if (!isFile(item)) {
			goto(resolve(`/files/${item.id}`));
			return;
		}
		viewerFile = item;
		viewerOpen = true;
	}

	async function unfavorite(item: FileItem | FolderItem) {
		const kind = isFile(item) ? 'file' : 'folder';
		try {
			await removeFavorite(kind, item.id);
			raw = raw.filter((i) => i.resource.id !== item.id);
		} catch (e) {
			errorToast(e);
		}
	}

	// ── Context-menu actions ──────────────────────────────────────────────────
	let moveOpen = $state(false);
	let moveTarget = $state<{ id: string; name: string; kind: 'file' | 'folder' } | null>(null);
	let moveItems = $state<{ id: string; name: string; kind: 'file' | 'folder' }[] | null>(null);
	let shareOpen = $state(false);
	let shareTarget = $state<{ id: string; name: string; kind: 'file' | 'folder' } | null>(null);

	function kindOf(item: FileItem | FolderItem): 'file' | 'folder' {
		return isFile(item) ? 'file' : 'folder';
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
			await load(true, orderByForGroup());
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
			raw = raw.filter((i) => i.resource.id !== item.id);
		} catch (e) {
			errorToast(e);
		}
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

	// See /recent's mirror for the rationale: files carry `folder_id`,
	// folders carry `parent_id`; nullable when the folder is a drive
	// root. Null → no meaningful parent to open.
	function parentFolderId(item: FileItem | FolderItem): string | null {
		// Generated `FolderItem.parent_id` is `string | null | undefined` —
		// the server emits JSON `null` on drive roots, and openapi-ts
		// adds `| undefined` on top because the key is `?: `. Collapse
		// the two "no parent" shapes to `null` so callers downstream
		// keep seeing one signal.
		return isFile(item) ? item.folder_id : (item.parent_id ?? null);
	}

	const contextActions: ContextAction[] = [
		{
			key: 'open_parent',
			label: t('files.open_parent', 'Open parent folder'),
			icon: 'folder-open',
			// Hidden only when there's literally no parent to open
			// (drive-root folders where `parent_id === null`); otherwise
			// the entry is always visible and shows up disabled when the
			// caller lacks read on the parent — a greyed row reads as
			// "you can't do this here" instead of "the option is missing."
			// `folderAccessCached` returns `true`/`false`/`undefined`;
			// disabled fires when the answer is explicitly `false`. On
			// first right-click of a fresh row, `menuPrepare` below has
			// primed the cache so the entry either enables or disables
			// without a "flash of enabled" beforehand.
			visible: (item) => parentFolderId(item) !== null,
			disabled: (item) => {
				const pid = parentFolderId(item);
				return pid === null || folderAccessCached(pid) === false;
			},
			run: (item) => {
				const pid = parentFolderId(item);
				if (pid) goto(resolve(`/files/${pid}`));
			}
		},
		{
			key: 'download',
			label: t('common.download', 'Download'),
			icon: 'download',
			run: downloadItem
		},
		{
			key: 'share',
			label: t('files.share', 'Share'),
			icon: 'share-alt',
			run: (item) => {
				shareTarget = { id: item.id, name: item.name, kind: kindOf(item) };
				shareOpen = true;
			}
		},
		{
			key: 'move',
			label: t('files.move', 'Move'),
			icon: 'arrows-alt',
			run: (item) => {
				moveItems = null;
				moveTarget = { id: item.id, name: item.name, kind: kindOf(item) };
				moveOpen = true;
			}
		},
		{
			// Every row on /favorites IS a favorite, so the entry is always
			// "Remove favorite" — no per-item state lookup needed. Mirrors
			// the star-widget behaviour: click, row un-stars, disappears
			// from the list on next reload. Placed between Move and Rename
			// to match the canonical context-menu order on `/files`.
			key: 'unfavorite',
			label: t('files.unfavorite', 'Remove favorite'),
			icon: 'star-outline',
			run: (item) => void unfavorite(item)
		},
		{ key: 'rename', label: t('common.rename', 'Rename'), icon: 'pen', run: rename },
		{ key: 'delete', label: t('common.delete', 'Delete'), icon: 'trash', danger: true, run: remove }
	];

	// ── Selection + batch ─────────────────────────────────────────────────────
	// Selected items arrive via the batchActions snippet param —
	// ResourceList already derives them (O(selection), not O(N)); a
	// host-side `items.filter(...)` shadow would re-run a second full scan
	// per selection toggle, and its id mirror is unnecessary (the component
	// prunes its own selection when items reload) — benches/ROUND11.md §S1.
	type Selectable = FileItem | FolderItem;

	function batchDownload(sel: Selectable[]) {
		for (const i of sel) downloadItem(i);
	}

	onMount(() => load(true));
</script>

<svelte:head><title>{t('nav.favorites', 'Favorites')} · OxiCloud</title></svelte:head>

<ResourceList
	title={t('nav.favorites', 'Favorites')}
	{items}
	{contextMap}
	resolveOwnerName={(id) => owners.name(id)}
	{loading}
	{error}
	emptyIcon="star"
	emptyText={t('favorites.empty_state', 'No favorites yet')}
	emptyHint={t('favorites.empty_hint', 'Star files and folders to find them here quickly')}
	hasMore={!!cursor}
	onloadmore={() => load(false, orderByForGroup())}
	onopen={open}
	onfavorite={unfavorite}
	onshared={(item) => {
		shareTarget = { id: item.id, name: item.name, kind: kindOf(item) };
		shareOpen = true;
	}}
	showOwner
	showPath
	dateLabel={t('files.col_added', 'Added')}
	selectable
	{contextActions}
	menuPrepare={async (item) => {
		// Lazy folder-access probe — fires only when the user actually
		// opens the context menu on a row, not proactively for every
		// row on load. Cached in the LRU (see `folderAccess.ts`) so
		// subsequent right-clicks on the same folder are instant.
		const pid = parentFolderId(item);
		if (pid) await probeFolderAccess(pid);
	}}
	{groupBys}
	bind:groupBy
	bind:reversed
	onreload={(orderBy, rev) => {
		cursor = undefined;
		load(true, orderBy, rev);
	}}
>
	{#snippet batchActions(sel)}
		<!--
			Favorites-scoped batch cluster: Download stays. Move + Delete
			were destructive-to-content operations carried over from the
			pre-refactor menu; on a favorites *bookmarks* view they
			belong in the row's context menu (rename/move/delete via
			`contextActions`), not in the batch bar. Batch "remove from
			favorite" un-stars the selected rows without touching the
			underlying files — mirrors the per-row favorite star.
		-->
		<Button
			icon="download"
			data-testid="favorites-batch-download-btn"
			onclick={() => batchDownload(sel)}>{t('common.download', 'Download')}</Button
		>
		<Button
			icon="star-outline"
			data-testid="favorites-batch-remove-btn"
			onclick={() => sel.forEach(unfavorite)}>{t('files.unfavorite', 'Remove favorite')}</Button
		>
	{/snippet}
</ResourceList>

{#if fileViewer.component}
	{@const FileViewer = fileViewer.component}
	<FileViewer bind:open={viewerOpen} file={viewerFile} />
{/if}
{#if moveDialog.component}
	{@const MoveDialog = moveDialog.component}
	<MoveDialog
		bind:open={moveOpen}
		item={moveTarget}
		items={moveItems}
		onmoved={() => load(true, orderByForGroup())}
	/>
{/if}
{#if shareDialog.component}
	{@const ShareDialog = shareDialog.component}
	<ShareDialog bind:open={shareOpen} item={shareTarget} />
{/if}
