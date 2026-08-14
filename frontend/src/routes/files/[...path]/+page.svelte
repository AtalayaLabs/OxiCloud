<script lang="ts">
	import { errorMessage, errorToast } from '$lib/utils/errors';
	import { goto } from '$app/navigation';
	import { resolve } from '$app/paths';
	import { page } from '$app/state';
	import { untrack } from 'svelte';
	import { SvelteSet } from 'svelte/reactivity';
	import Icon from '$lib/icons/Icon.svelte';
	import {
		createFolder,
		deleteFolder,
		fetchFolderPage,
		getFolderAncestors,
		invalidateFolderCache,
		moveFolder,
		rememberFolderName,
		renameFolder,
		type FolderListing
	} from '$lib/api/endpoints/folders';
	import {
		deleteFile,
		fileDownloadUrl,
		moveFile,
		renameFile,
		uploadFileWithProgress
	} from '$lib/api/endpoints/files';
	import { folderZipUrl } from '$lib/api/endpoints/folders';
	import {
		instantUploadOwned,
		resolveOwnedHashes,
		tryDeltaUpload
	} from '$lib/api/endpoints/deltaUpload';
	import {
		acquireUploadGuard,
		markUploadFinished,
		markUploadStarted,
		releaseUploadGuard
	} from '$lib/upload/interruption';
	import { addFavorite, removeFavorite } from '$lib/api/endpoints/favorites';
	import { canEditWithWopi, getEditorUrlWithFallback } from '$lib/api/endpoints/wopi';
	import { addTracks, createPlaylist, listPlaylists } from '$lib/api/endpoints/music';
	import { copyFiles, copyFolders } from '$lib/api/endpoints/batch';
	import { apiFetch } from '$lib/api/client';
	import { getCsrfHeaders } from '$lib/api/csrf';
	import { countHidden, filterDotfiles } from '$lib/utils/dotfileFilter';
	import { preferences } from '$lib/stores/preferences.svelte';
	import type { FileItem, FolderItem, ItemType } from '$lib/api/types';
	import ReadOnlyBanner from '$lib/components/ReadOnlyBanner.svelte';
	import FolderBreadcrumb from '$lib/components/FolderBreadcrumb.svelte';
	import ResourceList, {
		isFile,
		type GroupByDef as RLGroupByDef
	} from '$lib/components/ResourceList.svelte';
	import { lazyComponent } from '$lib/composables/lazyComponent.svelte';
	import { t } from '$lib/i18n/index.svelte';
	import { confirmDialog, promptDialog } from '$lib/stores/dialogs.svelte';
	import { drives as drivesStore } from '$lib/stores/drives.svelte';
	import { files as filesStore } from '$lib/stores/files.svelte';
	import { session } from '$lib/stores/session.svelte';
	import { ui } from '$lib/stores/ui.svelte';
	import { dateBucket, sizeBucket, typeLabel } from '$lib/stores/files.svelte';
	import { replaceSet } from '$lib/utils/sets';

	// File preview and the WOPI editor are heavy and only appear on demand, so
	// their modules load the first time the user opens one (see the effects that
	// call `.load()` when `viewerOpen` / `wopiOpen` flip true).
	const fileViewer = lazyComponent(() => import('$lib/components/FileViewer.svelte'));
	const wopiEditor = lazyComponent(() => import('$lib/components/WopiEditor.svelte'));
	const moveDialog = lazyComponent(() => import('$lib/components/MoveDialog.svelte'));
	const shareDialog = lazyComponent(() => import('$lib/components/ShareDialog.svelte'));

	// The URL rest param is the trail of folder ids from home's children down.
	// /files → home root; /files/a/b → folder b inside a inside home.
	const pathSegments = $derived((page.params.path ?? '').split('/').filter((s) => s.length > 0));

	// The drive whose content the user is currently browsing.
	//
	// Priorities (first match wins):
	//   1. `currentFolderDriveId` — set by `load()` from the ancestors
	//      response (`chain.ancestors.at(-1).drive_id`). Authoritative
	//      for deep-links too (the URL's leading segment might not be a
	//      drive root).
	//   2. `listing.folders[0]?.drive_id` — fast-path when the folder
	//      has at least one subfolder; avoids waiting on the ancestors
	//      response before the initial `applyListing`.
	//      (`FileDto` doesn't carry `drive_id` today, so we can't use
	//      files as a fallback source; folders alone.)
	//   3. `drivesStore.findByRootFolderId(pathSegments[0])` — legacy
	//      fallback for the common "sidebar picker → drive root URL"
	//      navigation, unchanged from `rootIcon` above.
	//
	// Feeds the read-only freeze banner further down: when this drive's
	// `policies.read_only` is on, mutation controls elsewhere in the app
	// will fail against the backend engine gate; the banner is the
	// affordance that tells the user why.
	let currentFolderDriveId = $state<string | null>(null);
	const currentDrive = $derived.by(() => {
		if (currentFolderDriveId) {
			const d = drivesStore.findById(currentFolderDriveId);
			if (d) return d;
		}
		const listingDriveId = listing.folders[0]?.drive_id ?? null;
		if (listingDriveId) return drivesStore.findById(listingDriveId);
		return drivesStore.findByRootFolderId(pathSegments[0] ?? null);
	});

	let listing = $state<FolderListing>({ folders: [], files: [] });
	// Server-order accumulator — items in the exact sequence the backend
	// returned across pages, honouring `sortField`+`reversed` on the wire.
	// Under order_by=name/type/size the server puts folders first then files;
	// under modified_at/created_at they interleave. `rlItems` reads this
	// directly so ResourceList renders in server order without a re-sort.
	let orderedItems = $state<Array<FileItem | FolderItem>>([]);
	// Cursor for the NEXT page. `undefined` after the final page has landed
	// (or before the first fetch). Bound to ResourceList's `hasMore`.
	let pageCursor = $state<string | undefined>(undefined);
	// Guard so a fast-firing onloadmore (double intersection tick) can't
	// enqueue two concurrent next-page fetches on the same cursor.
	let loadingMore = $state(false);

	// ── Transient "New elements" swimlane ────────────────────────────
	// Server sort is authoritative, so an uploaded file lands wherever
	// its sort key places it — possibly halfway down the list or on an
	// unloaded page. To confirm "your upload worked", we detect items
	// that appeared on the current page after a mutation (upload / new
	// folder / dropped tree) and hoist them into a first-class swimlane
	// at the top of the list. The swimlane clears on next folder nav.
	//
	// Detection strategy: caller of `reloadAndTrackNew()` snapshots the
	// current id set, runs `reload()`, and diffs the freshly-loaded
	// page 1 against the snapshot — every new id joins `newlyAdded`.
	//
	// Known limitation: server-sort places the new item on an unloaded
	// page (e.g. sort=size on a 5 000-item folder and the new file is
	// mid-size). It won't appear in the swimlane until the user scrolls
	// far enough for that page to load. Threading the returned FileItem
	// out of the 5-layer upload stack would fix this — deferred until
	// the diff approach proves insufficient.
	const newlyAdded = new SvelteSet<string>();

	// Dotfile hide filter is now applied inside `rlItems` (below) directly
	// on the server-ordered accumulator, so a single filter pass feeds
	// ResourceList. Selection / batch ops iterate ResourceList's own
	// selection set, which already excludes hidden rows. Direct lookups
	// by id (deep-links via `?file=<uuid>`) still go through
	// `listing.files` so hidden files remain reachable via their own URL
	// — same UX as macOS Finder.
	// Count of items suppressed by the filter — surfaced in the
	// empty-state hint when the folder isn't visually empty but
	// contains only dotfiles the user has hidden, so a "why is this
	// empty?" question is answerable at a glance.
	const hiddenCount = $derived(
		preferences.hideDotfiles ? countHidden(listing.folders) + countHidden(listing.files) : 0
	);
	let currentId = $state<string | null>(null);
	// Default `true` (not `false`) so the first render — before the
	// `$effect` fires `load()` — shows the "loading" arm of ResourceList
	// (skeleton, gated on 100 ms delay) instead of the "empty" arm
	// ("No elements here"). Ed's 2026-07-26 report: a brief empty-state
	// flash appeared between page mount and the first fetch landing.
	// `load()` still writes `loading = true` before its first await, so
	// mid-navigation clears work as before.
	let loading = $state(true);
	// `showSkeleton` used to sit 100 ms behind `loading` to avoid flashing
	// skeleton bars on fast loads. Retired 2026-07-26 because ResourceList
	// received `loading={showSkeleton}` (not the real `loading` state), so
	// during those 100 ms it saw `loading=false && items=[]` and rendered
	// the empty-state ("Folder is empty") — the flash Ed reported. Pass
	// the real `loading` instead; the skeleton renders instantly for
	// slow loads and instantly-disappears for fast loads (users don't
	// perceive a sub-100 ms frame flip).
	let error = $state<string | null>(null);
	let fileInput = $state<HTMLInputElement | null>(null);
	let uploading = $state(false);

	interface ActionTarget {
		id: string;
		name: string;
		kind: ItemType;
	}
	let moveOpen = $state(false);
	let moveMode = $state<'move' | 'copy'>('move');
	let shareOpen = $state(false);
	let actionTarget = $state<ActionTarget | null>(null);
	let moveItems = $state<ActionTarget[] | null>(null);

	// Favorite / shared state now lives inline on every `FileItem` /
	// `FolderItem` DTO (`is_favorite`, `is_shared` — see
	// `frontend/src/lib/api/types.ts`). Populated by the backend
	// listing SQL (per-row `EXISTS`) and single-item enrichment
	// helper. The row-badge snippet and star gate read these
	// fields directly; the toggle path mutates the item in place
	// inside `orderedItems`. No more `SvelteSet` shadowing.

	function openMove(kind: ItemType, id: string, name: string) {
		actionTarget = { id, name, kind };
		moveItems = null;
		moveMode = 'move';
		moveOpen = true;
	}
	function openCopy(kind: ItemType, id: string, name: string) {
		actionTarget = { id, name, kind };
		moveItems = null;
		moveMode = 'copy';
		moveOpen = true;
	}
	function openShare(kind: ItemType, id: string, name: string) {
		actionTarget = { id, name, kind };
		shareOpen = true;
	}

	async function toggleFavorite(kind: ItemType, id: string) {
		// Server-authoritative `is_favorite` lives on every
		// `FileItem`/`FolderItem` DTO. Read → optimistic flip → server
		// call → revert on failure. All state changes happen in-place
		// on the item inside `orderedItems`; there is no shadow set.
		const item = orderedItems.find((it) => it.id === id);
		if (!item) return; // row scrolled off / navigated away mid-click
		const wasFav = item.is_favorite;
		item.is_favorite = !wasFav;
		try {
			if (wasFav) await removeFavorite(kind, id);
			else await addFavorite(kind, id);
		} catch (e) {
			errorToast(e);
			item.is_favorite = wasFav;
		}
	}

	// Bumped on every load; a stale in-flight response checks this before it
	// writes state, so a fast navigation can't be clobbered by an older fetch.
	let loadSeq = 0;

	/**
	 * Load the current folder's listing.
	 *
	 * @param reset  Fresh load (folder nav / sort change / manual reload):
	 *               clears cursor+accumulator, redoes canonicalization +
	 *               breadcrumbs, then fetches page 1.
	 *
	 *               Append (from `loadMore()` on scroll-bottom): skips
	 *               preconditions, fetches the NEXT page using the stored
	 *               cursor and appends to `listing`+`orderedItems`.
	 *
	 * Server-side sort: `orderBy=sortField, reverse=reversed` are passed on
	 * every page request so items arrive already in the requested order —
	 * client-side sort was removed and `rlItems` reads `orderedItems`
	 * verbatim. Sort/group changes trigger `load(true)` via `$effect`.
	 */
	async function load(reset: boolean = true) {
		error = null;
		const seq = ++loadSeq;

		let folderId: string;
		if (reset) {
			// External users have no home folder; send them to shared-with-me.
			if (session.isExternalUser && pathSegments.length === 0) {
				await goto(resolve('/shared-with-me'), { replaceState: true });
				return;
			}
			const home = await session.loadHomeFolder();

			// Canonicalize bare `/files` → `/files/<last-chosen-drive-root>` (or
			// the default drive's root when there's no memory yet). Keeps the URL
			// explicit, the breadcrumb populated, and the drive picker correctly
			// highlighted. The DrivePicker writes `oxi-last-drive-root` on click.
			if (pathSegments.length === 0) {
				const last =
					typeof localStorage !== 'undefined' ? localStorage.getItem('oxi-last-drive-root') : null;
				const target = last ?? home;
				if (target) {
					await goto(resolve(`/files/${target}`), { replaceState: true });
					return;
				}
			}

			const resolvedId = pathSegments.at(-1) ?? home;
			if (!resolvedId) {
				error = t('files.no_home', 'No home folder available.');
				return;
			}
			folderId = resolvedId;
			currentId = folderId;
			filesStore.currentFolder = folderId;

			// Reset paging state: previous folder's cursor is meaningless
			// on the new folder — must clear or the first append would
			// paginate the OLD folder's next-page slice.
			//
			// `listing` / `orderedItems` are deliberately NOT cleared —
			// the previous folder's rows stay on screen during the (~25 ms)
			// fetch, then the response handler swaps in the new folder's
			// content atomically. Stale-while-revalidate for the inter-
			// folder case (Ed 2026-07-26: the pre-refactor clear-then-
			// fetch-then-render sequence flashed either the SkeletonList
			// or the "Folder is empty" empty-state for the fetch window,
			// depending on which arm ResourceList happened to render for
			// the empty-loading state; neither is useful for a 25 ms
			// transition). First-mount (no previous content) still hits
			// the skeleton correctly because `orderedItems` defaults `[]`
			// and `loading` defaults `true` — the empty-loading arm
			// gates on that.
			loading = true;
			pageCursor = undefined;

			// Legacy path-chain URLs canonicalize to the single-id form on
			// load. `/files/A/B/C` still resolves (router matches `[...path]`)
			// but the URL bar and any subsequent bookmark reflects the
			// canonical `/files/C` — see 2026-07-26 URL-format discussion.
			// `replaceState` (not `pushState`) so the back button doesn't
			// gain a spurious entry.
			if (pathSegments.length > 1 && typeof window !== 'undefined') {
				window.history.replaceState({}, '', resolve(`/files/${folderId}`));
			}

			// Resolve the current folder's drive_id via the ancestors
			// response — every `FolderAncestor` carries `drive_id`, so
			// the shared `<FolderBreadcrumb>`'s in-flight call is the
			// same round-trip we'd otherwise duplicate here. The
			// `ancestorsInflight` dedup map inside `getFolderAncestors`
			// means this second caller gets the same promise, not a
			// second HTTP request — the extra `getFolder(folderId)`
			// that used to fire here is gone (2026-07-26 UX pass on
			// /files load traffic).
			void getFolderAncestors(folderId)
				.then((chain) => {
					if (seq !== loadSeq) return;
					const leaf = chain.ancestors.at(-1);
					if (leaf) currentFolderDriveId = leaf.drive_id;
				})
				.catch(() => {
					// Fallback chain in `currentDrive` still gives us a
					// best-effort drive resolution (listing.folders[0].drive_id,
					// then drivesStore lookup by root-folder id).
				});
		} else {
			// Append path: reuse `currentId`. `pageCursor === undefined` means
			// we've already reached the last page; treat as no-op.
			if (!currentId || pageCursor === undefined) return;
			folderId = currentId;
		}

		try {
			const page = await fetchFolderPage(folderId, {
				orderBy: sortField,
				reverse: reversed,
				cursor: reset ? undefined : pageCursor
			});
			if (seq !== loadSeq) return; // superseded by a newer navigation
			if (reset) {
				listing = {
					folders: page.folders,
					files: page.files
				};
				orderedItems = page.items;
			} else {
				listing = {
					folders: [...listing.folders, ...page.folders],
					files: [...listing.files, ...page.files]
				};
				orderedItems = [...orderedItems, ...page.items];
			}
			pageCursor = page.nextCursor;
			error = null;
		} catch (e) {
			if (seq !== loadSeq) return;
			const status = (e as { status?: number })?.status;
			error =
				status === 403
					? t('errors.forbidden', 'Could not load files')
					: e instanceof Error
						? e.message
						: String(e);
		} finally {
			if (seq === loadSeq && reset) {
				loading = false;
			}
		}
	}

	/**
	 * Fetch and append the next page. Invoked by ResourceList's
	 * IntersectionObserver when the bottom sentinel enters the viewport.
	 * The `loadingMore` guard collapses a double-fire (the observer can
	 * tick twice on the same intersection edge).
	 */
	async function loadMore() {
		if (loadingMore || pageCursor === undefined) return;
		loadingMore = true;
		try {
			await load(false);
		} finally {
			loadingMore = false;
		}
	}

	/** Data changed — drop cached listings and reload the current folder fresh. */
	async function reload() {
		invalidateFolderCache();
		await load(true);
	}

	/**
	 * Reload + populate the "new elements" swimlane with anything that
	 * appeared on page 1 after the mutation.
	 *
	 * Called from mutation paths that ADD items (upload / dropped tree /
	 * create-folder). Renames, deletes, moves use plain `reload()`
	 * — nothing new to hoist.
	 */
	async function reloadAndTrackNew(): Promise<void> {
		const before = new SvelteSet<string>();
		for (const it of orderedItems) before.add(it.id);
		await reload();
		// `reload()` resets `pageCursor` + fetches page 1 fresh, so
		// `orderedItems` is now the freshly-loaded page. Every id that
		// wasn't there before this reload joins the swimlane.
		newlyAdded.clear();
		for (const it of orderedItems) if (!before.has(it.id)) newlyAdded.add(it.id);
		// Scroll the page back to the top so the freshly-hoisted "New
		// elements" swimlane is visible without the user having to hunt
		// for it — the whole point of the swimlane is to confirm "your
		// upload landed". Only fires when we actually detected new items,
		// so a bare reload doesn't yank the user's scroll position.
		// Smooth scroll for the visual continuity — instant would feel
		// like the page reloaded. `scrollTo` at (0, 0) is a no-op if
		// the user was already at the top; no jitter cost.
		if (newlyAdded.size > 0 && typeof window !== 'undefined') {
			window.scrollTo({ top: 0, behavior: 'smooth' });
		}
	}

	function openFolder(folder: FolderItem) {
		// Canonical single-id URL. Legacy `/files/A/B/C` still resolves
		// (canonicalize-on-load rewrites it inside `load()`), but new
		// navigation lands directly on `/files/{id}`.
		goto(resolve(`/files/${folder.id}`));
	}

	async function onNewFolder() {
		const name = await promptDialog({
			title: t('files.new_folder', 'New folder'),
			placeholder: t('files.new_folder_prompt', 'New folder name'),
			confirmText: t('common.create', 'Create')
		});
		if (!name) return;
		try {
			await createFolder(name, currentId);
			await reloadAndTrackNew();
			// Vanish-warning: user just made a `.folder` and it's
			// already hidden by their preference — otherwise the new
			// folder would appear to have not been created. Third hook
			// point in the "creating a dotfile while hide is on" family
			// (upload + rename cover the other two).
			if (preferences.hideDotfiles && name.startsWith('.')) {
				ui.notify(
					t(
						'files.new_folder_dotfile_hidden',
						{ name },
						"Created folder '{{name}}' — hidden by your dotfile preference."
					),
					'info'
				);
			}
		} catch (e) {
			errorToast(e);
		}
	}

	// Upload at most this many files concurrently. Kept low so we stay well under
	// the browser's ~6 connections-per-host budget — leaving headroom for the
	// session-refresh/poll requests and (for genuinely huge files) a delta worker,
	// which itself opens several connections. Over-subscribing here is what made
	// small uploads queue until the watchdog cancelled them ("stuck at N%").
	const UPLOAD_CONCURRENCY = 2;

	/** Outer backstop deadline (ms). The plain-upload path already self-aborts on
	 *  a stalled connection (see `uploadFileWithProgress`); this only catches a
	 *  wedged delta worker or by-hash request so a lane can never hang forever. */
	const FILE_BACKSTOP_MS = 4 * 60_000;

	/** Reject after `ms` if `p` hasn't settled. */
	function withTimeout<T>(p: Promise<T>, ms: number): Promise<T> {
		return new Promise((resolve, reject) => {
			const timer = setTimeout(() => reject(new Error('upload timed out')), ms);
			p.then(
				(v) => {
					clearTimeout(timer);
					resolve(v);
				},
				(e) => {
					clearTimeout(timer);
					reject(e);
				}
			);
		});
	}

	/**
	 * Upload one file through the best available path, returning the bytes saved
	 * by deduplication (0 when the body was sent in full). Order:
	 *   1. Instant by-hash upload — zero bytes when `ownedHash` is set (the batch
	 *      check found the server already has this exact blob).
	 *   2. Delta upload — sub-file CDC dedup for large files (>= 8 MB).
	 *   3. Plain byte upload — fallback when neither applies.
	 * Throws on a hard failure; the error carries `isQuota` so the batch can stop
	 * early when the disk is full.
	 */
	async function uploadOneFile(
		folderId: string | null,
		file: File,
		report: (frac: number) => void,
		ownedHash: string | null
	): Promise<number> {
		const dedup =
			(ownedHash && folderId ? await instantUploadOwned(folderId, file, ownedHash) : null) ??
			(await tryDeltaUpload(file, folderId, (pct) => report(pct / 100)));
		if (dedup) {
			if (!dedup.ok) {
				const err = new Error(dedup.errorMsg ?? 'upload failed') as Error & { isQuota?: boolean };
				err.isQuota = dedup.isQuotaError ?? false;
				throw err;
			}
			return dedup.savedBytes ?? 0;
		}
		await uploadFileWithProgress(folderId, file, report);
		return 0;
	}

	/**
	 * Upload one file, retrying once on a transient failure. A connection the
	 * watchdog aborted, or an upload the server actually committed before the
	 * client gave up, both recover here: the backend treats a re-upload of
	 * byte-identical content as success (idempotent), so the retry is a clean
	 * no-op for anything that already landed and a real second chance for the
	 * rest. A quota error is never retried — the disk won't free up mid-batch.
	 */
	async function uploadWithRetry(
		folderId: string | null,
		file: File,
		report: (frac: number) => void,
		ownedHash: string | null
	): Promise<number> {
		try {
			return await withTimeout(uploadOneFile(folderId, file, report, ownedHash), FILE_BACKSTOP_MS);
		} catch (e) {
			if ((e as { isQuota?: boolean } | null)?.isQuota) throw e;
			report(0); // reset this file's progress for the second attempt
			return await withTimeout(uploadOneFile(folderId, file, report, ownedHash), FILE_BACKSTOP_MS);
		}
	}

	/**
	 * Probe whether a file's content can actually be read. FIFOs / sockets /
	 * device files — e.g. the `supervise/control` named pipes in a copied
	 * s6/runit service tree — report a size but BLOCK FOREVER on read; uploading
	 * one would hang its lane until the watchdog (~30 s). We read only the first
	 * chunk, raced against a short timeout: a normal file (even 0-byte) yields
	 * immediately, a pipe stalls and is reported unreadable so we can skip it fast.
	 */
	async function isReadable(file: File, timeoutMs = 3000): Promise<boolean> {
		if (typeof file.stream !== 'function') return true; // can't probe → let the watchdog catch it
		let reader: ReadableStreamDefaultReader<Uint8Array> | undefined;
		let timer: ReturnType<typeof setTimeout> | undefined;
		try {
			reader = file.stream().getReader();
			const firstChunk = reader.read().then(() => true);
			const timedOut = new Promise<false>((res) => {
				timer = setTimeout(() => res(false), timeoutMs);
			});
			return await Promise.race([firstChunk, timedOut]);
		} catch {
			return false; // read threw → not a normal, uploadable file
		} finally {
			clearTimeout(timer);
			reader?.cancel().catch(() => {});
		}
	}

	/** Map `fn` over `items` with at most `limit` concurrent calls, preserving order. */
	async function mapLimit<T, R>(
		items: T[],
		limit: number,
		fn: (item: T) => Promise<R>
	): Promise<R[]> {
		const out = new Array<R>(items.length);
		let next = 0;
		const worker = async () => {
			while (next < items.length) {
				const i = next++;
				out[i] = await fn(items[i]);
			}
		};
		await Promise.all(Array.from({ length: Math.min(limit, items.length) }, worker));
		return out;
	}

	/** Split items into the readable ones and the unreadable (FIFO/socket/…) ones. */
	async function partitionReadable<T>(
		items: T[],
		fileOf: (item: T) => File
	): Promise<{ readable: T[]; skipped: T[] }> {
		const ok = await mapLimit(items, 24, (it) => isReadable(fileOf(it)));
		const readable: T[] = [];
		const skipped: T[] = [];
		items.forEach((it, i) => (ok[i] ? readable : skipped).push(it));
		return { readable, skipped };
	}

	/**
	 * Upload `items` ({file, folderId}) with bounded concurrency, a per-file
	 * deadline and live aggregate progress. A stuck or failing file no longer
	 * freezes the batch: it blocks only its own lane (the rest keep going), is
	 * retried once, and only then counted as failed. Quota exhaustion stops the
	 * run early. Returns the bytes deduplicated and the count of files that failed.
	 */
	async function uploadAll(
		items: { file: File; folderId: string | null }[],
		nid: number,
		label: (done: number) => string
	): Promise<{ savedBytes: number; failures: number }> {
		const total = items.length;
		const owned = await resolveOwnedHashes(items.map((it) => it.file));
		const frac = new Array<number>(total).fill(0);
		let progressSum = 0;
		let savedBytes = 0;
		let failures = 0;
		let next = 0;

		const refresh = () => {
			ui.updateProgress(
				nid,
				Math.round((progressSum / total) * 100),
				label(Math.round(progressSum))
			);
		};

		const worker = async () => {
			while (next < total) {
				const i = next++;
				const { file, folderId } = items[i];
				const report = (f: number) => {
					if (!Number.isNaN(f)) {
						const updated = Math.min(1, f);
						progressSum += updated - frac[i];
						frac[i] = updated;
					}
					refresh();
				};
				try {
					savedBytes += await uploadWithRetry(folderId, file, report, owned.get(file) ?? null);
				} catch (e) {
					failures++;
					// A full disk won't recover within this batch — stop pulling new
					// work so we don't fire hundreds of doomed uploads.
					if ((e as { isQuota?: boolean } | null)?.isQuota) next = total;
				} finally {
					progressSum += 1 - frac[i];
					frac[i] = 1;
					refresh();
				}
			}
		};

		await Promise.all(Array.from({ length: Math.min(UPLOAD_CONCURRENCY, total) }, worker));
		return { savedBytes, failures };
	}

	/** Resolve the upload's bell notification: success, partial, skipped, or failure.
	 *  `total` is the count of *readable* files actually attempted; `skipped` is the
	 *  non-regular files (FIFOs/sockets/…) that were filtered out before uploading. */
	function finishUpload(
		nid: number,
		savedBytes: number,
		failures: number,
		total: number,
		skipped = 0
	) {
		const ok = total - failures;
		if (failures === 0 && skipped === 0) {
			ui.finishProgress(nid, uploadDoneMessage(savedBytes), 'success');
		} else if (failures === 0) {
			// Every readable file uploaded; the rest were non-regular files
			// (FIFOs/sockets/devices) that can't be read and were skipped.
			ui.finishProgress(
				nid,
				t(
					'files.uploaded_skipped',
					{ ok, skipped },
					`${ok} uploaded · ${skipped} skipped (not regular files)`
				),
				ok > 0 ? 'success' : 'warning'
			);
		} else if (ok > 0) {
			ui.finishProgress(
				nid,
				t('files.uploaded_partial', { ok, failed: failures }, `${ok} uploaded, ${failures} failed`),
				'warning'
			);
		} else {
			ui.finishProgress(nid, t('files.upload_failed', 'Upload failed'), 'error');
		}
	}

	/** Final bell message for a fully-successful upload, noting deduplicated bytes. */
	function uploadDoneMessage(savedBytes: number): string {
		if (savedBytes <= 0) return t('files.uploaded', 'Upload complete');
		const mb = (savedBytes / (1024 * 1024)).toFixed(1);
		return t('files.uploaded_saved', { mb }, `Upload complete — ${mb} MB deduplicated`);
	}

	/**
	 * Upload a batch of files into the current folder, reporting aggregate
	 * progress through a single bell notification with a progress bar.
	 */
	async function uploadBatch(files: File[]) {
		if (files.length === 0) return;
		uploading = true;
		// Arm the reload-guard + persist a "batch in flight" marker so a
		// page refresh mid-upload (a) prompts the browser's "Leave site?"
		// dialog and (b) leaves a breadcrumb the layout picks up on the
		// next mount → toasts a "uploads were interrupted, re-drop to
		// resume (chunks reuse)" hint.
		acquireUploadGuard();
		const batchDescription =
			files.length === 1
				? files[0].name
				: t('files.n_files_batch', { n: files.length }, `${files.length} files`);
		const batchHandle = markUploadStarted(batchDescription, currentId);
		const nid = ui.startProgress(
			t('files.uploading_n', { done: 0, total: files.length }, `Uploading 0/${files.length} files…`)
		);
		try {
			// Filter out non-regular files (FIFOs/sockets/devices) up front so they
			// can't hang a lane — progress then runs over only what's uploadable.
			const { readable, skipped } = await partitionReadable(files, (f) => f);
			const total = readable.length;
			const label = (done: number) =>
				total === 1
					? t('files.uploading_file', { name: readable[0].name }, `Uploading ${readable[0].name}…`)
					: t('files.uploading_n', { done, total }, `Uploading ${done}/${total} files…`);
			if (total > 0) {
				const { savedBytes, failures } = await uploadAll(
					readable.map((file) => ({ file, folderId: currentId })),
					nid,
					label
				);
				finishUpload(nid, savedBytes, failures, total, skipped.length);
			} else {
				finishUpload(nid, 0, 0, 0, skipped.length);
			}
			await reloadAndTrackNew();
			// Storage usage changed server-side — pull the fresh figure so the
			// "Almacenamiento" bar moves off its login value instead of 0%.
			void session.refresh();
			// Vanish-warning: if hide-dotfiles is on and any uploaded
			// files start with `.`, the successfully-uploaded rows are
			// invisible in the grid the moment they land. Fire a
			// single grouped nudge so users don't think the upload
			// failed. Only fires when the preference is on AND at
			// least one uploaded file matched. Bell notification
			// stays quiet (already covers success/failure counts).
			if (preferences.hideDotfiles) {
				const hidden = files.filter((f) => f.name.startsWith('.')).length;
				if (hidden > 0) {
					ui.notify(
						t(
							'files.upload_dotfile_hidden',
							{ n: hidden },
							'{{n}} file(s) uploaded but hidden by your dotfile preference.'
						),
						'info'
					);
				}
			}
		} catch (err) {
			ui.finishProgress(nid, errorMessage(err), 'error');
		} finally {
			uploading = false;
			markUploadFinished(batchHandle);
			releaseUploadGuard();
		}
	}

	async function onUpload(e: Event) {
		const input = e.target as HTMLInputElement;
		if (!input.files?.length) return;
		await uploadBatch(Array.from(input.files));
		input.value = '';
	}

	async function onDrop(e: DragEvent) {
		e.preventDefault();
		const dt = e.dataTransfer;
		if (!dt) return;
		// A dropped folder isn't expanded into `.files`, so walk the dropped entry
		// tree (webkitGetAsEntry) when one is present and recreate it server-side;
		// otherwise fall back to the flat file list.
		const tree = await collectDroppedEntries(dt);
		if (tree) await uploadTree(tree);
		else if (dt.files?.length) await uploadBatch(Array.from(dt.files));
	}

	/**
	 * Page-wide OS drag/drop safety net.
	 *
	 * `ResourceList` wires system-drop handlers on `.rl-root`, so a drop
	 * on the list surface hits `onDrop` correctly. But `.rl-root` doesn't
	 * cover the whole viewport — dropping OS files on the title strip,
	 * the outer margin, or any gap around the list would either be
	 * ignored (bad UX) or opened as a top-level navigation by the
	 * browser (data-loss-esque: the tab replaces the app with the
	 * dropped file). Both `dragover` and `drop` are captured at window
	 * level here so anywhere on the `/files` page accepts the drop.
	 *
	 * The two `defaultPrevented` guards below defer to `.rl-root`'s
	 * handler when the drop lands there: ResourceList's handlers
	 * already `preventDefault`, so a bubbling window listener sees
	 * `defaultPrevented === true` and skips — no double-upload.
	 */
	function onWindowDragOver(e: DragEvent) {
		if (!e.dataTransfer?.types?.includes('Files')) return;
		if (e.defaultPrevented) return;
		// preventDefault on `dragover` is what tells the browser "this
		// element accepts drops" — without it, `drop` never fires and
		// the OS shows the "no-drop" cursor over the page's margins.
		e.preventDefault();
		if (e.dataTransfer) e.dataTransfer.dropEffect = 'copy';
	}
	function onWindowDrop(e: DragEvent) {
		if (!e.dataTransfer?.types?.includes('Files')) return;
		// Force-clear the overlay counter regardless of who handles the
		// drop — .rl-root's inner handler doesn't touch the page-level
		// counter and a stray unbalanced dragenter would otherwise
		// leave the overlay stuck on.
		pageSystemDragDepth = 0;
		if (e.defaultPrevented) return;
		onDrop(e);
	}

	/**
	 * Page-wide drop-overlay trigger. The overlay itself lives in
	 * `ResourceList` (`position: fixed`, already viewport-sized), but its
	 * default trigger is a counter on `.rl-root` — so dragging OS files
	 * over the page's title strip or the outer margins wouldn't light
	 * up the overlay even though the drop WOULD be accepted (see
	 * `onWindowDragOver`/`onWindowDrop` above). This counter tracks
	 * the drag at window level and drives ResourceList via the
	 * `systemDropOverlayActive` prop so the visual matches the
	 * accepts-drop area exactly.
	 *
	 * Same enter/leave-counter shape ResourceList uses internally —
	 * dragenter/leave chatter as the pointer moves between child
	 * elements would flicker a naive boolean, so we count.
	 * `drop`/`dragend` force-zero the counter to survive an
	 * unbalanced enter (e.g. drag cancelled by the user pressing Esc).
	 */
	let pageSystemDragDepth = $state(0);
	const pageSystemDropOver = $derived(pageSystemDragDepth > 0);
	function onWindowDragEnter(e: DragEvent) {
		if (!e.dataTransfer?.types?.includes('Files')) return;
		pageSystemDragDepth++;
	}
	function onWindowDragLeave(e: DragEvent) {
		if (!e.dataTransfer?.types?.includes('Files')) return;
		if (pageSystemDragDepth > 0) pageSystemDragDepth--;
	}
	function onWindowDragEnd() {
		pageSystemDragDepth = 0;
	}

	/**
	 * Expand dropped OS entries into `{file, relativePath}` rows, walking any
	 * directory tree via the (non-standard but ubiquitous) `webkitGetAsEntry` /
	 * `createReader` API. Returns `null` when nothing dropped was a directory, so
	 * the caller takes the simpler flat-`FileList` path.
	 */
	async function collectDroppedEntries(
		dt: DataTransfer
	): Promise<{ file: File; relativePath: string }[] | null> {
		// `webkitGetAsEntry()` must be read synchronously while the event is live.
		const roots: FileSystemEntry[] = [];
		let sawDir = false;
		for (const item of Array.from(dt.items)) {
			const entry = item.webkitGetAsEntry();
			if (entry) {
				roots.push(entry);
				if (entry.isDirectory) sawDir = true;
			}
		}
		if (!sawDir) return null;

		const out: { file: File; relativePath: string }[] = [];
		async function walk(entry: FileSystemEntry, prefix: string): Promise<void> {
			if (entry.isFile) {
				const file = await new Promise<File>((resolve, reject) =>
					(entry as FileSystemFileEntry).file(resolve, reject)
				);
				out.push({ file, relativePath: prefix + entry.name });
			} else if (entry.isDirectory) {
				const reader = (entry as FileSystemDirectoryEntry).createReader();
				const dirPrefix = `${prefix}${entry.name}/`;
				// readEntries yields in batches; loop until it returns an empty one.
				for (;;) {
					const batch = await new Promise<FileSystemEntry[]>((resolve, reject) =>
						reader.readEntries(resolve, reject)
					);
					if (batch.length === 0) break;
					for (const child of batch) await walk(child, dirPrefix);
				}
			}
		}
		for (const root of roots) await walk(root, '');
		return out;
	}

	async function renameItem(kind: 'file' | 'folder', id: string, current: string) {
		const name = await promptDialog({
			title: t('common.rename', 'Rename'),
			defaultValue: current,
			confirmText: t('common.save', 'Save')
		});
		if (!name || name === current) return;
		try {
			if (kind === 'file') await renameFile(id, name);
			else {
				await renameFolder(id, name);
				rememberFolderName(id, name); // keep breadcrumbs current immediately
			}
			await reload();
			// Vanish-warning: the file didn't start with `.` before but
			// does now, AND the user has hide-dotfiles on → the row is
			// about to disappear from the grid. Toast so the operation
			// doesn't feel like a silent failure. Only fires on the
			// transition (`.env` renamed to `.env2` doesn't need the
			// nudge — it was already hidden). No toast when hide is off
			// because nothing vanished.
			if (preferences.hideDotfiles && name.startsWith('.') && !current.startsWith('.')) {
				ui.notify(
					t(
						'files.rename_dotfile_hidden',
						{ name },
						"Renamed to '{{name}}' — now hidden by your preference."
					),
					'info'
				);
			}
		} catch (e) {
			errorToast(e);
		}
	}

	async function deleteItem(kind: 'file' | 'folder', id: string, name: string) {
		const ok = await confirmDialog({
			title: t('common.delete', 'Delete'),
			message: t('files.confirm_delete', { name }, 'Move "{{name}}" to trash?'),
			confirmText: t('common.delete', 'Delete'),
			danger: true
		});
		if (!ok) return;
		try {
			if (kind === 'file') await deleteFile(id);
			else await deleteFolder(id);
			await reload();
			void session.refresh();
		} catch (e) {
			errorToast(e);
		}
	}

	let viewerOpen = $state(false);
	let viewerFile = $state<FileItem | null>(null);

	function openFile(file: FileItem) {
		// Drive the viewer from the URL (?file=<id>) so a preview is bookmarkable,
		// reload-restorable, and Back/Forward open/close it. The effect below
		// reflects the param into viewerOpen/viewerFile.
		const url = new URL(page.url);
		url.searchParams.set('file', file.id);
		// Same-origin URL object built from page.url (already resolved); resolve()
		// only accepts a route string, so it can't type a dynamic URL instance.
		// eslint-disable-next-line svelte/no-navigation-without-resolve
		void goto(url, { keepFocus: true, noScroll: true });
	}

	// ── File-preview deep link (?file=<id>) ──────────────────────────────────
	// URL → viewer. Runs on navigation, on popstate (Back/Forward), and once the
	// listing for an initial deep link arrives. `untrack` stops it re-firing on
	// viewer-state changes, so a user-initiated close can't be re-opened here.
	$effect(() => {
		const fileId = page.url.searchParams.get('file');
		const files = listing.files;
		untrack(() => {
			if (!fileId) {
				if (viewerOpen) viewerOpen = false;
				return;
			}
			if (viewerOpen && viewerFile?.id === fileId) return;
			const f = files.find((x) => x.id === fileId);
			if (f) {
				viewerFile = f;
				viewerOpen = true;
			}
		});
	});

	// viewer → URL: when the user closes the viewer (X / Esc / backdrop), drop the
	// `?file=` param (replaceState, so closing doesn't add a history entry). Only
	// act on a genuine open→closed transition: on a cold deep link the viewer
	// starts closed *with* the param while the listing is still loading, and
	// stripping it there would race the URL→viewer effect above and the preview
	// would never open.
	let viewerWasOpen = false;
	$effect(() => {
		const open = viewerOpen;
		const hasParam = page.url.searchParams.get('file') !== null;
		untrack(() => {
			if (viewerWasOpen && !open && hasParam) {
				const url = new URL(page.url);
				url.searchParams.delete('file');
				// Same-origin URL object (see note above); resolve() can't type it.
				// eslint-disable-next-line svelte/no-navigation-without-resolve
				void goto(url, { keepFocus: true, noScroll: true, replaceState: true });
			}
			viewerWasOpen = open;
		});
	});

	// ── Multi-select + batch ────────────────────────────────────────────────
	// After the ResourceList migration the row-level selection UX (shift-
	// range, ctrl-toggle, anchor tracking, header select-all) lives inside
	// `<ResourceList>` and mirrors state OUT via `onselectionchange`. This
	// SvelteSet is the local reflection the batch action functions consume;
	// it stays a plain in-place `SvelteSet` (per benches ROUND11 §S2) so
	// batch buttons see the same set as the row template does.
	const selected = new SvelteSet<string>();

	function clearSelection() {
		selected.clear();
	}

	/**
	 * Download the whole selection as a single zip via POST /api/batch/download —
	 * folders are included (the old per-item loop silently skipped them). A lone
	 * file still streams directly so it keeps its original name/extension.
	 */
	/** Name for a server-zipped multi-item archive (matches the legacy format). */
	function batchZipName(): string {
		const stamp = new Date().toISOString().replace('T', ' ').replace(/\..*/, '').replace(/:/g, '-');
		return `oxicloud ${stamp}.zip`;
	}

	async function batchDownload() {
		const fileIds: string[] = [];
		const folderIds: string[] = [];
		// One O(M) pass over the listing instead of an O(N·M) `some` per id.
		const folderIdSet = new Set(listing.folders.map((f) => f.id));
		const fileIdSet = new Set(listing.files.map((f) => f.id));
		for (const id of selected) {
			if (folderIdSet.has(id)) folderIds.push(id);
			else if (fileIdSet.has(id)) fileIds.push(id);
		}
		if (fileIds.length === 0 && folderIds.length === 0) return;

		// Single file, no folders → direct download (preserves the real name).
		if (fileIds.length === 1 && folderIds.length === 0) {
			const file = listing.files.find((f) => f.id === fileIds[0]);
			if (file) {
				const a = document.createElement('a');
				a.href = fileDownloadUrl(file.id);
				a.download = file.name;
				document.body.appendChild(a);
				a.click();
				a.remove();
			}
			return;
		}

		const zipName = batchZipName();
		try {
			const res = await apiFetch('/api/batch/download', {
				method: 'POST',
				credentials: 'same-origin',
				headers: { 'Content-Type': 'application/json', ...getCsrfHeaders() },
				body: JSON.stringify({ file_ids: fileIds, folder_ids: folderIds })
			});
			if (!res.ok) throw new Error(`Server returned ${res.status}`);
			const blob = await res.blob();
			const url = URL.createObjectURL(blob);
			const a = document.createElement('a');
			a.href = url;
			a.download = zipName;
			document.body.appendChild(a);
			a.click();
			a.remove();
			URL.revokeObjectURL(url);
		} catch (e) {
			errorToast(e);
		}
	}

	/** Batch add the selection to favorites — single /api/favorites/batch call. */
	async function batchFavorites() {
		// Build an id → item index so the "already favorite" filter is
		// O(1) per selection member instead of an O(N·M) scan. Reused
		// after success to flip `is_favorite` in place on each row.
		// eslint-disable-next-line svelte/prefer-svelte-reactivity -- ephemeral local index, discarded before any reactive read
		const byId = new Map<string, FileItem | FolderItem>();
		for (const it of orderedItems) byId.set(it.id, it);
		const items = selectionTargets().filter((it) => !(byId.get(it.id)?.is_favorite ?? false));
		if (items.length === 0) {
			ui.notify(t('files.already_favorites', 'All selected items are already favorites'), 'info');
			clearSelection();
			return;
		}
		try {
			const res = await apiFetch('/api/favorites/batch', {
				method: 'POST',
				credentials: 'same-origin',
				headers: { 'Content-Type': 'application/json', ...getCsrfHeaders() },
				body: JSON.stringify({
					items: items.map((it) => ({ item_id: it.id, item_type: it.kind }))
				})
			});
			if (!res.ok) throw new Error(`Server returned ${res.status}`);
			for (const it of items) {
				const row = byId.get(it.id);
				if (row) row.is_favorite = true;
			}
			ui.notify(t('files.added_favorites', 'Added to favorites'), 'success');
			clearSelection();
		} catch (e) {
			errorToast(e);
		}
	}

	function selectionTargets(): ActionTarget[] {
		// One O(M) index build instead of an O(N·M) `find` per selected id.
		// Folders win id collisions, matching the old folder-first probe.
		// eslint-disable-next-line svelte/prefer-svelte-reactivity -- ephemeral local index, discarded before any reactive read
		const byId = new Map<string, ActionTarget>();
		for (const f of listing.files) byId.set(f.id, { id: f.id, name: f.name, kind: 'file' });
		for (const f of listing.folders) byId.set(f.id, { id: f.id, name: f.name, kind: 'folder' });
		return [...selected]
			.map((id) => byId.get(id) ?? null)
			.filter((x): x is ActionTarget => x !== null);
	}

	function batchMove() {
		const items = selectionTargets();
		if (items.length) {
			moveItems = items;
			moveMode = 'move';
			moveOpen = true;
		}
	}

	function batchCopy() {
		const items = selectionTargets();
		if (items.length) {
			moveItems = items;
			moveMode = 'copy';
			moveOpen = true;
		}
	}

	function onKeydown(e: KeyboardEvent) {
		const tag = (e.target as HTMLElement)?.tagName;
		if (tag === 'INPUT' || tag === 'TEXTAREA' || tag === 'SELECT') return;
		if (e.key === 'Escape' && selected.size) {
			clearSelection();
		} else if (e.key === 'Delete' && selected.size) {
			// Delete only — Backspace was dropped: it triggered accidental deletes.
			e.preventDefault();
			void batchDelete();
		}
		// Ctrl+A "select all" moved to the list-header checkbox owned by
		// ResourceList — the row-level selection UX now lives entirely
		// there, so the shortcut is served by clicking that box. Kept the
		// binding surface here for the still-page-level Escape / Delete
		// gestures that reference the local `selected` mirror.
	}

	async function batchDelete() {
		const ids = [...selected];
		const ok = await confirmDialog({
			title: t('files.batch_delete', 'Delete selected'),
			message: t('files.confirm_batch_delete', { n: ids.length }, 'Move {{n}} items to trash?'),
			confirmText: t('common.delete', 'Delete'),
			danger: true
		});
		if (!ok) return;
		// Bounded fan-out instead of a serial await per item: 100 deletes at
		// ~30 ms RTT collapse from ~3 s of waterfall to a few round-trip
		// windows. Failures toast individually and the rest still proceed,
		// exactly like the old serial loop.
		const folderIdSet = new Set(listing.folders.map((f) => f.id));
		await mapLimit(ids, 6, async (id) => {
			try {
				if (folderIdSet.has(id)) await deleteFolder(id);
				else await deleteFile(id);
			} catch (e) {
				errorToast(e);
			}
		});
		clearSelection();
		await reload();
		void session.refresh();
	}

	// ── Drag-to-move ─────────────────────────────────────────────────────────
	const DRAG_TYPE = 'application/x-oxi-item';
	let dropFolderId = $state<string | null>(null);
	// Per-crumb drop highlight state lived here until the breadcrumb
	// migrated to the shared `<FolderBreadcrumb>` component (2026-07-26),
	// which owns its own hover state. The `CRUMB_HOME_ID` sentinel is
	// gone too — the shared component's root icon isn't a drop target
	// (the drive root's ancestor is always the drive itself, and
	// dropping "at the drive" is ambiguous).

	// Copy-vs-move on drop.
	//
	// Cursor visual: `effectAllowed = 'copyMove'` set at dragstart AND
	// the dragover handlers touching only `preventDefault()` (no
	// `dropEffect` write) leaves the browser free to negotiate the
	// cursor glyph from OS-native modifier keys (macOS Option, Win/Linux
	// Ctrl) — arrow-with-plus for copy, plain arrow for move.
	//
	// Routing: at DROP time we read the modifier keys DIRECTLY on the
	// event (DragEvent extends MouseEvent, so `altKey`/`metaKey`/`ctrlKey`
	// are all live). This is deliberately independent of
	// `dataTransfer.dropEffect` — that value isn't reliably updated by
	// every browser for JS-initiated drags, but the raw modifier bits
	// on the drop event ARE. One source of truth, one path.
	//
	// OS convention:
	//   * macOS: ⌥ Option (altKey) → copy — Finder convention. ⌘
	//     (metaKey) accepted too as a forgiving fallback.
	//   * Windows / Linux: Ctrl (ctrlKey) → copy — Explorer / Nautilus
	//     convention.
	const IS_MAC =
		typeof navigator !== 'undefined' &&
		/Mac|iPhone|iPad|iPod/i.test(navigator.platform || navigator.userAgent || '');
	function isCopyDrop(e: DragEvent): boolean {
		return IS_MAC ? e.altKey || e.metaKey : e.ctrlKey;
	}

	/**
	 * Begin dragging an item. When the dragged row is part of the current
	 * selection, the whole selection travels (mirrors ui.js' multi-item drag);
	 * otherwise just the single item moves.
	 */
	function onItemDragStart(e: DragEvent, kind: ItemType, id: string, name: string) {
		const items: ActionTarget[] =
			selected.has(id) && selected.size > 1 ? selectionTargets() : [{ id, name, kind }];
		e.dataTransfer?.setData(DRAG_TYPE, JSON.stringify(items));
		if (e.dataTransfer) {
			// `copyMove` advertises both operations; the drop-target's
			// `dropEffect` (set on dragover by our handlers based on
			// modifier key) picks the effective operation and the OS
			// cursor reflects it (arrow-with-plus for copy, plain for
			// move). Was `'move'`, which forced the cursor to always
			// show move even when Ctrl/Cmd was held.
			e.dataTransfer.effectAllowed = 'copyMove';
			// Custom drag ghost for both single and multi-item drags —
			// consistent UX. The `.dragged-items-badge` always shows
			// the count (reads "1" on a single-item drag) so the user
			// gets the same visual feedback shape regardless of how
			// many rows they're moving.
			showDragGhost(e.dataTransfer, items);
			// Drag-out-to-OS download: the OS reads `DownloadURL` (a GET URL) and
			// downloads the dragged item(s) — a single file directly, a folder or a
			// multi-selection as one server-zipped archive.
			const dl = dragDownloadDescriptor(items);
			if (dl) {
				e.dataTransfer.setData(
					'DownloadURL',
					`application/octet-stream:${dl.name}:${location.origin}${dl.url}`
				);
			}
		}
	}

	/** `{ name, GET url }` for the drag-out download of the current drag set. */
	function dragDownloadDescriptor(items: ActionTarget[]): { name: string; url: string } | null {
		if (items.length === 0) return null;
		if (items.length === 1) {
			const it = items[0];
			return it.kind === 'folder'
				? { name: `${it.name}.zip`, url: folderZipUrl(it.id) }
				: { name: it.name, url: fileDownloadUrl(it.id) };
		}
		// Multi-selection → one archive via the GET twin of POST /api/batch/download
		// (DownloadURL can only point at a GET URL); file_ids/folder_ids are CSV.
		const fileIds = items.filter((i) => i.kind === 'file').map((i) => i.id);
		const folderIds = items.filter((i) => i.kind === 'folder').map((i) => i.id);
		// Transient query-string builder for a one-off download URL — not reactive
		// state, so a plain URLSearchParams is correct here.
		// eslint-disable-next-line svelte/prefer-svelte-reactivity
		const params = new URLSearchParams();
		if (fileIds.length) params.set('file_ids', fileIds.join(','));
		if (folderIds.length) params.set('folder_ids', folderIds.join(','));
		return { name: batchZipName(), url: `/api/batch/download?${params.toString()}` };
	}

	/**
	 * Custom drag image for a multi-item drag: a stack of the first few rows plus
	 * a count badge (ported from ui.js). Reuses the .drag-preview / .dragged-items
	 * / .dragged-items-badge styles already in resourceList.css.
	 */
	function showDragGhost(dt: DataTransfer, items: ActionTarget[]) {
		const MAX = 4;
		const preview = document.createElement('div');
		preview.className = 'drag-preview';

		const stack = document.createElement('div');
		stack.className = 'dragged-items';
		for (const [i, it] of items.slice(0, MAX).entries()) {
			const row = document.createElement('div');
			row.className = 'file-item';
			if (i === MAX - 1 && items.length > MAX) row.classList.add('fading');
			const icon = document.createElement('div');
			icon.className = 'file-icon';
			icon.textContent = it.kind === 'folder' ? '📁' : '📄';
			const label = document.createElement('div');
			label.textContent = it.name;
			row.append(icon, label);
			stack.appendChild(row);
		}

		const badge = document.createElement('div');
		badge.className = 'dragged-items-badge';
		badge.textContent = String(items.length);

		preview.append(stack, badge);
		document.body.appendChild(preview);
		dt.setDragImage(preview, 0, 0);
		// The browser snapshots the drag image synchronously; drop the node next tick.
		setTimeout(() => preview.remove(), 0);
	}

	function dragPayload(e: DragEvent): ActionTarget[] {
		const raw = e.dataTransfer?.getData(DRAG_TYPE);
		if (!raw) return [];
		try {
			const parsed = JSON.parse(raw);
			return Array.isArray(parsed) ? parsed : [parsed];
		} catch {
			return [];
		}
	}

	async function moveInto(targetFolderId: string, e: DragEvent) {
		const items = dragPayload(e).filter((it) => it.id !== targetFolderId);
		if (items.length === 0) return;
		// Bounded fan-out (was a serial await per item). Every item is
		// attempted; on any failure the first error is surfaced and the
		// selection is kept so the drop can be retried, like the old loop.
		const failures = (
			await mapLimit(items, 6, async (it) => {
				try {
					if (it.kind === 'file') await moveFile(it.id, targetFolderId);
					else await moveFolder(it.id, targetFolderId);
					return null;
				} catch (err) {
					return err ?? new Error('move failed');
				}
			})
		).filter((err) => err !== null);
		if (failures.length > 0) {
			errorToast(failures[0]);
			return;
		}
		clearSelection();
		await reload();
	}

	/**
	 * Copy variant of `moveInto` used when the drop happens with the copy
	 * modifier held (Ctrl on Win/Linux, ⌘ on macOS). Files and folders go
	 * through the batch copy endpoints (`copyFiles` / `copyFolders`) —
	 * there's no per-item copy in the REST API today, only batch.
	 */
	async function copyInto(targetFolderId: string, e: DragEvent) {
		const items = dragPayload(e).filter((it) => it.id !== targetFolderId);
		if (items.length === 0) return;
		const fileIds = items.filter((it) => it.kind === 'file').map((it) => it.id);
		const folderIds = items.filter((it) => it.kind === 'folder').map((it) => it.id);
		try {
			// Two batch calls in parallel — the endpoints are independent
			// and the aggregate error surface is a single toast anyway.
			await Promise.all([
				copyFiles(fileIds, targetFolderId),
				copyFolders(folderIds, targetFolderId)
			]);
		} catch (err) {
			errorToast(err);
			return;
		}
		clearSelection();
		await reloadAndTrackNew();
	}

	function onFolderDrop(e: DragEvent, folder: FolderItem) {
		if (!e.dataTransfer?.types.includes(DRAG_TYPE)) return; // external file drop → page dropzone
		e.preventDefault();
		e.stopPropagation();
		dropFolderId = null;
		if (isCopyDrop(e)) void copyInto(folder.id, e);
		else void moveInto(folder.id, e);
	}

	function onCrumbDrop(e: DragEvent, folderId: string) {
		if (!e.dataTransfer?.types.includes(DRAG_TYPE)) return;
		e.preventDefault();
		if (isCopyDrop(e)) void copyInto(folderId, e);
		else void moveInto(folderId, e);
	}

	// ── Right-click context menu ─────────────────────────────────────────────
	let ctxOpen = $state(false);
	let ctxX = $state(0);
	let ctxY = $state(0);
	let ctxTarget = $state<ActionTarget | null>(null);

	function openContext(e: MouseEvent, kind: ItemType, id: string, name: string) {
		e.preventDefault();
		e.stopPropagation();
		ctxTarget = { id, name, kind };
		// Clamp to viewport so the menu never overflows offscreen.
		ctxX = Math.min(e.clientX, window.innerWidth - 200);
		ctxY = Math.min(e.clientY, window.innerHeight - 320);
		ctxOpen = true;
	}
	function closeContext() {
		ctxOpen = false;
		ctxTarget = null;
	}

	// ── WOPI office editor (context-menu entries) ────────────────────────────
	let wopiOpen = $state(false);
	let wopiAction = $state<'edit' | 'view'>('edit');
	let wopiFile = $state<{ id: string; name: string } | null>(null);

	// Pull in the on-demand modules the moment they're first needed; after that
	// the chunk is cached and the component stays mounted (controlled by `open`).
	$effect(() => {
		if (viewerOpen) void fileViewer.load();
		if (wopiOpen) void wopiEditor.load();
		if (moveOpen) void moveDialog.load();
		if (shareOpen) void shareDialog.load();
	});
	// Editability of the current context-menu target file, resolved async.
	let ctxCanEditWopi = $state(false);

	// Probe WOPI editability whenever the context target changes to a file. Image
	// files use the inline viewer, so they never show the editor entries.
	$effect(() => {
		const tg = ctxTarget;
		ctxCanEditWopi = false;
		if (!tg || tg.kind !== 'file') return;
		const f = listing.files.find((x) => x.id === tg.id);
		if (!f || (f.mime_type ?? '').startsWith('image/')) return;
		void canEditWithWopi(tg.name).then((ok) => {
			// Guard against a stale resolve after the menu moved to another target.
			if (ctxTarget?.id === tg.id) ctxCanEditWopi = ok;
		});
	});

	function openWopi(id: string, name: string, action: 'edit' | 'view') {
		wopiFile = { id, name };
		wopiAction = action;
		wopiOpen = true;
	}

	async function openWopiTab(id: string, name: string) {
		// New-tab editing posts the token to /wopi/edit; open a blank tab first so
		// the editor URL fetch (async) isn't treated as a popup by the browser.
		const win = window.open('', '_blank');
		try {
			const data = await getEditorUrlWithFallback(id, name, 'edit');
			const url = `/wopi/edit/${encodeURIComponent(id)}?access_token=${encodeURIComponent(data.access_token)}`;
			if (win) win.location.href = url;
			else window.open(url, '_blank');
		} catch (e) {
			win?.close();
			errorToast(e);
		}
	}

	// ── Add audio file to a playlist ──────────────────────────────────────────
	function isAudio(file: FileItem | undefined): boolean {
		return (file?.mime_type ?? '').startsWith('audio/');
	}

	/**
	 * Prompt for a playlist (by name) and add the file to it; an unknown name
	 * creates a new playlist. Mirrors contextMenus.showPlaylistDialog without the
	 * bespoke modal — kept lightweight via the shared prompt dialog.
	 */
	async function addToPlaylist(file: FileItem) {
		let playlists: Awaited<ReturnType<typeof listPlaylists>>;
		try {
			playlists = await listPlaylists();
		} catch (e) {
			errorToast(e);
			return;
		}
		const existing = playlists.map((p) => p.name).join(', ');
		const name = await promptDialog({
			title: t('music.add_to_playlist', 'Add to playlist'),
			message: existing
				? t(
						'music.pick_or_create',
						{ list: existing },
						'Existing: {{list}}. Type a name to add or create.'
					)
				: t('music.create_playlist_hint', 'Type a playlist name to create one.'),
			placeholder: t('music.playlist_name', 'Playlist name'),
			confirmText: t('common.add', 'Add')
		});
		if (!name) return;
		try {
			const match = playlists.find((p) => p.name.toLowerCase() === name.toLowerCase());
			const playlist = match ?? (await createPlaylist(name));
			await addTracks(playlist.id, [file.id]);
			ui.notify(t('music.added_to_playlist', 'Added to playlist'), 'success');
		} catch (e) {
			errorToast(e);
		}
	}

	// ── Open the parent folder of a file ──────────────────────────────────────
	function openParentFolder(file: FileItem) {
		// The current view already lists files inside their folder; navigate to the
		// file's own folder id (handles deep-link / search contexts where the file's
		// folder differs from the current path).
		goto(resolve(`/files/${file.folder_id}`));
	}

	// ── Download a folder as a zip archive ────────────────────────────────────
	function downloadFolderZip(folder: { id: string; name: string }) {
		const a = document.createElement('a');
		a.href = folderZipUrl(folder.id);
		a.download = `${folder.name}.zip`;
		document.body.appendChild(a);
		a.click();
		a.remove();
	}

	// ── Recursive folder upload ──────────────────────────────────────────────
	let folderInput = $state<HTMLInputElement | null>(null);

	/**
	 * Upload files that carry a relative directory path, recreating the folder
	 * tree under the current folder. Shared by the folder picker and folder drops.
	 */
	async function uploadTree(entries: { file: File; relativePath: string }[]) {
		if (entries.length === 0) return;
		uploading = true;
		// Same reload-guard + interrupted-uploads breadcrumb as uploadBatch —
		// the browser prompts on refresh, and if the user reloads anyway
		// the layout picks up the marker on next mount and toasts.
		acquireUploadGuard();
		const treeHandle = markUploadStarted(
			t('files.n_files_batch', { n: entries.length }, `${entries.length} files`),
			currentId
		);
		// Same bell progress notification as uploadBatch, so folder uploads show
		// live progress + a final result instead of staying silent until the end.
		const nid = ui.startProgress(
			t(
				'files.uploading_n',
				{ done: 0, total: entries.length },
				`Uploading 0/${entries.length} files…`
			)
		);
		try {
			// Drop non-regular files (FIFOs/sockets/devices) before building the tree
			// so they neither create empty folders nor hang a lane on read.
			const { readable, skipped } = await partitionReadable(entries, (e) => e.file);
			const total = readable.length;
			const label = (done: number) =>
				t('files.uploading_n', { done, total }, `Uploading ${done}/${total} files…`);
			if (total === 0) {
				finishUpload(nid, 0, 0, 0, skipped.length);
				await reload();
				return;
			}

			// Map each relative directory path to its created folder id; '' = current.
			// Local computation scratch map (discarded after upload) — not reactive state.
			// eslint-disable-next-line svelte/prefer-svelte-reactivity
			const dirIds = new Map<string, string | null>([['', currentId]]);

			async function ensureDir(relDir: string): Promise<string | null> {
				if (dirIds.has(relDir)) return dirIds.get(relDir) ?? null;
				const parts = relDir.split('/');
				const name = parts.pop() as string;
				const parentId = await ensureDir(parts.join('/'));
				const created = await createFolder(name, parentId);
				dirIds.set(relDir, created.id);
				return created.id;
			}

			// Create the folder tree first (sequentially — folders are few, and
			// concurrent creation of the same dir would race), then upload the
			// files into it with bounded concurrency.
			const items: { file: File; folderId: string | null }[] = [];
			for (const { file, relativePath } of readable) {
				const segs = relativePath.split('/');
				segs.pop(); // drop the filename, keep the directory trail
				items.push({ file, folderId: await ensureDir(segs.join('/')) });
			}

			const { savedBytes, failures } = await uploadAll(items, nid, label);
			finishUpload(nid, savedBytes, failures, total, skipped.length);
			await reloadAndTrackNew();
			void session.refresh();
		} catch (err) {
			ui.finishProgress(nid, errorMessage(err), 'error');
		} finally {
			uploading = false;
			markUploadFinished(treeHandle);
			releaseUploadGuard();
		}
	}

	async function onUploadFolder(e: Event) {
		const input = e.target as HTMLInputElement;
		const files = input.files ? Array.from(input.files) : [];
		// webkitRelativePath: "chosenDir/sub/.../file.ext" — recreate the whole tree.
		await uploadTree(
			files.map((file) => ({
				file,
				relativePath:
					(file as File & { webkitRelativePath?: string }).webkitRelativePath ?? file.name
			}))
		);
		input.value = '';
	}

	// Client-side sort (flat, Drive-style). The listing endpoint returns the
	// folder contents unsorted; sorting here avoids a refetch per column click.
	//
	// `reversed` is the single source of truth for direction — bound to
	// ResourceList's `bind:reversed` below and read by the comparators.
	// The legacy `sortDir: 1 | -1` value used by the comparators is a
	// read-only `$derived` off `reversed` so we don't need two-way sync
	// (the previous `$state` + two `$effect` mirror was fragile — a
	// programmatic write to either side triggered an update on the
	// other, and eslint's `prefer-writable-derived` rightly flagged it).
	type SortField = 'name' | 'type' | 'size' | 'modified_at' | 'created_at';
	let sortField = $state<SortField>('name');
	let reversed = $state(false);

	// Server does the sort (order_by=sortField, reverse=reversed on every
	// page request), so ResourceList reads `orderedItems` in server order
	// straight through the dotfile filter. No client-side comparator
	// necessary. Under order_by=name/type/size the server puts folders
	// first then files; under modified_at/created_at they interleave —
	// preserving the accumulator order is what surfaces that correctly.
	//
	// Hoist step: items in `newlyAdded` (populated by `reloadAndTrackNew`
	// after an upload / create / dropped tree) are pulled OUT of their
	// natural-order position and PREPENDED to the list, so the
	// "__new__" bucket rendered by the composed groupBy below appears
	// at the top of the swimlanes regardless of what sort/group the
	// user has active. First-appearance bucketing in
	// `buildResourceSections` keys off the item order in the input list.
	const rlItems = $derived.by<Array<FileItem | FolderItem>>(() => {
		const filtered = filterDotfiles(orderedItems, preferences.hideDotfiles);
		if (newlyAdded.size === 0) return filtered;
		const hoisted: Array<FileItem | FolderItem> = [];
		const rest: Array<FileItem | FolderItem> = [];
		for (const it of filtered) {
			if (newlyAdded.has(it.id)) hoisted.push(it);
			else rest.push(it);
		}
		return [...hoisted, ...rest];
	});

	// Group-by state (bound to <ResourceList>). Kept as a `string` prop
	// value; the current `sortField` mirrors from the picked group's
	// `orderBy` so a group-by change also drives the sort.
	let groupBy = $state<string>('');

	// Same swimlane keys as the bespoke groups above (Type / Size /
	// modifiedAt / createdAt). The `orderBy` values are what the
	// GROUP_BYS toolbar emits, so <ResourceList>'s onreload gets the
	// legacy `sortField` name and can drive the same sort path.
	//
	// Every dimension composes a `__new__` branch on top of its natural
	// `bucketOf` so that whenever the transient "new elements" swimlane
	// is active, hoisted items get their own bucket-first-in-order
	// regardless of the user's chosen group. On the default `''` (flat)
	// dimension the wrapped `bucketOf` returns the empty string for
	// non-new items — that renders as one unlabeled section (header
	// suppressed by ResourceList when `label === ''`), preserving the
	// current flat-list look with just the "New elements" header on
	// top. `labelForNew` renders the localised header.
	const NEW_KEY = '__new__';
	const labelForNew = $derived(t('files.new_elements', 'New elements'));
	const wrapNew =
		<T extends FileItem | FolderItem>(inner?: (item: T) => string | null) =>
		(item: T): string | null => {
			if (newlyAdded.has(item.id)) return NEW_KEY;
			return inner ? inner(item) : '';
		};
	const wrapLabel =
		(inner?: (key: string) => string) =>
		(key: string): string => {
			if (key === NEW_KEY) return labelForNew;
			return inner ? inner(key) : key;
		};
	const rlGroupBys = $derived<RLGroupByDef[]>([
		{
			key: '',
			label: t('files.name', 'Name'),
			orderBy: 'name',
			icon: 'arrow-up-a-z',
			// Only synthesize a bucketOf when the swimlane is active; when
			// no new items exist we want the plain flat-list rendering
			// (no bucketing pass at all).
			bucketOf: newlyAdded.size > 0 ? wrapNew() : undefined,
			labelOf: newlyAdded.size > 0 ? wrapLabel() : undefined
		},
		{
			key: 'type',
			label: t('groupby.type', 'Type'),
			orderBy: 'type',
			icon: 'layer-group',
			bucketOf: wrapNew((item) =>
				isFile(item) ? typeLabel(item.category) : t('files.file_types.folder', 'Folders')
			),
			labelOf: wrapLabel((k) => k)
		},
		{
			key: 'size',
			label: t('groupby.size', 'Size'),
			orderBy: 'size',
			icon: 'layer-group',
			bucketOf: wrapNew((item) => (isFile(item) ? sizeBucket(item.size ?? 0) : sizeBucket(-1))),
			labelOf: wrapLabel((k) => k)
		},
		{
			key: 'modifiedAt',
			label: t('groupby.modifiedAt', 'Modified date'),
			orderBy: 'modified_at',
			icon: 'layer-group',
			bucketOf: wrapNew((item) => dateBucket(item.modified_at)),
			labelOf: wrapLabel((k) => k)
		},
		{
			key: 'createdAt',
			label: t('groupby.createdAt', 'Created date'),
			orderBy: 'created_at',
			icon: 'layer-group',
			bucketOf: wrapNew((item) => dateBucket(item.created_at)),
			labelOf: wrapLabel((k) => k)
		}
	]);

	// Bridge for <ResourceList>'s callbacks — the row's open/favorite/drag
	// props take one item; the legacy handlers take `(kind, id, name)`.
	function rlOnOpen(item: FileItem | FolderItem) {
		if (isFile(item)) openFile(item);
		else openFolder(item);
	}
	function rlOnFavorite(item: FileItem | FolderItem) {
		void toggleFavorite(isFile(item) ? 'file' : 'folder', item.id);
	}
	function rlOnContextMenu(e: MouseEvent, item: FileItem | FolderItem) {
		openContext(e, isFile(item) ? 'file' : 'folder', item.id, item.name);
	}
	// Row-drag: everything is draggable; only folders accept drops.
	function rlIsDraggable(_item: FileItem | FolderItem): boolean {
		return true;
	}
	function rlIsDropTarget(item: FileItem | FolderItem): boolean {
		return !isFile(item);
	}
	function rlOnItemDragStart(e: DragEvent, item: FileItem | FolderItem) {
		onItemDragStart(e, isFile(item) ? 'file' : 'folder', item.id, item.name);
	}
	function rlOnItemDragOver(e: DragEvent, item: FileItem | FolderItem) {
		if (isFile(item)) return;
		if (e.dataTransfer?.types.includes(DRAG_TYPE)) {
			// preventDefault is what accepts the drop. Leaving
			// `dropEffect` untouched lets the browser negotiate it
			// from OS-native modifier keys.
			e.preventDefault();
			dropFolderId = item.id;
		}
	}
	function rlOnItemDragLeave(_e: DragEvent, item: FileItem | FolderItem) {
		if (dropFolderId === item.id) dropFolderId = null;
	}
	function rlOnItemDrop(e: DragEvent, item: FileItem | FolderItem) {
		if (isFile(item)) return;
		onFolderDrop(e, item);
	}

	// ── Upload split-button popup state ─────────────────────────────────────
	let uploadMenuOpen = $state(false);

	// Close the upload popup when clicking outside of it.
	$effect(() => {
		if (!uploadMenuOpen) return;
		const onDown = (e: MouseEvent) => {
			if (!(e.target as HTMLElement).closest('.upload-dropdown')) uploadMenuOpen = false;
		};
		window.addEventListener('pointerdown', onDown);
		return () => window.removeEventListener('pointerdown', onDown);
	});

	// Reload whenever the route path OR the server sort dimension/direction
	// changes.
	//
	// `load()` reads several reactive signals in its sync phase
	// (session.isExternalUser, session.homeFolderId, plus whatever
	// its awaited callees touch). Naively calling `void load()` here
	// tracks all of those as dependencies of this effect — and
	// `session.loadHomeFolder()`'s own writes to `homeFolderId`
	// during its resolution then re-trigger the effect, firing a
	// second and third `load()` before the first has settled. Wrap
	// in `untrack` so the ONLY dependencies are the three we WANT
	// to reload on: pathSegments, sortField, reversed.
	$effect(() => {
		void pathSegments;
		void sortField;
		void reversed;
		untrack(() => {
			// Route/sort change → drop the transient "new elements"
			// swimlane. It's a per-folder confirmation of "here's what
			// you just added"; carrying it across folders would surface
			// stale ids that don't belong to the new listing.
			newlyAdded.clear();
			void load(true);
		});
	});

	// The command palette's "Upload files" action navigates here then dispatches
	// this event so the hidden file picker opens (the input lives on this page).
	$effect(() => {
		const open = () => fileInput?.click();
		window.addEventListener('oxicloud:upload-files', open);
		return () => window.removeEventListener('oxicloud:upload-files', open);
	});
</script>

<svelte:head><title>{t('nav.files', 'Files')} · OxiCloud</title></svelte:head>

<svelte:window
	onkeydown={onKeydown}
	ondragenter={onWindowDragEnter}
	ondragover={onWindowDragOver}
	ondragleave={onWindowDragLeave}
	ondragend={onWindowDragEnd}
	ondrop={onWindowDrop}
/>

<div class="files-page" data-testid="files-dropzone">
	<!-- Read-only freeze banner, placed inside the listing container so it
	     lives in the same visual/scroll flow as the file list (previously
	     rendered above the page container, which visually detached it from
	     the listing). Scrolls with the content — sticky header below stays
	     pinned. Shows when either any item in the current listing surfaces
	     a `drive_id` for a frozen drive, or (empty folder fallback) the
	     URL's leading segment resolves to one. -->
	{#if currentDrive?.policies?.read_only}
		<ReadOnlyBanner driveName={currentDrive.name} />
	{/if}

	<!-- Hidden upload inputs stay mounted even while the batch bar is shown.
	     Kept OUTSIDE ResourceList so the split-button dropdown in the
	     `actions` snippet can click() them without ResourceList's internal
	     scoping getting in the way. -->
	<input
		bind:this={fileInput}
		type="file"
		multiple
		hidden
		data-testid="files-upload-file-input"
		onchange={onUpload}
	/>
	<input
		bind:this={folderInput}
		type="file"
		multiple
		hidden
		webkitdirectory
		data-testid="files-upload-folder-input"
		onchange={onUploadFolder}
	/>

	<ResourceList
		title={t('nav.files', 'Files')}
		items={rlItems}
		emptyText={hiddenCount > 0
			? t('files.empty_hidden_title', { n: hiddenCount }, '{{n}} hidden item(s) in this folder')
			: t('files.empty_title', 'This folder is empty')}
		emptyHint={hiddenCount > 0
			? t(
					'files.empty_hidden_hint',
					"Files whose name starts with '.' are hidden. Toggle the setting to see them."
				)
			: t('files.empty_hint', 'Drop files here or use the Upload button to add files.')}
		emptyIcon={hiddenCount > 0 ? 'eye-slash' : undefined}
		{loading}
		error={error ?? undefined}
		selectable
		shiftRangeSelect
		showOwner
		showType
		showDate
		dateLabel={t('files.col_modified', 'Modified')}
		showPath={false}
		showDotfileToggle
		enableSystemDrop
		onsystemdrop={onDrop}
		systemDropOverlayActive={pageSystemDropOver}
		groupBys={rlGroupBys}
		bind:groupBy
		bind:reversed
		hasMore={pageCursor !== undefined}
		onloadmore={loadMore}
		onreload={(orderBy) => {
			sortField = orderBy as SortField;
		}}
		onopen={rlOnOpen}
		onfavorite={rlOnFavorite}
		onshared={(item) => openShare(isFile(item) ? 'file' : 'folder', item.id, item.name)}
		oncontextmenu={rlOnContextMenu}
		onselectionchange={(ids) => replaceSet(selected, ids)}
		isDraggable={rlIsDraggable}
		isDropTarget={rlIsDropTarget}
		dropTargetId={dropFolderId}
		onitemdragstart={rlOnItemDragStart}
		onitemdragover={rlOnItemDragOver}
		onitemdragleave={rlOnItemDragLeave}
		onitemdrop={rlOnItemDrop}
	>
		{#snippet emptyAction()}
			<!-- Surfaces only when the folder isn't really empty — it's just
			     filtered because the user chose to hide dotfiles. Clicking
			     flips the app-wide `preferences.hideDotfiles` back off,
			     re-populating the list without a hunt through settings. -->
			{#if hiddenCount > 0}
				<button
					class="btn btn-secondary"
					onclick={() => preferences.setHideDotfiles(false)}
					data-testid="files-show-hidden-btn"
				>
					<Icon name="eye" />
					{t('files.show_hidden', 'Show hidden files')}
				</button>
			{/if}
		{/snippet}

		{#snippet breadcrumb()}
			<!--
				Shared component (2026-07-26 migration). Fetches the ancestor
				chain in ONE round-trip via `GET /api/folders/{id}/ancestors`
				(replaces the per-segment `buildCrumbs` walker + N `getFolder`
				requests). Root icon is derived from `access_source.kind` on
				the endpoint response — no more `drivesStore.findByRootFolderId`
				lookup here.

				`onDrop` prop preserves the row-drop-to-crumb behaviour: the
				component handles the `dragover`/`dragenter`/`dragleave` UI +
				`.drop-target` highlight; we get the target folder id + the
				raw event and dispatch to `onCrumbDrop`.
			-->
			<FolderBreadcrumb
				folderId={currentId}
				onDrop={(target, e) => onCrumbDrop(e, target)}
				dragMime={DRAG_TYPE}
			/>
		{/snippet}

		{#snippet actions()}
			<div class="upload-dropdown" data-testid="files-upload-dropdown">
				<button
					class="btn btn-primary"
					data-testid="files-upload-btn"
					onclick={() => (uploadMenuOpen = !uploadMenuOpen)}
					disabled={uploading}
					aria-haspopup="true"
					aria-expanded={uploadMenuOpen}
				>
					<Icon name="cloud-upload-alt" class="icon-mr" />
					<span
						>{uploading ? t('files.uploading', 'Uploading…') : t('actions.upload', 'Upload')}</span
					>
					<Icon name="caret-down" class="upload-caret" />
				</button>
				{#if uploadMenuOpen}
					<div class="upload-dropdown-menu" data-testid="files-upload-menu">
						<button
							class="upload-dropdown-item"
							data-testid="files-upload-files-item"
							onclick={() => {
								uploadMenuOpen = false;
								fileInput?.click();
							}}
						>
							<Icon name="file" />
							<span>{t('actions.upload_files', 'Upload files')}</span>
						</button>
						<button
							class="upload-dropdown-item"
							data-testid="files-upload-folder-item"
							onclick={() => {
								uploadMenuOpen = false;
								folderInput?.click();
							}}
						>
							<Icon name="folder-open" />
							<span>{t('actions.upload_folder', 'Upload folder')}</span>
						</button>
					</div>
				{/if}
			</div>
			<button class="btn btn-secondary" data-testid="files-new-folder-btn" onclick={onNewFolder}>
				<Icon name="folder-plus" class="icon-mr" />
				<span>{t('actions.new_folder', 'New folder')}</span>
			</button>
		{/snippet}

		{#snippet batchActions(_sel)}
			<button
				class="batch-btn"
				title={t('files.add_favorites', 'Add to favorites')}
				data-testid="files-batch-favorite-btn"
				onclick={() => void batchFavorites()}
			>
				<Icon name="star" />
				<span>{t('files.add_favorites', 'Add to favorites')}</span>
			</button>
			<button
				class="batch-btn"
				title={t('files.move', 'Move')}
				data-testid="files-batch-move-btn"
				onclick={batchMove}
			>
				<Icon name="arrows-alt" />
				<span>{t('files.move', 'Move')}</span>
			</button>
			<button
				class="batch-btn"
				title={t('files.copy', 'Copy')}
				data-testid="files-batch-copy-btn"
				onclick={batchCopy}
			>
				<Icon name="copy" />
				<span>{t('files.copy', 'Copy')}</span>
			</button>
			<button
				class="batch-btn"
				title={t('common.download', 'Download')}
				data-testid="files-batch-download-btn"
				onclick={() => void batchDownload()}
			>
				<Icon name="download" />
				<span>{t('common.download', 'Download')}</span>
			</button>
			<button
				class="batch-btn batch-btn-danger"
				title={t('common.delete', 'Delete')}
				data-testid="files-batch-delete-btn"
				onclick={batchDelete}
			>
				<Icon name="trash" />
				<span>{t('common.delete', 'Delete')}</span>
			</button>
		{/snippet}
	</ResourceList>
</div>

{#if moveDialog.component}
	{@const MoveDialog = moveDialog.component}
	<MoveDialog
		bind:open={moveOpen}
		item={actionTarget}
		items={moveItems}
		mode={moveMode}
		onmoved={() => {
			clearSelection();
			void reload();
		}}
	/>
{/if}
{#if shareDialog.component}
	{@const ShareDialog = shareDialog.component}
	<ShareDialog
		bind:open={shareOpen}
		item={actionTarget}
		onshared={(id) => {
			// Optimistic in-place flip so the shared chip appears on
			// the row without waiting for the next listing refetch.
			const row = orderedItems.find((it) => it.id === id);
			if (row) row.is_shared = true;
		}}
	/>
{/if}
{#if fileViewer.component}
	{@const FileViewer = fileViewer.component}
	<FileViewer bind:open={viewerOpen} file={viewerFile} />
{/if}
{#if wopiEditor.component}
	{@const WopiEditor = wopiEditor.component}
	<WopiEditor
		bind:open={wopiOpen}
		fileId={wopiFile?.id ?? null}
		fileName={wopiFile?.name ?? ''}
		action={wopiAction}
	/>
{/if}

{#if ctxOpen && ctxTarget}
	<div
		class="ctx-scrim"
		role="presentation"
		data-testid="files-context-menu-scrim"
		onclick={closeContext}
		oncontextmenu={(e) => e.preventDefault()}
	></div>
	<div
		class="ctx-menu"
		style:left="{ctxX}px"
		style:top="{ctxY}px"
		role="menu"
		data-testid="files-context-menu"
	>
		{#if ctxTarget.kind === 'folder'}
			<button
				class="ctx-item"
				role="menuitem"
				data-testid="files-ctx-folder-open-item"
				onclick={() => {
					const id = ctxTarget!.id;
					closeContext();
					// Canonical single-id URL — see `openFolder` above.
					goto(resolve(`/files/${id}`));
				}}><Icon name="folder-open" /> {t('files.open', 'Open')}</button
			>
			<button
				class="ctx-item"
				role="menuitem"
				data-testid="files-ctx-download-zip-item"
				onclick={() => {
					const tg = ctxTarget!;
					closeContext();
					downloadFolderZip({ id: tg.id, name: tg.name });
				}}><Icon name="download" /> {t('files.download_zip', 'Download as ZIP')}</button
			>
		{:else}
			<button
				class="ctx-item"
				role="menuitem"
				data-testid="files-ctx-file-open-item"
				onclick={() => {
					const f = listing.files.find((x) => x.id === ctxTarget!.id);
					closeContext();
					if (f) openFile(f);
				}}><Icon name="eye" /> {t('files.open', 'Open')}</button
			>
			{#if ctxCanEditWopi}
				<button
					class="ctx-item"
					role="menuitem"
					data-testid="files-ctx-edit-item"
					onclick={() => {
						const tg = ctxTarget!;
						closeContext();
						openWopi(tg.id, tg.name, 'edit');
					}}><Icon name="pen" /> {t('files.edit', 'Edit')}</button
				>
				<button
					class="ctx-item"
					role="menuitem"
					data-testid="files-ctx-edit-new-tab-item"
					onclick={() => {
						const tg = ctxTarget!;
						closeContext();
						void openWopiTab(tg.id, tg.name);
					}}><Icon name="external-link-alt" /> {t('files.edit_new_tab', 'Edit in new tab')}</button
				>
			{/if}
			<a
				class="ctx-item"
				role="menuitem"
				href={fileDownloadUrl(ctxTarget.id)}
				rel="external"
				download
				data-testid="files-ctx-download-item"
				onclick={closeContext}><Icon name="download" /> {t('common.download', 'Download')}</a
			>
			<button
				class="ctx-item"
				role="menuitem"
				data-testid="files-ctx-open-parent-item"
				onclick={() => {
					const f = listing.files.find((x) => x.id === ctxTarget!.id);
					closeContext();
					if (f) openParentFolder(f);
				}}><Icon name="folder-open" /> {t('files.open_parent', 'Open parent folder')}</button
			>
			{#if isAudio(listing.files.find((x) => x.id === ctxTarget!.id))}
				<button
					class="ctx-item"
					role="menuitem"
					data-testid="files-ctx-add-playlist-item"
					onclick={() => {
						const f = listing.files.find((x) => x.id === ctxTarget!.id);
						closeContext();
						if (f) void addToPlaylist(f);
					}}><Icon name="music" /> {t('music.add_to_playlist', 'Add to playlist')}</button
				>
			{/if}
		{/if}
		<button
			class="ctx-item"
			role="menuitem"
			data-testid="files-ctx-share-item"
			onclick={() => {
				const tg = ctxTarget!;
				closeContext();
				openShare(tg.kind, tg.id, tg.name);
			}}><Icon name="share-alt" /> {t('files.share', 'Share')}</button
		>
		<button
			class="ctx-item"
			role="menuitem"
			data-testid="files-ctx-move-item"
			onclick={() => {
				const tg = ctxTarget!;
				closeContext();
				openMove(tg.kind, tg.id, tg.name);
			}}><Icon name="arrows-alt" /> {t('files.move', 'Move')}</button
		>
		<button
			class="ctx-item"
			role="menuitem"
			data-testid="files-ctx-copy-item"
			onclick={() => {
				const tg = ctxTarget!;
				closeContext();
				openCopy(tg.kind, tg.id, tg.name);
			}}><Icon name="copy" /> {t('files.copy', 'Copy')}</button
		>
		<button
			class="ctx-item"
			role="menuitem"
			data-testid="files-ctx-favorite-item"
			onclick={() => {
				const tg = ctxTarget!;
				closeContext();
				void toggleFavorite(tg.kind, tg.id);
			}}
		>
			<Icon name="star" />
			{orderedItems.find((it) => it.id === ctxTarget!.id)?.is_favorite
				? t('files.unfavorite', 'Remove favorite')
				: t('files.favorite', 'Add favorite')}
		</button>
		<button
			class="ctx-item"
			role="menuitem"
			data-testid="files-ctx-rename-item"
			onclick={() => {
				const tg = ctxTarget!;
				closeContext();
				void renameItem(tg.kind, tg.id, tg.name);
			}}><Icon name="pen" /> {t('common.rename', 'Rename')}</button
		>
		<button
			class="ctx-item ctx-item--danger"
			role="menuitem"
			data-testid="files-ctx-delete-item"
			onclick={() => {
				const tg = ctxTarget!;
				closeContext();
				void deleteItem(tg.kind, tg.id, tg.name);
			}}><Icon name="trash" /> {t('common.delete', 'Delete')}</button
		>
	</div>
{/if}

<style>
	.files-page {
		min-height: 100%;
	}

	/* Scrim + menu must sit ABOVE `.page-sticky-header` (breadcrumb +
	   action bar, `z-index: var(--z-sticky)` = 100) so a click on the
	   breadcrumb or the action bar closes the menu. Otherwise the
	   sticky header covers the scrim and swallows the outside-click.
	   Aligns with ResourceList's built-in menu (`.rl-ctx-scrim` uses
	   `1000` / `.rl-ctx-menu` uses `1001`). */
	.ctx-scrim {
		position: fixed;
		inset: 0;
		z-index: 1000;
	}

	.ctx-menu {
		position: fixed;
		z-index: 1001;
		min-width: 12rem;
		padding: var(--space-1);
		background: var(--color-bg-surface);
		border: 1px solid var(--color-border);
		border-radius: var(--radius-md);
		box-shadow: var(--shadow-lg, 0 10px 30px var(--color-overlay-shadow));
	}

	.ctx-item {
		display: flex;
		align-items: center;
		gap: 0.6rem;
		width: 100%;
		padding: 0.5rem 0.6rem;
		border: none;
		background: none;
		color: var(--color-text);
		cursor: pointer;
		text-align: left;
		text-decoration: none;
		border-radius: var(--radius-sm);
		font-size: var(--text-sm);
	}

	.ctx-item:hover {
		background: var(--color-bg-hover);
	}

	.ctx-item--danger {
		/* Danger *foreground* on the light menu surface — must be the red accent,
		   not --color-danger-text (which is white, for text ON a red fill, and
		   rendered near-invisible here). Mirrors the user-menu logout red. */
		color: var(--color-danger-alt);
	}
</style>
