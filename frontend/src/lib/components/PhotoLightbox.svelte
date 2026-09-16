<script lang="ts">
	/**
	 * Full-screen photo/video lightbox, shared by the Photos timeline, People and
	 * Places views. Driven by an `items` list and a bindable `index` (-1 = closed);
	 * deletions are reported via `onDelete` so the parent can update its own list.
	 */
	import Icon from '$lib/icons/Icon.svelte';
	import { addFavorite } from '$lib/api/endpoints/favorites';
	import {
		deleteFile,
		fileDownloadUrl,
		fileInlineUrl,
		fileThumbnailUrl
	} from '$lib/api/endpoints/files';
	import { fetchFileMetadata, type FileMetadata } from '$lib/api/endpoints/photos';
	import type { FileItem } from '$lib/api/types';
	import { confirmDialog } from '$lib/stores/dialogs.svelte';
	import { t } from '$lib/i18n/index.svelte';
	import { errorToast } from '$lib/utils/errors';
	import { dateTimeFormatFor } from '$lib/utils/display';
	import { isVideo, photoTimestamp } from '$lib/utils/media';

	interface Props {
		items: FileItem[];
		/** Current index into `items`; -1 means closed. */
		index: number;
		/** Called after a successful delete so the parent can drop it from `items`. */
		onDelete?: (id: string) => void;
		/**
		 * Hide every affordance that mutates. Set by the public-share page,
		 * whose viewer holds a link rather than an account.
		 *
		 * Favorite and delete are the two that need it: unlike `ResourceList`,
		 * where each mutating affordance is opt-in via its own callback, these
		 * two are rendered unconditionally and call the API directly. A share
		 * visitor would see both and get a 403 from each — an affordance that
		 * can only fail is worse than no affordance.
		 *
		 * Download and full-resolution stay: a visitor is entitled to both,
		 * and `expandFullRes` is exactly what issue #721 is about.
		 */
		readOnly?: boolean;
	}

	let { items, index = $bindable(), onDelete, readOnly = false }: Props = $props();

	let showingOriginal = $state(false);
	let fullResBusy = $state(false);
	let meta = $state('');
	let favorited = $state(false);
	/** Token guarding against stale async loads during rapid prev/next. */
	let generation = 0;

	const item = $derived(index >= 0 ? (items[index] ?? null) : null);

	// Clamp the index when the list shrinks under us (e.g. after a delete): drop
	// to the last item, or close when nothing is left.
	$effect(() => {
		if (index < 0) return;
		if (items.length === 0) index = -1;
		else if (index >= items.length) index = items.length - 1;
	});

	function baseMeta(p: FileItem): string {
		const dateStr = dateTimeFormatFor(undefined, {
			year: 'numeric',
			month: 'short',
			day: 'numeric',
			hour: '2-digit',
			minute: '2-digit'
		}).format(photoTimestamp(p));
		return p.size_formatted ? `${dateStr} · ${p.size_formatted}` : dateStr;
	}

	function applyMetadata(p: FileItem, md: FileMetadata) {
		const parts = [baseMeta(p)];
		if (md.camera_make || md.camera_model) {
			parts.push([md.camera_make, md.camera_model].filter(Boolean).join(' '));
		}
		if (md.width && md.height) parts.push(`${md.width}×${md.height}`);
		meta = parts.join(' · ');
	}

	/** Reset per-item state and kick off metadata + neighbour preload. */
	function showItem(p: FileItem) {
		const gen = ++generation;
		showingOriginal = p.mime_type === 'image/gif';
		fullResBusy = false;
		favorited = false;
		meta = baseMeta(p);
		preloadNeighbors();
		void fetchFileMetadata(p.id).then((md) => {
			if (md && gen === generation) applyMetadata(p, md);
		});
	}

	// Re-run per-item setup whenever the visible item changes.
	$effect(() => {
		if (item) showItem(item);
	});

	function preloadNeighbors() {
		for (const i of [index - 1, index + 1]) {
			const it = items[i];
			if (it && !isVideo(it)) {
				const pre = new Image();
				pre.src = fileThumbnailUrl(it.id, 'large');
			}
		}
	}

	/** The image src to display: large thumbnail first, original on expand/GIF. */
	const imgSrc = $derived(
		item ? (showingOriginal ? fileInlineUrl(item.id) : fileThumbnailUrl(item.id, 'large')) : ''
	);

	function onImgError() {
		if (!item) return;
		// Thumbnail missing → fall back to the original; original failing is terminal.
		if (!showingOriginal) showingOriginal = true;
	}

	function onImgLoad() {
		fullResBusy = false;
	}

	function expandFullRes() {
		if (!item || showingOriginal) return;
		showingOriginal = true;
		fullResBusy = true;
	}

	function download() {
		if (!item) return;
		const a = document.createElement('a');
		a.href = fileDownloadUrl(item.id);
		a.download = item.name;
		document.body.appendChild(a);
		a.click();
		a.remove();
	}

	async function toggleFavorite() {
		if (!item) return;
		try {
			await addFavorite('file', item.id);
			favorited = !favorited;
		} catch (e) {
			errorToast(e);
		}
	}

	async function remove() {
		if (!item) return;
		const target = item;
		const ok = await confirmDialog({
			title: t('photos.delete', 'Delete photo'),
			message: t('photos.confirm_delete_one', { name: target.name }, 'Delete {{name}}?'),
			confirmText: t('common.delete', 'Delete'),
			danger: true
		});
		if (!ok) return;
		try {
			await deleteFile(target.id);
			onDelete?.(target.id);
		} catch (e) {
			errorToast(e);
		}
	}

	function prev() {
		if (index > 0) index -= 1;
	}
	function next() {
		if (index >= 0 && index < items.length - 1) index += 1;
	}
	function close() {
		index = -1;
	}
	function onKeydown(e: KeyboardEvent) {
		if (index < 0) return;
		if (e.key === 'Escape') close();
		else if (e.key === 'ArrowLeft') prev();
		else if (e.key === 'ArrowRight') next();
	}
</script>

<svelte:window onkeydown={onKeydown} />

{#if item}
	<!-- svelte-ignore a11y_click_events_have_key_events -->
	<div
		class="lb"
		role="dialog"
		aria-modal="true"
		aria-label={item.name}
		tabindex="-1"
		data-testid="photo-lightbox"
		onclick={(e) => e.target === e.currentTarget && close()}
	>
		<div class="lb__info">
			<div class="lb__filename">{item.name}</div>
			<div class="lb__meta">{meta}</div>
		</div>

		<button
			class="lb__close"
			aria-label={t('common.close', 'Close')}
			data-testid="photo-lightbox-close-btn"
			onclick={close}>×</button
		>

		<button
			class="lb__nav lb__nav--prev"
			aria-label={t('common.previous', 'Previous')}
			disabled={index === 0}
			data-testid="photo-lightbox-prev-btn"
			onclick={(e) => {
				e.stopPropagation();
				prev();
			}}><Icon name="chevron-left" /></button
		>

		<div class="lb__content">
			{#if isVideo(item)}
				{#key item.id}
					<video class="lb__media" controls autoplay poster={fileThumbnailUrl(item.id, 'large')}>
						<source src={fileInlineUrl(item.id)} type={item.mime_type} />
					</video>
				{/key}
			{:else}
				<img
					class="lb__media"
					src={imgSrc}
					alt={item.name}
					onload={onImgLoad}
					onerror={onImgError}
				/>
			{/if}
		</div>

		<button
			class="lb__nav lb__nav--next"
			aria-label={t('common.next', 'Next')}
			disabled={index === items.length - 1}
			data-testid="photo-lightbox-next-btn"
			onclick={(e) => {
				e.stopPropagation();
				next();
			}}><Icon name="chevron-right" /></button
		>

		<div class="lb__toolbar">
			{#if !isVideo(item) && item.mime_type !== 'image/gif' && !showingOriginal}
				<button
					class="lb__tool"
					title={t('photos.full_resolution', 'Full resolution')}
					disabled={fullResBusy}
					onclick={expandFullRes}><Icon name={fullResBusy ? 'spinner' : 'expand'} /></button
				>
			{/if}
			<button class="lb__tool" title={t('common.download', 'Download')} onclick={download}>
				<Icon name="download" />
			</button>
			{#if !readOnly}
				<button
					class="lb__tool"
					class:active={favorited}
					title={t('common.favorite', 'Favorite')}
					onclick={toggleFavorite}><Icon name={favorited ? 'star' : 'star-outline'} /></button
				>
				<button class="lb__tool" title={t('common.delete', 'Delete')} onclick={remove}>
					<Icon name="trash" />
				</button>
			{/if}
		</div>

		<div class="lb__counter">{index + 1} / {items.length}</div>
	</div>
{/if}

<style>
	.lb {
		position: fixed;
		inset: 0;
		z-index: 1000;
		background: var(--color-lightbox-overlay);
		display: flex;
		align-items: center;
		justify-content: center;
	}

	.lb__content {
		max-width: 92vw;
		max-height: 88vh;
		display: flex;
		align-items: center;
		justify-content: center;
	}

	.lb__media {
		max-width: 92vw;
		max-height: 88vh;
		object-fit: contain;
	}

	.lb__info {
		position: absolute;
		top: 1rem;
		left: 1rem;
		color: var(--color-on-accent);
		max-width: 60vw;
	}

	.lb__filename {
		font-weight: var(--weight-medium);
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}

	.lb__meta {
		font-size: var(--text-sm);
		opacity: 0.8;
	}

	.lb__close {
		position: absolute;
		top: 1rem;
		right: 1rem;
		font-size: 2rem;
		line-height: 1;
		background: none;
		border: none;
		color: var(--color-on-accent);
		cursor: pointer;
	}

	.lb__nav {
		position: absolute;
		top: 50%;
		transform: translateY(-50%);
		font-size: 2rem;
		background: none;
		border: none;
		color: var(--color-on-accent);
		cursor: pointer;
		padding: 1rem;
	}

	.lb__nav:disabled {
		opacity: 0.3;
		cursor: default;
	}

	.lb__nav--prev {
		left: 0.5rem;
	}

	.lb__nav--next {
		right: 0.5rem;
	}

	.lb__toolbar {
		position: absolute;
		bottom: 1rem;
		left: 50%;
		transform: translateX(-50%);
		display: flex;
		gap: var(--space-2);
	}

	.lb__tool {
		width: 40px;
		height: 40px;
		border-radius: 50%;
		border: none;
		background: var(--color-scrim-control);
		color: var(--color-on-accent);
		cursor: pointer;
		display: grid;
		place-items: center;
	}

	.lb__tool:disabled {
		opacity: 0.5;
		cursor: default;
	}

	.lb__tool.active {
		color: var(--color-accent);
	}

	.lb__counter {
		position: absolute;
		bottom: 1rem;
		right: 1rem;
		color: var(--color-on-accent);
		font-size: var(--text-sm);
		opacity: 0.8;
	}
</style>
