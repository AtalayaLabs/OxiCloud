<script lang="ts">
	// Route-scoped styles: kept off the global critical path (Vite code-splits
	// this into the /music route chunk, loaded only when this page renders).
	import '$lib/styles/ported/music.css';
	import { useSelection } from '$lib/composables/useSelection.svelte';
	import { errorMessage, errorToast } from '$lib/utils/errors';
	import { onMount } from 'svelte';
	import { player } from '$lib/stores/player.svelte';
	import {
		addTracks,
		createPlaylist,
		deletePlaylist,
		listPlaylists,
		listShares,
		listTracks,
		removeShare,
		removeTrack,
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
	import EmptyState from '$lib/components/EmptyState.svelte';
	import { formDialog, promptDialog } from '$lib/stores/dialogs.svelte';
	import ConfirmButton from '$lib/components/ConfirmButton.svelte';
	import { t } from '$lib/i18n/index.svelte';
	import { ui } from '$lib/stores/ui.svelte';

	let playlists = $state<Playlist[]>([]);
	let current = $state<Playlist | null>(null);
	let tracks = $state<PlaylistItem[]>([]);
	let loading = $state(false);
	let error = $state<string | null>(null);

	let dragIndex = $state<number | null>(null);

	// ── Player ────────────────────────────────────────────────────────────────
	// State and the `<audio>` element itself moved to `$lib/stores/player` and
	// `MusicPlayer`, which `AppShell` renders above the router. They lived here
	// until now, which is why leaving this page cut playback off mid-track.
	// This page is a CONSUMER: it fills the queue and reflects what is playing.

	/** A duration discovered by the player for a row we show as 0:00. */
	function onTrackDuration(e: Event) {
		const { id, seconds } = (e as CustomEvent<{ id: string; seconds: number }>).detail;
		tracks = tracks.map((q) => (q.id === id ? { ...q, duration_secs: seconds } : q));
	}

	$effect(() => {
		window.addEventListener('oxicloud:track-duration', onTrackDuration);
		return () => window.removeEventListener('oxicloud:track-duration', onTrackDuration);
	});

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
		await promptDialog({
			title: t('music.new_playlist', 'New playlist'),
			confirmText: t('common.create', 'Create'),
			// Inline-error UX — see `frontend/AGENTS.md § Dialog error UX`.
			action: async (name) => {
				if (!name) return;
				const p = await createPlaylist(name);
				playlists = [p, ...playlists];
				await select(p);
				ui.notify(t('music.created', { name: p.name }, 'Created “{{name}}”.'), 'success');
			}
		});
	}

	async function openEdit() {
		if (!current) return;
		const playlist = current;
		const values = await formDialog({
			title: t('music.edit_playlist', 'Edit playlist'),
			fields: [
				{
					key: 'name',
					label: t('music.playlist_name', 'Playlist name'),
					value: playlist.name
				},
				{
					key: 'description',
					label: t('music.description', 'Description'),
					value: playlist.description ?? '',
					multiline: true
				}
			],
			confirmText: t('common.save', 'Save')
		});
		if (!values) return;
		const name = values.name.trim();
		if (!name) return;
		const description = values.description.trim() || null;
		if (name === playlist.name && description === playlist.description) return;
		try {
			await updatePlaylist(playlist.id, { name, description });
			current.name = name;
			current.description = description;
			playlists = playlists.map((p) => (p.id === current!.id ? { ...p, name, description } : p));
		} catch (e) {
			errorToast(e);
		}
	}

	async function onDelete(p: Playlist) {
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

	// ── Starting playback ─────────────────────────────────────────────────────
	// Every one of these hands the queue to the player and stops caring. The
	// cover id goes WITH the queue rather than being read later from the
	// selected playlist: the queue outlives the selection, so reading it later
	// would show the wrong art as soon as you browse elsewhere.

	/** Play the visible playlist from a given row, seeding the queue from it. */
	function playFromTracks(i: number) {
		if (i < 0 || i >= tracks.length) return;
		player.play(tracks, i, current?.cover_file_id ?? null);
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
		player.play(shuffled, 0, current?.cover_file_id ?? null);
	}

	/** Row play button: toggle if it is already the current track, else start it. */
	function toggleRow(i: number) {
		if (player.current?.id === tracks[i]?.id) player.toggle();
		else playFromTracks(i);
	}

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
				// Was `fileTypes: AUDIO_TYPES` — a THIRD copy of the audio
				// extension table (the search page had its own, the server has
				// `category_for`), and the three disagreed: this list carried
				// `wma`/`webm` that search's did not. The server now narrows on
				// `mime_type LIKE 'audio/%'` via the indexed `category_order`,
				// so there is nothing here to keep in sync.
				typeFilter: 'audio',
				resourceTypes: ['file'],
				limit: 200
			});
			// The `resource_type === 'file'` filter stays — it is what makes
			// the `FileItem` cast below honest even if the wire ever surfaces
			// a folder.
			//
			// The audio re-narrow that followed it is GONE. It tested
			// `mime_type.startsWith('audio/') || AUDIO_TYPES.includes(ext)`,
			// and with `typeFilter: 'audio'` the server already guarantees
			// `category_order = 300`. Worse, keeping only the mime half would
			// have been a regression: the server's classifier falls back to
			// the extension when the mime is generic, so a `.flac` stored as
			// `application/octet-stream` is legitimately audio and a mime-only
			// client filter would have dropped it.
			addResults = res.items
				.filter((it) => it.resource_type === 'file')
				.map((it) => it.resource as FileItem);
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
		<!-- `EmptyState` rather than a local `.music-error` block: it was a
		     second copy of icon-over-message, and the copy that stayed on
		     `--color-danger-text` (white in both themes) is how error states
		     ended up invisible in light mode. One class, one token. -->
		<EmptyState icon="exclamation-circle" title={error} error />
	{:else if loading && playlists.length === 0}
		<div class="music-loading"><Icon name="spinner" /></div>
	{:else if playlists.length === 0}
		<EmptyState
			icon="music"
			title={t('music.no_playlists', 'No playlists yet.')}
			hint={t('music.empty_hint', 'Create a playlist to start collecting your tracks.')}
		>
			<button
				class="btn btn-primary"
				data-testid="music-create-playlist-empty-btn"
				onclick={onCreate}
				aria-label={t('music.create_playlist', 'Create playlist')}
				title={t('music.create_playlist', 'Create playlist')}
			>
				<Icon name="plus" />
			</button>
		</EmptyState>
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
						<!--
							A FIXED testid plus the name in its own hook, rather than
							`data-testid={p.name}`. Keying the hook on user data meant a
							test looking for a playlist it had just renamed got
							"element(s) not found" when the name came out wrong — the
							locator and the thing under test were the same string, so
							the failure could not say what the name actually was.
							Specs now filter rows by text and assert the name
							separately, which reports "expected X, received Y".
						-->
						<div
							class="music-playlist-item"
							class:active={current?.id === p.id}
							data-testid="music-playlist-item"
							data-playlist-id={p.id}
							onclick={() => select(p)}
						>
							<div class="music-playlist-icon"><Icon name="music" /></div>
							<div class="music-playlist-item-info">
								<span class="music-playlist-item-name" data-testid="music-playlist-name"
									>{p.name}</span
								>
								{#if p.track_count != null}
									<span class="music-playlist-item-count"
										>{p.track_count}
										{t('music.tracks', 'tracks')}</span
									>
								{/if}
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
									{t('music.track_count', { n: tracks.length }, '{{n}} tracks')}
									{#if current.description}· {current.description}{/if}
								</p>
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
								title={t('music.add_tracks', 'Add tracks')}
								aria-label={t('music.add_tracks', 'Add tracks')}
							>
								<Icon name="plus" />
							</button>
							<button
								class="btn btn-secondary"
								data-testid="music-rename-playlist-btn"
								onclick={openEdit}
								title={t('music.edit_playlist', 'Edit playlist')}
								aria-label={t('music.edit_playlist', 'Edit playlist')}
							>
								<Icon name="pen" />
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
							<ConfirmButton
								icon="trash"
								class="btn btn-secondary"
								data-testid="music-delete-playlist-btn"
								onconfirm={() => onDelete(current!)}
								title={t('common.delete', 'Delete')}
							/>
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
										class:playing={player.current?.id === track.id && player.playing}
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
													toggleRow(i);
												}}
											>
												{#if player.current?.id === track.id}
													<Icon name={player.playing ? 'pause' : 'play'} />
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

<!--
	The now-playing bar and its queue drawer lived here, with the <audio>
	element. They are `MusicPlayer` in the top bar now, rendered by `AppShell`
	above the router: playback no longer stops when you leave this page, and
	there is one player rather than one per section.
-->

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
	   Only the off-screen file input lives here — the rule that padded the
	   container for the fixed player bar went with the bar itself, and was
	   already dead: nothing renders `.music-player` any more. */
	.hidden-input {
		display: none;
	}
</style>
