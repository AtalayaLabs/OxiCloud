<script lang="ts">
	/**
	 * Public share page.
	 *
	 * `GET /api/s/{token}` is the only share-specific call left: it resolves
	 * the token, enforces the password gate, and — the part that matters —
	 * issues the `oxi_shares` ring cookie. From that point the visitor holds
	 * an anonymous session, and this page addresses the shared resource
	 * through the ORDINARY API (`/api/folders/*`, `/api/files/*`) using the
	 * ordinary components.
	 *
	 * That is the whole design: a share is a login method, not a parallel
	 * application. The ~700 lines of bespoke grid, lightbox and lazy-video
	 * wiring this file used to carry were a second implementation of
	 * `ResourceList` + `PhotoLightbox`, and drifted from them — issue #721
	 * (full-resolution images where the real grid uses thumbnails) was one
	 * symptom.
	 *
	 * Read-only is enforced server-side: the anonymous allowlist admits seven
	 * GET routes and nothing else. The `readOnly` props below are a UX
	 * concern only — an affordance that can only ever 403 is worse than no
	 * affordance. `ResourceList` needs no such prop because every mutating
	 * action there is already opt-in via its own callback, and this page
	 * simply passes none of them.
	 */
	import { page } from '$app/state';
	import { onMount, untrack } from 'svelte';
	import Icon from '$lib/icons/Icon.svelte';
	import BrandMark from '$lib/components/BrandMark.svelte';
	import FileViewer from '$lib/components/FileViewer.svelte';
	import FolderBreadcrumb from '$lib/components/FolderBreadcrumb.svelte';
	import PhotoLightbox from '$lib/components/PhotoLightbox.svelte';
	import ResourceList from '$lib/components/ResourceList.svelte';
	import { fileDownloadUrl, getFile } from '$lib/api/endpoints/files';
	import { fetchFolderPage, folderZipUrl } from '$lib/api/endpoints/folders';
	import { getShareMeta, verifySharePassword, type ShareMeta } from '$lib/api/endpoints/share';
	import type { FileItem, FolderItem } from '$lib/api/types';
	import { t } from '$lib/i18n/index.svelte';

	type View = 'loading' | 'password' | 'expired' | 'invalid' | 'file' | 'folder';

	const token = $derived(page.params.token ?? '');

	let view = $state<View>('loading');
	let meta = $state<ShareMeta | null>(null);

	// Folder-share state
	let folderId = $state<string | null>(null);
	let items = $state<Array<FileItem | FolderItem>>([]);
	let nextCursor = $state<string | undefined>(undefined);
	let listLoading = $state(false);
	let listError = $state<string | null>(null);

	// File-share state (also used for a non-media file opened from a folder)
	let viewerFile = $state<FileItem | null>(null);
	let viewerOpen = $state(false);

	// Lightbox over the media files in the CURRENT folder, matching the
	// photos grid: index into `mediaFiles`, -1 when closed.
	let lightboxIndex = $state(-1);

	// Password gate
	let pwInput = $state('');
	let pwError = $state('');
	let pwBusy = $state(false);

	const isMedia = (f: FileItem) =>
		f.mime_type.startsWith('image/') || f.mime_type.startsWith('video/');

	const files = $derived(items.filter((i): i is FileItem => 'mime_type' in i));
	const mediaFiles = $derived(files.filter(isMedia));

	/** `#folder=<id>` deep link, so a sub-folder stays bookmarkable. */
	function hashFolderId(): string | undefined {
		if (typeof location === 'undefined') return undefined;
		const m = location.hash.match(/[#&]folder=([A-Za-z0-9-]{1,64})/);
		return m ? m[1] : undefined;
	}

	/** `?file=<id>` — the open preview, the same contract `/files` uses. */
	function urlFileId(): string | null {
		if (typeof location === 'undefined') return null;
		return new URLSearchParams(location.search).get('file');
	}

	/**
	 * Reflect the open preview into the URL so it is bookmarkable and Back
	 * closes it.
	 *
	 * `push` on open — Back should dismiss the preview rather than leave the
	 * share entirely. `replace` on close and on stepping through the lightbox,
	 * so dismissing adds no entry and arrow-keying a gallery does not bury the
	 * folder under a hundred history states.
	 *
	 * Raw `history` rather than `goto()` (which is how `/files` does it)
	 * because folder navigation on this page already writes `#folder=` the
	 * same way: one mechanism owning the URL cannot desync with the other.
	 */
	function syncFileParam(id: string | null, mode: 'push' | 'replace') {
		if (typeof history === 'undefined' || typeof location === 'undefined') return;
		const url = new URL(location.href);
		if (id) url.searchParams.set('file', id);
		else url.searchParams.delete('file');
		if (url.href === location.href) return;
		const state = history.state as unknown;
		if (mode === 'push') history.pushState(state, '', url);
		else history.replaceState(state, '', url);
	}

	/** Open whichever previewer suits the file's type. */
	function showFile(file: FileItem) {
		if (isMedia(file)) {
			const i = mediaFiles.findIndex((m) => m.id === file.id);
			if (i >= 0) lightboxIndex = i;
			return;
		}
		viewerFile = file;
		viewerOpen = true;
	}

	/** Open the file named by `?file=` once the listing that holds it exists. */
	function openUrlFile() {
		const id = urlFileId();
		if (!id) return;
		const f = files.find((x) => x.id === id);
		if (f) showFile(f);
	}

	async function loadFolder(id: string, pushHistory = false) {
		listLoading = true;
		listError = null;
		lightboxIndex = -1;
		try {
			const pageData = await fetchFolderPage(id);
			items = pageData.items;
			nextCursor = pageData.nextCursor;
			folderId = id;
			view = 'folder';
			if (pushHistory && typeof history !== 'undefined') {
				// The share ROOT carries no fragment, so the bare link stays
				// clean; only a descendant needs one. `?file=` is dropped: it
				// named a preview in the folder being left, and carrying it
				// over would point at something this listing does not contain.
				const hash = id === meta?.item_id ? '' : `#folder=${encodeURIComponent(id)}`;
				const url = new URL(location.href);
				url.searchParams.delete('file');
				history.pushState({ folderId: id }, '', `${url.pathname}${url.search}${hash}`);
			}
		} catch {
			listError = t('share.error', 'Something went wrong. Please try again.');
		} finally {
			listLoading = false;
		}
	}

	async function loadMore() {
		if (!nextCursor || !folderId || listLoading) return;
		listLoading = true;
		try {
			const pageData = await fetchFolderPage(folderId, { cursor: nextCursor });
			items = [...items, ...pageData.items];
			nextCursor = pageData.nextCursor;
		} catch {
			/* keep what we have — the visitor can retry by scrolling again */
		} finally {
			listLoading = false;
		}
	}

	async function loadMeta() {
		view = 'loading';
		if (!token) {
			view = 'invalid';
			return;
		}
		try {
			const r = await getShareMeta(token);
			if (r.status !== 'ok') {
				view = r.status === 'password' ? 'password' : r.status;
				return;
			}
			meta = r.data;
			if (r.data.item_type === 'folder') {
				// A deep link names a DESCENDANT of the share root; fall back
				// to the root itself. Either way the ring cookie issued above
				// is what authorizes the listing.
				await loadFolder(hashFolderId() ?? r.data.item_id);
				// The listing now exists, so a `?file=` from a cold deep link
				// has something to resolve against.
				openUrlFile();
			} else {
				viewerFile = await getFile(r.data.item_id);
				view = 'file';
				// Open straight into the preview — a one-file link is a
				// request to see that file, not a landing page. The card
				// behind is what a close returns to.
				viewerOpen = true;
			}
		} catch {
			view = 'expired';
		}
	}

	function openItem(item: FileItem | FolderItem) {
		if (!('mime_type' in item)) {
			void loadFolder(item.id, true);
			return;
		}
		showFile(item);
		syncFileParam(item.id, 'push');
	}

	/**
	 * Browser back/forward. Reconciles BOTH halves of the URL: the folder from
	 * `#folder=` and the preview from `?file=`. Going back out of a preview
	 * changes only the query, so the folder reload is skipped — reloading it
	 * would throw away the listing the preview is still reading from.
	 */
	function onPopState() {
		if (view !== 'folder' || !meta) return;
		const target = hashFolderId() ?? meta.item_id;
		if (target !== folderId) {
			void loadFolder(target);
			return;
		}
		const id = urlFileId();
		if (!id) {
			lightboxIndex = -1;
			viewerOpen = false;
			return;
		}
		openUrlFile();
	}

	async function submitPassword(e: SubmitEvent) {
		e.preventDefault();
		if (!pwInput) return;
		pwBusy = true;
		pwError = '';
		try {
			const ok = await verifySharePassword(token, pwInput);
			if (!ok) {
				pwError = t('share.bad_password', 'Incorrect password. Please try again.');
				return;
			}
			// `/verify` appended this share to the ring — reload through the
			// normal path so the unlocked listing renders.
			await loadMeta();
		} catch {
			pwError = t('share.error', 'Something went wrong. Please try again.');
		} finally {
			pwBusy = false;
		}
	}

	/**
	 * Preview state → URL.
	 *
	 * Only a genuine open→closed transition clears the param. On a cold deep
	 * link the previewer is briefly closed WITH `?file=` set while the listing
	 * loads; clearing there would race the open and the preview would never
	 * appear. While the lightbox is open, the param tracks the current photo,
	 * so sharing the URL mid-gallery shares the photo being looked at.
	 */
	let previewWasOpen = false;
	$effect(() => {
		const open = lightboxIndex >= 0 || viewerOpen;
		const currentId = lightboxIndex >= 0 ? mediaFiles[lightboxIndex]?.id : viewerFile?.id;
		untrack(() => {
			if (open && currentId) syncFileParam(currentId, 'replace');
			else if (previewWasOpen && !open) syncFileParam(null, 'replace');
			previewWasOpen = open;
		});
	});

	onMount(() => {
		void loadMeta();
	});
</script>

<svelte:head><title>{meta?.item_name ?? t('share.title', 'Shared')} · OxiCloud</title></svelte:head>
<svelte:window onpopstate={onPopState} />

<!--
	`main-content` + `content-area` are the two global classes AppShell wraps
	every authenticated page in (`ported/topbar.css`, `ported/content.css`).
	Reused verbatim so this page lays out exactly like `/files` — the shell
	minus its sidebar — rather than approximating it.

	`main-content` is load-bearing, not cosmetic: `body` is `display: flex`
	(a row, for sidebar + content), so a bare `<main>` is a flex ITEM and
	sizes to its own content — a ~390px column with the grid crushed inside
	it. `flex-grow: 1` is what makes it fill the viewport.
-->
<main class="share main-content">
	{#if view === 'loading'}
		<p class="share__status">{t('common.loading', 'Loading…')}</p>
	{:else if view === 'invalid'}
		<!--
			Branded like the gate: a dead link is one of the three screens a
			stranger can land on, and the only thing that says where they are.
		-->
		<div class="share__center">
			<BrandMark />
			<Icon name="ban" class="share__big-icon" />
			<p>{t('share.invalid', 'This share link is invalid.')}</p>
		</div>
	{:else if view === 'expired'}
		<div class="share__center">
			<BrandMark />
			<Icon name="ban" class="share__big-icon" />
			<p>{t('share.expired', 'This share link is no longer available.')}</p>
		</div>
	{:else if view === 'password'}
		<!--
			The gate is the first thing a visitor sees on a protected link, and
			the only screen before the grid exists to carry the mark — so the
			brand goes here rather than nowhere.
		-->
		<div class="share__gate">
			<BrandMark />
			<form class="share__pw" data-testid="public-share-password-form" onsubmit={submitPassword}>
				<h1>{t('share.password_title', 'Password required')}</h1>
				<input
					type="password"
					data-testid="public-share-password-input"
					bind:value={pwInput}
					placeholder={t('share.password', 'Password')}
					disabled={pwBusy}
					autocomplete="off"
				/>
				{#if pwError}<p class="share__error" role="alert">{pwError}</p>{/if}
				<button type="submit" data-testid="public-share-unlock-btn" disabled={pwBusy}
					>{t('share.unlock', 'Unlock')}</button
				>
			</form>
		</div>
	{:else if view === 'file' && viewerFile}
		<!--
			A single shared file. The viewer opens over this card rather than
			instead of it: a share with one file has no listing to fall back
			to, so dismissing a viewer that WAS the whole page left a blank
			screen with no way back. The card is what the visitor returns to,
			and it carries the two things they came for — the name and the
			download — without needing the preview at all.
		-->
		<div class="share__center">
			<BrandMark />
			<Icon name="file" class="share__big-icon" />
			<h1>{viewerFile.name}</h1>
			<div class="share__file-actions">
				<button type="button" class="share__btn" onclick={() => (viewerOpen = true)}>
					<Icon name="expand" />
					{t('share.preview', 'Preview')}
				</button>
				<a
					class="share__btn"
					data-testid="public-share-download-btn"
					href={fileDownloadUrl(viewerFile.id)}
					download
					rel="external"
				>
					<Icon name="download" />
					{t('share.download', 'Download')}
				</a>
			</div>
		</div>
	{:else if view === 'folder' && folderId}
		<!--
			`content-area` is AppShell's scrolling content container (global,
			`styles/ported/content.css`). Reused rather than reproduced:
			`ResourceList` emits `page-sticky-header`, whose
			`top: calc(-1 * var(--space-5))` exists precisely to cancel this
			container's top padding. Outside it the sticky header sits wrong
			and the list loses the shared `--gutter`.
		-->
		<div class="content-area">
			<!--
				The measure wrapper goes INSIDE `.content-area`, not on it:
				`.content-area` is the scroll container, so capping it there
				would pull the scrollbar off the viewport edge and into the
				middle of the page.
			-->
			<div class="share__measure">
				<ResourceList
					{items}
					loading={listLoading}
					error={listError}
					hasMore={nextCursor !== undefined}
					onloadmore={loadMore}
					onopen={openItem}
					allowThumbnailGenerate={false}
					emptyIcon="folder-open"
					emptyText={t('share.empty', 'This shared folder is empty.')}
				>
					<!--
					The brand sits with the heading rather than in a bar of its
					own, so it scrolls away on descent and the pinned strip
					stays just the action bar + breadcrumb. A visitor has no
					sidebar, so this is the only place the mark appears.
					Unlinked: `/files` would only bounce them to login.
				-->
					{#snippet heading()}
						<div class="share__heading">
							<BrandMark />
							<h1 class="page-title share__title">{meta?.item_name ?? ''}</h1>
						</div>
					{/snippet}

					<!--
					Breadcrumb and actions go THROUGH ResourceList, exactly as
					`/files` passes them, so they render inside its own
					`page-sticky-header` and stay put while the grid scrolls.
					Rendering them in a sibling <header> looked close but was
					not sticky and did not share the header's alignment.
				-->
					{#snippet breadcrumb()}
						<FolderBreadcrumb {folderId} />
					{/snippet}

					<!--
					Re-tested inside the snippet: a snippet is a closure, so
					the `folderId` narrowing on the branch above does not reach
					in here.
				-->
					{#snippet actions()}
						{#if folderId}
							<a
								class="share__btn"
								data-testid="public-share-download-zip-btn"
								href={folderZipUrl(folderId)}
								download
								rel="external"
							>
								<Icon name="download" />
								{t('share.download_zip', 'Download all')}
							</a>
						{/if}
					{/snippet}
				</ResourceList>
			</div>
		</div>
	{/if}
</main>

<PhotoLightbox items={mediaFiles} bind:index={lightboxIndex} readOnly />

<!--
	One viewer for both shapes of share. Bound, so its own close button drives
	`viewerOpen` back to false and the page beneath reappears — the folder grid
	or the single-file card.
-->
<FileViewer bind:open={viewerOpen} file={viewerFile} readOnly />

<style>
	/*
	 * Layout comes from `main-content` + `content-area` (see the markup) —
	 * the same global classes AppShell gives `/files`. Nothing here restates
	 * them: the flex column, the gutter and the scroll container are all
	 * inherited, so this page cannot drift from the app's layout the way the
	 * old hand-rolled grid drifted from `ResourceList`.
	 *
	 * Only the text colour is set. `body` already paints `--color-bg-page`,
	 * and adding padding here would double `.content-area`'s inset and break
	 * `page-sticky-header`'s negative offset.
	 */
	.share {
		/*
		 * Reading measure for the share surface. `/files` runs full-bleed
		 * because a sidebar already eats the left third; a visitor has no
		 * sidebar, so an uncapped grid stretches a handful of tiles across a
		 * whole desktop. `rem`, not `px`, so it tracks the user's font size.
		 */
		--share-measure: 80rem;

		color: var(--color-text);
	}

	.share__measure {
		max-width: var(--share-measure);
		margin-inline: auto;
	}

	/*
	 * The non-folder states are direct children of the flex column with no
	 * `.content-area` to pad them, so they carry their own inset.
	 */
	.share__status,
	.share__center,
	.share__gate {
		padding-inline: var(--space-6);
	}

	/*
	 * `flex: none` so the brand bar keeps its height instead of being
	 * squeezed by `.content-area`'s `flex-grow: 1` sibling.
	 *
	 * `.logo-container` (global) already supplies the padding and separator;
	 * only its sidebar-specific bottom margin is dropped, since here the
	 * scroll container follows immediately.
	 */
	/*
	 * Mark and title on one row, replacing ResourceList's bare `<h1>`. The
	 * row takes over `page-title`'s bottom margin (zeroed below), keeping the
	 * gap above the sticky header identical to every other page.
	 */
	.share__heading {
		display: flex;
		align-items: center;
		gap: var(--space-3);
		margin-bottom: var(--space-5);
	}

	/*
	 * `.logo-container` carries the sidebar's own chrome — a bottom separator,
	 * block padding and a bottom margin — none of which belongs on a heading
	 * row or above a form. Stripped here rather than in the shared rule, which
	 * the sidebar still wants exactly as it is.
	 */
	.share__heading :global(.logo-container),
	.share__center :global(.logo-container),
	.share__gate :global(.logo-container) {
		padding: 0;
		margin-bottom: 0;
		border-bottom: none;
	}

	/*
	 * `.app-name` is coloured `--color-sidebar-text-active`, which is legible
	 * only against the sidebar's gradient — on the page background it was
	 * white-on-white in light mode. Dark mode happened to look right, which is
	 * what made it easy to miss.
	 *
	 * Recoloured rather than given the sidebar's background: this page has no
	 * sidebar, so the mark should sit on the page like everything else around
	 * it. `--color-text` is the same token the `<h1>` beside it uses, so the
	 * two read as one heading instead of a transplanted widget.
	 */
	.share__heading :global(.app-name),
	.share__center :global(.app-name),
	.share__gate :global(.app-name) {
		color: var(--color-text);
	}

	/* Owns the centring the form used to do, so the mark and the form move
	   as one block. */
	.share__gate {
		display: flex;
		flex-direction: column;
		align-items: center;
		gap: var(--space-6);
		max-width: 22rem;
		margin: 15vh auto 0;
	}

	/* `page-title` supplies the type; its bottom margin now belongs to the
	   row, which owns the spacing below the heading. */
	.share__title {
		margin-bottom: 0;
	}

	.share__status {
		text-align: center;
		padding: var(--space-12);
		color: var(--color-text-secondary);
	}

	.share__center {
		display: flex;
		flex-direction: column;
		align-items: center;
		justify-content: center;
		gap: var(--space-4);
		min-height: 60vh;
		text-align: center;
	}

	.share__pw {
		display: flex;
		flex-direction: column;
		gap: var(--space-4);
		/* `.share__gate` positions the block now; the form just fills it. */
		width: 100%;
	}

	.share__pw input {
		padding: var(--space-2);
		border: 1px solid var(--color-border);
		border-radius: var(--radius-md);
		background: var(--color-bg-input);
		color: var(--color-text);
	}

	.share__pw button {
		padding: var(--space-2) var(--space-4);
		border: none;
		border-radius: var(--radius-md);
		background: var(--color-accent);
		color: var(--color-on-accent);
		cursor: pointer;
	}

	.share__pw button:disabled {
		opacity: 0.6;
		cursor: default;
	}

	.share__error {
		color: var(--color-danger-bg);
		margin: 0;
	}

	.share__file-actions {
		display: flex;
		gap: var(--space-3);
		flex-wrap: wrap;
		justify-content: center;
	}

	.share__btn {
		display: inline-flex;
		border: none;
		cursor: pointer;
		font: inherit;
		align-items: center;
		gap: var(--space-2);
		padding: var(--space-2) var(--space-4);
		border-radius: var(--radius-md);
		background: var(--color-accent);
		color: var(--color-on-accent);
		text-decoration: none;
	}
</style>
