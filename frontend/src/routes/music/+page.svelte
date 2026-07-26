<script lang="ts">
	// Route-scoped styles: kept off the global critical path (Vite code-splits
	// this into the /music route chunk, loaded only when this page renders).
	import '$lib/styles/ported/music.css';
	import { useSelection } from '$lib/composables/useSelection.svelte';
	import { errorMessage, errorToast } from '$lib/utils/errors';
	import { onMount } from 'svelte';
	import { fileInlineUrl } from '$lib/api/endpoints/files';
	import {
		addTracks,
		createPlaylist,
		deletePlaylist,
		listPlaylists,
		listShares,
		listTracks,
		removeShare,
		removeTrack,
		renamePlaylist,
		reorderTracks,
		sharePlaylist,
		updatePlaylist,
		uploadCoverImage,
		type MusicShare,
		type Playlist,
		type PlaylistItem
	} from '$lib/api/endpoints/music';
	import { searchResources } from '$lib/api/endpoints/search';
	import type { FileItem } from '$lib/api/types';
	import Icon from '$lib/icons/Icon.svelte';
	import { confirmDialog, promptDialog } from '$lib/stores/dialogs.svelte';
	import { t } from '$lib/i18n/index.svelte';
	import { ui } from '$lib/stores/ui.svelte';

	let playlists = $state<Playlist[]>([]);
	let current = $state<Playlist | null>(null);
	let tracks = $state<PlaylistItem[]>([]);
	let loading = $state(false);
	let error = $state<string | null>(null);

	let dragIndex = $state<number | null>(null);

	// ── Player (independent global queue) ──────────────────────────────────────
	let audio = $state<HTMLAudioElement | null>(null);
	/** Playback queue — independent of the visible `tracks` list. */
	let queue = $state<PlaylistItem[]>([]);
	let currentIndex = $state(-1);
	let playing = $state(false);
	let currentTime = $state(0);
	let duration = $state(0);
	let volume = $state(0.7);
	let muted = $state(false);
	let shuffle = $state(false);
	let repeat = $state<'none' | 'all' | 'one'>('none');
	let queueOpen = $state(false);

	const currentTrack = $derived(currentIndex >= 0 ? (queue[currentIndex] ?? null) : null);

	function fmtTime(s: number | null | undefined): string {
		if (s == null || !Number.isFinite(s)) return '0:00';
		const m = Math.floor(s / 60);
		const sec = Math.floor(s % 60);
		return `${m}:${sec.toString().padStart(2, '0')}`;
	}
	function fmtDuration(s: number | null | undefined): string {
		return s ? fmtTime(s) : '-';
	}

	async function loadPlaylists() {
		loading = true;
		error = null;
		try {
			playlists = await listPlaylists();
			if (!current && playlists.length > 0) await select(playlists[0]);
		} catch (e) {
			error = errorMessage(e);
		} finally {
			loading = false;
		}
	}

	async function select(p: Playlist) {
		current = p;
		// NOTE: do NOT touch the player here — browsing a playlist must not stop playback.
		try {
			tracks = await listTracks(p.id);
		} catch (e) {
			errorToast(e);
		}
	}

	async function onCreate() {
		const name = await promptDialog({
			title: t('music.new_playlist', 'New playlist'),
			confirmText: t('common.create', 'Create')
		});
		if (!name) return;
		try {
			const p = await createPlaylist(name);
			playlists = [p, ...playlists];
			await select(p);
			ui.notify(t('music.created', { name: p.name }, 'Created “{{name}}”.'), 'success');
		} catch (e) {
			errorToast(e);
		}
	}

	async function onRenamePlaylist() {
		if (!current) return;
		const name = await promptDialog({
			title: t('music.rename_playlist', 'Rename playlist'),
			defaultValue: current.name,
			confirmText: t('common.save', 'Save')
		});
		if (!name || name === current.name) return;
		try {
			await renamePlaylist(current.id, name);
			current.name = name;
			playlists = playlists.map((p) => (p.id === current!.id ? { ...p, name } : p));
		} catch (e) {
			errorToast(e);
		}
	}

	async function onEditDescription() {
		if (!current) return;
		const desc = await promptDialog({
			title: t('music.edit_description', 'Edit description'),
			defaultValue: current.description ?? '',
			confirmText: t('common.save', 'Save')
		});
		if (desc === null) return;
		try {
			await updatePlaylist(current.id, { description: desc || null });
			current.description = desc || null;
			playlists = playlists.map((p) =>
				p.id === current!.id ? { ...p, description: desc || null } : p
			);
		} catch (e) {
			errorToast(e);
		}
	}

	async function onDelete(p: Playlist) {
		const ok = await confirmDialog({
			title: t('music.delete_playlist', 'Delete playlist'),
			message: t('music.confirm_delete', { name: p.name }, 'Delete playlist "{{name}}"?'),
			confirmText: t('common.delete', 'Delete'),
			danger: true
		});
		if (!ok) return;
		try {
			await deletePlaylist(p.id);
			playlists = playlists.filter((x) => x.id !== p.id);
			if (current?.id === p.id) {
				current = playlists[0] ?? null;
				tracks = current ? await listTracks(current.id) : [];
			}
			ui.notify(t('music.deleted', { name: p.name }, 'Deleted “{{name}}”.'), 'success');
		} catch (e) {
			errorToast(e);
		}
	}

	async function onRemoveTrack(track: PlaylistItem) {
		if (!current) return;
		try {
			await removeTrack(current.id, track.file_id);
			tracks = tracks.filter((x) => x.id !== track.id);
			ui.notify(t('music.track_removed', 'Track removed.'), 'success');
		} catch (e) {
			errorToast(e);
		}
	}

	async function onTogglePublic() {
		if (!current) return;
		const next = !current.is_public;
		try {
			await updatePlaylist(current.id, { is_public: next });
			current.is_public = next;
			playlists = playlists.map((p) => (p.id === current!.id ? { ...p, is_public: next } : p));
			ui.notify(
				next
					? t('music.now_public', 'Playlist is now public.')
					: t('music.now_private', 'Playlist is now private.'),
				'success'
			);
		} catch (e) {
			errorToast(e);
		}
	}

	function onDragStart(i: number) {
		dragIndex = i;
	}
	function onDragOver(e: DragEvent, i: number) {
		e.preventDefault();
		if (dragIndex === null || dragIndex === i) return;
		const next = [...tracks];
		const [moved] = next.splice(dragIndex, 1);
		next.splice(i, 0, moved);
		dragIndex = i;
		tracks = next;
	}
	async function onDrop() {
		dragIndex = null;
		if (!current) return;
		try {
			await reorderTracks(
				current.id,
				tracks.map((tr) => tr.id)
			);
			ui.notify(t('music.reordered', 'Playlist reordered.'), 'success');
		} catch (e) {
			errorToast(e);
			await select(current);
		}
	}

	function trackLabel(tr: PlaylistItem): string {
		return tr.title || tr.file_name || tr.file_id;
	}

	// ── Transport (operates on the independent queue) ──────────────────────────
	/** Replace the queue (e.g. when starting playback of the visible playlist). */
	function setQueue(list: PlaylistItem[]) {
		queue = [...list];
	}

	function playIndex(i: number) {
		if (i < 0 || i >= queue.length) return;
		currentIndex = i;
		// $effect swaps the src; ensure playback starts.
		queueMicrotask(() => audio?.play().catch(() => {}));
	}

	/** Play the visible playlist from a given row, seeding the queue from it. */
	function playFromTracks(i: number) {
		if (i < 0 || i >= tracks.length) return;
		setQueue(tracks);
		playIndex(i);
	}

	function playAll() {
		if (tracks.length) playFromTracks(0);
	}

	function shufflePlay() {
		if (!tracks.length) return;
		const shuffled = [...tracks];
		for (let i = shuffled.length - 1; i > 0; i--) {
			const j = Math.floor(Math.random() * (i + 1));
			[shuffled[i], shuffled[j]] = [shuffled[j], shuffled[i]];
		}
		setQueue(shuffled);
		playIndex(0);
	}

	function togglePlay() {
		if (!audio) return;
		if (currentIndex < 0 && queue.length) {
			playIndex(0);
			return;
		}
		if (playing) audio.pause();
		else audio.play().catch(() => {});
	}
	function next() {
		if (!queue.length) return;
		if (shuffle) {
			playIndex(Math.floor(Math.random() * queue.length));
			return;
		}
		if (currentIndex + 1 < queue.length) playIndex(currentIndex + 1);
		else if (repeat === 'all') playIndex(0);
	}
	function prev() {
		if (!queue.length) return;
		if (currentTime > 3 && audio) {
			audio.currentTime = 0;
			return;
		}
		// Wrap to the last track when at the start (OLD behavior).
		playIndex(currentIndex > 0 ? currentIndex - 1 : queue.length - 1);
	}
	function onEnded() {
		if (repeat === 'one') {
			if (audio) audio.currentTime = 0;
			audio?.play().catch(() => {});
			return;
		}
		next();
	}
	function seek(e: Event) {
		const v = Number((e.target as HTMLInputElement).value);
		if (audio) audio.currentTime = v;
	}
	function applyVolume() {
		if (audio) {
			audio.volume = volume;
			audio.muted = muted;
		}
	}
	function setVolume(e: Event) {
		volume = Number((e.target as HTMLInputElement).value);
		muted = volume === 0;
		applyVolume();
	}
	function toggleMute() {
		muted = !muted;
		applyVolume();
	}
	function cycleRepeat() {
		repeat = repeat === 'none' ? 'all' : repeat === 'all' ? 'one' : 'none';
	}

	const volumeIcon = $derived(muted || volume === 0 ? 'volume' : 'volume-up');

	function jumpQueue(i: number) {
		playIndex(i);
	}
	function removeFromQueue(i: number) {
		const next = [...queue];
		next.splice(i, 1);
		if (i === currentIndex) {
			queue = next;
			if (next.length === 0) {
				audio?.pause();
				currentIndex = -1;
			} else {
				playIndex(i >= next.length ? 0 : i);
			}
		} else {
			if (i < currentIndex) currentIndex -= 1;
			queue = next;
		}
	}

	/** Live duration backfill: write the real duration into rows/queue lacking it. */
	function onLoadedMetadata() {
		duration = audio?.duration ?? 0;
		const tr = currentTrack;
		if (tr && audio?.duration && !tr.duration_secs) {
			const secs = Math.round(audio.duration);
			tr.duration_secs = secs;
			queue = queue.map((q) => (q.id === tr.id ? { ...q, duration_secs: secs } : q));
			tracks = tracks.map((q) => (q.id === tr.id ? { ...q, duration_secs: secs } : q));
		}
	}

	function onAudioError() {
		if (!currentTrack) return;
		ui.notify(
			t('music.playback_error', { name: trackLabel(currentTrack) }, 'Playback error: {{name}}'),
			'error'
		);
		playing = false;
	}

	// Keep the <audio> src in sync with the current queued track.
	$effect(() => {
		if (audio && currentTrack) {
			const url = fileInlineUrl(currentTrack.file_id);
			if (audio.getAttribute('src') !== url) audio.src = url;
		}
	});

	// ── Cover art picker ───────────────────────────────────────────────────────
	let coverInput = $state<HTMLInputElement | null>(null);

	function pickCover() {
		coverInput?.click();
	}
	async function onCoverChosen(e: Event) {
		const input = e.target as HTMLInputElement;
		const file = input.files?.[0];
		input.value = '';
		if (!file || !current) return;
		try {
			const fileId = await uploadCoverImage(file);
			await updatePlaylist(current.id, { cover_file_id: fileId });
			current.cover_file_id = fileId;
			playlists = playlists.map((p) =>
				p.id === current!.id ? { ...p, cover_file_id: fileId } : p
			);
			ui.notify(t('music.cover_updated', 'Cover updated.'), 'success');
		} catch (err) {
			errorToast(err);
		}
	}

	function coverUrl(p: Playlist | null): string | null {
		return p?.cover_file_id ? `/api/files/${encodeURIComponent(p.cover_file_id)}` : null;
	}

	// ── Share dialog ───────────────────────────────────────────────────────────
	let sharesOpen = $state(false);
	let shares = $state<MusicShare[]>([]);
	let shareUser = $state('');
	let shareCanWrite = $state(false);
	let sharesLoading = $state(false);

	async function openShares() {
		if (!current) return;
		sharesOpen = true;
		await loadShares();
	}
	async function loadShares() {
		if (!current) return;
		sharesLoading = true;
		try {
			shares = await listShares(current.id);
		} catch (e) {
			errorToast(e);
		} finally {
			sharesLoading = false;
		}
	}
	async function onAddShare() {
		if (!current || !shareUser.trim()) return;
		try {
			await sharePlaylist(current.id, shareUser.trim(), shareCanWrite);
			shareUser = '';
			shareCanWrite = false;
			await loadShares();
			ui.notify(t('music.share_added', 'Shared.'), 'success');
		} catch (e) {
			errorToast(e);
		}
	}
	async function onRemoveShare(userId: string) {
		if (!current) return;
		try {
			await removeShare(current.id, userId);
			await loadShares();
		} catch (e) {
			errorToast(e);
		}
	}

	// ── Add tracks dialog ──────────────────────────────────────────────────────
	const AUDIO_TYPES = ['mp3', 'ogg', 'flac', 'wav', 'aac', 'm4a', 'wma', 'opus', 'webm'];
	let addOpen = $state(false);
	let addQuery = $state('');
	let addResults = $state<FileItem[]>([]);
	const addSelected = useSelection();
	let addSearching = $state(false);
	let addDebounce: ReturnType<typeof setTimeout> | null = null;

	async function runAddSearch(query = '') {
		addSearching = true;
		try {
			const res = await searchResources(query.trim(), {
				recursive: true,
				fileTypes: AUDIO_TYPES,
				resourceTypes: ['file'],
				limit: 200
			});
			// Belt-and-braces: keep only audio mime types. The envelope
			// items are `{resource_type, resource}` — resourceTypes:['file']
			// already restricts to files, but re-narrow here so the
			// downstream `FileItem[]` cast is honest even if the wire
			// ordering ever surfaces a folder.
			addResults = res.items
				.filter((it) => it.resource_type === 'file')
				.map((it) => it.resource as FileItem)
				.filter(
					(f) =>
						(f.mime_type ?? '').startsWith('audio/') ||
						AUDIO_TYPES.some((e) => f.name.toLowerCase().endsWith(`.${e}`))
				);
		} catch (e) {
			errorToast(e);
			addResults = [];
		} finally {
			addSearching = false;
		}
	}
	function onAddQueryInput() {
		if (addDebounce) clearTimeout(addDebounce);
		addDebounce = setTimeout(() => runAddSearch(addQuery), 300);
	}
	function openAdd() {
		addOpen = true;
		addQuery = '';
		addResults = [];
		addSelected.clear();
		void runAddSearch(''); // show all audio files immediately
	}
	async function confirmAdd() {
		if (!current || addSelected.size === 0) return;
		const count = addSelected.size;
		try {
			await addTracks(current.id, addSelected.values());
			addOpen = false;
			tracks = await listTracks(current.id);
			ui.notify(t('music.tracks_added', { n: count }, 'Added {{n}} track(s).'), 'success');
		} catch (e) {
			errorToast(e);
		}
	}

	onMount(loadPlaylists);
</script>

<svelte:head><title>{t('nav.music', 'Music')} · OxiCloud</title></svelte:head>

<div class="music-container active">
	{#if error}
		<div class="music-error">
			<Icon name="exclamation-circle" />
			<p>{error}</p>
		</div>
	{:else if loading && playlists.length === 0}
		<div class="music-loading"><Icon name="spinner" /></div>
	{:else if playlists.length === 0}
		<div class="music-empty-state">
			<div class="music-empty-state-icon"><Icon name="music" /></div>
			<h3 class="music-empty-state-title">{t('music.no_playlists', 'No playlists yet.')}</h3>
			<p class="music-empty-state-desc">
				{t('music.empty_hint', 'Create a playlist to start collecting your tracks.')}
			</p>
			<button
				class="btn btn-primary"
				data-testid="music-create-playlist-empty-btn"
				onclick={onCreate}
			>
				<Icon name="plus" />
				<span>{t('music.create_playlist', 'Create playlist')}</span>
			</button>
		</div>
	{:else}
		<div class="music-content">
			<div class="music-sidebar">
				<div class="music-sidebar-header">
					<h3>{t('music.playlists', 'Playlists')}</h3>
					<button
						class="music-sidebar-add-btn"
						title={t('music.create_playlist', 'Create playlist')}
						aria-label={t('music.create_playlist', 'Create playlist')}
						data-testid="music-create-playlist-btn"
						onclick={onCreate}
					>
						<Icon name="plus" />
					</button>
				</div>
				<div class="music-playlist-list">
					{#each playlists as p (p.id)}
						<!-- svelte-ignore a11y_click_events_have_key_events -->
						<!-- svelte-ignore a11y_no_static_element_interactions -->
						<div
							class="music-playlist-item"
							class:active={current?.id === p.id}
							data-testid={p.name}
							onclick={() => select(p)}
						>
							<div class="music-playlist-icon"><Icon name="music" /></div>
							<div class="music-playlist-item-info">
								<span class="music-playlist-item-name">{p.name}</span>
								<span class="music-playlist-item-count"
									>{p.track_count}
									{t('music.tracks', 'tracks')}</span
								>
							</div>
						</div>
					{/each}
				</div>
			</div>

			<div class="music-main">
				{#if current}
					<div class="music-playlist-detail">
						<div class="music-playlist-header">
							<button
								class="music-playlist-cover"
								title={t('music.set_cover', 'Set cover')}
								aria-label={t('music.set_cover', 'Set cover')}
								data-testid="music-set-cover-btn"
								onclick={pickCover}
							>
								{#if coverUrl(current)}
									<img class="music-cover-img" src={coverUrl(current)} alt="" />
								{:else}
									<Icon name="music" />
								{/if}
								<div class="music-cover-overlay"><Icon name="camera" /></div>
							</button>
							<div class="music-playlist-info">
								<h2>{current.name}</h2>
								<p>
									{t('music.track_count', { n: current.track_count }, '{{n}} tracks')}
									{#if current.description}· {current.description}{/if}
								</p>
								{#if current.is_public}
									<span class="music-public-badge">
										<Icon name="globe" /> <span>{t('music.public', 'Public')}</span>
									</span>
								{/if}
							</div>
						</div>

						<div class="music-playlist-actions">
							<button
								class="btn btn-secondary"
								data-testid="music-play-all-btn"
								onclick={playAll}
								disabled={tracks.length === 0}
							>
								<Icon name="play" />
								<span>{t('music.play_all', 'Play all')}</span>
							</button>
							<button
								class="btn btn-secondary"
								data-testid="music-shuffle-play-btn"
								onclick={shufflePlay}
								disabled={tracks.length === 0}
								title={t('music.shuffle', 'Shuffle')}
								aria-label={t('music.shuffle', 'Shuffle')}
							>
								<Icon name="shuffle" />
							</button>
							<button
								class="btn btn-secondary"
								data-testid="music-add-tracks-btn"
								onclick={openAdd}
							>
								<Icon name="plus" />
								<span>{t('music.add_tracks', 'Add tracks')}</span>
							</button>
							<button
								class="btn btn-secondary"
								data-testid="music-rename-playlist-btn"
								onclick={onRenamePlaylist}
								title={t('common.rename', 'Rename')}
								aria-label={t('common.rename', 'Rename')}
							>
								<Icon name="pen" />
							</button>
							<button
								class="btn btn-secondary"
								data-testid="music-edit-description-btn"
								onclick={onEditDescription}
								title={t('music.edit_description', 'Edit description')}
								aria-label={t('music.edit_description', 'Edit description')}
							>
								<Icon name="pencil-alt" />
							</button>
							<button
								class="btn btn-secondary"
								data-testid="music-manage-shares-btn"
								onclick={openShares}
								title={t('music.manage_shares', 'Manage shares')}
								aria-label={t('music.manage_shares', 'Manage shares')}
							>
								<Icon name="users" />
							</button>
							<button
								class="btn btn-secondary"
								class:active={current.is_public}
								data-testid="music-toggle-public-btn"
								onclick={onTogglePublic}
								title={current.is_public
									? t('music.make_private', 'Make private')
									: t('music.make_public', 'Make public')}
								aria-label={current.is_public
									? t('music.make_private', 'Make private')
									: t('music.make_public', 'Make public')}
							>
								<Icon name="globe" />
							</button>
							<button
								class="btn btn-secondary"
								data-testid="music-delete-playlist-btn"
								onclick={() => onDelete(current!)}
								title={t('common.delete', 'Delete')}
								aria-label={t('common.delete', 'Delete')}
							>
								<Icon name="trash" />
							</button>
						</div>

						<div class="music-track-list">
							{#if tracks.length === 0}
								<div class="music-empty">
									<Icon name="music" />
									<p>{t('music.empty_playlist', 'This playlist has no tracks yet.')}</p>
								</div>
							{:else}
								<div class="music-track-header">
									<span class="music-track-col music-track-drag"></span>
									<span class="music-track-col music-track-num">#</span>
									<span class="music-track-col music-track-title">{t('music.title', 'Title')}</span>
									<span class="music-track-col music-track-artist"
										>{t('music.artist', 'Artist')}</span
									>
									<span class="music-track-col music-track-album">{t('music.album', 'Album')}</span>
									<span class="music-track-col music-track-duration"><Icon name="clock" /></span>
									<span class="music-track-col music-track-actions"></span>
								</div>
								{#each tracks as track, i (track.id)}
									<!-- svelte-ignore a11y_click_events_have_key_events -->
									<!-- svelte-ignore a11y_no_static_element_interactions -->
									<div
										class="music-track"
										class:playing={currentTrack?.id === track.id && playing}
										data-testid={trackLabel(track)}
										draggable="true"
										ondblclick={() => playFromTracks(i)}
										ondragstart={() => onDragStart(i)}
										ondragover={(e) => onDragOver(e, i)}
										ondrop={onDrop}
										ondragend={() => (dragIndex = null)}
									>
										<span class="music-track-col music-track-drag" aria-hidden="true">
											<Icon name="grip-vertical" />
										</span>
										<span class="music-track-col music-track-num">
											<!-- svelte-ignore a11y_click_events_have_key_events -->
											<!-- svelte-ignore a11y_no_static_element_interactions -->
											<span
												data-testid={`music-track-play-${track.id}`}
												onclick={(e) => {
													e.stopPropagation();
													if (currentTrack?.id === track.id) togglePlay();
													else playFromTracks(i);
												}}
											>
												{#if currentTrack?.id === track.id}
													<Icon name={playing ? 'pause' : 'play'} />
												{:else}
													<span class="track-num-text">{i + 1}</span>
												{/if}
											</span>
										</span>
										<span class="music-track-col music-track-title">
											<Icon name="music" class="music-track-icon" />
											<span class="music-track-name">{trackLabel(track)}</span>
										</span>
										<span class="music-track-col music-track-artist">{track.artist || '—'}</span>
										<span class="music-track-col music-track-album">{track.album || '-'}</span>
										<span class="music-track-col music-track-duration"
											>{fmtDuration(track.duration_secs)}</span
										>
										<span class="music-track-col music-track-actions">
											<button
												class="music-track-remove-btn"
												title={t('common.remove', 'Remove')}
												aria-label={t('common.remove', 'Remove')}
												data-testid={`music-track-remove-${track.id}`}
												onclick={(e) => {
													e.stopPropagation();
													onRemoveTrack(track);
												}}
											>
												<Icon name="times" />
											</button>
										</span>
									</div>
								{/each}
							{/if}
						</div>
					</div>
				{:else}
					<div class="music-welcome">
						<Icon name="music" />
						<h3>{t('music.select_playlist', 'Select a playlist.')}</h3>
						<p>{t('music.select_hint', 'Pick a playlist to view its tracks.')}</p>
					</div>
				{/if}
			</div>
		</div>
	{/if}
</div>

<!-- Now-playing bar (persists while browsing) -->
{#if currentTrack}
	<div class="music-player has-track">
		<div class="player-track-info">
			<div class="player-album-art">
				{#if coverUrl(current)}
					<img src={coverUrl(current)} alt="" />
				{:else}
					<Icon name="music" />
				{/if}
			</div>
			<div class="player-track-details">
				<span class="player-track-name">{trackLabel(currentTrack)}</span>
				<span class="player-track-artist">{currentTrack.artist ?? ''}</span>
			</div>
		</div>

		<div class="player-controls">
			<div class="player-buttons">
				<button
					class="player-btn"
					class:active={shuffle}
					title={t('music.shuffle', 'Shuffle')}
					aria-label={t('music.shuffle', 'Shuffle')}
					data-testid="music-player-shuffle-btn"
					onclick={() => (shuffle = !shuffle)}
				>
					<Icon name="shuffle" />
				</button>
				<button
					class="player-btn"
					title={t('music.prev', 'Previous')}
					aria-label={t('music.prev', 'Previous')}
					data-testid="music-player-prev-btn"
					onclick={prev}
				>
					<Icon name="backward" />
				</button>
				<button
					class="player-btn player-btn-main"
					title={t('music.play', 'Play')}
					aria-label={t('music.play', 'Play')}
					data-testid="music-player-play-btn"
					onclick={togglePlay}
				>
					<Icon name={playing ? 'pause' : 'play'} />
				</button>
				<button
					class="player-btn"
					title={t('music.next', 'Next')}
					aria-label={t('music.next', 'Next')}
					data-testid="music-player-next-btn"
					onclick={next}
				>
					<Icon name="forward" />
				</button>
				<button
					class="player-btn"
					class:active={repeat !== 'none'}
					class:repeat-one={repeat === 'one'}
					title={t('music.repeat', 'Repeat')}
					aria-label={t('music.repeat', 'Repeat')}
					data-testid="music-player-repeat-btn"
					onclick={cycleRepeat}
				>
					<Icon name="repeat" />
					{#if repeat === 'one'}<span class="repeat-one-badge">1</span>{/if}
				</button>
			</div>
			<div class="player-progress">
				<span class="player-time player-time-current">{fmtTime(currentTime)}</span>
				<input
					class="player-progress-range"
					type="range"
					min="0"
					max={duration || 0}
					value={currentTime}
					oninput={seek}
					aria-label={t('music.seek', 'Seek')}
					data-testid="music-player-seek-input"
				/>
				<span class="player-time player-time-total">{fmtTime(duration)}</span>
			</div>
		</div>

		<div class="player-extra">
			<button
				class="player-btn player-btn-small"
				class:active={queueOpen}
				title={t('music.queue', 'Queue')}
				aria-label={t('music.queue', 'Queue')}
				data-testid="music-player-queue-toggle-btn"
				onclick={() => (queueOpen = !queueOpen)}
			>
				<Icon name="list" />
			</button>
			<button
				class="player-btn player-btn-small"
				title={t('music.mute', 'Mute')}
				aria-label={t('music.mute', 'Mute')}
				data-testid="music-player-mute-btn"
				onclick={toggleMute}
			>
				<Icon name={volumeIcon} />
			</button>
			<div class="player-volume-slider">
				<input
					id="player-volume-input"
					type="range"
					min="0"
					max="1"
					step="0.05"
					value={muted ? 0 : volume}
					oninput={setVolume}
					aria-label={t('music.volume', 'Volume')}
					data-testid="music-player-volume-input"
				/>
			</div>
		</div>
	</div>

	{#if queueOpen}
		<div class="player-queue" data-testid="music-queue-panel">
			<div class="player-queue-header">
				<h3>{t('music.queue', 'Queue')}</h3>
				<button
					class="player-btn player-btn-small"
					data-testid="music-queue-close-btn"
					onclick={() => (queueOpen = false)}
					aria-label={t('common.close', 'Close')}
				>
					<Icon name="times" />
				</button>
			</div>
			<div class="player-queue-list">
				{#if queue.length === 0}
					<div class="player-queue-empty">
						<Icon name="music" />
						<p>{t('music.queue_empty', 'Queue is empty.')}</p>
					</div>
				{:else}
					{#each queue as qt, i (qt.id)}
						<!-- svelte-ignore a11y_click_events_have_key_events -->
						<!-- svelte-ignore a11y_no_static_element_interactions -->
						<div
							class="player-queue-item"
							class:active={i === currentIndex}
							data-testid={trackLabel(qt)}
							onclick={() => jumpQueue(i)}
						>
							<span class="queue-item-num">{i + 1}</span>
							<span class="queue-item-info">
								<span class="queue-item-name">{trackLabel(qt)}</span>
								<span class="queue-item-artist">{qt.artist ?? ''}</span>
							</span>
							<span class="queue-item-duration">{fmtDuration(qt.duration_secs)}</span>
							<button
								class="queue-item-remove"
								aria-label={t('common.remove', 'Remove')}
								data-testid={`music-queue-remove-${qt.id}`}
								onclick={(e) => {
									e.stopPropagation();
									removeFromQueue(i);
								}}
							>
								<Icon name="times" />
							</button>
						</div>
					{/each}
				{/if}
			</div>
		</div>
	{/if}
{/if}

<audio
	bind:this={audio}
	onplay={() => (playing = true)}
	onpause={() => (playing = false)}
	ontimeupdate={() => (currentTime = audio?.currentTime ?? 0)}
	onloadedmetadata={onLoadedMetadata}
	onended={onEnded}
	onerror={onAudioError}
></audio>

<input
	bind:this={coverInput}
	type="file"
	accept="image/*"
	class="hidden-input"
	data-testid="music-cover-input"
	onchange={onCoverChosen}
/>

{#if addOpen}
	<!-- svelte-ignore a11y_click_events_have_key_events -->
	<!-- svelte-ignore a11y_no_static_element_interactions -->
	<div
		class="music-picker-overlay active"
		data-testid="music-add-tracks-dialog"
		onclick={(e) => {
			if (e.target === e.currentTarget) addOpen = false;
		}}
	>
		<div class="music-picker-modal">
			<div class="music-picker-header">
				<h3><Icon name="music" /> {t('music.add_tracks', 'Add tracks')}</h3>
				<button
					class="music-picker-close"
					aria-label={t('common.close', 'Close')}
					data-testid="music-add-tracks-close-btn"
					onclick={() => (addOpen = false)}>&times;</button
				>
			</div>
			<div class="music-picker-search">
				<Icon name="search" />
				<!-- svelte-ignore a11y_autofocus -->
				<input
					type="text"
					placeholder={t('music.search_audio', 'Search audio files…')}
					bind:value={addQuery}
					oninput={onAddQueryInput}
					autocomplete="off"
					autofocus
					data-testid="music-add-tracks-search-input"
				/>
			</div>
			<div class="music-picker-list">
				{#if addSearching}
					<div class="music-picker-loading">
						<Icon name="spinner" />
						{t('common.loading', 'Loading…')}
					</div>
				{:else if addResults.length === 0}
					<div class="music-picker-empty">
						<Icon name="folder-open" />
						{t('music.no_audio', 'No audio files found.')}
					</div>
				{:else}
					{#each addResults as f (f.id)}
						<label class="music-picker-item" class:selected={addSelected.has(f.id)}>
							<input
								type="checkbox"
								checked={addSelected.has(f.id)}
								onchange={() => addSelected.toggle(f.id)}
								data-testid={f.name}
							/>
							<Icon name="file-audio" />
							<span class="music-picker-name" title={f.name}>{f.name}</span>
						</label>
					{/each}
				{/if}
			</div>
			<div class="music-picker-footer">
				<span class="music-picker-selected-count">
					{t('music.selected_count', { n: addSelected.size }, '{{n}} selected')}
				</span>
				<div class="music-picker-actions">
					<button
						class="btn btn-secondary"
						data-testid="music-add-tracks-cancel-btn"
						onclick={() => (addOpen = false)}
					>
						{t('common.cancel', 'Cancel')}
					</button>
					<button
						class="btn btn-primary"
						disabled={addSelected.size === 0}
						data-testid="music-add-tracks-confirm-btn"
						onclick={confirmAdd}
					>
						<Icon name="plus" />
						{t('music.add_selected', 'Add selected')}
					</button>
				</div>
			</div>
		</div>
	</div>
{/if}

{#if sharesOpen}
	<!-- svelte-ignore a11y_click_events_have_key_events -->
	<!-- svelte-ignore a11y_no_static_element_interactions -->
	<div
		class="music-shares-overlay"
		data-testid="music-shares-dialog"
		onclick={(e) => {
			if (e.target === e.currentTarget) sharesOpen = false;
		}}
	>
		<div class="music-shares-panel">
			<div class="music-shares-header">
				<h3><Icon name="users" /> {t('music.manage_shares', 'Manage shares')}</h3>
				<button
					class="music-shares-close-btn"
					aria-label={t('common.close', 'Close')}
					data-testid="music-shares-close-btn"
					onclick={() => (sharesOpen = false)}
				>
					<Icon name="times" />
				</button>
			</div>
			<div class="music-shares-body">
				{#if sharesLoading}
					<div class="music-shares-loading"><Icon name="spinner" /></div>
				{:else if shares.length === 0}
					<p class="music-shares-empty">{t('music.no_shares', 'Not shared with anyone yet.')}</p>
				{:else}
					{#each shares as s (s.user_id)}
						<div class="music-share-item">
							<span class="music-share-user"><Icon name="user" /> {s.user_id}</span>
							<span class="music-share-perm">
								{s.can_write ? t('music.can_write', 'Can edit') : t('music.read_only', 'Read only')}
							</span>
							<button
								class="music-share-remove-btn"
								title={t('music.remove_share', 'Remove')}
								aria-label={t('music.remove_share', 'Remove')}
								data-testid={`music-share-remove-${s.user_id}`}
								onclick={() => onRemoveShare(s.user_id)}
							>
								<Icon name="times" />
							</button>
						</div>
					{/each}
				{/if}
			</div>
			<div class="music-shares-add">
				<input
					type="text"
					class="music-shares-input"
					placeholder={t('music.share_with_user', 'User ID or email')}
					bind:value={shareUser}
					autocomplete="off"
					data-testid="music-share-user-input"
				/>
				<label class="music-shares-write-label">
					<input
						type="checkbox"
						bind:checked={shareCanWrite}
						data-testid="music-share-can-write-checkbox"
					/>
					{t('music.can_write', 'Can edit')}
				</label>
				<button
					class="btn btn-primary btn-sm"
					disabled={!shareUser.trim()}
					data-testid="music-share-add-btn"
					onclick={onAddShare}
				>
					<Icon name="plus" />
					{t('music.share', 'Share')}
				</button>
			</div>
		</div>
	</div>
{/if}

<style>
	/* Almost all visuals come from the ported music.css (global, token-based).
	   Only the off-screen file input and player spacing live here. */
	.hidden-input {
		display: none;
	}

	/* Leave room above the OS chrome for the fixed player bar so the last
	   tracks/sidebar content isn't hidden behind it while playing. */
	:global(body:has(.music-player.has-track)) .music-container {
		padding-bottom: 90px;
	}
</style>
