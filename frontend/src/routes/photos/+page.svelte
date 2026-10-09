<script lang="ts">
	import EmptyState from '$lib/components/EmptyState.svelte';
	import VirtualRows from '$lib/components/VirtualRows.svelte';
	import { lazyComponent } from '$lib/composables/lazyComponent.svelte';
	import { useSelection } from '$lib/composables/useSelection.svelte';
	import { errorToast } from '$lib/utils/errors';
	import { onMount } from 'svelte';
	import {
		batchTrash,
		fetchPhotos,
		type PhotoItem,
		type PhotosKind
	} from '$lib/api/endpoints/photos';
	import { fetchHashSiblings, type HashSibling } from '$lib/api/endpoints/dedup';
	import DedupSiblingsDialog from '$lib/components/DedupSiblingsDialog.svelte';
	import { peopleEnabled } from '$lib/api/endpoints/people';
	import { fileDownloadUrl, fileThumbnailUrl } from '$lib/api/endpoints/files';
	import { listDrives } from '$lib/api/endpoints/drives';
	import { driveIcon } from '$lib/stores/drives.svelte';
	import type { Drive } from '$lib/api/types';
	import Icon from '$lib/icons/Icon.svelte';
	import { confirmDialog } from '$lib/stores/dialogs.svelte';
	import { preferences } from '$lib/stores/preferences.svelte';
	import { t } from '$lib/i18n/index.svelte';
	import { ui } from '$lib/stores/ui.svelte';
	import { filterDotfiles } from '$lib/utils/dotfileFilter';
	import { dateTimeFormatFor } from '$lib/utils/display';
	import { isVideo, photoTimestamp } from '$lib/utils/media';
	import { PhotoTimeline, type GroupMode, type PhotoRow } from '$lib/utils/photoTimeline';

	type Tab = 'moments' | 'places' | 'people';
	let tab = $state<Tab>('moments');

	// The lightbox, the (maplibre-backed) places map and the people view are all
	// heavy and off the initial path, so each loads on first use: the lightbox
	// when a photo is opened, the map/people views when their tab is selected.
	const photoLightbox = lazyComponent(() => import('$lib/components/PhotoLightbox.svelte'));
	const placesMap = lazyComponent(() => import('$lib/components/PlacesMap.svelte'));
	const peopleView = lazyComponent(() => import('$lib/components/PeopleView.svelte'));
	let peopleAvailable = $state(false);

	let items = $state<PhotoItem[]>([]);
	// Client-side dotfile filter over `items`. Applied here (not
	// server-side) because the filter is a UI-only preference and
	// applies uniformly across every listing surface. Lightbox +
	// grouping consume `visibleItems`; mutations still target `items`
	// (the raw fetched set) so a deletion still removes the photo even
	// if it's currently hidden by the filter.
	const visibleItems = $derived(filterDotfiles(items, preferences.hideDotfiles));
	// Count of items suppressed by the dotfile filter — surfaced in
	// the empty-state hint below so a `.thumbnails/`-only photos view
	// doesn't read as "no photos yet".
	const hiddenCount = $derived(preferences.hideDotfiles ? items.length - visibleItems.length : 0);
	let cursor = $state<string | null>(null);
	let exhausted = $state(false);
	let loading = $state(false);
	let error = $state<string | null>(null);
	let sentinel = $state<HTMLElement | null>(null);
	/** Usable content width of the grid, for the justified layout. */
	let gridWidth = $state(0);

	const GROUP_KEY = 'oxi-photos-group';
	const KIND_KEY = 'oxi-photos-kind';
	const DRIVE_KEY = 'oxi-photos-drive';
	const FAVORITE_KEY = 'oxi-photos-favorite-only';
	let groupMode = $state<GroupMode>('month');
	/**
	 * §6 — media-kind filter. Backed by `?kind=` on the server;
	 * `'all'` sends no query param so the default URL stays short.
	 */
	let kindFilter = $state<PhotosKind>('all');
	/**
	 * §6b — drive-scope filter. `null` is the cross-drive feed; a
	 * uuid restricts to that drive. The drive-select dropdown lists
	 * only drives with `policies.include_in_photo_index === true`
	 * — otherwise the uploaded content never shows up in the
	 * timeline anyway, so picking one would land on an empty view.
	 */
	let driveFilter = $state<string | null>(null);
	/**
	 * Toggle for the "favourites only" filter — passes
	 * `?favorite_only=true` to the server when on. Default off keeps
	 * the full feed. Reset-and-reload fires on flip since the server
	 * returns 400 if a cursor is reused across filter axes.
	 */
	let favoriteFilter = $state(false);
	/**
	 * Drives the dropdown lists. Loaded once on mount; filtered to
	 * entries whose typed policy bag has `include_in_photo_index`
	 * set true (the server-side eligibility rule for the photo
	 * axis).
	 */
	let availableDrives = $state<Drive[]>([]);
	/** Dropdown open/close for the drive-filter trigger. Matches the
	 *  pattern used by `DisplayModeControls` and the /shared kind
	 *  filter — same `.group-by-selector` CSS classes in
	 *  `styles/ported/buttons.css`. */
	let driveFilterOpen = $state(false);
	/** Current label shown on the dropdown trigger — the chosen drive's
	 *  name, or "All drives" when the filter is off. */
	const driveFilterLabel = $derived(
		driveFilter
			? (availableDrives.find((d) => d.id === driveFilter)?.name ??
					t('photos.all_drives', 'All drives'))
			: t('photos.all_drives', 'All drives')
	);
	/** Icon on the trigger button — mirrors the active drive's icon
	 *  when scoped, falls back to the generic `hdd` glyph in the
	 *  cross-drive view. */
	const driveFilterIcon = $derived.by(() => {
		if (!driveFilter) return 'hdd';
		const d = availableDrives.find((x) => x.id === driveFilter);
		return d ? driveIcon(d) : 'hdd';
	});
	// Close the drive-filter dropdown on an outside click. Same
	// mechanism DisplayModeControls uses for the group-by menu.
	$effect(() => {
		if (!driveFilterOpen) return;
		const onDown = (e: MouseEvent) => {
			if (!(e.target as HTMLElement).closest('.drive-filter')) driveFilterOpen = false;
		};
		window.addEventListener('pointerdown', onDown);
		return () => window.removeEventListener('pointerdown', onDown);
	});
	const selected = useSelection();
	let lightbox = $state(-1); // index into `items`, -1 = closed

	$effect(() => {
		if (lightbox >= 0) void photoLightbox.load();
		if (tab === 'places') void placesMap.load();
		else if (tab === 'people') void peopleView.load();
	});

	/** Locale-aware label for a bucket's representative date. */
	function bucketLabel(d: Date, mode: GroupMode): string {
		if (mode === 'year') return `${d.getFullYear()}`;
		if (mode === 'month')
			return dateTimeFormatFor(undefined, { year: 'numeric', month: 'long' }).format(d);
		return dateTimeFormatFor(undefined, {
			weekday: 'long',
			year: 'numeric',
			month: 'long',
			day: 'numeric'
		}).format(d);
	}

	// ── Virtualized row model ────────────────────────────────────────────────
	// Flatten the date groups into a single list of fixed-height rows (a header
	// or a strip of sized tiles) that VirtualRows windows. Because pages arrive
	// newest-first, each append only extends the last group or adds new ones, so
	// PhotoTimeline re-buckets only the fresh page and re-lays-out only the
	// groups that changed — a full scroll stays O(N), not O(N²) (the old
	// `groups`→`photoRows` derive chain re-grouped + re-packed the whole library
	// on every 60-item page). See photoGrouping.bench.test.ts.
	// `sync` mutates the timeline's (non-reactive) internal group/row caches and
	// returns the flat rows. Driven from `$derived.by` for idempotence: if the
	// deps re-fire without an actual append, `sync` sees a non-growing list and
	// safely full-rebuilds — same output as the pure `buildPhotoRows`.
	const timeline = new PhotoTimeline();
	// `mobile` as state fed by one MediaQueryList listener: the derive below
	// re-runs on every page append, and `window.matchMedia(...)` inside it was
	// a per-recompute style/layout read that only changes on viewport-class
	// crossings — now those crossings push the boolean instead.
	let isMobile = $state(false);
	$effect(() => {
		if (typeof window === 'undefined' || typeof window.matchMedia !== 'function') return;
		const mql = window.matchMedia('(max-width: 768px)');
		isMobile = mql.matches;
		const onchange = (e: MediaQueryListEvent) => {
			isMobile = e.matches;
		};
		mql.addEventListener('change', onchange);
		return () => mql.removeEventListener('change', onchange);
	});
	const photoRows = $derived.by<PhotoRow[]>(() =>
		timeline.sync(visibleItems, {
			groupMode,
			// Pinned to `'square'` since the per-user layout toggle was
			// retired alongside the §6 filter wiring; keeping the util
			// signature intact means the justified-packing code stays
			// available if a future preference brings it back. The
			// Flickr-style justified layout + its toolbar button
			// originally shipped in commit 75ee9b7c
			// (`feat(photos): justified (aspect-preserving) layout
			// option`, Jun 2026) — git-show it to recover the toggle
			// markup, localStorage persistence, and the i18n copy.
			layoutMode: 'square',
			width: gridWidth,
			mobile: isMobile,
			timestampOf: photoTimestamp,
			labelOf: bucketLabel
		})
	);

	async function loadMore() {
		if (loading || exhausted) return;
		loading = true;
		error = null;
		try {
			const page = await fetchPhotos(60, {
				cursor,
				kind: kindFilter,
				driveId: driveFilter,
				favoriteOnly: favoriteFilter
			});
			items = [...items, ...page.items];
			cursor = page.nextCursor;
			if (!page.nextCursor) exhausted = true;
		} catch (e) {
			error = e instanceof Error ? e.message : String(e);
			exhausted = true;
		} finally {
			loading = false;
		}
	}

	function setGroupMode(m: GroupMode) {
		if (groupMode === m) return;
		groupMode = m;
		if (typeof localStorage !== 'undefined') localStorage.setItem(GROUP_KEY, m);
	}

	/**
	 * Called on every filter change (`kindFilter`, `driveFilter`).
	 * Pagination state keys off the server-issued cursor, and the
	 * server returns 400 if a cursor is reused across a filter flip
	 * — so flipping either filter MUST reset `items` / `cursor` /
	 * `exhausted` and refetch from page 1. Selection also clears:
	 * a photo selected under one filter may not exist in the next
	 * view, and the batch bar would otherwise refer to invisible
	 * ids.
	 */
	function resetAndReload() {
		items = [];
		cursor = null;
		exhausted = false;
		selected.clear();
		void loadMore();
	}

	function setKindFilter(k: PhotosKind) {
		if (kindFilter === k) return;
		kindFilter = k;
		if (typeof localStorage !== 'undefined') localStorage.setItem(KIND_KEY, k);
		resetAndReload();
	}

	function setDriveFilter(id: string | null) {
		if (driveFilter === id) return;
		driveFilter = id;
		if (typeof localStorage !== 'undefined') {
			if (id) localStorage.setItem(DRIVE_KEY, id);
			else localStorage.removeItem(DRIVE_KEY);
		}
		resetAndReload();
	}

	function toggleFavoriteFilter() {
		favoriteFilter = !favoriteFilter;
		if (typeof localStorage !== 'undefined') {
			if (favoriteFilter) localStorage.setItem(FAVORITE_KEY, 'true');
			else localStorage.removeItem(FAVORITE_KEY);
		}
		resetAndReload();
	}

	/** A plain tile click toggles selection once anything is selected, else opens the lightbox. */
	function onTileClick(p: PhotoItem) {
		if (selected.size > 0) selected.toggle(p.id);
		// Lightbox index refers to what's actually rendered — grouping
		// loops `visibleItems`, so the index space must too. If we
		// used `items` here a hidden photo could ride the paging
		// buttons even though it doesn't appear in the grid.
		else lightbox = visibleItems.findIndex((x) => x.id === p.id);
	}

	function onDeletePhoto(id: string) {
		items = items.filter((p) => p.id !== id);
		selected.delete(id);
	}

	function downloadSelected() {
		for (const id of selected.ids) {
			const a = document.createElement('a');
			a.href = fileDownloadUrl(id);
			a.download = '';
			document.body.appendChild(a);
			a.click();
			a.remove();
		}
	}

	// §9b Layer 2 dedup chooser state. Opened only on the single-select
	// path when the clicked tile carries `has_blob_siblings: true` — the
	// listing-level signal that the §9 within-drive DISTINCT ON hid at
	// least one other visible file referencing the same bytes. For
	// multi-select trashing we fall back to the plain confirm; the
	// per-tile sibling fan-out can diverge arbitrarily across a mixed
	// selection and a chooser-of-choosers is not what the UX needs.
	let dedupDialogOpen = $state(false);
	let dedupSiblings = $state<HashSibling[]>([]);
	let dedupTruncated = $state(false);
	let dedupOriginId = $state<string>('');

	async function trashSelected() {
		const ids = selected.values();

		// Single-tile trash where the server flagged `has_blob_siblings:
		// true` — hand off to the dedup chooser so the user can decide
		// which copies to actually trash. On any dedup-fetch failure we
		// degrade to the plain confirm rather than block the delete
		// path; the user can still trash what they asked for.
		if (ids.length === 1) {
			const target = items.find((p) => p.id === ids[0]);
			if (target && target.has_blob_siblings && target.content_hash) {
				try {
					const resp = await fetchHashSiblings(target.content_hash);
					if (resp && resp.siblings.length > 0) {
						dedupSiblings = resp.siblings;
						dedupTruncated = resp.truncated;
						dedupOriginId = target.id;
						dedupDialogOpen = true;
						return;
					}
				} catch {
					// Fall through to the simple confirm — a failed probe
					// should not prevent the user from trashing the one
					// tile they explicitly selected.
				}
			}
		}

		const ok = await confirmDialog({
			title: t('photos.delete', 'Delete photos'),
			message: t('photos.confirm_delete', { n: ids.length }, 'Move {{n}} photos to trash?'),
			confirmText: t('common.delete', 'Delete'),
			danger: true
		});
		if (!ok) return;
		await runBatchTrash(ids);
	}

	async function runBatchTrash(ids: string[]) {
		try {
			const trashed = await batchTrash(ids);
			if (trashed.size > 0) {
				items = items.filter((p) => !trashed.has(p.id));
				for (const id of trashed) selected.delete(id);
			}
			if (trashed.size < ids.length) {
				ui.notify(
					t(
						'photos.trash_partial',
						{ ok: trashed.size, total: ids.length },
						'{{ok}} of {{total}} moved to trash.'
					),
					'warning'
				);
			} else {
				ui.notify(t('photos.trashed', { n: trashed.size }, '{{n}} moved to trash.'), 'success');
			}
		} catch (e) {
			errorToast(e);
		}
	}

	function onDedupConfirm(ids: string[]) {
		if (ids.length === 0) return;
		void runBatchTrash(ids);
	}

	/**
	 * A tile thumbnail failed to load — no server thumbnail yet (SVGs the backend
	 * can't rasterise; a video whose server-side frame extraction is still running
	 * or unavailable) or a transient error. Hide the broken <img> so the
	 * always-present placeholder (and, for videos, the play badge) shows through.
	 *
	 * Video thumbnails are now produced server-side (ffmpeg) on upload through the
	 * same WebP pipeline as photos — the browser no longer re-downloads the video
	 * to extract a frame.
	 */
	function onThumbError(e: Event) {
		(e.currentTarget as HTMLImageElement).style.display = 'none';
	}

	onMount(() => {
		const savedGroup = typeof localStorage !== 'undefined' ? localStorage.getItem(GROUP_KEY) : null;
		if (savedGroup === 'day' || savedGroup === 'month' || savedGroup === 'year')
			groupMode = savedGroup;
		const savedKind = typeof localStorage !== 'undefined' ? localStorage.getItem(KIND_KEY) : null;
		if (savedKind === 'all' || savedKind === 'photo' || savedKind === 'video')
			kindFilter = savedKind;
		if (typeof localStorage !== 'undefined' && localStorage.getItem(FAVORITE_KEY) === 'true')
			favoriteFilter = true;
		const savedDrive = typeof localStorage !== 'undefined' ? localStorage.getItem(DRIVE_KEY) : null;
		// Restored unconditionally — if the drive is since gone (deleted,
		// policy flipped off), the server returns an empty page via the
		// anti-enum path and the dropdown will drop the stale entry once
		// the drives load lands below.
		if (savedDrive) driveFilter = savedDrive;
		// Hydrate the drive-selector list. Filter to drives that opt into
		// the photo axis — picking a non-opted-in drive would always land
		// on an empty view, so there is no point surfacing them. Failure
		// is non-fatal: the dropdown stays empty, the cross-drive feed
		// still works.
		void listDrives()
			.then((drives) => {
				availableDrives = drives.filter(
					(d) =>
						(d.policies as { include_in_photo_index?: boolean })?.include_in_photo_index === true
				);
				// If a restored `driveFilter` isn't in the eligible set,
				// drop back to the cross-drive view silently.
				if (driveFilter && !availableDrives.some((d) => d.id === driveFilter)) {
					driveFilter = null;
					if (typeof localStorage !== 'undefined') localStorage.removeItem(DRIVE_KEY);
				}
			})
			.catch(() => {
				/* intentional: dropdown stays empty, cross-drive feed still works */
			});
		void loadMore();
		void peopleEnabled().then((ok) => (peopleAvailable = ok));
		if (!sentinel) return;
		const obs = new IntersectionObserver(
			(entries) => {
				if (entries.some((e) => e.isIntersecting)) void loadMore();
			},
			{ rootMargin: '600px' }
		);
		obs.observe(sentinel);
		return () => obs.disconnect();
	});

	const MODES: GroupMode[] = ['day', 'month', 'year'];
</script>

<svelte:head><title>{t('nav.photos', 'Photos')} · OxiCloud</title></svelte:head>

<!-- Title + subnav live ABOVE the sticky block — they scroll away
     with the page so vertical space is only paid for them while the
     user is near the top. Only the Moments toolbar sticks (next
     block) so the batch cluster stays reachable during long scrolls. -->
<div class="photos-head">
	<h1 class="page-title">{t('nav.photos', 'Photos')}</h1>
	<div class="photos-subnav" role="tablist" aria-label={t('nav.photos', 'Photos')}>
		<button
			class="subnav__tab"
			class:active={tab === 'moments'}
			role="tab"
			aria-selected={tab === 'moments'}
			data-testid="photos-tab-moments"
			onclick={() => (tab = 'moments')}
		>
			{t('photos.tab_moments', 'Moments')}
		</button>
		<button
			class="subnav__tab"
			class:active={tab === 'places'}
			role="tab"
			aria-selected={tab === 'places'}
			data-testid="photos-tab-places"
			onclick={() => (tab = 'places')}
		>
			{t('photos.tab_places', 'Places')}
		</button>
		{#if peopleAvailable}
			<button
				class="subnav__tab"
				class:active={tab === 'people'}
				role="tab"
				aria-selected={tab === 'people'}
				data-testid="photos-tab-people"
				onclick={() => (tab = 'people')}
			>
				{t('photos.tab_people', 'People')}
			</button>
		{/if}
	</div>
</div>

<!-- Sticky block — carries ONLY the Moments toolbar. Title + subnav
     above have already scrolled off; the toolbar (and the batch
     cluster inside it) stays reachable during long gallery scrolls. -->
<div class="page-sticky-header">
	{#if tab === 'moments'}
		<!-- Toolbar uses the shared `.actions-bar` class from
		     `styles/ported/content.css` (same contract as /files,
		     /favorites, /recent, /trash via ResourceList's ActionBar).
		     Fixed 60px height eliminates layout shift when
		     `BatchSelectionBar` mounts; `justify-content: space-between`
		     distributes the always-present start slot (`.action-buttons`
		     — holds the batch pill) and the end slot (filter clusters).
		     The start slot carries `flex: auto` globally so the batch
		     pill fills the leading space, pushing filter clusters to
		     the trailing edge. -->
		<div class="actions-bar">
			<!-- Start slot. `.action-buttons` always renders (reserves
			     the leading space + carries `flex: auto` so the slot
			     fills); when selection is non-empty it ALSO gets the
			     `.batch-selection-bar` modifier so the shared
			     `styles/ported/batchToolbar.css` paints the pill
			     styling (background, padding, rounded corners) on the
			     same element. Matches ResourceList's inline pattern on
			     /files — SAME element carries BOTH classes
			     simultaneously. -->
			<div class="action-buttons" class:batch-selection-bar={selected.size > 0}>
				{#if selected.size > 0}
					<button
						class="batch-bar-close"
						title={t('common.clear', 'Clear selection')}
						aria-label={t('common.clear', 'Clear selection')}
						data-testid="photos-batch-bar-clear-btn"
						onclick={() => selected.clear()}
					>
						<Icon name="times" />
					</button>
					<span class="batch-bar-count">
						{t('files.selected_count', { count: selected.size }, '{{count}} selected')}
					</span>
					<div class="batch-bar-actions">
						<button
							class="batch-btn"
							title={t('common.download', 'Download')}
							data-testid="photos-batch-download-btn"
							onclick={downloadSelected}
						>
							<Icon name="download" />
							<span>{t('common.download', 'Download')}</span>
						</button>
						<button
							class="batch-btn batch-btn-danger"
							title={t('common.delete', 'Delete')}
							data-testid="photos-batch-delete-btn"
							onclick={trashSelected}
						>
							<Icon name="trash" />
							<span>{t('common.delete', 'Delete')}</span>
						</button>
					</div>
				{/if}
			</div>
			<div class="actions-bar__end">
				<div class="seg" role="group" aria-label={t('photos.group_by', 'Group by')}>
					{#each MODES as m (m)}
						<button class="seg__btn" class:active={groupMode === m} onclick={() => setGroupMode(m)}>
							{t(`photos.${m}`, m)}
						</button>
					{/each}
				</div>
				<div class="seg" role="group" aria-label={t('photos.filter_kind', 'Media type')}>
					<button
						class="seg__btn"
						class:active={kindFilter === 'all'}
						title={t('photos.kind.all', 'All')}
						aria-label={t('photos.kind.all', 'All')}
						data-testid="photos-kind-all-btn"
						onclick={() => setKindFilter('all')}
					>
						<Icon name="images" />
					</button>
					<button
						class="seg__btn"
						class:active={kindFilter === 'photo'}
						title={t('photos.kind.photo', 'Photos')}
						aria-label={t('photos.kind.photo', 'Photos')}
						data-testid="photos-kind-photo-btn"
						onclick={() => setKindFilter('photo')}
					>
						<Icon name="image" />
					</button>
					<button
						class="seg__btn"
						class:active={kindFilter === 'video'}
						title={t('photos.kind.video', 'Videos')}
						aria-label={t('photos.kind.video', 'Videos')}
						data-testid="photos-kind-video-btn"
						onclick={() => setKindFilter('video')}
					>
						<Icon name="video" />
					</button>
				</div>
				{#if availableDrives.length > 0}
					<!-- Drive filter — uses the shared `.group-by-selector`
			     dropdown classes (styles/ported/buttons.css) so photos,
			     /shared, and every `DisplayModeControls` consumer share
			     the same trigger-button + popup pattern. -->
					<div class="group-by-selector drive-filter" data-testid="photos-drive-filter">
						<button
							type="button"
							class="toggle-btn group-by-btn active"
							title={t('photos.filter_drive', 'Drive')}
							aria-haspopup="true"
							aria-expanded={driveFilterOpen}
							data-testid="photos-drive-filter-btn"
							onclick={(e) => {
								e.stopPropagation();
								driveFilterOpen = !driveFilterOpen;
							}}
						>
							<Icon name={driveFilterIcon} />
							<span class="group-by-label">{driveFilterLabel}</span>
						</button>
						{#if driveFilterOpen}
							<div
								class="group-by-menu"
								role="menu"
								tabindex="-1"
								onclick={(e) => e.stopPropagation()}
								onkeydown={(e) => e.key === 'Escape' && (driveFilterOpen = false)}
							>
								<button
									type="button"
									class="group-by-option"
									class:active={driveFilter === null}
									data-testid="photos-drive-filter-all"
									onclick={() => {
										setDriveFilter(null);
										driveFilterOpen = false;
									}}
								>
									<Icon name="hdd" />
									{t('photos.all_drives', 'All drives')}
								</button>
								{#each availableDrives as drive (drive.id)}
									<button
										type="button"
										class="group-by-option"
										class:active={driveFilter === drive.id}
										data-testid={`photos-drive-filter-${drive.id}`}
										onclick={() => {
											setDriveFilter(drive.id);
											driveFilterOpen = false;
										}}
									>
										<Icon name={driveIcon(drive)} />
										{drive.name}
									</button>
								{/each}
							</div>
						{/if}
					</div>
				{/if}
				<!-- Favourites-only toggle. Same `.toggle-btn` pattern as the
		     dotfile eye on DisplayModeControls, with a `.favorite-btn`
		     modifier that opts OUT of the shared active-state
		     background change — the only visible toggle signal is
		     the star's fill colour (grey → gold), matching the
		     favorite-star treatment on the file list. -->
				<button
					type="button"
					class="toggle-btn favorite-btn"
					class:active={favoriteFilter}
					title={favoriteFilter
						? t('photos.filter_favorite_on', 'Showing favourites only — click to show all')
						: t('photos.filter_favorite_off', 'Show favourites only')}
					aria-label={t('photos.filter_favorite', 'Favourites only')}
					aria-pressed={favoriteFilter}
					data-testid="photos-favorite-filter-btn"
					onclick={toggleFavoriteFilter}
				>
					<Icon name="star" />
				</button>
			</div>
		</div>
	{/if}
</div>

{#if tab === 'moments'}
	{#if error}
		<p class="status status--error" role="alert">{error}</p>
	{:else if visibleItems.length === 0 && exhausted}
		{#if hiddenCount > 0}
			<EmptyState
				icon="eye-slash"
				title={t(
					'photos.empty_hidden',
					{ n: hiddenCount },
					'{{n}} photo(s) hidden by your dotfile preference'
				)}
				hint={t(
					'photos.empty_hidden_hint',
					'Turn off "Hide dotfiles" in your profile to see them.'
				)}
			/>
		{:else}
			<EmptyState
				icon="images"
				title={t('photos.empty', 'No photos yet.')}
				hint={t(
					'photos.empty_hint',
					'Photos and videos you upload will appear here, grouped by date.'
				)}
			/>
		{/if}
	{:else}
		<div class="photos-area">
			<div class="photos-measure" bind:clientWidth={gridWidth}>
				{#if photoRows.length}
					<VirtualRows rows={photoRows} overscan={1000}>
						{#snippet row(r)}
							{#if r.kind === 'header'}
								<div class="photos-group" style:height="{r.height}px">
									{r.label} <span class="photos-group__count">{r.count}</span>
								</div>
							{:else}
								<div class="photos-strip" style:height="{r.height}px" style:gap="{r.gap}px">
									{#each r.tiles as cell (cell.file.id)}
										{@render tile(cell.file, `width:${cell.w}px;height:${cell.h}px`)}
									{/each}
								</div>
							{/if}
						{/snippet}
					</VirtualRows>
				{/if}
			</div>
		</div>
	{/if}

	<div bind:this={sentinel} class="sentinel" aria-hidden="true"></div>
	<!-- Loading indicator hidden for the first ~1s via a CSS animation
	     delay (feedback_ui_css_first: no JS setTimeout for visual
	     timing). The element always mounts while `loading` is true;
	     a sub-second fetch unmounts before the delay elapses, so the
	     indicator never fades in and the filter flip renders cleanly
	     with no flash. Longer requests surface the status normally. -->
	{#if loading}
		<p class="status status--delayed" role="status" aria-live="polite">
			{t('common.loading', 'Loading…')}
		</p>
	{/if}

	{#if photoLightbox.component}
		{@const PhotoLightbox = photoLightbox.component}
		<!-- Lightbox operates on `visibleItems` — indices align with
		     what the grid rendered, so next/prev never surfaces a
		     hidden photo the user can't see in the grid behind. -->
		<PhotoLightbox items={visibleItems} bind:index={lightbox} onDelete={onDeletePhoto} />
	{/if}
{:else if tab === 'places'}
	{#if placesMap.component}
		{@const PlacesMap = placesMap.component}
		<PlacesMap />
	{/if}
{:else if tab === 'people'}
	{#if peopleView.component}
		{@const PeopleView = peopleView.component}
		<PeopleView />
	{/if}
{/if}

{#snippet tile(photo: PhotoItem, sizeStyle?: string)}
	<div class="photo-tile" class:selected={selected.has(photo.id)} style={sizeStyle}>
		<button
			class="photo-tile__open"
			data-testid={`photo-tile-${photo.id}`}
			onclick={() => onTileClick(photo)}
		>
			<!-- Always-present placeholder: the thumbnail <img> overlays it and, when
			     it can't load (no server thumbnail, e.g. SVG), hides itself to reveal
			     this default rather than the browser's broken-image glyph. -->
			<span class="photo-tile__placeholder" aria-hidden="true"><Icon name="file-image" /></span>
			<img
				src={fileThumbnailUrl(photo.id, 'preview')}
				srcset={`${fileThumbnailUrl(photo.id, 'icon')} 150w, ${fileThumbnailUrl(photo.id, 'preview')} 400w, ${fileThumbnailUrl(photo.id, 'large')} 800w`}
				sizes="(max-width: 768px) 33vw, 200px"
				alt={photo.name}
				loading="lazy"
				decoding="async"
				onerror={onThumbError}
			/>
			{#if isVideo(photo)}
				<span class="photo-tile__video-badge" aria-hidden="true"><Icon name="play" /></span>
			{/if}
		</button>
		<button
			class="photo-tile__check"
			class:on={selected.has(photo.id)}
			aria-label={t('common.select', 'Select')}
			data-testid={`photo-tile-check-${photo.id}`}
			onclick={() => selected.toggle(photo.id)}
		>
			<Icon name="check" />
		</button>
	</div>
{/snippet}

<DedupSiblingsDialog
	bind:open={dedupDialogOpen}
	siblings={dedupSiblings}
	truncated={dedupTruncated}
	originId={dedupOriginId}
	onconfirm={onDedupConfirm}
/>

<style>
	.photos-head {
		display: flex;
		align-items: center;
		justify-content: space-between;
		gap: var(--space-3);
		flex-wrap: wrap;
		/* No top/left padding — the parent `.content-area` already
		   provides the gutter, and the sticky `.actions-bar` below
		   shouldn't inherit a double-indent on its leading edge. */
		padding: 0 1rem 0 0;
	}

	.page-title {
		/* Keep the shared `.page-title` margin-bottom (var(--space-5))
		   from `styles/ported/content.css` so Photos sits at the same
		   vertical rhythm as /files / /favorites / /recent / /trash.
		   Only override font-size + colour-token to match the Photos
		   look (slightly smaller, dedicated heading colour). */
		font-size: 1.5rem;
		color: var(--color-text-heading);
	}

	.photos-subnav {
		display: flex;
		gap: var(--space-1);
	}

	.subnav__tab {
		padding: var(--space-2) var(--space-3);
		border: none;
		border-bottom: 2px solid transparent;
		background: none;
		color: var(--color-text-muted);
		cursor: pointer;
		font-size: var(--text-base);
	}

	.subnav__tab.active {
		color: var(--color-accent);
		border-bottom-color: var(--color-accent);
	}

	/* Toolbar visual contract (fixed height, flex row, padding) comes
	   from the shared `.actions-bar` rule in
	   `styles/ported/content.css` — Photos only adds a side-margin to
	   align with the sticky-header padding and `align-items: center`
	   so the segmented controls / dropdown / star all vertically
	   centre on the 60 px row the shared rule reserves. */
	.actions-bar {
		align-items: center;
		margin-left: 1rem;
		margin-right: 1rem;
	}

	/* End slot — groups the group-by segment, kind filter, drive
	   dropdown, and favourites toggle so the `.actions-bar`'s
	   `justify-content: space-between` sees exactly two children:
	   `.action-buttons` (start, auto-grow) and this trailing cluster. */
	.actions-bar__end {
		display: flex;
		align-items: center;
		gap: var(--space-3);
	}

	/* The shared `.batch-selection-bar` rule in
	   `styles/ported/batchToolbar.css` carries
	   `transform: translateY(-8px)` — a layout-specific hack for the
	   /files ActionBar that pulls the pill 8 px up to connect
	   visually to the row above. Inside Photos' sticky header that
	   overflow bleeds into the subnav row and the pill reads as
	   mis-aligned against `.actions-bar__end`. Zero the transform
	   locally so the pill vertical-centres on the 60 px row like
	   every other control in the toolbar. */
	.actions-bar :global(.batch-selection-bar) {
		transform: none;
	}

	.seg {
		display: flex;
		border: 1px solid var(--color-border);
		border-radius: var(--radius-md);
		overflow: hidden;
	}

	.seg__btn {
		display: inline-flex;
		align-items: center;
		justify-content: center;
		/* `gap` spaces an optional icon from its label without
		   requiring every button to carry both — group-by buttons
		   are text-only, kind-filter buttons are icon-only, and
		   both lay out correctly with the same base rules. */
		gap: var(--space-2);
		/* Pin the row height to `.toggle-btn` (32 px in
		   `styles/ported/buttons.css`) so the segmented controls
		   sit flush with the dropdown trigger + favourites toggle.
		   Padding stays for the icon's horizontal breathing room. */
		height: 32px;
		padding: 0 var(--space-3);
		border: none;
		background: var(--color-bg-surface);
		color: var(--color-text-muted);
		cursor: pointer;
		text-transform: capitalize;
	}

	.seg__btn.active {
		background: var(--color-accent);
		color: var(--color-on-accent);
	}

	/* §6b drive-scope dropdown. All visual weight comes from the
	   shared `.group-by-selector` / `.group-by-btn` / `.group-by-menu`
	   / `.group-by-option` classes in `styles/ported/buttons.css`;
	   the `.drive-filter` modifier exists only as a selector hook
	   for the outside-click dismiss listener in the parent script. */
	.drive-filter {
		position: relative;
	}

	/* Favourites-only toggle: inherit the base `.toggle-btn` sizing
	   / border radius from `styles/ported/buttons.css` but pin a
	   visible background at rest in BOTH states so the only toggle
	   signal is the star's fill colour (grey → gold) — matches the
	   favorite-star treatment on the file list. The shared
	   `.toggle-btn.active` would otherwise shift the background and
	   add a shadow, which reads as a different kind of state tell. */
	.favorite-btn,
	.favorite-btn.active {
		background-color: var(--color-border);
		box-shadow: none;
	}

	.favorite-btn.active {
		color: var(--color-star-text-hover);
	}

	.favorite-btn.active:hover {
		color: var(--color-star-text-hover);
	}

	.photos-area {
		padding: 0 1rem;
	}

	/* Date header — fixed height (set inline) so the virtualizer's offset table
	   matches the rendered layout exactly. */
	.photos-group {
		display: flex;
		align-items: center;
		gap: var(--space-2);
		margin: 0;
		font-size: 1rem;
		color: var(--color-text-heading);
	}

	.photos-group__count {
		color: var(--color-text-muted);
		font-size: var(--text-sm);
		font-weight: var(--weight-normal);
	}

	/* A horizontal strip of explicitly-sized tiles — one virtualized row, used by
	   both the square and justified layouts (the bottom gap is baked into the
	   row's declared height). */
	.photos-strip {
		display: flex;
	}

	.photo-tile {
		position: relative;
		overflow: hidden;
		border-radius: var(--radius-sm);
		background: var(--color-bg-muted);
	}

	.photo-tile.selected {
		outline: 3px solid var(--color-accent);
		outline-offset: -3px;
	}

	.photo-tile__open {
		position: relative;
		display: block;
		width: 100%;
		height: 100%;
		border: none;
		padding: 0;
		cursor: pointer;
		background: none;
	}

	/* Default placeholder shown until the thumbnail paints over it (or when the
	   thumbnail can't load). Both this and the <img> are absolutely positioned so
	   DOM order — placeholder first, image second — keeps the image on top. */
	.photo-tile__placeholder {
		position: absolute;
		inset: 0;
		display: grid;
		place-items: center;
		font-size: 2rem;
		color: var(--color-text-faint);
		pointer-events: none;
	}

	.photo-tile__open img {
		position: absolute;
		inset: 0;
		width: 100%;
		height: 100%;
		object-fit: cover;
		display: block;
	}

	.photo-tile__video-badge {
		position: absolute;
		right: 6px;
		bottom: 6px;
		width: 26px;
		height: 26px;
		border-radius: 50%;
		background: var(--color-scrim-control);
		color: var(--color-on-accent);
		display: grid;
		place-items: center;
		font-size: 0.7rem;
		pointer-events: none;
	}

	.photo-tile__check {
		position: absolute;
		top: 6px;
		left: 6px;
		width: 24px;
		height: 24px;
		border-radius: 50%;
		border: 2px solid var(--color-on-accent);
		background: var(--color-scrim-control);
		color: transparent;
		display: grid;
		place-items: center;
		cursor: pointer;
		opacity: 0;
		transition: opacity 0.15s;
	}

	.photo-tile:hover .photo-tile__check,
	.photo-tile__check.on {
		opacity: 1;
	}

	.photo-tile__check.on {
		background: var(--color-accent);
		color: var(--color-on-accent);
		border-color: var(--color-accent);
	}

	.status {
		text-align: center;
		color: var(--color-text-muted);
		padding: 2rem 0;
	}

	/* CSS-first timing: the loading paragraph mounts with opacity:0
	   and only fades in after a 1s delay, so a sub-second fetch
	   (the common filter-flip case) unmounts before the keyframe
	   starts and the indicator is never visible. Longer requests —
	   the ones a user actually waits on — reveal normally. See
	   `feedback_ui_css_first` for why this is pure CSS rather than
	   a JS setTimeout. */
	.status--delayed {
		opacity: 0;
		animation: delayed-fade-in 150ms ease-out 1s forwards;
	}

	@keyframes delayed-fade-in {
		to {
			opacity: 1;
		}
	}

	.status--error {
		color: var(--color-danger-text);
	}

	.sentinel {
		height: 1px;
	}
</style>
