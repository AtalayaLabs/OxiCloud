<script lang="ts" module>
	import type { FileItem, FolderItem } from '$lib/api/types';

	/**
	 * Per-item envelope info that isn't on `FileItem` / `FolderItem` itself
	 * — supplied by the page in a `contextMap` keyed by item id.
	 *
	 * Example use-cases:
	 * - `/trash`: `date = deletion_date`, `extras = { driveId, trashedAt }`.
	 * - `/recent`: `date = accessed_at`, `ownerId = updated_by`.
	 * - `/favorites`: `ownerId = created_by`.
	 *
	 * All fields are optional; ResourceList falls back to the equivalent
	 * intrinsic on the item (`modified_at`, `created_by`) when absent.
	 */
	export interface ItemContext {
		/**
		 * Overrides `item.modified_at` for the date column + `modifiedAt`
		 * group-by dimension. Accepts epoch (ms or s), an ISO string, or
		 * `null` — same shape `formatDate` and the date-bucket helpers
		 * already tolerate. `/trash` uses ISO strings; `/recent` uses
		 * epoch ms.
		 */
		date?: number | string | null;
		/** Overrides `item.created_by` for the owner column + vignette. */
		ownerId?: string | null;
		/** Free-form extras that page-provided `bucketOf` / `contextActions` read. */
		extras?: Record<string, string | number | null>;
	}

	/**
	 * A group-by ("swimlane") dimension a page can offer. `orderBy` is sent to the
	 * API; the optional `bucketOf` maps an item + its context to a section key,
	 * and `labelOf` maps that key to a header label. Omitting `bucketOf` means a
	 * flat list.
	 */
	export interface GroupByDef {
		key: string;
		label: string;
		orderBy: string;
		/** Optional icon for the dropdown option (defaults to the group glyph). */
		icon?: string;
		bucketOf?: (item: FileItem | FolderItem, ctx?: ItemContext) => string | null;
		labelOf?: (bucketKey: string) => string;
	}

	/** A right-click / overflow context-menu action. */
	export interface ContextAction {
		key: string;
		label: string;
		icon: string;
		danger?: boolean;
		/**
		 * Optional per-item visibility gate. Called at menu-open time
		 * with the target item + context; return `false` to hide the
		 * entry entirely for that row (e.g. `open_parent` on a drive-
		 * root folder that has no parent to open). Prefer `disabled?`
		 * over hiding when the action *could* apply but the caller
		 * lacks the required permission — a greyed entry answers
		 * "this option exists" for the user instead of leaving a hole
		 * that reads as a forgotten feature.
		 */
		visible?: (item: FileItem | FolderItem, ctx?: ItemContext) => boolean;
		/**
		 * Optional per-item disabled gate. Called at menu-open time;
		 * `true` renders the entry non-interactive (dimmed, no click).
		 * Kept sync by the same contract as `visible?` — use the
		 * `menuPrepare` prop to prime any cache the predicate depends
		 * on before the menu renders.
		 */
		disabled?: (item: FileItem | FolderItem, ctx?: ItemContext) => boolean;
		run: (item: FileItem | FolderItem, ctx?: ItemContext) => void;
	}

	/**
	 * True when the item is a file. Uses structural narrowing on
	 * `mime_type` (only present on `FileItem`) so callers can pattern
	 * match without importing the discriminator manually.
	 */
	export function isFile(item: FileItem | FolderItem): item is FileItem {
		return 'mime_type' in item;
	}
</script>

<script lang="ts">
	import type { Snippet } from 'svelte';
	import { SvelteSet } from 'svelte/reactivity';
	import Icon from '$lib/icons/Icon.svelte';
	import EmptyState from '$lib/components/EmptyState.svelte';
	import SkeletonList from '$lib/components/SkeletonList.svelte';
	import ActionBar from '$lib/components/ActionBar.svelte';
	import DisplayModeControls from '$lib/components/DisplayModeControls.svelte';
	import UserVignette from '$lib/components/UserVignette.svelte';
	import VirtualList from '$lib/components/VirtualList.svelte';
	import { goto } from '$app/navigation';
	import { resolve } from '$app/paths';
	import { t } from '$lib/i18n/index.svelte';
	import { ui } from '$lib/stores/ui.svelte';
	import { files as filesStore } from '$lib/stores/files.svelte';
	import { preferences } from '$lib/stores/preferences.svelte';
	import { formatBytes } from '$lib/utils/format';
	import { formatDate, iconNameFromClass, fileIconKindClass } from '$lib/utils/display';
	import { gridColumns } from '$lib/utils/grid';
	import { ResourceSectionsBuilder } from '$lib/utils/resourceSections';
	import { ItemIndexBuilder } from '$lib/utils/itemIndex';
	import { fileThumbnailUrl, thumbSizeForView } from '$lib/api/endpoints/files';
	import {
		canThumbnailClientSide,
		preloadPdf,
		queueGenerate as queueThumbnailGenerate
	} from '$lib/utils/thumbnail';

	interface Props {
		title: string;
		items: Array<FileItem | FolderItem>;
		/**
		 * Per-item envelope info keyed by `item.id`. See `ItemContext`
		 * above. When absent, ResourceList uses the intrinsic item
		 * fields (`modified_at`, `created_by`).
		 */
		contextMap?: Map<string, ItemContext>;
		/**
		 * Resolve `userId → display name`. Optional; when absent
		 * `UserVignette` falls back to its own internal resolution.
		 * Accepts `null` for consistency with the useOwnerCache API
		 * (returns `null` for a not-yet-resolved id).
		 */
		resolveOwnerName?: (userId: string) => string | null | undefined;
		loading?: boolean;
		error?: string | null;
		/** Empty-state primary line. */
		emptyText?: string;
		/** Empty-state secondary hint line. */
		emptyHint?: string;
		/** Empty-state icon-registry name (e.g. "star", "clock", "trash"). */
		emptyIcon?: string;
		/**
		 * Call-to-action rendered inside the empty state. Used by
		 * `/files` to surface a "Show hidden files" button when the
		 * folder holds only dotfiles the user has chosen to hide — the
		 * page-specific hint stays in `emptyHint`, the action goes
		 * here. `<EmptyState>` renders it below the hint text.
		 */
		emptyAction?: Snippet;
		hasMore?: boolean;
		onloadmore?: () => void;
		/** Show the path/location column (list view only). */
		showPath?: boolean;
		/** Override the path column header label (e.g. trash → "Original location"). */
		pathLabel?: string;
		showSize?: boolean;
		showType?: boolean;
		showDate?: boolean;
		/** Override the date column header label (e.g. trash → "Remaining"). */
		dateLabel?: string;
		/** Custom renderer for the date cell (e.g. trash expiry chip). */
		dateCell?: Snippet<[FileItem | FolderItem, ItemContext | undefined]>;
		/**
		 * Optional per-bucket action button rendered alongside the swimlane
		 * header label. Receives the bucket key (the value `bucketOf`
		 * returned for the active group-by). Used by the trash page to expose
		 * a per-drive "Empty" affordance — the page decides which group-bys
		 * the action is meaningful for and returns nothing otherwise.
		 */
		bucketAction?: Snippet<[string]>;
		/** Show the owner column + vignette (list view) and hover tooltip. */
		showOwner?: boolean;
		/**
		 * Override the owner column header (and the hover-tooltip prefix). The
		 * default reads "Created by", matching the semantic of `created_by` used
		 * on /files, /favorites, /recent. /shared-with-me overrides to
		 * "Shared by" since the column there actually renders `granted_by`
		 * (the sharer, not the resource author).
		 */
		ownerLabel?: string;
		/** Allow grid/list toggle (shares the app-wide view mode). */
		showViewToggle?: boolean;
		/** Show the dotfile-visibility eye toggle in the toolbar AND
		 * apply the corresponding filter to `items` when
		 * `preferences.hideDotfiles` is true. Opt-in per host page —
		 * surfaces that never filter dotfiles (favorites, trash) leave
		 * this false so the button doesn't appear AND the filter never
		 * kicks in. Single flag governs both concerns so a page can't
		 * accidentally expose the button without wiring the filter or
		 * vice-versa.
		 *
		 * A host page that needs to surface "N items hidden" in its
		 * empty state derives that count independently via the shared
		 * `isDotfile` predicate in `$lib/utils/dotfileFilter` — no
		 * count-out prop here (avoids a bindable whose $bindable
		 * default is always shadowed by the effect that would sync it,
		 * and keeps the component's API one-way-inbound). */
		showDotfileToggle?: boolean;
		/** Multi-select checkboxes + selection model. */
		selectable?: boolean;
		/** Right-click / overflow context-menu actions. */
		contextActions?: ContextAction[];
		/** Group-by dimensions; when provided, a swimlane selector is shown. */
		groupBys?: GroupByDef[];
		/** Active group-by key (bind:groupBy from the page). */
		groupBy?: string;
		/** Reverse sort toggle state (bind:reversed from the page). */
		reversed?: boolean;
		/** Called when group-by or direction changes; page should reload page 1. */
		onreload?: (orderBy: string, reversed: boolean) => void;
		onopen?: (item: FileItem | FolderItem) => void;
		/** Per-item favorite star toggle. */
		onfavorite?: (item: FileItem | FolderItem) => void;
		/**
		 * Per-item share affordance — opens the page's ShareDialog for
		 * this row. Wired symmetrically to `onfavorite`: the button lives
		 * in `.action-cell`, its `.active` styling tracks `item.is_shared`,
		 * and rows that are shared keep the button visible in list view
		 * even when the row isn't hovered.
		 */
		onshared?: (item: FileItem | FolderItem) => void;
		/** Selection changed (set of selected item ids). */
		onselectionchange?: (ids: Set<string>) => void;
		/**
		 * Right-click / long-press handler. When provided, ResourceList
		 * forwards the row's `contextmenu` event to this callback and
		 * SKIPS its built-in menu — the page renders and positions its
		 * own. Useful when the page needs conditional entries (WOPI
		 * editability, audio-only actions) that don't fit the flat
		 * `contextActions` array. If both `oncontextmenu` and
		 * `contextActions` are provided, `oncontextmenu` wins.
		 */
		oncontextmenu?: (e: MouseEvent, item: FileItem | FolderItem) => void;
		/**
		 * Optional async pre-open hook. When provided, ResourceList
		 * awaits it before the built-in context menu appears — so a
		 * page can lazily prime any per-item cache the menu's
		 * `visible?` predicates depend on WITHOUT the page having to
		 * pre-warm every row at load time (which would fire N HTTP
		 * calls for a feature the user may never invoke).
		 *
		 * Reference use: `/recent` / `/favorites` probe folder-access
		 * for the row's parent inside `menuPrepare` so the "Open parent
		 * folder" entry shows up on the first right-click of a
		 * previously-unseen row. Short-typically-cached call; typical
		 * menu-open latency stays well under a UI frame.
		 */
		menuPrepare?: (item: FileItem | FolderItem, ctx?: ItemContext) => Promise<void>;
		/**
		 * Per-item action cell (renders at the end of a row). Kept as a
		 * distinct slot from the action-bar snippets below so callers
		 * that want an item-scoped affordance (a per-row overflow menu)
		 * don't have to piggyback on the bar.
		 */
		itemActions?: Snippet<[FileItem | FolderItem]>;
		/**
		 * Action-bar left cluster — always-visible page action buttons
		 * (Upload / New folder / Empty trash / Clear recent / …). Swaps
		 * to `batchActions` when the selection is non-empty. Every
		 * section provides its own buttons; ResourceList doesn't ship
		 * any defaults.
		 */
		actions?: Snippet;
		/**
		 * Action-bar left cluster when selection is non-empty —
		 * replaces `actions`. Receives the selected items so buttons
		 * can be scoped to the batch. Replaces the phase-1
		 * `batchToolbar` floating strip pattern.
		 */
		batchActions?: Snippet<[Array<FileItem | FolderItem>]>;
		/**
		 * Rendered next to the item name in each row. `/trash` uses
		 * this for its expiration badge; other sections omit it.
		 * ResourceList stays ignorant of what the badge means — the
		 * page decides. Empty return = no badge.
		 */
		rowBadge?: Snippet<[FileItem | FolderItem, ItemContext | undefined]>;
		/**
		 * Rendered above the toolbar in the sticky header. Only
		 * `/files` wires this today; every other section leaves the
		 * snippet undefined so no breadcrumb strip appears. Kept as a
		 * snippet (not a boolean) so the page owns crumb rendering and
		 * their click / drag-drop behavior.
		 */
		breadcrumb?: Snippet;
		/**
		 * When true, drops from the OS file system on the ResourceList
		 * wrapper are forwarded to `onsystemdrop` (upload path). When
		 * false (default), the wrapper still intercepts the OS drop —
		 * `preventDefault` so the browser doesn't navigate to the file
		 * — and fires a "wrong section" `ui.notify()` pointing the user
		 * at the Files section (the legacy behaviour). Item-drag drops
		 * (row → folder) are unaffected either way; those go through
		 * `onitemdrop` per the existing row hooks.
		 */
		enableSystemDrop?: boolean;
		/**
		 * Called with the OS-dropped files when `enableSystemDrop` is
		 * true. The page keeps ownership of the upload code (walking
		 * webkitGetAsEntry trees, chunked uploader, etc.) — this
		 * component just delivers the payload. Ignored when
		 * `enableSystemDrop` is false.
		 */
		onsystemdrop?: (e: DragEvent) => void;
		/**
		 * Forces the OS-file drop overlay on regardless of whether the
		 * pointer is currently over `.rl-root`. Only takes effect when
		 * `enableSystemDrop` is true (otherwise there's no drop route
		 * behind the overlay). Meant for pages that track drag state
		 * at window level and want the overlay to react anywhere on
		 * the viewport — not just over the list — so drops on the
		 * page's title strip / margins get the same "drop to upload"
		 * cue. `/files/[...path]/+page.svelte` uses this. The overlay
		 * itself is already `position: fixed`, so activating it from a
		 * broader trigger costs no visual change — it OR's with the
		 * internal `systemDropOver` counter.
		 */
		systemDropOverlayActive?: boolean;
		/**
		 * Render `<img>` thumbnails on file rows and fall back to
		 * client-side generation when the server doesn't have one
		 * (image / PDF / video via `$lib/utils/thumbnail`). Default on
		 * — every view that lists real files gets the same behaviour.
		 * Set false for views that never benefit (empty states,
		 * synthetic rows).
		 */
		enableThumbnails?: boolean;
		/**
		 * Enable per-row drag/drop hooks. Used by the files browser so
		 * a folder row is a drop target and any row is draggable to
		 * another folder or the breadcrumb. Pages that don't wire these
		 * (trash, favorites, recent, shared-with-me) opt out of the
		 * drag-drop UX entirely by leaving the callbacks unset.
		 */
		isDraggable?: (item: FileItem | FolderItem) => boolean;
		isDropTarget?: (item: FileItem | FolderItem) => boolean;
		/**
		 * Which item id currently shows the drop-target highlight (page
		 * owns the state so it can share it with breadcrumb / other drop
		 * zones). Only meaningful when `isDropTarget` is provided.
		 */
		dropTargetId?: string | null;
		onitemdragstart?: (e: DragEvent, item: FileItem | FolderItem) => void;
		onitemdragover?: (e: DragEvent, item: FileItem | FolderItem) => void;
		onitemdragleave?: (e: DragEvent, item: FileItem | FolderItem) => void;
		onitemdrop?: (e: DragEvent, item: FileItem | FolderItem) => void;
		/**
		 * Override the list-view column header. When provided,
		 * ResourceList renders this instead of its default header —
		 * used by the files browser to expose clickable column-sort
		 * buttons (name / size / type / modified). Pages that override
		 * this typically also handle sorting on their side (pass
		 * pre-sorted `items`) rather than relying on `onreload`.
		 */
		listHeader?: Snippet;
		/**
		 * Open the row on single click (default) vs. double click.
		 * Files browser prefers double-click so single-click can drive
		 * the shift-range selection model without accidentally
		 * navigating.
		 */
		openOnDoubleClick?: boolean;
		/**
		 * Enable shift-click range selection. The row that was clicked
		 * without shift becomes the anchor; the next shift-click
		 * selects the range between anchor and target in visible order.
		 * Requires `selectable`.
		 */
		shiftRangeSelect?: boolean;
	}

	let {
		title,
		items,
		contextMap,
		resolveOwnerName,
		loading = false,
		error = null,
		emptyText,
		emptyHint,
		emptyIcon,
		emptyAction,
		hasMore = false,
		onloadmore,
		showPath = true,
		pathLabel,
		showSize = true,
		showType = false,
		showDate = true,
		dateLabel,
		dateCell,
		bucketAction,
		showOwner = false,
		ownerLabel,
		showViewToggle = true,
		showDotfileToggle = false,
		selectable = false,
		contextActions,
		groupBys,
		groupBy = $bindable(''),
		reversed = $bindable(false),
		onreload,
		onopen,
		onfavorite,
		onshared,
		onselectionchange,
		oncontextmenu: onContextMenuOverride,
		menuPrepare,
		itemActions,
		actions,
		batchActions,
		rowBadge,
		breadcrumb,
		enableSystemDrop = false,
		onsystemdrop,
		systemDropOverlayActive = false,
		enableThumbnails = true,
		isDraggable,
		isDropTarget,
		dropTargetId = null,
		onitemdragstart,
		onitemdragover,
		onitemdragleave,
		onitemdrop,
		listHeader: listHeaderOverride,
		openOnDoubleClick = false,
		shiftRangeSelect = false
	}: Props = $props();

	// ── Per-item accessors ────────────────────────────────────────────────────
	// Every read of an item field goes through these helpers so the
	// contextMap override for date + owner is centralised. Kept as
	// module-level fns (not $derived) — they run on each row render;
	// caching a Map on every items/contextMap change would be wasteful.
	function ctxOf(id: string): ItemContext | undefined {
		return contextMap?.get(id);
	}
	function dateOf(item: FileItem | FolderItem): number | string | null {
		return ctxOf(item.id)?.date ?? item.modified_at;
	}
	function ownerIdOf(item: FileItem | FolderItem): string | null {
		const ctx = ctxOf(item.id);
		return ctx && 'ownerId' in ctx ? (ctx.ownerId ?? null) : (item.created_by ?? null);
	}
	function sizeOf(item: FileItem | FolderItem): number | null {
		return isFile(item) ? item.size : null;
	}
	function mimeOf(item: FileItem | FolderItem): string | null {
		return isFile(item) ? item.mime_type : null;
	}
	function iconClassOf(item: FileItem | FolderItem): string {
		return item.icon_class;
	}

	// ── Dotfile filter ────────────────────────────────────────────────────────
	// Two conditions gate the filter (both must be true):
	//   1. Host page opted in via `showDotfileToggle` — so pages where
	//      dotfiles are always visible (favorites, trash) never hide them
	//      even if the user's global preference is on.
	//   2. User preference is set to hide — read from the reactive
	//      `preferences.hideDotfiles` getter, so a toolbar click flips
	//      this list in real time without a reload.
	// The `visibleItems` derived is what every downstream reader
	// (bucketing, rendering, "all-selected", range-select) uses, so
	// hidden rows disappear consistently across grid, list, and every
	// group-by dimension. `selectedItems` and the reap-stale-selection
	// effect stay on the raw `items` — selection persists across a
	// display filter toggle, matching how file managers treat a
	// filter-hide as "hidden, not gone".
	const filterDotfiles = $derived(showDotfileToggle && preferences.hideDotfiles);
	const visibleItems = $derived(
		filterDotfiles ? items.filter((i) => !i.name.startsWith('.')) : items
	);

	// isEmpty tracks the VISIBLE list — an all-dotfile page with the
	// filter on shows the empty state (the host page's `emptyHint` can
	// reference `hiddenCount` to say "3 items hidden by the filter").
	const isEmpty = $derived(visibleItems.length === 0);
	/** Content width, for computing the grid's column count to match auto-fill. */
	let gridWidth = $state(0);
	const gridCols = $derived(gridColumns(gridWidth));

	// Whether an action-cell renders per row — matches the row-template
	// gate below. Feeds both the list-view column track and the header
	// row's trailing placeholder so the layout stays in sync.
	const hasActionCell = $derived(
		!!onfavorite ||
			!!onshared ||
			!!itemActions ||
			!!onContextMenuOverride ||
			!!contextActions?.length
	);

	// Build the list-view column track from the enabled cells.
	const columns = $derived(
		[
			selectable ? '36px' : '',
			'minmax(200px, 2fr)',
			showOwner ? 'minmax(120px, 1fr)' : '',
			showPath ? 'minmax(140px, 1.5fr)' : '',
			showType ? '120px' : '',
			showSize ? '110px' : '',
			showDate ? '160px' : '',
			hasActionCell ? '120px' : ''
		]
			.filter(Boolean)
			.join(' ')
	);

	const SKELETON = [0, 1, 2, 3, 4, 5];

	// ── Delayed-skeleton reveal ──────────────────────────────────────────
	// Fast fetches (< 150 ms) don't render the skeleton bars — the flash
	// is worse UX than briefly-empty content. The skeleton appears only
	// when a load is genuinely slow. Ed's 2026-07-26 report: navigating
	// from an empty folder to its parent showed "6 blank elements" (the
	// skeleton) for the ~25 ms fetch window because stale-while-revalidate
	// at the /files layer has no previous content to keep on screen here.
	//
	// Pairs with the empty-state gate below (`!loading && isEmpty`) so
	// the pre-fix "Folder is empty" flash during the delay window
	// doesn't come back — during load, neither skeleton nor empty state
	// renders; the container just holds empty until content or the
	// 150 ms timer elapses.
	let renderSkeleton = $state(false);
	$effect(() => {
		if (loading && items.length === 0) {
			const timer = setTimeout(() => {
				renderSkeleton = true;
			}, 150);
			return () => {
				clearTimeout(timer);
				renderSkeleton = false;
			};
		}
		renderSkeleton = false;
	});

	// ── Group-by / direction ──────────────────────────────────────────────────
	const activeGroup = $derived(groupBys?.find((g) => g.key === groupBy));

	function selectGroup(key: string) {
		if (groupBy === key) return;
		groupBy = key;
		const def = groupBys?.find((g) => g.key === key);
		onreload?.(def?.orderBy ?? 'name', reversed);
	}

	function toggleDirection() {
		reversed = !reversed;
		onreload?.(activeGroup?.orderBy ?? 'name', reversed);
	}

	/**
	 * Partition the visible items into grouped sections when a `bucketOf` is
	 * active. Server order is preserved within and across buckets (first-seen).
	 *
	 * `ResourceSectionsBuilder` re-buckets only the freshly-appended page rather
	 * than the whole accumulated list, and hands `VirtualList` the same rows
	 * array reference for every untouched bucket so it skips re-rendering it. An
	 * infinite-scroll drain of a grouped listing (trash / recent / favorites /
	 * shared-with-me) collapses from Σ O(N²/page) to O(N) bucketing work
	 * (benches/ROUND15.md §F1). Held off the reactive graph — a plain
	 * accumulator keyed by the append cursor, not $state; `sync` is idempotent,
	 * so if the derive re-fires without an actual append it safely full-rebuilds
	 * to the same output the pure `buildResourceSections` reference produces.
	 */
	const sectionsBuilder = new ResourceSectionsBuilder<FileItem | FolderItem, ItemContext>();
	const sections = $derived.by(() =>
		sectionsBuilder.sync(visibleItems, {
			bucketOf: activeGroup?.bucketOf,
			labelOf: activeGroup?.labelOf,
			ctxOf: (item) => ctxOf(item.id)
		})
	);
	const grouped = $derived(!!activeGroup?.bucketOf);

	// ── Selection ─────────────────────────────────────────────────────────────
	// SvelteSet is reactive on its own; mutate in place rather than reassigning.
	const selected = new SvelteSet<string>();

	function toggleSelected(id: string) {
		if (selected.has(id)) selected.delete(id);
		else selected.add(id);
		onselectionchange?.(selected);
	}

	/**
	 * Anchor id for shift-range selection. The row clicked without
	 * shift becomes the anchor; the next shift-click selects every
	 * row between anchor and target in visible order. Kept in module
	 * state so it survives re-renders that don't drop the component.
	 */
	let selectionAnchor = $state<string | null>(null);
	function selectRange(anchorId: string, targetId: string) {
		// Range-select over the VISIBLE order — a shift-click can't reach
		// a row the user can't see.
		const order = visibleItems.map((i) => i.id);
		const a = order.indexOf(anchorId);
		const b = order.indexOf(targetId);
		if (a < 0 || b < 0) return;
		const [lo, hi] = a < b ? [a, b] : [b, a];
		for (let i = lo; i <= hi; i++) selected.add(order[i]);
		onselectionchange?.(selected);
	}
	// True when the client is macOS. Sets which modifier toggles a row
	// on click:
	//   * macOS: ⌘ (metaKey) — because Ctrl+Click is reserved by the
	//     OS/browser for the native contextmenu event. Intercepting
	//     Ctrl+Click here would collide with the right-click menu; the
	//     browser fires `contextmenu` BEFORE `click`, so both would run
	//     and the user would see the menu AND a rogue toggle.
	//   * Windows / Linux: Ctrl (ctrlKey) — standard file-manager
	//     convention (Explorer, Nautilus, etc.). ⌘ (Win/Super key) also
	//     accepted defensively; it never collides with anything on the
	//     row itself.
	const IS_MAC =
		typeof navigator !== 'undefined' &&
		/Mac|iPhone|iPad|iPod/i.test(navigator.platform || navigator.userAgent || '');

	function isToggleModifier(e: MouseEvent | KeyboardEvent): boolean {
		return IS_MAC ? e.metaKey : e.ctrlKey || e.metaKey;
	}

	/**
	 * Left-click handler that either navigates (`onopen`) or manages
	 * selection depending on modifiers + config. Returns `true` when
	 * the click was consumed by selection, so callers can suppress the
	 * open.
	 *
	 * Selection gestures:
	 *   * Shift+Click — range selection between the anchor and this row
	 *     (requires `shiftRangeSelect` opt-in — the anchor is only
	 *     tracked when that flag is on).
	 *   * ⌘+Click (Mac) / Ctrl+Click (Win/Linux) — toggle a single row.
	 *     Available whenever `selectable` is on; no `shiftRangeSelect`
	 *     required, so sections that just want checkboxes get the
	 *     shortcut too. See `IS_MAC` note above for why Ctrl+Click is
	 *     NOT intercepted on macOS (native contextmenu conflict).
	 */
	function handleRowClick(e: MouseEvent, id: string): boolean {
		if (!selectable) return false;
		if (shiftRangeSelect && e.shiftKey && selectionAnchor) {
			e.preventDefault();
			selectRange(selectionAnchor, id);
			return true;
		}
		if (isToggleModifier(e)) {
			e.preventDefault();
			toggleSelected(id);
			if (shiftRangeSelect) selectionAnchor = id;
			return true;
		}
		// Plain click: only sets the anchor (when range-select is on);
		// `onopen` still fires so navigation works normally.
		if (shiftRangeSelect) selectionAnchor = id;
		return false;
	}
	function clearSelection() {
		selected.clear();
		onselectionchange?.(selected);
	}
	// "All-selected" means every VISIBLE row is selected — hiding
	// dotfiles by preference shouldn't be confused with "not selected".
	const allSelected = $derived(
		visibleItems.length > 0 && visibleItems.every((i) => selected.has(i.id))
	);
	function toggleSelectAll() {
		if (allSelected) clearSelection();
		else {
			selected.clear();
			// Select all VISIBLE rows only. A user hiding dotfiles then
			// pressing select-all shouldn't sweep in the hidden files
			// they can't see — that would be a footgun for destructive
			// batch actions.
			for (const i of visibleItems) selected.add(i.id);
			onselectionchange?.(selected);
		}
	}

	// Ctrl+A (Linux / Windows) / ⌘+A (macOS) selects every visible row.
	// Handled here rather than in each page's own `svelte:window` so all
	// consumers get the shortcut for free — /files, /trash, /favorites,
	// /recent, /shared-with-me — with identical semantics. Only fires
	// when `selectable` is on, and only when the focused element isn't
	// a text input (typing inside a search box shouldn't hijack it).
	// The modifier check goes through `isToggleModifier` so keyboard
	// and mouse gestures agree on the platform (⌘ on Mac; Ctrl or ⌘
	// on Win/Linux).
	function onSelectAllShortcut(e: KeyboardEvent) {
		if (!selectable) return;
		if (!isToggleModifier(e)) return;
		if (e.key.toLowerCase() !== 'a') return;
		const tag = (e.target as HTMLElement | null)?.tagName;
		if (tag === 'INPUT' || tag === 'TEXTAREA' || tag === 'SELECT') return;
		// Also skip when the focus is inside a contentEditable region
		// (rich-text popups, name inline-edit if ever added).
		if ((e.target as HTMLElement | null)?.isContentEditable) return;
		e.preventDefault();
		toggleSelectAll();
	}

	// ── Rubberband (marquee) selection ────────────────────────────────────────
	//
	// Click-and-drag on empty space draws a translucent rectangle; every row
	// whose bounding box intersects the rectangle joins the selection. Behavior:
	//   * Plain drag → replace the current selection with what the box covers.
	//   * Shift+drag → add to the current selection (union).
	//   * ⌘/Ctrl+drag → toggle: rows inside the box flip their state relative
	//     to the pre-drag baseline.
	//
	// The gesture only starts when the mousedown lands on truly empty space —
	// mousedowns on `.file-item`, links, buttons, or the checkbox pass through
	// to their own handlers. This keeps row-drag (files browser) uncontested.
	//
	// Intersections are computed via `getBoundingClientRect()` on every mouse
	// move, so this only sees VISIBLE rows — which is what a user in a
	// virtualized list expects anyway ("I can't rubberband something I can't
	// see"). No auto-scroll during drag today; the user can release, scroll,
	// then start another gesture with Shift held.
	let rlRoot = $state<HTMLElement | null>(null);
	let rubberband = $state<{
		startX: number;
		startY: number;
		x: number;
		y: number;
		w: number;
		h: number;
		mode: 'replace' | 'add' | 'toggle';
		baseline: Set<string>;
	} | null>(null);

	function onRootPointerDown(e: PointerEvent) {
		if (!selectable) return;
		if (e.button !== 0) return; // Left button only
		const target = e.target as HTMLElement | null;
		if (!target || !rlRoot) return;
		// Ignore mousedowns on interactive descendants or on a row.
		if (
			target.closest('.file-item') ||
			target.closest('a, button, input, select, textarea, [role="menuitem"]')
		) {
			return;
		}
		// Ignore when the click landed on the sticky header (bar + breadcrumb).
		if (target.closest('.page-sticky-header')) return;

		const rect = rlRoot.getBoundingClientRect();
		const startX = e.clientX - rect.left;
		const startY = e.clientY - rect.top;
		const mode: 'replace' | 'add' | 'toggle' = e.shiftKey
			? 'add'
			: isToggleModifier(e)
				? 'toggle'
				: 'replace';
		const baseline = mode === 'replace' ? new Set<string>() : new Set(selected);

		rubberband = { startX, startY, x: startX, y: startY, w: 0, h: 0, mode, baseline };

		if (mode === 'replace') selected.clear();

		// preventDefault so text under the drag doesn't get selected as we drag.
		e.preventDefault();
		window.addEventListener('pointermove', onRubberbandMove);
		window.addEventListener('pointerup', onRubberbandUp, { once: true });
	}

	function onRubberbandMove(e: PointerEvent) {
		if (!rubberband || !rlRoot) return;
		const rect = rlRoot.getBoundingClientRect();
		const curX = Math.max(0, Math.min(rect.width, e.clientX - rect.left));
		const curY = Math.max(0, Math.min(rect.height, e.clientY - rect.top));
		rubberband.x = Math.min(rubberband.startX, curX);
		rubberband.y = Math.min(rubberband.startY, curY);
		rubberband.w = Math.abs(curX - rubberband.startX);
		rubberband.h = Math.abs(curY - rubberband.startY);
		applyRubberbandSelection();
	}

	function onRubberbandUp() {
		window.removeEventListener('pointermove', onRubberbandMove);
		rubberband = null;
	}

	function applyRubberbandSelection() {
		if (!rubberband || !rlRoot) return;
		const rootRect = rlRoot.getBoundingClientRect();
		// Absolute viewport rect of the current band.
		const bandLeft = rootRect.left + rubberband.x;
		const bandTop = rootRect.top + rubberband.y;
		const bandRight = bandLeft + rubberband.w;
		const bandBottom = bandTop + rubberband.h;

		const rows = rlRoot.querySelectorAll<HTMLElement>('.file-item[data-item-id]');
		// Transient scratch set for computing the diff before mutating
		// `selected`. `SvelteSet` (not plain `Set`) per the codebase's
		// `svelte/prefer-svelte-reactivity` convention — the lint rule
		// exists so a future refactor that stashes this in `$state`
		// can't silently break reactivity.
		const nextSelection = new SvelteSet(rubberband.baseline);
		for (const row of rows) {
			const id = row.dataset.itemId;
			if (!id) continue;
			const b = row.getBoundingClientRect();
			const overlaps =
				b.left < bandRight && b.right > bandLeft && b.top < bandBottom && b.bottom > bandTop;
			if (overlaps) {
				if (rubberband.mode === 'toggle') {
					if (rubberband.baseline.has(id)) nextSelection.delete(id);
					else nextSelection.add(id);
				} else {
					nextSelection.add(id);
				}
			}
		}
		// Rewrite the `selected` set in place — SvelteSet is reactive on
		// per-key operations, so we only mutate the diff.
		for (const id of selected) if (!nextSelection.has(id)) selected.delete(id);
		for (const id of nextSelection) if (!selected.has(id)) selected.add(id);
		onselectionchange?.(selected);
	}
	// `selectedItems` and the reap-stale effect below stay on the RAW
	// items — selection persists across a display-filter toggle, and
	// stale-selection cleanup only fires when items truly leave the
	// dataset (reload, delete, etc.), not when the filter hides them.
	//
	// Index extended over the freshly-appended page only (never re-scanned in
	// full) via `ItemIndexBuilder`: an infinite-scroll drain with a selection
	// active collapses from Σ O(N²) Map rebuilds to O(N) total, and the Map
	// reference is reused across appends so the reap-stale effect below no
	// longer re-fires (nor re-allocates an O(N) id Set) on a page that removed
	// nothing — its reference only changes on a rebuild (reload / deletion),
	// exactly when a reap is warranted. The projection is then O(k · log k) in
	// the selection size k, not a full O(N) re-scan on every toggle
	// (benches/ROUND11.md §S1, benches/ROUND18.md §F1). The index sort preserves
	// item order, so the toolbar sees the same array the old filter produced.
	const itemIndex = new ItemIndexBuilder<FileItem | FolderItem>();
	const itemIndexById = $derived(itemIndex.sync(items));
	const selectedItems = $derived.by(() => {
		const picked: { idx: number; item: FileItem | FolderItem }[] = [];
		for (const id of selected) {
			const idx = itemIndexById.get(id);
			if (idx !== undefined) picked.push({ idx, item: items[idx] });
		}
		picked.sort((a, b) => a.idx - b.idx);
		return picked.map((p) => p.item);
	});

	// Listen for out-of-band `is_favorite` changes broadcast by other
	// surfaces — the sidebar drop-on-Favorites target in `AppShell` is
	// today's only emitter, but any future "toggle from elsewhere"
	// (bulk import, notification action, …) can dispatch the same
	// event and every mounted resource list will patch its row without
	// needing the source page to re-fetch. Matches the existing
	// `oxicloud:upload-files` window-event pattern.
	$effect(() => {
		function onFavoriteChanged(e: Event) {
			const detail = (e as CustomEvent<{ id: string; is_favorite: boolean }>).detail;
			if (!detail) return;
			const item = items.find((i) => i.id === detail.id);
			if (item) item.is_favorite = detail.is_favorite;
		}
		window.addEventListener('oxicloud:favorite-changed', onFavoriteChanged);
		return () => window.removeEventListener('oxicloud:favorite-changed', onFavoriteChanged);
	});

	// Drop selection ids that are no longer present after a reload.
	$effect(() => {
		// With nothing selected (the common case) the loop never runs — skip
		// straight out. `selected.size` is reactive, so the effect re-fires
		// when a selection appears.
		if (selected.size === 0) return;
		// Test membership against the incremental `itemIndexById` rather than a
		// throwaway O(N) id Set rebuilt per page. Its reference is stable across
		// infinite-scroll appends (which never remove an id — nothing to reap)
		// so this effect no longer re-fires on every page; the reference changes
		// only on a rebuild (reload / deletion), which is exactly when a stale
		// selection must be dropped (benches/ROUND18.md §F1).
		const index = itemIndexById;
		let changed = false;
		for (const id of selected) {
			if (!index.has(id)) {
				selected.delete(id);
				changed = true;
			}
		}
		if (changed) onselectionchange?.(selected);
	});

	// ── Right-click context menu ──────────────────────────────────────────────
	let ctxOpen = $state(false);
	let ctxX = $state(0);
	let ctxY = $state(0);
	let ctxItem = $state<FileItem | FolderItem | null>(null);

	async function openContext(e: MouseEvent, item: FileItem | FolderItem) {
		if (!contextActions?.length) return;
		e.preventDefault();
		e.stopPropagation();
		// Snapshot the pointer coords now — after an `await menuPrepare`
		// tick the event object may be reused / stale, and reading
		// `e.clientX` post-await could pin the menu to the wrong spot.
		const x = Math.min(e.clientX, window.innerWidth - 220);
		const y = Math.min(e.clientY, window.innerHeight - (contextActions.length * 44 + 24));
		// Give the page a chance to prime any per-item cache the
		// `visible?` predicates read (e.g. folder-access on /recent +
		// /favorites for the "Open parent folder" entry). Awaited so the
		// menu opens with the final visibility state — avoids a
		// flash-of-hidden-then-shown when the probe resolves.
		if (menuPrepare) {
			try {
				await menuPrepare(item, ctxOf(item.id));
			} catch {
				/* prepare failures degrade to the sync-only visibility */
			}
		}
		ctxItem = item;
		ctxX = x;
		ctxY = y;
		ctxOpen = true;
	}
	function closeContext() {
		ctxOpen = false;
		ctxItem = null;
	}

	// ── Infinite scroll (IntersectionObserver) ────────────────────────────────
	let sentinel = $state<HTMLElement | null>(null);
	$effect(() => {
		const el = sentinel;
		if (!el || typeof IntersectionObserver === 'undefined') return;
		const obs = new IntersectionObserver(
			(entries) => {
				for (const en of entries) {
					if (en.isIntersecting && hasMore && !loading) onloadmore?.();
				}
			},
			{ rootMargin: '200px' }
		);
		obs.observe(el);
		return () => obs.disconnect();
	});

	function ownerTitle(item: FileItem | FolderItem): string {
		const ownerId = ownerIdOf(item);
		const owner = ownerId ? (resolveOwnerName?.(ownerId) ?? ownerId) : '';
		const path = item.path ?? '';
		return [
			owner && `${ownerLabel ?? t('files.col_created_by', 'Created by')}: ${owner}`,
			path && `${t('files.col_path', 'Location')}: ${path}`
		]
			.filter(Boolean)
			.join('\n');
	}

	// ── System-drop handling (OS files onto the wrapper) ───────────────────────
	// Two modes:
	//
	//   * `enableSystemDrop = true`: the page has an upload code path
	//     ready (the `/files` browser). We `preventDefault` the browser's
	//     default (which would open the dragged file as a top-level
	//     navigation), highlight the drop zone, and hand the DragEvent
	//     off to the page via `onsystemdrop`. The page walks the entries
	//     (webkitGetAsEntry / DataTransferItemList) and drives the upload.
	//
	//   * `enableSystemDrop = false` (default): the page has no upload
	//     path. Still `preventDefault` so the browser doesn't navigate
	//     away, but instead of forwarding, fire a `ui.notify()` that
	//     points the user at `/files` — this restores the legacy vanilla
	//     frontend's "wrong drop zone" behaviour so users don't wonder
	//     why their drag was silently ignored.
	//
	// Row-scoped drops (dragging an in-app row onto a folder row / the
	// breadcrumb) are handled by the existing `onitemdrop` hooks and use
	// a private `application/x-oxi-item` MIME so the `Files` type check
	// below never matches them.
	// Drag-enter/leave chatter is unavoidable when the drag pointer moves
	// between the wrapper and its descendants — the browser fires
	// `dragleave` on the parent BEFORE firing `dragenter` on the child,
	// so a naive `systemDropOver = false` in the leave handler produces
	// a false→true flash on every row hover during the drag. Counter
	// approach: increment on every dragenter, decrement on every
	// dragleave; the overlay is visible when the count is positive. The
	// count zeroes only when the drag has truly left the wrapper (or
	// hit `drop`/`dragend`), so the overlay stays stable throughout.
	let systemDragDepth = $state(0);
	const systemDropOver = $derived(systemDragDepth > 0);
	function isSystemDrag(e: DragEvent): boolean {
		return !!e.dataTransfer?.types?.includes('Files');
	}
	function onSystemDragEnter(e: DragEvent) {
		if (!isSystemDrag(e)) return;
		e.preventDefault();
		systemDragDepth++;
	}
	function onSystemDragOver(e: DragEvent) {
		if (!isSystemDrag(e)) return;
		// preventDefault on `dragover` is what tells the browser this
		// element accepts drops — without it, `drop` never fires and
		// the pointer shows the OS "no-drop" cursor.
		e.preventDefault();
		// `dropEffect = 'none'` would tell the browser to REJECT the
		// drop before `drop` fires — the toast/notification path in
		// `onSystemDrop` would never run for wrong-zone drops. Always
		// accept at the pointer level; the drop handler decides
		// whether to upload (`enableSystemDrop`) or fire the
		// "go to Files" toast.
		if (e.dataTransfer) e.dataTransfer.dropEffect = 'copy';
	}
	function onSystemDragLeave(e: DragEvent) {
		if (!isSystemDrag(e)) return;
		if (systemDragDepth > 0) systemDragDepth--;
	}
	function onSystemDrop(e: DragEvent) {
		if (!isSystemDrag(e)) return;
		e.preventDefault();
		// Drop ends the drag; force-clear regardless of counter state
		// (a stray unbalanced dragenter would otherwise leave the
		// overlay stuck on).
		systemDragDepth = 0;
		if (enableSystemDrop && onsystemdrop) {
			onsystemdrop(e);
		} else if (!enableSystemDrop) {
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
						// One-click recovery from a mis-drop: land the user in
						// /files so they can re-drag from the OS. We don't
						// re-attach the dropped files (browsers throw away
						// DataTransfer once the drop event returns), so this
						// is the best we can offer without a second drag.
						onClick: () => goto(resolve('/files'))
					}
				}
			);
		}
	}
</script>

{#snippet row(item: FileItem | FolderItem)}
	{@const kind = isFile(item) ? 'file' : 'folder'}
	{@const iconName = kind === 'folder' ? 'folder' : iconNameFromClass(iconClassOf(item))}
	{@const ctx = ctxOf(item.id)}
	{@const ownerId = ownerIdOf(item)}
	{@const dateVal = dateOf(item)}
	{@const sizeVal = sizeOf(item)}
	{@const mimeVal = mimeOf(item)}
	{@const draggable = isDraggable?.(item) ?? false}
	{@const dropTarget = isDropTarget?.(item) ?? false}
	<!-- svelte-ignore a11y_no_noninteractive_tabindex -->
	<div
		class="file-item"
		class:selected={selectable && selected.has(item.id)}
		class:drop-target={dropTarget && dropTargetId === item.id}
		role={onopen ? 'button' : undefined}
		tabindex={onopen ? 0 : undefined}
		aria-label={onopen ? item.name : undefined}
		data-testid={item.name}
		data-item-id={item.id}
		title={showOwner ? ownerTitle(item) : undefined}
		{draggable}
		ondragstart={draggable && onitemdragstart ? (e) => onitemdragstart(e, item) : undefined}
		ondragover={dropTarget && onitemdragover ? (e) => onitemdragover(e, item) : undefined}
		ondragleave={dropTarget && onitemdragleave ? (e) => onitemdragleave(e, item) : undefined}
		ondrop={dropTarget && onitemdrop ? (e) => onitemdrop(e, item) : undefined}
		onclick={onopen || selectable
			? (e) => {
					// Selection-first for shift/meta clicks; only "open" fires on a
					// plain click when the click wasn't consumed by selection. The
					// handler runs even without `onopen` so ⌘/Ctrl+Click still
					// toggles the row on selection-only surfaces (no navigation).
					if (handleRowClick(e, item.id)) return;
					if (onopen && !openOnDoubleClick) onopen(item);
				}
			: undefined}
		ondblclick={onopen && openOnDoubleClick ? () => onopen(item) : undefined}
		onkeydown={onopen ? (e) => e.key === 'Enter' && onopen(item) : undefined}
		oncontextmenu={onContextMenuOverride
			? (e) => onContextMenuOverride(e, item)
			: contextActions?.length
				? (e) => void openContext(e, item)
				: undefined}
	>
		{#if selectable}
			<div class="checkbox-cell" role="presentation" onclick={(e) => e.stopPropagation()}>
				<input
					type="checkbox"
					aria-label={t('common.select', 'Select')}
					data-testid={`resource-list-select-${item.id}-checkbox`}
					checked={selected.has(item.id)}
					onchange={() => toggleSelected(item.id)}
				/>
			</div>
		{/if}
		<div class="name-cell">
			<span class="file-icon {fileIconKindClass(iconName)}">
				<!-- Type icon always renders. When `enableThumbnails` is on
				     and the item is a file with a supported mime, an
				     `<img>` overlays the icon on success; onerror hides it
				     (revealing the icon) and kicks off client-side
				     generation for image / PDF / video so the next viewer
				     hits the server-side thumbnail. -->
				<Icon name={iconName} />
				{#if enableThumbnails && kind === 'file' && mimeVal && canThumbnailClientSide( { id: item.id, name: item.name, mime_type: mimeVal } )}
					<img
						class="file-thumb"
						src={fileThumbnailUrl(item.id, thumbSizeForView(filesStore.viewMode))}
						alt=""
						loading="lazy"
						onerror={(e) => {
							const img = e.currentTarget as HTMLImageElement;
							img.style.display = 'none';
							if (mimeVal === 'application/pdf') preloadPdf();
							void queueThumbnailGenerate(
								{ id: item.id, name: item.name, mime_type: mimeVal },
								(dataUrl) => {
									img.src = dataUrl;
									img.style.display = '';
								}
							);
						}}
					/>
				{/if}
				<!-- `rowBadge` snippet — page-specific extension slot (e.g.
				     `/trash`'s expiration chip). `is_favorite` / `is_shared`
				     are NOT rendered here — both are surfaced as buttons in
				     `.action-cell` so the two flags share a single visual
				     grammar (button whose `.active` styling tracks the flag). -->
				{#if rowBadge}
					<span class="file-icon__badge">{@render rowBadge(item, ctx)}</span>
				{/if}
			</span>
			<span class="name-cell__text">{item.name}</span>
		</div>
		{#if showOwner}
			<div class="owner-cell">
				{#if ownerId}
					<UserVignette userId={ownerId} fallbackLabel={resolveOwnerName?.(ownerId) ?? undefined} />
				{:else}
					<span class="owner-cell__placeholder">—</span>
				{/if}
			</div>
		{/if}
		{#if showPath}<div class="path-cell">{item.path ?? ''}</div>{/if}
		{#if showType}<div class="type-cell">{item.category ?? ''}</div>{/if}
		{#if showSize}
			<div class="size-cell">{sizeVal != null ? formatBytes(sizeVal) : '—'}</div>
		{/if}
		{#if showDate}
			<div class="date-cell">
				{#if dateCell}{@render dateCell(item, ctx)}{:else}{formatDate(dateVal)}{/if}
			</div>
		{/if}
		<div class="grid-meta">
			<!-- Grid-view meta line — size + modified date at the bottom of
			     the card. The `dateCell` snippet override (used by /trash
			     for the expiration chip) intentionally does NOT render
			     here: /trash surfaces expiration as the `.file-icon__badge`
			     overlay above the preview, and duplicating it in the top-
			     right corner would be redundant. Any date the section
			     wants to display in the card meta line is `formatDate` on
			     the intrinsic `dateVal` (per the fallback below). -->
			<span class="grid-meta__line">
				{#if sizeVal != null}<span class="grid-meta__size">{formatBytes(sizeVal)}</span>{/if}
				{#if dateVal != null}<span class="grid-meta__date">{formatDate(dateVal)}</span>{/if}
			</span>
		</div>
		<!--
			Every row that surfaces an action puts everything into a single
			`.action-cell` — the shared `ported/resourceList.css` styles the
			favorite-star, the shared-button, and the `.file-actions` kebab
			expecting them to live inside `.action-cell` (grid view uses the
			corner-overlay CSS to float them into the top-right; list view
			flexes them inline). The cell renders when ANY of favorite /
			shared / itemActions / context-menu is enabled; a row with none
			of those still lays out cleanly because the columns collapse via
			the grid track.

			Fav-star and shared-button share the same interaction grammar:
			each toggles/opens the corresponding affordance, each carries an
			`.active` class that tracks its DTO flag, and (via CSS in
			`ported/resourceList.css`) each stays visible in list view even
			without a row hover when its `.active` flag is set — so a
			favorited or shared row is discoverable at a glance without the
			user having to mouse over it.
		-->
		{#if hasActionCell}
			<div class="action-cell">
				{#if onshared}
					<button
						class="shared-button"
						class:active={item.is_shared}
						data-testid={`resource-list-shared-${item.id}-btn`}
						title={item.is_shared ? t('files.shared', 'Shared') : t('files.share', 'Share')}
						aria-pressed={item.is_shared}
						onclick={(e) => {
							e.stopPropagation();
							onshared(item);
						}}
					>
						<Icon name="oxiexport" />
					</button>
				{/if}
				{#if onfavorite}
					<button
						class="favorite-star"
						class:active={item.is_favorite}
						data-testid={`resource-list-favorite-${item.id}-btn`}
						title={item.is_favorite
							? t('files.unfavorite', 'Remove favorite')
							: t('files.favorite', 'Add favorite')}
						aria-pressed={item.is_favorite}
						onclick={(e) => {
							e.stopPropagation();
							onfavorite(item);
						}}
					>
						<Icon name={item.is_favorite ? 'star' : 'star-outline'} />
					</button>
				{/if}
				{#if itemActions}{@render itemActions(item)}{/if}
				{#if onContextMenuOverride || contextActions?.length}
					<button
						class="file-actions"
						data-testid={`resource-list-more-${item.id}-btn`}
						title={t('files.more_actions', 'More actions')}
						aria-label={t('files.more_actions', 'More actions')}
						aria-haspopup="menu"
						onclick={(e) => {
							e.stopPropagation();
							if (onContextMenuOverride) onContextMenuOverride(e, item);
							else void openContext(e, item);
						}}
					>
						<Icon name="ellipsis-v" />
					</button>
				{/if}
			</div>
		{/if}
	</div>
{/snippet}

<!-- Global Ctrl+A / ⌘+A → select every visible row. See
     `onSelectAllShortcut` in the script for the guards (selectable
     only, ignores keys inside INPUT/TEXTAREA/SELECT/contentEditable). -->
<svelte:window onkeydown={onSelectAllShortcut} />

<!--
	`.rl-root` is the drop-target boundary for OS file drops. Every
	descendant listens through here — dragging in from the desktop over
	the sticky header or the content area both hit the same handler and
	either forward to the page's upload code (files browser) or fire the
	"wrong section" toast (everywhere else).

	svelte-ignore a11y_no_static_element_interactions — the drop handlers
	are pointer-only fallbacks; keyboard users don't have a "drop a
	file" action, and the content inside is separately keyboard-nav'd.
-->
<!-- svelte-ignore a11y_no_static_element_interactions -->
<div
	class="rl-root"
	class:rl-root--rubberbanding={rubberband !== null}
	bind:this={rlRoot}
	onpointerdown={onRootPointerDown}
	ondragenter={onSystemDragEnter}
	ondragover={onSystemDragOver}
	ondragleave={onSystemDragLeave}
	ondrop={onSystemDrop}
>
	<!--
		Title sits OUTSIDE `.page-sticky-header` so it scrolls away on
		descent — pinning it would waste the always-scarce top-of-viewport
		strip. What stays sticky (defined by `.page-sticky-header`'s
		`position: sticky` in shared CSS): the action bar and, when
		provided, the breadcrumb — the two controls the user reaches for
		while scrolling.
	-->
	<h1 class="page-title">{title}</h1>
	<div class="page-sticky-header">
		<ActionBar>
			{#snippet start()}
				<!--
				The action-bar left cluster has two states:
				  1. `batchActions` — when the user has selected items, the
				     page's batch buttons (Move / Delete / Restore / …)
				     replace the default cluster, prefixed with a "clear
				     selection" close button + count label so the batch is
				     dismissable without unchecking every row by hand. The
				     wrapper carries the `.batch-selection-bar` class so
				     the shared `batchToolbar.css` styling (muted
				     background, count typography, action hover states)
				     applies — matching the pre-migration `/files` look.
				  2. `actions` — the page's default cluster
				     (Upload / New folder / Empty trash / Clear recent).
			-->
				<div
					class="action-buttons"
					class:batch-selection-bar={selectable && selected.size > 0 && batchActions}
				>
					{#if selectable && selected.size > 0 && batchActions}
						<button
							class="batch-bar-close"
							title={t('common.clear', 'Clear selection')}
							aria-label={t('common.clear', 'Clear selection')}
							data-testid="resource-list-batch-close-btn"
							onclick={clearSelection}
						>
							<Icon name="times" />
						</button>
						<span class="batch-bar-count"
							>{t('files.selected_count', { count: selected.size }, '{{count}} selected')}</span
						>
						<div class="batch-bar-actions">
							{@render batchActions(selectedItems)}
						</div>
					{:else if actions}
						{@render actions()}
					{/if}
				</div>
			{/snippet}
			{#snippet end()}
				<DisplayModeControls
					groups={groupBys}
					{groupBy}
					{reversed}
					ongroup={selectGroup}
					ondirection={toggleDirection}
					showViewMode={showViewToggle}
					{showDotfileToggle}
				/>
			{/snippet}
		</ActionBar>
		{#if breadcrumb}
			<!--
				Breadcrumb sits BELOW the action bar (same sticky block) so
				the two travel together as the sticky top strip: bar +
				breadcrumb pin, everything above (title) and below (list)
				scrolls normally.
			-->
			<div class="rl-breadcrumb">{@render breadcrumb()}</div>
		{/if}
	</div>

	{#if error}
		<EmptyState icon="exclamation-circle" title={error} error />
	{:else if renderSkeleton}
		<!-- Only renders after the 150 ms delay elapses AND we're still
		     loading with no items — fast loads skip this entirely. -->
		<SkeletonList count={SKELETON.length} />
	{:else if isEmpty && !loading}
		<!-- Empty state gates on `!loading` (not just `isEmpty`) so
		     mid-load empty-content windows don't flash the "Folder is
		     empty" banner. Renders only when the fetch has definitively
		     completed with zero items. -->
		<EmptyState
			icon={emptyIcon}
			title={emptyText ?? t('common.empty', 'Nothing here yet.')}
			hint={emptyHint}
		>
			{#if emptyAction}{@render emptyAction()}{/if}
		</EmptyState>
	{:else}
		<div class="files-container" bind:clientWidth={gridWidth}>
			{#if grouped && filesStore.viewMode === 'list'}
				<div class="files-list-view" style="--files-list-columns: {columns}">
					{#if listHeaderOverride}{@render listHeaderOverride()}{:else}{@render listHeader()}{/if}
					{#each sections as section (section.key)}
						{#if section.label}
							<div class="rl-swimlane-header" role="rowheader">
								<span class="rl-swimlane-header__label">{section.label}</span>
								{#if bucketAction}
									<span class="rl-swimlane-header__action">
										{@render bucketAction(section.key)}
									</span>
								{/if}
							</div>
						{/if}
						<!-- Window each section's rows so a large grouped list (e.g. a big
					     trash, grouped by remaining days) doesn't mount every row. -->
						<VirtualList items={section.rows} rowHeight={56} key={(e) => e.id} {row} />
					{/each}
				</div>
			{:else if grouped}
				<!-- Grouped GRID: a vertical stack of (header + its own windowed card
			     grid) per section. The outer is a flex column, NOT `.files-grid-view`
			     (which is itself a grid and would place each header/VirtualList into a
			     cell) — the grid lives on each VirtualList's inner window via
			     `windowClass`, exactly like the flat-grid arm. This was the last
			     unwindowed path: a grouped-by-default grid (trash) mounted every card
			     (benches/ROUND13.md §V1). -->
				<div class="rl-grouped-grid">
					{#each sections as section (section.key)}
						{#if section.label}
							<div class="rl-swimlane-header rl-swimlane-header--grid" role="rowheader">
								<span class="rl-swimlane-header__label">{section.label}</span>
								{#if bucketAction}
									<span class="rl-swimlane-header__action">
										{@render bucketAction(section.key)}
									</span>
								{/if}
							</div>
						{/if}
						<VirtualList
							items={section.rows}
							columns={gridCols}
							rowHeight={240}
							windowClass="files-grid-view"
							key={(e) => e.id}
							{row}
						/>
					{/each}
				</div>
			{:else if filesStore.viewMode === 'list'}
				<!-- Flat list view: only the visible rows are mounted. The spacer keeps the
			     full scroll height so the end-of-list sentinel still fires. -->
				<div class="files-list-view" style="--files-list-columns: {columns}">
					{#if listHeaderOverride}{@render listHeaderOverride()}{:else}{@render listHeader()}{/if}
					<VirtualList items={visibleItems} rowHeight={56} key={(e) => e.id} {row} />
				</div>
			{:else}
				<!-- Grid view: the windowed list's inner element IS the card grid. -->
				<VirtualList
					items={visibleItems}
					columns={gridCols}
					rowHeight={240}
					windowClass="files-grid-view"
					key={(e) => e.id}
					{row}
				/>
			{/if}

			{#if hasMore}
				<button
					class="btn btn-secondary rl-more"
					data-testid="resource-list-load-more-btn"
					onclick={onloadmore}
					disabled={loading}
				>
					{loading ? t('common.loading', 'Loading…') : t('common.load_more', 'Load more')}
				</button>
			{/if}
			<!-- Infinite-scroll sentinel: auto-loads the next page as it nears the viewport. -->
			<div bind:this={sentinel} class="rl-sentinel" aria-hidden="true"></div>
		</div>
	{/if}
	{#if rubberband}
		<!-- Marquee selection rectangle. Positioned relative to `.rl-root`
	     (which is `position: relative`); pointer-events off so the
	     live pointermove handler on window still sees the drag. -->
		<div
			class="rl-rubberband"
			style:left="{rubberband.x}px"
			style:top="{rubberband.y}px"
			style:width="{rubberband.w}px"
			style:height="{rubberband.h}px"
			aria-hidden="true"
		></div>
	{/if}

	<!-- System-drop overlay. Renders on top of the list ONLY while the
	     user is dragging OS files over the wrapper AND this surface
	     accepts them (`enableSystemDrop`). Filled tint + centred
	     icon+message so the drop target is unmistakable — the dashed
	     border alone read as decoration on busy folders. `pointer-events:
	     none` on the container keeps it inert (drag events still hit
	     `.rl-root` underneath so `dragleave`/`drop` fire correctly). -->
	{#if enableSystemDrop && (systemDropOver || systemDropOverlayActive)}
		<div class="rl-drop-overlay" aria-hidden="true">
			<div class="rl-drop-overlay__inner">
				<Icon name="cloud-arrow-up" class="rl-drop-overlay__icon" />
				<span class="rl-drop-overlay__label">
					{t('files.drop_to_upload', 'Drop files here to upload')}
				</span>
			</div>
		</div>
	{/if}
</div>
<!-- /.rl-root -->

{#snippet listHeader()}
	<div class="list-header">
		{#if selectable}
			<div class="checkbox-cell">
				<input
					type="checkbox"
					aria-label={t('common.select_all', 'Select all')}
					data-testid="resource-list-select-all-checkbox"
					checked={allSelected}
					onchange={toggleSelectAll}
				/>
			</div>
		{/if}
		<div class="name-cell">{t('files.col_name', 'Name')}</div>
		{#if showOwner}<div class="owner-cell">
				{ownerLabel ?? t('files.col_created_by', 'Created by')}
			</div>{/if}
		{#if showPath}<div class="path-cell">{pathLabel ?? t('files.col_path', 'Location')}</div>{/if}
		{#if showType}<div class="type-cell">{t('files.col_type', 'Type')}</div>{/if}
		{#if showSize}<div class="size-cell">{t('files.col_size', 'Size')}</div>{/if}
		{#if showDate}<div class="date-cell">{dateLabel ?? t('files.col_modified', 'Date')}</div>{/if}
		{#if hasActionCell}<div class="action-cell"></div>{/if}
	</div>
{/snippet}

{#if ctxOpen && ctxItem && contextActions}
	{@const visibleActions = contextActions.filter(
		(a) => a.visible?.(ctxItem!, ctxOf(ctxItem!.id)) !== false
	)}
	<div
		class="rl-ctx-scrim"
		role="presentation"
		onclick={closeContext}
		oncontextmenu={(e) => e.preventDefault()}
	></div>
	<div
		class="rl-ctx-menu"
		style:left="{ctxX}px"
		style:top="{ctxY}px"
		role="menu"
		data-testid="resource-list-context-menu"
	>
		{#each visibleActions as action (action.key)}
			{@const dis = action.disabled?.(ctxItem!, ctxOf(ctxItem!.id)) === true}
			<button
				class="rl-ctx-item"
				class:rl-ctx-item--danger={action.danger}
				class:rl-ctx-item--disabled={dis}
				role="menuitem"
				disabled={dis}
				aria-disabled={dis}
				data-testid={`resource-list-context-${action.key}-item`}
				onclick={() => {
					if (dis) return;
					const target = ctxItem!;
					closeContext();
					action.run(target, ctxOf(target.id));
				}}
			>
				<Icon name={action.icon} />
				{action.label}
			</button>
		{/each}
	</div>
{/if}

<style>
	.rl-more {
		margin: var(--space-4) auto 0;
	}

	.rl-sentinel {
		height: 1px;
		width: 100%;
	}

	/* ── OS-drop wrapper ──
	   `.rl-root` catches drops that miss a specific in-app drop target
	   (row → folder). Its highlight fires ONLY when
	   `enableSystemDrop && dragging` — the "wrong drop zone" toast path
	   deliberately leaves the surface unhighlighted so users don't get a
	   false accept cue. */
	.rl-root {
		position: relative;
	}

	/* Viewport-fixed drop overlay. `position: fixed` (not absolute) so
	   it covers the whole visible browser window regardless of the
	   user's scroll position — an absolute-inset-0 inside `.rl-root`
	   would center the card at the middle of the FULL list height,
	   which sits above the fold on a scrolled folder. The dashed border
	   also gets painted at the true viewport edge, so the sticky
	   action-bar + breadcrumb are covered rather than clipping the
	   border. `pointer-events: none` so drag events still fall through
	   to `.rl-root`'s handlers underneath.
	   `--z-overlay` beats `--z-sticky` (page chrome) + `--z-dropdown`
	   (search suggestions); stays below `--z-modal` so a modal opened
	   concurrently still wins. */
	.rl-drop-overlay {
		position: fixed;
		inset: 0;
		display: flex;
		align-items: center;
		justify-content: center;
		background: color-mix(in srgb, var(--color-accent) 12%, transparent);
		border: 2px dashed var(--color-accent);
		border-radius: var(--radius-md);
		pointer-events: none;
		z-index: var(--z-overlay);
	}

	.rl-drop-overlay__inner {
		display: flex;
		flex-direction: column;
		align-items: center;
		gap: var(--space-3);
		padding: var(--space-6) var(--space-8);
		color: var(--color-accent);
		background: var(--color-bg-surface);
		border-radius: var(--radius-lg);
		box-shadow: var(--shadow-lg);
	}

	.rl-drop-overlay :global(.rl-drop-overlay__icon) {
		font-size: 3rem;
	}

	.rl-drop-overlay__label {
		font-weight: var(--weight-semibold);
		font-size: var(--text-lg);
	}

	/* ── Rubberband (marquee) selection ────────────────────────────
	   Absolute overlay drawn while the user drags. Positioned inside
	   `.rl-root`; the pointer events go to the window listener, so
	   the rectangle itself is inert. `.rl-root--rubberbanding`
	   suppresses text selection under the cursor so dragging over row
	   text doesn't leave a highlighted mess behind. */
	.rl-root--rubberbanding {
		user-select: none;
	}

	.rl-rubberband {
		position: absolute;
		z-index: 5;
		background: color-mix(in srgb, var(--color-accent) 12%, transparent);
		border: 1px solid var(--color-accent);
		border-radius: var(--radius-sm);
		pointer-events: none;
	}

	/* ── Breadcrumb strip inside the sticky header ── */
	.rl-breadcrumb {
		display: flex;
		align-items: center;
		gap: var(--space-2);
		margin-bottom: var(--space-2);
		min-height: 28px;
	}

	/* ── Owner vignette ── */
	.owner-cell {
		display: flex;
		align-items: center;
		min-width: 0;
	}

	.owner-cell__placeholder {
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
		color: var(--color-text-secondary);
	}

	.name-cell__text {
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}

	/* ── Swimlane section header ── */
	.rl-swimlane-header {
		grid-column: 1 / -1;
		padding: var(--space-3) var(--space-1) var(--space-1);
		font-size: var(--text-sm);
		font-weight: var(--weight-semibold);
		color: var(--color-text-secondary);
		border-bottom: 1px solid var(--color-border-faint);
		display: flex;
		align-items: center;
		justify-content: space-between;
		gap: var(--space-2);
	}

	/* Optional per-bucket action (e.g. trash page's per-drive Empty). */
	.rl-swimlane-header__action {
		display: inline-flex;
		align-items: center;
	}

	/* Grouped-grid container: a vertical stack of (header + its own windowed
	   card grid) per section. Not `.files-grid-view` — the grid is on each
	   VirtualList's inner window, so this outer element just stacks. */
	.rl-grouped-grid {
		display: flex;
		flex-direction: column;
		gap: var(--space-2);
	}

	/* In the flex stack the `grid-column: 1 / -1` span (meant for the grid
	   context) is inert; the header spans naturally as a block-level flex
	   child. */
	.rl-swimlane-header--grid {
		grid-column: auto;
	}

	/* Grid view date meta line. */
	.grid-meta__line {
		display: flex;
		align-items: center;
		gap: var(--space-2);
	}

	/* ── Row-badge placement ───────────────────────────────────────
	   Single copy of the `rowBadge` snippet lives inside `.file-icon`
	   — one DOM location for BOTH list and grid views. `.file-icon`
	   is `position: relative` in the shared CSS so this absolute
	   overlay anchors to it. Positioned bottom-center with NO
	   vertical translate → the badge sits fully inside the preview
	   area with its bottom edge flush against the icon's bottom, so
	   it doesn't crop or spill outside. Because it's absolutely
	   positioned it never contributes to the row/card height. */
	.file-icon__badge {
		display: inline-flex;
		position: absolute;
		left: 50%;
		bottom: 0;
		transform: translateX(-50%);
		z-index: 2;
		white-space: nowrap;
		pointer-events: none;
	}

	/* ── Context menu ── */
	.rl-ctx-scrim {
		position: fixed;
		inset: 0;
		z-index: 1000;
	}

	.rl-ctx-menu {
		position: fixed;
		z-index: 1001;
		min-width: 200px;
		padding: var(--space-1);
		background: var(--color-bg-surface);
		border: 1px solid var(--color-border);
		border-radius: var(--radius-md);
		box-shadow: var(--shadow-lg);
	}

	.rl-ctx-item {
		display: flex;
		align-items: center;
		gap: var(--space-2);
		width: 100%;
		padding: var(--space-2) var(--space-3);
		border: none;
		border-radius: var(--radius-sm);
		background: transparent;
		color: var(--color-text);
		text-align: left;
		cursor: pointer;
	}

	.rl-ctx-item:hover:not(:disabled) {
		background: var(--color-bg-hover);
	}

	/* Disabled entry — dimmed but STILL RENDERED so the user sees that
	   the option exists and infers "I can't do this here" rather than
	   assuming a forgotten feature. No `cursor: not-allowed` badge on
	   hover (deliberate — a forbidden-sign cursor reads as alarming for
	   an entry the user didn't try to activate). The `disabled`
	   attribute alone still blocks click + keyboard activation and
	   flags the element to assistive tech via `aria-disabled`. */
	.rl-ctx-item--disabled {
		opacity: 0.5;
	}

	.rl-ctx-item--disabled:hover {
		background: transparent;
	}

	.rl-ctx-item--danger {
		/* Danger *foreground* on the light menu surface — the red accent, not
		   --color-danger-text (white, for text ON a red fill, invisible here). */
		color: var(--color-danger-alt);
	}
</style>
