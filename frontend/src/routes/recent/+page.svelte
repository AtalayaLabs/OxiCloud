<script lang="ts">
	import Button from '$lib/components/Button.svelte';
	import { useOwnerCache } from '$lib/composables/useOwnerCache.svelte';
	import { errorToast } from '$lib/utils/errors';
	import { goto } from '$app/navigation';
	import { resolve } from '$app/paths';
	import { onMount } from 'svelte';
	import { SvelteMap } from 'svelte/reactivity';
	import { primeContextPage } from '$lib/utils/listContext';
	// Phase 4 — recent/* operations ride the generated SDK directly.
	// `listRecentResources` returns `CursorListResponse` whose `items`
	// is typed as `Array<OutgoingResourceItemDto>` due to a utoipa
	// generic-specialization quirk on the Rust side; the real items
	// are `RecentResourceItemDto`. One cast at the boundary.
	//
	// `RecentResourceItemDto.resource` is `FileDto | FolderDto | DriveDto`
	// on the SDK (`ResourceContentDto`). Server-side invariant: a recent
	// row only ever points at a file or a folder — drives aren't tracked.
	// Tightening the type locally lets `.created_by` reads compile cleanly
	// without per-site `'created_by' in resource` narrowing.
	import {
		clearRecentItems,
		listRecentResources,
		removeFromRecent
	} from '$lib/api/generated/openapi/sdk.gen';
	import type {
		FileDto,
		FolderDto,
		RecentResourceItemDto
	} from '$lib/api/generated/openapi/types.gen';
	type RecentFileOrFolderItem = RecentResourceItemDto & {
		resource: FileDto | FolderDto;
	};
	import { addFavorite, removeFavorite } from '$lib/api/endpoints/favorites';
	import { dateBucket, sizeBucket, typeLabel } from '$lib/utils/resourceBuckets';
	import { resolveOwnerName } from '$lib/utils/ownerName';
	import { fileDownloadUrl, renameFile, deleteFile } from '$lib/api/endpoints/files';
	import { renameFolder, deleteFolder } from '$lib/api/endpoints/folders';
	import type { FileItem, FolderItem, ItemType } from '$lib/api/types';
	import { lazyComponent } from '$lib/composables/lazyComponent.svelte';
	import ResourceList, {
		isFile,
		type ContextAction,
		type GroupByDef,
		type ItemContext
	} from '$lib/components/ResourceList.svelte';
	import { confirmDialog, promptDialog } from '$lib/stores/dialogs.svelte';
	// `preferences.hideDotfiles` + `isDotfile` are read here only to
	// derive `hiddenCount` for the empty-state message — the actual
	// filter is inside ResourceList (gated on `showDotfileToggle`).
	import { preferences } from '$lib/stores/preferences.svelte';
	import { isDotfile } from '$lib/utils/dotfileFilter';
	import { folderAccessCached, probeFolderAccess } from '$lib/utils/folderAccess';
	import { t } from '$lib/i18n/index.svelte';
	import Icon from '$lib/icons/Icon.svelte';

	let raw = $state<RecentFileOrFolderItem[]>([]);
	let cursor = $state<string | undefined>(undefined);
	let loading = $state(false);
	let error = $state<string | null>(null);
	let groupBy = $state('');
	let reversed = $state(false);
	const owners = useOwnerCache(resolveOwnerName);

	// Envelope shape: `accessed_at` → `ctx.date`, `created_by` → `ctx.ownerId`.
	// Recent is a per-user view of items the caller accessed; the "who
	// touched this last" (`updated_by`) semantic is real but adds noise
	// (mostly the current user), so we align with Files / Favorites and
	// show the original author instead. Cross-surface consistency wins
	// over the finer-grained signal.
	//
	// Dotfile hiding is delegated to ResourceList via `showDotfileToggle`
	// — the component reads `preferences.hideDotfiles` and drops matching
	// rows from every downstream reader (bucketing, rendering, select-
	// all). The `hiddenCount` here is derived independently via the
	// shared `isDotfile` predicate purely for the empty-state message
	// below (distinguishes "genuinely empty" from "everything filtered").
	const items = $derived(raw.map((it) => it.resource as FileItem | FolderItem));
	// Persistent reactive map, primed per page in `load()` (benches/ROUND16.md §F2)
	// instead of rebuilding a fresh Map that re-hashes the whole accumulated list
	// on every infinite-scroll page.
	const contextMap = new SvelteMap<string, ItemContext>();
	const hiddenCount = $derived(
		preferences.hideDotfiles ? items.filter((i) => isDotfile(i.name)).length : 0
	);

	const groupBys: GroupByDef[] = [
		{ key: '', label: t('files.name', 'Name'), orderBy: 'name', icon: 'arrow-up-a-z' },
		{
			key: 'owner',
			label: t('groupby.owner', 'Owner'),
			orderBy: 'owner',
			bucketOf: (_item, ctx) => ctx?.ownerId ?? null,
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
			key: 'accessedAt',
			label: t('groupby.accessedAt', 'Accessed date'),
			orderBy: 'accessed_at',
			bucketOf: (_item, ctx) => dateBucket(ctx?.date)
		},
		{
			key: 'modifiedAt',
			label: t('groupby.modifiedAt', 'Modified date'),
			orderBy: 'modified_at',
			bucketOf: (item) => dateBucket(item.modified_at)
		}
	];

	// Recent defaults to most-recently-accessed first (accessed_at DESC).
	async function load(reset = false, orderBy = 'accessed_at', rev = reversed) {
		loading = true;
		error = null;
		try {
			const page = await listRecentResources({
				throwOnError: true,
				query: {
					cursor: reset ? null : (cursor ?? null),
					order_by: orderBy,
					reverse: rev,
					resource_types: 'file,folder',
					limit: 50
				}
			});
			// `CursorListResponse.items` carries the wrong concrete type
			// on the SDK (utoipa generic-specialization quirk); cast once.
			const items = page.items as unknown as RecentFileOrFolderItem[];
			raw = reset ? items : [...raw, ...items];
			primeContextPage(contextMap, reset, items, (it) => [
				it.resource.id,
				{ date: it.accessed_at, ownerId: it.resource.created_by ?? null }
			]);
			cursor = page.next_cursor ?? undefined;
			void owners.resolve(items.map((i) => i.resource.created_by));
		} catch (e) {
			console.error('recent: load error', e);
			error = t('errors_loadFailed', 'Failed to load items');
		} finally {
			loading = false;
		}
	}

	function orderByForGroup(): string {
		return groupBys.find((g) => g.key === groupBy)?.orderBy ?? 'accessed_at';
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

	function kindOf(item: FileItem | FolderItem): ItemType {
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

	/**
	 * Remove a single item from the caller's recent history. The
	 * per-row "broom" affordance replaces the favorite-star that
	 * existed here before — /recent is a history view, so surfacing
	 * "forget this one" is more useful than "favorite this one"
	 * (users go to the item's real home to favorite it).
	 *
	 * Optimistic: the row disappears immediately; if the DELETE
	 * fails, we re-add it at its original position and toast the
	 * error so the state stays honest.
	 */
	async function removeItem(item: FileItem | FolderItem) {
		const kind = kindOf(item);
		const idx = raw.findIndex((it) => it.resource.id === item.id);
		if (idx < 0) return;
		const snapshot = raw[idx];
		raw = raw.filter((it) => it.resource.id !== item.id);
		contextMap.delete(item.id);
		try {
			await removeFromRecent({
				throwOnError: true,
				path: { item_type: kind, item_id: item.id }
			});
		} catch (e) {
			raw = [...raw.slice(0, idx), snapshot, ...raw.slice(idx)];
			contextMap.set(item.id, {
				date: snapshot.accessed_at,
				ownerId: snapshot.resource.created_by ?? null
			});
			errorToast(e);
		}
	}

	async function clearAll() {
		const ok = await confirmDialog({
			title: t('recent.clear', 'Clear recent'),
			message: t('recent.confirm_clear', 'Clear your recent items?'),
			confirmText: t('recent.clear', 'Clear recent')
		});
		if (!ok) return;
		try {
			await clearRecentItems({ throwOnError: true });
			raw = [];
			cursor = undefined;
		} catch (e) {
			errorToast(e);
		}
	}

	// ── Context-menu actions ──────────────────────────────────────────────────
	let moveOpen = $state(false);
	let moveTarget = $state<{ id: string; name: string; kind: ItemType } | null>(null);
	let moveItems = $state<{ id: string; name: string; kind: ItemType }[] | null>(null);
	let shareOpen = $state(false);
	let shareTarget = $state<{ id: string; name: string; kind: ItemType } | null>(null);

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

	// Extract the parent-folder id from any item — files carry `folder_id`
	// (required by the DTO), folders carry `parent_id` (nullable when the
	// folder is a drive root). `null` means "no meaningful parent to open";
	// the "Open parent folder" entry stays hidden in that case.
	function parentFolderId(item: FileItem | FolderItem): string | null {
		// Generated `FolderItem.parent_id` is `string | null | undefined`;
		// collapse to `null` so downstream callers keep one "no parent"
		// signal. See /favorites mirror.
		return isFile(item) ? item.folder_id : (item.parent_id ?? null);
	}

	const contextActions: ContextAction[] = [
		{
			key: 'open_parent',
			label: t('files.open_parent', 'Open parent folder'),
			icon: 'folder-open',
			// Same disabled-not-hidden pattern as /favorites: hide only
			// when there's no parent (drive-root folder), otherwise
			// show and disable when the caller lacks Read on the
			// parent. `menuPrepare` primes the cache before the menu
			// renders so the final enabled/disabled state is correct
			// on the very first right-click of a row.
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
			key: 'favorite_add',
			label: t('files.favorite', 'Add favorite'),
			icon: 'star',
			visible: (item) => !item.is_favorite,
			run: toggleFavorite
		},
		{
			key: 'favorite_remove',
			label: t('files.unfavorite', 'Remove favorite'),
			icon: 'star',
			visible: (item) => item.is_favorite,
			run: toggleFavorite
		},
		{ key: 'rename', label: t('common.rename', 'Rename'), icon: 'pen', run: rename },
		{ key: 'delete', label: t('common.delete', 'Delete'), icon: 'trash', danger: true, run: remove }
	];

	async function toggleFavorite(item: FileItem | FolderItem) {
		const kind = kindOf(item);
		const wasFav = item.is_favorite;
		item.is_favorite = !wasFav;
		try {
			if (wasFav) await removeFavorite(kind, item.id);
			else await addFavorite(kind, item.id);
		} catch (e) {
			errorToast(e);
			item.is_favorite = wasFav;
		}
	}

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

	onMount(() => {
		void load(true);
	});
</script>

<svelte:head><title>{t('nav.recent', 'Recent')} · OxiCloud</title></svelte:head>

<ResourceList
	title={t('nav.recent', 'Recent')}
	{items}
	{contextMap}
	resolveOwnerName={(id) => owners.name(id)}
	{loading}
	{error}
	emptyIcon={hiddenCount > 0 ? 'eye-slash' : 'clock'}
	emptyText={hiddenCount > 0
		? t(
				'recent.empty_hidden_state',
				{ n: hiddenCount },
				'{{n}} recent item(s) hidden by your dotfile preference'
			)
		: t('recent.empty_state', 'No recent files')}
	emptyHint={hiddenCount > 0
		? t('recent.empty_hidden_hint', 'Turn off "Hide dotfiles" in your profile to see them.')
		: t('recent.empty_hint', 'Files you open will appear here')}
	hasMore={!!cursor}
	onloadmore={() => load(false, orderByForGroup())}
	onopen={open}
	onfavorite={toggleFavorite}
	onshared={(item) => {
		shareTarget = { id: item.id, name: item.name, kind: kindOf(item) };
		shareOpen = true;
	}}
	showOwner
	showPath
	dateLabel={t('files.col_opened', 'Opened')}
	showDotfileToggle
	selectable
	{contextActions}
	menuPrepare={async (item) => {
		// Lazy folder-access probe — fires only when the user actually
		// opens the context menu on a row, not proactively for every
		// row on load. Cached in the LRU forever after (per-session);
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
	{#snippet actions()}
		{#if items.length > 0}
			<Button icon="broom" data-testid="recent-clear-btn" onclick={clearAll}
				>{t('recent.clear', 'Clear recent')}</Button
			>
		{/if}
	{/snippet}
	{#snippet batchActions(sel)}
		<!--
			Recent-scoped batch cluster: what makes sense on a HISTORY
			view. Download stays (common bulk fetch). Move + Delete
			were destructive-to-content actions carried over from the
			pre-refactor menu; on a history view they belong in the
			row's context menu (rename/move/delete via `contextActions`
			above), not in the batch bar. Batch "remove from recent"
			mirrors the per-row broom and forgets the selected rows
			from history without touching the files themselves.
		-->
		<Button
			icon="download"
			data-testid="recent-batch-download-btn"
			onclick={() => batchDownload(sel)}>{t('common.download', 'Download')}</Button
		>
		<Button
			icon="broom"
			data-testid="recent-batch-remove-btn"
			onclick={() => sel.forEach(removeItem)}
			>{t('recent.remove_item', 'Remove from recent')}</Button
		>
	{/snippet}
	{#snippet itemActions(item)}
		<!--
			Per-row "broom" — remove this single item from the recent
			history. Replaces the favorite star; on a history view a
			"forget this one" affordance is more useful than a
			favorite gesture. Grid view: the shared corner-cluster
			CSS turns this into a 30x30 scrim pill sitting next to
			the kebab in the top-right of the card. List view: same
			`.btn-action` treatment as trash's Restore / Delete
			buttons at the row's action-cell.
		-->
		<button
			class="btn-action btn-action--hover"
			data-testid={`recent-remove-btn-${item.id}`}
			title={t('recent.remove_item', 'Remove from recent')}
			aria-label={t('recent.remove_item', 'Remove from recent')}
			onclick={(e) => {
				e.stopPropagation();
				void removeItem(item);
			}}
		>
			<Icon name="broom" />
		</button>
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
