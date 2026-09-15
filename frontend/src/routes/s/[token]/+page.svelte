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
	import { onMount } from 'svelte';
	import Icon from '$lib/icons/Icon.svelte';
	import BrandMark from '$lib/components/BrandMark.svelte';
	import FileViewer from '$lib/components/FileViewer.svelte';
	import FolderBreadcrumb from '$lib/components/FolderBreadcrumb.svelte';
	import PhotoLightbox from '$lib/components/PhotoLightbox.svelte';
	import ResourceList from '$lib/components/ResourceList.svelte';
	import { getFile } from '$lib/api/endpoints/files';
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
				// clean; only a descendant needs one.
				const hash = id === meta?.item_id ? '' : `#folder=${encodeURIComponent(id)}`;
				history.pushState({ folderId: id }, '', location.pathname + location.search + hash);
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
			} else {
				viewerFile = await getFile(r.data.item_id);
				view = 'file';
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
		if (isMedia(item)) {
			const i = mediaFiles.findIndex((m) => m.id === item.id);
			if (i >= 0) lightboxIndex = i;
			return;
		}
		viewerFile = item;
		viewerOpen = true;
	}

	/** Browser back/forward — re-resolve the folder from the hash. */
	function onPopState() {
		if (view !== 'folder' || !meta) return;
		void loadFolder(hashFolderId() ?? meta.item_id);
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
		<div class="share__center">
			<Icon name="ban" class="share__big-icon" />
			<p>{t('share.invalid', 'This share link is invalid.')}</p>
		</div>
	{:else if view === 'expired'}
		<div class="share__center">
			<Icon name="ban" class="share__big-icon" />
			<p>{t('share.expired', 'This share link is no longer available.')}</p>
		</div>
	{:else if view === 'password'}
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
	{:else if view === 'file' && viewerFile}
		<!--
			A single shared file. The viewer is always open — there is nothing
			to navigate back to, so closing it would leave a blank page.
		-->
		<FileViewer open={true} file={viewerFile} readOnly />
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

{#if view === 'folder'}
	<FileViewer bind:open={viewerOpen} file={viewerFile} readOnly />
{/if}

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
	.share__pw {
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
	 * block padding and a bottom margin — none of which belong on a heading
	 * row. Stripped here rather than in the shared rule, which the sidebar
	 * still wants exactly as it is.
	 */
	.share__heading :global(.logo-container) {
		padding: 0;
		margin-bottom: 0;
		border-bottom: none;
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
		max-width: 22rem;
		margin: 15vh auto 0;
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

	.share__btn {
		display: inline-flex;
		align-items: center;
		gap: var(--space-2);
		padding: var(--space-2) var(--space-4);
		border-radius: var(--radius-md);
		background: var(--color-accent);
		color: var(--color-on-accent);
		text-decoration: none;
	}
</style>
