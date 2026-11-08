<script lang="ts">
	import { resolve } from '$app/paths';
	import Icon from '$lib/icons/Icon.svelte';
	import { t } from '$lib/i18n/index.svelte';
	import { player } from '$lib/stores/player.svelte';
	import { serverConfig } from '$lib/stores/serverConfig.svelte';
	import { ui } from '$lib/stores/ui.svelte';

	/**
	 * The app's player: a trigger in the top bar's user controls, and a panel
	 * holding the transport, the progress and volume bars, and the queue.
	 *
	 * The `<audio>` element lives HERE, outside the `{#if open}`, because this
	 * component is mounted for the whole session while the panel comes and
	 * goes. It used to live in `routes/music/+page.svelte`, so leaving Music
	 * unmounted it and cut playback mid-track — the bug this component exists
	 * to fix. For the same reason nothing about playback may move inside the
	 * conditional.
	 *
	 * The panel mirrors the notification bell's shape (380px, same anchoring
	 * and dismissal) so it introduces no new interaction to learn.
	 */
	let open = $state(false);
	/** Queue drag-to-reorder: the row picked up, and the row hovered. */
	let dragFrom = $state<number | null>(null);
	let dragOver = $state<number | null>(null);
	let audioEl = $state<HTMLAudioElement | null>(null);
	let wrapper = $state<HTMLElement | null>(null);

	$effect(() => {
		player.attach(audioEl);
		// Pick the previous session back up — the queue, the modes and the
		// position. Must follow `attach`, since restoring sets the element's
		// source and asks it to play; `restore` itself is idempotent.
		if (audioEl) player.restore();
	});

	// A tab can vanish without a `pause`: closed, navigated away from, or sent
	// to the background on a phone where the OS may kill it outright.
	// `pagehide` is the last event guaranteed to run in all three, and
	// `visibilitychange` covers the phone case early enough to matter.
	$effect(() => {
		const save = () => player.persist();
		window.addEventListener('pagehide', save);
		document.addEventListener('visibilitychange', save);
		return () => {
			window.removeEventListener('pagehide', save);
			document.removeEventListener('visibilitychange', save);
		};
	});

	// Keep the element's source on the current track.
	$effect(() => {
		// Touch the dependency explicitly: `syncSource` reads it internally,
		// and an effect only tracks what it reads in its own body.
		void player.current;
		player.syncSource();
	});

	// Media keys / lock screen / headset buttons. Registered once; the
	// handlers stay valid because they close over the store, not over state.
	$effect(() => {
		player.bindMediaKeys();
	});

	// Dismissal, matching the notification panel: outside click or Escape.
	$effect(() => {
		if (!open) return;
		const onPointerDown = (e: MouseEvent) => {
			if (wrapper && !wrapper.contains(e.target as Node)) open = false;
		};
		const onKey = (e: KeyboardEvent) => {
			if (e.key === 'Escape') open = false;
		};
		window.addEventListener('mousedown', onPointerDown);
		window.addEventListener('keydown', onKey);
		return () => {
			window.removeEventListener('mousedown', onPointerDown);
			window.removeEventListener('keydown', onKey);
		};
	});

	function fmtTime(s: number | null | undefined): string {
		if (s == null || !Number.isFinite(s)) return '0:00';
		const m = Math.floor(s / 60);
		const sec = Math.floor(s % 60);
		return `${m}:${String(sec).padStart(2, '0')}`;
	}

	function onLoadedMetadata() {
		const patched = player.onLoadedMetadata();
		// The music page keeps its own copy of the visible rows; tell it a
		// duration was discovered so a row showing 0:00 can fill in.
		if (patched) {
			window.dispatchEvent(new CustomEvent('oxicloud:track-duration', { detail: patched }));
		}
	}

	function onError() {
		player.onError();
		ui.notify(
			t('music.playback_error', { name: player.label() }, 'Playback error: {{name}}'),
			'error'
		);
	}

	const volumeIcon = $derived(player.muted || player.volume === 0 ? 'volume' : 'volume-up');
</script>

<!--
	Nothing at all when the deployment has music switched off: no trigger, and
	no `<audio>` either, since there is nothing to play. Gated HERE rather than
	at the call site so a second mount point — the mobile nav is next — cannot
	forget it. `features.music` is the same flag that hides `/music` from the
	sidebar, so the player disappears with the section it belongs to.
-->
{#if serverConfig.features.music}
	<!-- Never inside `{#if open}` — see the component doc. -->
	<audio
		bind:this={audioEl}
		onplay={() => player.onPlay()}
		onpause={() => player.onPause()}
		ontimeupdate={() => player.onTimeUpdate()}
		onloadedmetadata={onLoadedMetadata}
		onended={() => player.onEnded()}
		onerror={onError}
	></audio>

	<div class="player-wrapper" class:open bind:this={wrapper}>
		<button
			class="player-trigger"
			class:active={open}
			aria-haspopup="dialog"
			aria-expanded={open}
			title={t('music.player', 'Player')}
			aria-label={t('music.player', 'Player')}
			data-testid="music-player-toggle-btn"
			onclick={() => (open = !open)}
		>
			<Icon name="music" />
			<!--
				State badge, only once something is queued: an empty player has no
				state worth reporting, and a permanent badge would train people to
				ignore it. Mirrors the notification bell's badge placement, so the
				two controls beside each other behave alike.
			-->
			{#if player.active}
				<span class="player-trigger__badge" aria-hidden="true">
					<Icon name={player.playing ? 'play' : 'pause'} />
				</span>
			{/if}
		</button>

		{#if open}
			<div class="player-panel" role="dialog" aria-label={t('music.player', 'Player')}>
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
								Transport sits BESIDE the cover rather than on its own row:
								the cover is 96px tall and the title/artist only fill two
								thirds of it, so the buttons ride in space that was empty.
								Buys ~44px of queue without shrinking anything.
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
							Volume stands vertically alongside the head rather than
							taking a row of its own: the head is already 96px tall, so
							the slider costs nothing but ~32px of width, and the panel
							keeps that row for the queue.
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
						Shuffle and repeat live here rather than with the transport:
						they change how the QUEUE is traversed, which is what this
						section is, and grouping them with clear keeps the row above
						to the three controls people actually aim at.
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
							No visible heading: the row's own controls say what this
							section is, and the list below includes the CURRENT track
							(highlighted), so "up next" was describing it wrongly. The
							name lives on the list as an accessible label instead.
						-->
						<button
							class="player-btn player-btn--small"
							title={t('music.clear_queue', 'Clear queue')}
							aria-label={t('music.clear_queue', 'Clear queue')}
							data-testid="music-player-clear-btn"
							onclick={() => {
								player.clear();
								open = false;
							}}
						>
							<!--
								`eraser`, not `times`: a cross in the corner of a panel reads
								as "close the panel", and this empties the queue and stops
								playback. It is also what `/recent`'s clear button already
								uses, so "clear this list" looks the same in both places.
							-->
							<Icon name="eraser" />
						</button>
					</div>

					<ul
						class="player-queue"
						aria-label={t('music.queue', 'Queue')}
						data-testid="music-queue-panel"
					>
						{#each player.queue as track, i (track.id)}
							<!--
								Drag to reorder, same gesture the track list on /music
								uses: the row is draggable, the grip is the affordance.
								`dragover` must `preventDefault` or the drop never
								fires.
							-->
							<li
								class="player-queue-row"
								class:drag-over={dragOver === i && dragFrom !== i}
								draggable="true"
								ondragstart={(e) => {
									dragFrom = i;
									e.dataTransfer?.setData('text/plain', String(i));
									if (e.dataTransfer) e.dataTransfer.effectAllowed = 'move';
								}}
								ondragover={(e) => {
									if (dragFrom === null) return;
									e.preventDefault();
									dragOver = i;
								}}
								ondrop={(e) => {
									e.preventDefault();
									if (dragFrom !== null) player.move(dragFrom, i);
									dragFrom = null;
									dragOver = null;
								}}
								ondragend={() => {
									dragFrom = null;
									dragOver = null;
								}}
							>
								<span class="player-queue-grip" aria-hidden="true">
									<Icon name="grip-vertical" />
								</span>
								<button
									class="player-queue-item"
									class:active={i === player.index}
									data-testid={`music-queue-item-${track.id}`}
									onclick={() => player.jump(i)}
								>
									<span class="player-queue-num">{i + 1}</span>
									<span class="player-queue-name">{player.label(track)}</span>
									<span class="player-queue-time">{fmtTime(track.duration_secs)}</span>
								</button>
								<!--
									A SIBLING of the row button, not a child: a button inside
									a button is invalid markup and the inner one would never
									get its click. It is positioned over the duration, which
									is idle text — so the row gains an action without gaining
									a column.
								-->
								<button
									class="player-queue-remove"
									title={t('common.remove', 'Remove')}
									aria-label={t('common.remove', 'Remove')}
									data-testid={`music-queue-remove-${track.id}`}
									onclick={() => player.removeAt(i)}
								>
									<Icon name="eraser" />
								</button>
							</li>
						{/each}
					</ul>
				{:else}
					<!-- Queue empty: the control keeps ONE meaning (open the player)
					     and this state does the teaching, rather than the trigger
					     silently behaving as a navigation link instead. -->
					<div class="player-empty" data-testid="music-player-empty">
						<Icon name="music" />
						<p>{t('music.nothing_playing', 'Nothing playing.')}</p>
						<a href={resolve('/music')} onclick={() => (open = false)}>
							{t('music.browse', 'Browse music')}
						</a>
					</div>
				{/if}
			</div>
		{/if}
	</div>
{/if}

<style>
	.player-wrapper {
		position: relative;
		display: flex;
		align-items: center;
	}

	/* Geometry and states lifted from `.notif-bell-btn` (notifications.css):
	   40px round, same resting colour, same tinted circle on hover, same
	   accent ring while its panel is open. They sit next to each other, so
	   anything less than identical reads as a mistake — and the badge's
	   `top/right: 4px` only lands where the bell's does because the button
	   underneath is the same size. */
	.player-trigger {
		position: relative;
		display: flex;
		align-items: center;
		justify-content: center;
		width: 40px;
		height: 40px;
		border: none;
		border-radius: 50%;
		background: none;
		color: var(--color-text-subtle);
		font-size: var(--text-lg);
		cursor: pointer;
		transition: all 0.2s ease;
	}

	.player-trigger:hover {
		background: var(--color-accent-bg-sm);
		color: var(--color-accent);
	}

	.player-trigger.active {
		background: var(--color-accent-ring);
		color: var(--color-accent);
	}

	/* Geometry copied from `.notif-badge` (notifications.css) — same corner,
	   same 16px, same radius and colours — so the two badges in this corner
	   of the bar are plainly the same device reporting different things. The
	   glyph is small inside it because a play triangle at 10px reads as a
	   smudge where a digit does not. */
	.player-trigger__badge {
		position: absolute;
		top: 4px;
		right: 4px;
		display: flex;
		align-items: center;
		justify-content: center;
		width: 16px;
		height: 16px;
		border-radius: var(--radius-lg);
		background: var(--color-notification-badge);
		color: var(--color-notification-bg);
		font-size: 7px;
		line-height: 1;
		pointer-events: none;
	}

	/* Mirrors `.notif-panel` (notifications.css): same width, offset, radius
	   and elevation, so the two panels in this corner read as siblings. */
	.player-panel {
		position: absolute;
		top: calc(100% + 10px);
		right: -40px;
		z-index: 2000;
		display: flex;
		flex-direction: column;
		width: 380px;
		max-height: 480px;
		padding: var(--space-4);
		gap: var(--space-3);
		background: var(--color-notification-bg);
		border-radius: var(--radius-3xl);
		/* Clip to the rounded box, as `.notif-panel` does: the queue scrolls
		   inside, and nothing can paint over the corners. */
		overflow: hidden;
		box-shadow:
			0 12px 40px var(--color-shadow-md),
			0 0 0 1px var(--color-shadow-xs);
	}

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
	   height the title and artist leave (rather than being pushed to the
	   bottom edge by `space-between`, which left the buttons sitting low), and
	   centres them across that column's width. The negative start margin that
	   used to align the first glyph with the text is gone — it only made sense
	   while the row was left-aligned. */
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

	.player-bar {
		display: flex;
		align-items: center;
		gap: var(--space-2);
	}

	.player-volume {
		display: flex;
		flex-direction: column;
		align-items: center;
		gap: var(--space-1);
		flex-shrink: 0;
		/* `.player-head` is `align-items: center`, so this column sized to its
		   contents — a 32px stub next to a 96px cover. Stretching it to the
		   flex line gives the slider the head's full height to grow into. */
		align-self: stretch;
	}

	/* A vertical range, stated three ways because engines disagree:
	   `writing-mode` + `direction: rtl` is the standard one (and puts zero at
	   the BOTTOM), `slider-vertical` covers older WebKit, `-moz-orient`
	   older Firefox. Newer engines ignore the legacy pair. */
	.player-volume input[type='range'] {
		writing-mode: vertical-lr;
		direction: rtl;
		/* `appearance` is here only to satisfy the compatibility diagnostic
		   that wants a standard property beside the prefixed one; `auto` is a
		   range input's initial value, so it changes nothing. The prefixed
		   line is the one doing work, for Safari 16.4–17.3 and Chrome before
		   121, which predate vertical `writing-mode` on a range and would
		   otherwise squeeze a HORIZONTAL slider into a 6px-wide box. */
		appearance: auto;
		-webkit-appearance: slider-vertical;
		-moz-orient: vertical;
		width: 6px;
		flex: 1;
		min-height: 0;
		accent-color: var(--color-accent);
		cursor: pointer;
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

	/* `flex: 1` is the load-bearing part, not `overflow-y`. Without a flex
	   basis this list keeps its natural height, so a queue of any length grows
	   straight past the panel's `max-height` and renders as a slab hanging
	   below it — the scroll never engages because the box never gets smaller
	   than its content. Same flexbox trap as the sidebar, one layer in. */
	.player-queue {
		flex: 1;
		min-height: 0;
		overflow-y: auto;
		list-style: none;
		margin: 0;
		padding: 0;
	}

	.player-queue-row {
		position: relative;
		display: flex;
		align-items: center;
		border-radius: var(--radius-md);
	}

	/* Where the dragged row would land. A top edge rather than a filled
	   background, so it reads as an insertion point and not as a selection. */
	.player-queue-row.drag-over {
		box-shadow: inset 0 2px 0 0 var(--color-accent);
	}

	.player-queue-grip {
		display: flex;
		flex-shrink: 0;
		align-items: center;
		padding: 0 var(--space-1);
		color: var(--color-text-faint);
		cursor: grab;
	}

	.player-queue-row:active .player-queue-grip {
		cursor: grabbing;
	}

	/* A GRID, and one that always fills the row.
	   
	   It used to be `display: flex; width: 100%` — but this button is itself a
	   flex item of the row, where the default `min-width: auto` lets content
	   push it wider than its siblings. So its width varied row to row, and the
	   position/title/duration inside it drifted with it: the columns never
	   lined up down the list. `flex: 1` + `min-width: 0` fixes the button's
	   width to the row, and the three fixed-or-`1fr` tracks below put every
	   cell at the same x on every row. The title is the only `1fr`, so it is
	   the only thing that ever gives, and it crops with an ellipsis. */
	.player-queue-item {
		display: grid;
		grid-template-columns: 1.5rem 1fr 2.75rem;
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

	.player-queue-item:hover {
		background: var(--color-bg-subtle);
	}

	.player-queue-item.active {
		color: var(--color-accent);
		font-weight: var(--weight-semibold);
	}

	/* Sits exactly over the duration and only while the row is hovered or
	   holds focus — the time is idle text, so the action costs no width. Kept
	   out of the layout entirely (absolute) rather than toggled with
	   `visibility`, so it can never nudge the row. `:focus-within` is what
	   makes it reachable by keyboard, where there is no hover to speak of. */
	.player-queue-remove {
		position: absolute;
		right: var(--space-2);
		display: none;
		align-items: center;
		justify-content: center;
		width: 24px;
		height: 24px;
		border: none;
		border-radius: var(--radius-sm);
		background: none;
		color: var(--color-text-muted);
		cursor: pointer;
	}

	.player-queue-row:hover .player-queue-remove,
	.player-queue-row:focus-within .player-queue-remove {
		display: flex;
	}

	.player-queue-remove:hover {
		color: var(--color-error-text);
	}

	/* The duration steps aside for it, so the two never overlap visually. */
	.player-queue-row:hover .player-queue-time,
	.player-queue-row:focus-within .player-queue-time {
		visibility: hidden;
	}

	.player-queue-num {
		font-size: var(--text-xs);
		color: var(--color-text-faint);
		font-variant-numeric: tabular-nums;
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
