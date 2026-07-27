<script lang="ts">
	import { errorMessage, errorToast } from '$lib/utils/errors';
	import { SvelteMap } from 'svelte/reactivity';
	import { primeContextPage } from '$lib/utils/listContext';
	import { goto } from '$app/navigation';
	import { resolve } from '$app/paths';
	import { onMount } from 'svelte';
	import {
		addFavorite,
		dateBucket,
		removeFavorite,
		resolveOwnerName,
		typeLabel
	} from '$lib/api/endpoints/favorites';
	import { fileDownloadUrl } from '$lib/api/endpoints/files';
	import { folderZipUrl } from '$lib/api/endpoints/folders';
	import { fetchSharedWithMe, type IncomingGrantItem } from '$lib/api/endpoints/grants';
	import type { FileItem, FolderItem } from '$lib/api/types';
	import { lazyComponent } from '$lib/composables/lazyComponent.svelte';
	import { useOwnerCache } from '$lib/composables/useOwnerCache.svelte';
	import ResourceList, {
		isFile,
		type ContextAction,
		type GroupByDef,
		type ItemContext
	} from '$lib/components/ResourceList.svelte';
	import UserAvatar from '$lib/components/UserAvatar.svelte';
	import { t } from '$lib/i18n/index.svelte';
	import { session } from '$lib/stores/session.svelte';

	// External users landing here are the natural audience for the
	// "upgrade to a full account" prompt — they don't own a drive of
	// their own, this view IS their entry point. Internal users don't
	// see the banner even when their /shared-with-me happens to be
	// non-empty (they already have a drive; nothing to upgrade).
	const showUpgradeBanner = $derived(session.isExternalUser);

	let raw = $state<IncomingGrantItem[]>([]);
	let cursor = $state<string | undefined>(undefined);
	let loading = $state(false);
	let error = $state<string | null>(null);
	let groupBy = $state<string>('');
	let reversed = $state(false);

	const sharers = useOwnerCache(resolveOwnerName);

	// Drive resources also surface in `/api/grants/incoming/resources`
	// since the role_grants rewrite, but they don't belong in the
	// file/folder ResourceList — they're reached through the drive
	// picker / breadcrumb. Filter them out so the row UI keeps its
	// file|folder type contract.
	const fileFolderGrants = $derived(raw.filter((it) => it.resource_type !== 'drive'));

	// `granted_at` → `ctx.date`; `granted_by` overrides the owner column
	// so the sharer shows up in the vignette (rather than the resource's
	// intrinsic `created_by`, which is a stranger for grantees).
	const items = $derived(fileFolderGrants.map((it) => it.resource as FileItem | FolderItem));
	// Persistent reactive map, primed per page in `load()` (benches/ROUND16.md §F2)
	// instead of rebuilding a fresh Map that re-hashes the whole accumulated list
	// on every infinite-scroll page. Drives are skipped (they never reach the
	// row UI), so the map covers exactly the displayed `fileFolderGrants`.
	const contextMap = new SvelteMap<string, ItemContext>();

	// Server-supported sort_by values (see grant_handler.rs:615):
	//   granted_at, granted_by, name, type
	// The first entry (no `bucketOf`) renders a flat list sorted by name —
	// the A-Z icon flags it as "sort, not group" so users don't read it as
	// a real bucket dimension. The remaining three are honest groupings and
	// get the default layer-group icon.
	const groupBys: GroupByDef[] = [
		{ key: '', label: t('files.name', 'Name'), orderBy: 'name', icon: 'arrow-up-a-z' },
		{
			key: 'sharedBy',
			label: t('groupby.sharedBy', 'Shared by'),
			orderBy: 'granted_by',
			bucketOf: (_item, ctx) => ctx?.ownerId ?? null,
			labelOf: (id) => sharers.label(id)
		},
		{
			key: 'type',
			label: t('groupby.type', 'Type'),
			orderBy: 'type',
			bucketOf: (item) => item.category ?? 'other',
			labelOf: (k) => typeLabel(k)
		},
		{
			key: 'sharedAt',
			label: t('groupby.sharedAt', 'Shared date'),
			orderBy: 'granted_at',
			bucketOf: (_item, ctx) => dateBucket(ctx?.date)
		}
	];

	function orderByForGroup(): string {
		return groupBys.find((g) => g.key === groupBy)?.orderBy ?? 'granted_at';
	}

	async function load(reset = false, orderBy = 'granted_at', rev = reversed) {
		loading = true;
		error = null;
		try {
			const page = await fetchSharedWithMe({
				cursor: reset ? undefined : cursor,
				orderBy,
				reverse: rev
			});
			raw = reset ? page.items : [...raw, ...page.items];
			primeContextPage(contextMap, reset, page.items, (it) =>
				it.resource_type === 'drive'
					? null
					: [it.resource.id, { date: it.granted_at, ownerId: it.granted_by ?? null }]
			);
			cursor = page.next_cursor;
			// Warm the sharer-name cache so the "Shared by" group headers
			// show real names instead of UUIDs.
			void sharers.resolve(page.items.map((i) => i.granted_by).filter((id): id is string => !!id));
		} catch (e) {
			error = errorMessage(e);
		} finally {
			loading = false;
		}
	}

	let viewerOpen = $state(false);
	let viewerFile = $state<FileItem | null>(null);

	// The file preview is loaded the first time a file is opened, keeping its
	// module out of this route's initial chunk.
	const fileViewer = lazyComponent(() => import('$lib/components/FileViewer.svelte'));
	$effect(() => {
		if (viewerOpen) void fileViewer.load();
	});

	function open(item: FileItem | FolderItem) {
		if (!isFile(item)) {
			goto(resolve(`/files/${item.id}`));
			return;
		}
		viewerFile = item;
		viewerOpen = true;
	}

	/**
	 * Kick off a download using an ephemeral `<a download>` so the file
	 * saves to disk instead of navigating away. Files stream directly
	 * from `/api/files/{id}/content`; folders come back as a server-
	 * side zip via `/api/folders/{id}/zip`.
	 */
	function downloadItem(item: FileItem | FolderItem) {
		const a = document.createElement('a');
		a.href = isFile(item) ? fileDownloadUrl(item.id) : folderZipUrl(item.id);
		a.download = isFile(item) ? item.name : `${item.name}.zip`;
		document.body.appendChild(a);
		a.click();
		a.remove();
	}

	// Context menu ordering mirrors `/files` / `/favorites` / `/recent`:
	//   Download / Download as ZIP  →  (later entries as we grow the menu)
	//   Favorite  →  (destructive actions if / when introduced)
	//
	// Kind-gated download: files show "Download" (direct stream);
	// folders show "Download as ZIP" (server-side archive). Two entries
	// with `visible?` predicates rather than one label that changes,
	// so the `.icon` reads correctly per kind too.
	async function toggleFavorite(item: FileItem | FolderItem) {
		const kind = isFile(item) ? 'file' : 'folder';
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

	// Favorite entry mirrors the star toggle in the row action cell —
	// same wording, same behavior. Context-menu label flips based on
	// `item.is_favorite` so keyboard users get the same state read as
	// the button-hovering ones.
	const contextActions: ContextAction[] = [
		{
			key: 'download',
			label: t('common.download', 'Download'),
			icon: 'download',
			visible: (item) => isFile(item),
			run: downloadItem
		},
		{
			key: 'download_zip',
			label: t('files.download_zip', 'Download as ZIP'),
			icon: 'download',
			visible: (item) => !isFile(item),
			run: downloadItem
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
		}
	];

	onMount(() => load(true));
</script>

<svelte:head><title>{t('nav.shared_with_me', 'Shared with me')} · OxiCloud</title></svelte:head>

{#if showUpgradeBanner}
	<div class="upgrade-banner" role="region" aria-label={t('upgrade.banner_aria', 'Upgrade prompt')}>
		<div class="upgrade-banner__body">
			<strong>{t('upgrade.banner_title', 'Get your own storage')}</strong>
			<span
				>{t(
					'upgrade.banner_body',
					"You're using a guest account. Upgrade to get a personal drive and start uploading files."
				)}</span
			>
		</div>
		<a
			class="upgrade-banner__cta"
			data-testid="shared-with-me-upgrade-btn"
			href={resolve('/upgrade')}
		>
			{t('upgrade.banner_cta', 'Upgrade')}
		</a>
	</div>
{/if}

<ResourceList
	title={t('nav.shared_with_me', 'Shared with me')}
	{items}
	{contextMap}
	resolveOwnerName={(id) => sharers.name(id)}
	{contextActions}
	{loading}
	{error}
	emptyText={t('shared_with_me.empty', 'Nothing has been shared with you yet.')}
	hasMore={!!cursor}
	showOwner={true}
	ownerLabel={t('share.col_shared_by', 'Shared by')}
	dateLabel={t('share.col_shared', 'Shared')}
	{groupBys}
	bind:groupBy
	bind:reversed
	onloadmore={() => load(false, orderByForGroup())}
	onopen={open}
	onfavorite={toggleFavorite}
	onreload={(orderBy, rev) => {
		cursor = undefined;
		load(true, orderBy, rev);
	}}
>
	{#snippet cardOverlay(_item, ctx)}
		<!--
			Sharer avatar anchored to the bottom-right of the CARD (not
			the file-icon). Uses ResourceList's `cardOverlay` slot,
			which renders inside `.file-item` as a sibling of
			`.action-cell` — same positioned ancestor, so the offset
			matches the top-right cluster exactly. Grid view only;
			list view surfaces the sharer through the owner column.

			Positioning lives in the `<style>` block below; the snippet
			just emits the chip and lets CSS place it. `ctx.ownerId`
			(seeded from `granted_by` in `load()`) is the source of
			truth for "who shared this."
		-->
		{#if ctx?.ownerId}
			<span class="shared-with-me__sharer">
				<UserAvatar userId={ctx.ownerId} size={30} />
			</span>
		{/if}
	{/snippet}
	{#snippet bucketLabel(_key, label)}
		<!--
			"Shared by" swimlane header — prefix the label with the
			sharer's avatar so the group is visually anchored to a
			person, not just a UUID-ish name. Other group-by dimensions
			(`type`, `sharedAt`, or the flat `Name` sort) don't have
			an ownerId as the key, so render only the plain label —
			`_key` for those is a category / date-bucket / empty string
			that has no avatar equivalent.
		-->
		{#if groupBy === 'sharedBy' && _key}
			<UserAvatar userId={_key} size={30} />
		{/if}
		<span class="rl-swimlane-header__text">{label}</span>
	{/snippet}
</ResourceList>

{#if fileViewer.component}
	{@const FileViewer = fileViewer.component}
	<FileViewer bind:open={viewerOpen} file={viewerFile} />
{/if}

<style>
	/*
	 * Sharer avatar chip — anchored to the bottom-right of the card
	 * (grid view). Rendered inside `.file-item` via ResourceList's
	 * `cardOverlay` snippet slot, so `position: absolute` climbs to
	 * `.file-item`'s own `position: relative` (the same containing
	 * block `.action-cell` uses in the top-right). The `right` +
	 * `bottom` offset matches `.action-cell`'s `top` + `right`
	 * offset so the two chips sit in the same vertical column at
	 * opposite corners — Ed 2026-07-26.
	 *
	 * The wrapper is `:global` so the scoped-CSS renaming Svelte
	 * applies to the `<span>` this snippet emits doesn't strip our
	 * selector. Only fires inside the grid view — the parent
	 * `{@render cardOverlay}` in ResourceList already gates on
	 * `filesStore.viewMode === 'grid'`, so no need to gate here.
	 */
	:global(.files-grid-view .file-item .shared-with-me__sharer) {
		position: absolute;
		right: 5px;
		/* Pushed 15 px closer to the card bottom so the chip sits at
		   the same vertical band as the item title instead of hovering
		   above it (Ed 2026-07-26). `calc(var(--space-3) + 8px)` was
		   20 px from the bottom edge, right in the thumbnail area
		   overlapping the title's top; 5 px anchors the chip cleanly
		   at the card's bottom-right corner. */
		bottom: 5px;
		z-index: 10;
	}

	.upgrade-banner {
		display: flex;
		align-items: center;
		justify-content: space-between;
		gap: var(--space-4);
		padding: var(--space-3) var(--space-4);
		margin-bottom: var(--space-4);
		background: var(--color-surface-raised);
		border: 1px solid var(--color-border);
		border-radius: var(--radius-md);
	}

	.upgrade-banner__body {
		display: flex;
		flex-direction: column;
		gap: var(--space-1);
		color: var(--color-text);
	}

	.upgrade-banner__body strong {
		font-weight: var(--weight-semibold);
	}

	.upgrade-banner__body span {
		color: var(--color-text-muted);
		font-size: var(--text-sm);
	}

	.upgrade-banner__cta {
		flex-shrink: 0;
		padding: var(--space-2) var(--space-4);
		background: var(--color-accent);
		color: var(--color-accent-contrast);
		text-decoration: none;
		font-weight: var(--weight-medium);
		border-radius: var(--radius-md);
	}

	.upgrade-banner__cta:hover {
		filter: brightness(0.95);
	}

	@media (width <= 600px) {
		.upgrade-banner {
			flex-direction: column;
			align-items: stretch;
		}

		.upgrade-banner__cta {
			text-align: center;
		}
	}
</style>
