<script lang="ts">
	import { resolve } from '$app/paths';
	import GestureList from '$lib/components/GestureList.svelte';
	import Icon from '$lib/icons/Icon.svelte';
	import { t } from '$lib/i18n/index.svelte';
	import { player } from '$lib/stores/player.svelte';

	/**
	 * The player's controls: what is playing, the transport, the progress and
	 * volume bars, and the queue.
	 *
	 * Placement is the caller's business. `MusicPlayer` renders this inside
	 * the popover hanging off the top bar; `/player` renders it as a page,
	 * which is how the mobile footer reaches it — same as Notifications and
	 * Account, and the reason a route rather than an overlay: a full-screen
	 * overlay has no history entry, so Android's back gesture navigates the
	 * page behind it instead of dismissing it.
	 *
	 * It owns no playback state. The `<audio>` element and everything about
	 * it live in `MusicPlayer` / `$lib/stores/player`, so both placements
	 * drive one player rather than two.
	 */
	interface Props {
		/**
		 * Called after an action that should dismiss a transient container —
		 * clearing the queue, or following the "browse music" link. The popover
		 * closes itself with it; the page passes nothing, because a page has
		 * nothing to close.
		 */
		onclose?: () => void;
	}
	let { onclose }: Props = $props();

	function fmtTime(s: number | null | undefined): string {
		if (s == null || !Number.isFinite(s)) return '0:00';
		const m = Math.floor(s / 60);
		const sec = Math.floor(s % 60);
		return `${m}:${String(sec).padStart(2, '0')}`;
	}

	const volumeIcon = $derived(player.muted || player.volume === 0 ? 'volume' : 'volume-up');
</script>

{#if player.current}
	<div class="player-head">
		<div class="player-art">
			{#if player.coverUrl}
				<img src={player.coverUrl} alt="" />
			{:else}
				<Icon name="music" />
			{/if}
		</div>
		<div class="player-meta">
			<span class="player-title" data-testid="music-player-title">{player.label()}</span>
			<span class="player-artist">{player.current.artist ?? ''}</span>

			<!--
				Transport sits BESIDE the cover rather than on its own row: the
				cover is 96px tall and the title/artist only fill two thirds of
				it, so the buttons ride in space that was empty. Buys ~44px of
				queue without shrinking anything.
			-->
			<div class="player-transport">
				<button
					class="player-btn"
					title={t('music.prev', 'Previous')}
					aria-label={t('music.prev', 'Previous')}
					data-testid="music-player-prev-btn"
					onclick={() => player.prev()}
				>
					<Icon name="backward" />
				</button>
				<button
					class="player-btn player-btn--main"
					title={player.playing ? t('music.pause', 'Pause') : t('music.play', 'Play')}
					aria-label={player.playing ? t('music.pause', 'Pause') : t('music.play', 'Play')}
					data-testid="music-player-play-btn"
					onclick={() => player.toggle()}
				>
					<Icon name={player.playing ? 'pause' : 'play'} />
				</button>
				<button
					class="player-btn"
					title={t('music.next', 'Next')}
					aria-label={t('music.next', 'Next')}
					data-testid="music-player-next-btn"
					onclick={() => player.next()}
				>
					<Icon name="forward" />
				</button>
			</div>
		</div>

		<!--
			Volume stands vertically alongside the head rather than taking a row
			of its own: the head is already 96px tall, so the slider costs
			nothing but ~32px of width.
		-->
		<div class="player-volume">
			<input
				type="range"
				min="0"
				max="1"
				step="0.05"
				value={player.muted ? 0 : player.volume}
				oninput={(e) => player.setVolume(Number(e.currentTarget.value))}
				aria-label={t('music.volume', 'Volume')}
				data-testid="music-player-volume-input"
			/>
			<button
				class="player-btn player-btn--small"
				title={t('music.mute', 'Mute')}
				aria-label={t('music.mute', 'Mute')}
				data-testid="music-player-mute-btn"
				onclick={() => player.toggleMute()}
			>
				<Icon name={volumeIcon} />
			</button>
		</div>
	</div>

	<div class="player-bar">
		<span class="player-time">{fmtTime(player.currentTime)}</span>
		<input
			type="range"
			min="0"
			max={player.duration || 0}
			value={player.currentTime}
			oninput={(e) => player.seek(Number(e.currentTarget.value))}
			aria-label={t('music.seek', 'Seek')}
			data-testid="music-player-seek-input"
		/>
		<span class="player-time">{fmtTime(player.duration)}</span>
	</div>

	<!--
		Shuffle and repeat live here rather than with the transport: they change
		how the QUEUE is traversed, which is what this section is, and grouping
		them with clear keeps the row above to the three controls people
		actually aim at.
	-->
	<div class="player-queue-head">
		<button
			class="player-btn player-btn--small"
			class:active={player.shuffle}
			title={t('music.shuffle', 'Shuffle')}
			aria-label={t('music.shuffle', 'Shuffle')}
			data-testid="music-player-shuffle-btn"
			onclick={() => player.toggleShuffle()}
		>
			<Icon name="shuffle" />
		</button>
		<button
			class="player-btn player-btn--small"
			class:active={player.repeat !== 'none'}
			title={t('music.repeat', 'Repeat')}
			aria-label={t('music.repeat', 'Repeat')}
			data-testid="music-player-repeat-btn"
			onclick={() => player.cycleRepeat()}
		>
			<Icon name="repeat" />
			{#if player.repeat === 'one'}<span class="player-repeat-one">1</span>{/if}
		</button>
		<!--
			No visible heading: the row's own controls say what this section is,
			and the list below includes the CURRENT track (highlighted), so "up
			next" was describing it wrongly. The name lives on the list as an
			accessible label instead.
		-->
		<button
			class="player-btn player-btn--small"
			title={t('music.clear_queue', 'Clear queue')}
			aria-label={t('music.clear_queue', 'Clear queue')}
			data-testid="music-player-clear-btn"
			onclick={() => {
				player.clear();
				onclose?.();
			}}
		>
			<!--
				`eraser`, not `times`: a cross in the corner of a panel reads as
				"close the panel", and this empties the queue and stops playback.
				It is also what `/recent`'s clear button already uses, so "clear
				this list" looks the same in both places.
			-->
			<Icon name="eraser" />
		</button>
	</div>

	<!--
		The gestures — reorder by grip, swipe to remove, and the hover button
		that is the mouse's route to the same removal — all live in
		`GestureList`, which the playlist's track list and the notification
		feed share. What stays here is a track's own markup: its name and
		duration.

		Keyed by POSITION, not by track id: a queue is a list of positions and
		the same track may hold several of them — which is exactly what "add to
		queue" does to a track that is already in it. Keying by id crashed the
		whole list with `each_key_duplicate` the second time anyone queued the
		same song.
	-->
	<GestureList
		items={player.queue}
		key={(track, i) => `${i}:${track.id}`}
		reorderCapable
		onmove={(from, to) => player.move(from, to)}
		swipeLeft={{
			icon: 'eraser',
			label: t('common.remove', 'Remove'),
			tone: 'danger',
			testId: (track) => `music-queue-remove-${track.id}`,
			run: (_track, i) => player.removeAt(i)
		}}
		gripLabel={t('common.reorder', 'Reorder')}
		listLabel={t('music.queue', 'Queue')}
		listTestId="music-queue-panel"
		actionsInset="1.125rem"
	>
		{#snippet row(track, i, state)}
			<button
				class="player-queue-item"
				class:active={i === player.index}
				class:hovered={state.hovered}
				data-testid={`music-queue-item-${track.id}`}
				onclick={() => player.jump(i)}
			>
				<span class="player-queue-name">{player.label(track)}</span>
				<!--
					The duration steps aside for the action button, which sits
					exactly over it: the time is idle text, so the row gains an
					action without gaining a column. `visibility` rather than a
					removal, because the cell is a grid track and the three
					columns have to stay put down the list.
				-->
				<span class="player-queue-time" class:is-hidden={state.hovered}>
					{fmtTime(track.duration_secs)}
				</span>
			</button>
		{/snippet}
	</GestureList>
{:else}
	<!-- Queue empty: the control keeps ONE meaning (show the player) and this
	     state does the teaching, rather than the trigger silently behaving as
	     a navigation link instead. -->
	<div class="player-empty" data-testid="music-player-empty">
		<Icon name="music" />
		<p>{t('music.nothing_playing', 'Nothing playing.')}</p>
		<a href={resolve('/music')} onclick={() => onclose?.()}>
			{t('music.browse', 'Browse music')}
		</a>
	</div>
{/if}

<style>
	.player-head {
		display: flex;
		align-items: center;
		gap: var(--space-3);
	}

	.player-art {
		display: flex;
		align-items: center;
		justify-content: center;
		flex-shrink: 0;
		width: 96px;
		height: 96px;
		border-radius: var(--radius-lg);
		background: var(--color-bg-subtle);
		color: var(--color-text-faint);
		font-size: var(--text-3xl);
		overflow: hidden;
	}

	.player-art img {
		width: 100%;
		height: 100%;
		object-fit: cover;
	}

	/* Title, artist and transport stack beside the cover and fill its height,
	   so the 96px of art costs nothing extra vertically. */
	.player-meta {
		display: flex;
		flex-direction: column;
		gap: var(--space-1);
		min-width: 0;
		height: 96px;
		flex: 1;
	}

	.player-title {
		font-weight: var(--weight-semibold);
		color: var(--color-text-heading);
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}

	.player-artist {
		font-size: var(--text-sm);
		color: var(--color-text-muted);
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}

	/* Centred both ways inside the column beside the cover: it takes whatever
	   height the title and artist leave, and centres across that column's
	   width. */
	.player-transport {
		display: flex;
		flex: 1;
		align-items: center;
		justify-content: center;
		gap: var(--space-1);
	}

	.player-btn {
		position: relative;
		display: flex;
		align-items: center;
		justify-content: center;
		width: 36px;
		height: 36px;
		border: none;
		border-radius: var(--radius-full);
		background: none;
		color: var(--color-text-secondary);
		cursor: pointer;
	}

	.player-btn:hover {
		background: var(--color-bg-subtle);
		color: var(--color-text-heading);
	}

	.player-btn.active {
		color: var(--color-accent);
	}

	.player-btn--main {
		width: 44px;
		height: 44px;
		background: var(--color-accent);
		color: var(--color-accent-contrast, var(--color-bg-surface));
		font-size: var(--text-lg);
	}

	.player-btn--main:hover {
		background: var(--color-accent);
		color: var(--color-accent-contrast, var(--color-bg-surface));
		filter: brightness(1.08);
	}

	.player-btn--small {
		width: 28px;
		height: 28px;
		font-size: var(--text-sm);
	}

	.player-repeat-one {
		position: absolute;
		right: 2px;
		bottom: 2px;
		font-size: 9px;
		font-weight: var(--weight-bold);
	}

	.player-volume {
		display: flex;
		flex-direction: column;
		align-items: center;
		gap: var(--space-1);
		flex-shrink: 0;
		/* The head is `align-items: center`, so this column would size to its
		   contents — a 32px stub next to a 96px cover. */
		align-self: stretch;
	}

	/* A vertical range, stated three ways because engines disagree:
	   `writing-mode` + `direction: rtl` is the standard one (and puts zero at
	   the BOTTOM), `slider-vertical` covers older WebKit, `-moz-orient` older
	   Firefox. Newer engines ignore the legacy pair. */
	.player-volume input[type='range'] {
		writing-mode: vertical-lr;
		direction: rtl;
		/* `appearance` is here only to satisfy the compatibility diagnostic
		   that wants a standard property beside the prefixed one; `auto` is a
		   range input's initial value, so it changes nothing. */
		appearance: auto;
		-webkit-appearance: slider-vertical;
		-moz-orient: vertical;
		width: 6px;
		flex: 1;
		min-height: 0;
		accent-color: var(--color-accent);
		cursor: pointer;
	}

	.player-bar {
		display: flex;
		align-items: center;
		gap: var(--space-2);
	}

	.player-bar input[type='range'] {
		flex: 1;
		min-width: 0;
		accent-color: var(--color-accent);
	}

	.player-time {
		font-size: var(--text-xs);
		color: var(--color-text-muted);
		font-variant-numeric: tabular-nums;
	}

	.player-queue-head {
		display: flex;
		align-items: center;
		gap: var(--space-1);
		padding-top: var(--space-2);
		border-top: 1px solid var(--color-border);
		font-size: var(--text-xs);
		font-weight: var(--weight-semibold);
		letter-spacing: 0.06em;
		text-transform: uppercase;
		color: var(--color-text-muted);
	}

	/* Pushes clear to the right edge, leaving shuffle and repeat on the left.
	   An auto margin rather than a spacer element: nothing renders, so nothing
	   needs explaining to the next reader. */
	.player-queue-head .player-btn:last-child {
		margin-inline-start: auto;
	}

	/* A GRID, and one that always fills the row.

	   It used to be `display: flex; width: 100%` — but this button is itself a
	   flex item of the row, where the default `min-width: auto` lets content
	   push it wider than its siblings. So its width varied row to row, and the
	   title and duration inside it drifted with it: the columns never lined up
	   down the list. `flex: 1` + `min-width: 0` fixes the button's width to the
	   row, and the two tracks below put every cell at the same x on every row.
	   The title is the only `1fr`, so it is the only thing that ever gives, and
	   it crops with an ellipsis.

	   A third track used to hold the track's position in the queue. It said
	   nothing the order itself does not, and it cost the title the width. */
	.player-queue-item {
		display: grid;
		grid-template-columns: 1fr 2.75rem;
		align-items: center;
		gap: var(--space-2);
		flex: 1;
		min-width: 0;
		padding: var(--space-2);
		border: none;
		border-radius: var(--radius-md);
		background: none;
		color: var(--color-text-secondary);
		font: inherit;
		text-align: left;
		cursor: pointer;
	}

	/* Driven by the LIST's hover state, not this button's own `:hover`. The
	   action buttons are siblings of this element, so with `:hover` the row
	   lost its highlight the moment the cursor reached the eraser — the row
	   looked like it had been left while the thing being pointed at belonged
	   to it. */
	.player-queue-item.hovered {
		background: var(--color-bg-subtle);
	}

	.player-queue-item.active {
		color: var(--color-accent);
		font-weight: var(--weight-semibold);
	}

	.player-queue-name {
		min-width: 0;
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}

	/* The duration's track is fixed (see the grid above) rather than sized to
	   its text: a row reading `12:05` is ~7px wider than `3:42`, and with the
	   title as the flexible column every such row stole that width from its
	   own title, so the ellipsis landed in a different place down the list.
	   2.75rem holds `59:59` with room to spare and still fits a six-character
	   value; `tabular-nums` keeps the digits from shifting as the clock runs,
	   and the eraser that replaces it on hover is 24px, well inside. */
	.player-queue-time {
		text-align: right;
		font-size: var(--text-xs);
		color: var(--color-text-faint);
		font-variant-numeric: tabular-nums;
	}

	/* The action button sits exactly here, so the duration gets out of its
	   way — `visibility` and not `display`, because this is a grid track and
	   the three columns must stay put down the whole list. Driven by the
	   list's hover state rather than a `:hover` of our own: the row is the
	   component's element, scoped to it, so our stylesheet cannot see it. */
	.player-queue-time.is-hidden {
		visibility: hidden;
	}

	.player-empty {
		display: flex;
		flex-direction: column;
		align-items: center;
		gap: var(--space-2);
		padding: var(--space-6) 0;
		color: var(--color-text-muted);
		text-align: center;
	}

	.player-empty p {
		margin: 0;
	}

	.player-empty a {
		color: var(--color-accent);
	}
</style>
