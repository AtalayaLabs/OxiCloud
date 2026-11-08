<script lang="ts">
	import { resolve } from '$app/paths';
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

	/** Queue reorder: the row picked up, and the row it is currently over. */
	let dragFrom = $state<number | null>(null);
	let dragOver = $state<number | null>(null);
	/** The list itself, for resolving a drag that strays past its rows. */
	let listEl = $state<HTMLElement | null>(null);
	/**
	 * How far the carried row has travelled, so it follows the pointer.
	 *
	 * HTML5 drag-and-drop drew a drag image for free; pointer events draw
	 * nothing, so without this the row stayed put while only the insertion
	 * line moved — the gesture looked like it had not taken hold.
	 */
	let dragStartY = 0;
	let dragY = $state(0);

	/**
	 * Reordering runs on POINTER events, not HTML5 drag-and-drop.
	 *
	 * Drag-and-drop does not exist on touch — `dragstart` never fires, so the
	 * grip was dead on a phone, which is where this panel has a page of its
	 * own. Pointer events cover mouse, touch and pen with one implementation,
	 * so there is no second gesture to keep in step with the first.
	 *
	 * The grip captures the pointer, which keeps the move and up events coming
	 * to it even when the finger strays off the row, and `touch-action: none`
	 * on the grip stops the browser treating the drag as a scroll.
	 */
	function onGripDown(e: PointerEvent, i: number) {
		e.preventDefault();
		(e.currentTarget as HTMLElement).setPointerCapture(e.pointerId);
		dragFrom = i;
		dragOver = i;
		dragStartY = e.clientY;
		dragY = 0;
	}

	function onGripMove(e: PointerEvent) {
		if (dragFrom === null) return;
		dragY = e.clientY - dragStartY;
		dragOver = targetIndex(e.clientY);
	}

	/**
	 * Where the carried track would land, as an index into the queue once it
	 * has been lifted out of it — exactly what `player.move` takes.
	 *
	 * Counted from the STATIONARY rows rather than hit-tested. Hit-testing
	 * failed in both directions: the carried row follows the pointer, so
	 * `elementFromPoint` either returned the dragged row itself (making the
	 * target equal the source, so every drop was a no-op) or, once that row
	 * was made transparent to hit-testing, returned nothing at all over its
	 * vacated slot — leaving the target stuck on the last neighbour crossed,
	 * which is why it became impossible to drop a track back where it
	 * started.
	 *
	 * Counting midpoints has neither problem, and needs no special case for
	 * either end of the list: above everything gives 0, below everything
	 * gives the last index.
	 */
	function targetIndex(clientY: number): number {
		const rows = [...(listEl?.querySelectorAll<HTMLElement>('[data-queue-index]') ?? [])];
		let index = 0;
		for (const row of rows) {
			// The dragged row's rect has moved with the pointer, so it says
			// nothing about where the gap is.
			if (Number(row.dataset.queueIndex) === dragFrom) continue;
			const rect = row.getBoundingClientRect();
			if (clientY > rect.top + rect.height / 2) index++;
		}
		return index;
	}

	/**
	 * Which edge of row `i` carries the insertion line, if any.
	 *
	 * The line marks the GAP the track would drop into, so it is drawn on the
	 * stationary row beside that gap — never on the dragged row, which is
	 * translated and would carry the line off with it. `dragOver` counts
	 * positions in the lifted queue, so row `dragOver` of the remaining rows
	 * takes a top line, and the last of them takes a bottom line when the
	 * track would land at the end.
	 */
	function dropEdge(i: number): 'top' | 'bottom' | null {
		if (dragFrom === null || dragOver === null) return null;
		const remaining = player.queue.map((_, n) => n).filter((n) => n !== dragFrom);
		if (dragOver < remaining.length) return remaining[dragOver] === i ? 'top' : null;
		return remaining[remaining.length - 1] === i ? 'bottom' : null;
	}

	function onGripUp() {
		if (dragFrom !== null && dragOver !== null) player.move(dragFrom, dragOver);
		dragFrom = null;
		dragOver = null;
		dragY = 0;
	}

	// ── Swipe left to remove ──────────────────────────────────────────────
	//
	// TOUCH AND PEN ONLY, and the division matters: a pointer that can hover
	// already has an affordance for this — the eraser the row reveals under
	// the cursor — so a mouse needs no swipe, and giving it one only means a
	// stray drag flings a row and deletes a track. A finger has no hover, so
	// the swipe is the whole affordance there. One removal per input kind.
	//
	// Three gestures then share a queue row on touch: tap to jump to the
	// track, drag the grip to reorder, swipe to remove. So the swipe stays
	// dormant until the movement is clearly HORIZONTAL — until then the
	// browser keeps the gesture and the list scrolls normally, which is the
	// behaviour anyone flicking through a long queue expects.

	/** Horizontal travel that claims the gesture as a swipe. */
	const SWIPE_CLAIM_PX = 8;
	/**
	 * Share of the row's own width the track must cross to be removed on
	 * release. Half the row is a distance the hand can feel — the red grows
	 * from nothing to half the row and the commitment is legible at a glance —
	 * where a fixed pixel count means something different on every screen.
	 */
	const SWIPE_REMOVE_RATIO = 0.5;
	/** Only for a row that reports no width, where a ratio says nothing. */
	const SWIPE_REMOVE_FALLBACK_PX = 96;

	let swipeIndex = $state<number | null>(null);
	let swipeX = $state(0);
	/** Claimed: the row follows the finger and the list no longer scrolls. */
	let swiping = $state(false);
	/**
	 * The row sliding back after a release that did not remove it. The red
	 * behind it has to stay lit for that trip: cleared with `swipeIndex` it
	 * would blink out and leave the list's own background showing through the
	 * gap, as if the row had been deleted after all.
	 */
	let settlingIndex = $state<number | null>(null);
	/** Set for the duration of a swipe so the row's tap handler stands down. */
	let swiped = false;
	let swipeStart = { x: 0, y: 0 };
	/** Measured at pointerdown: mid-gesture the row is translated, not resized. */
	let swipeWidth = 0;

	/** True while row `i` shows the red removal backing. */
	function swipeOpen(i: number): boolean {
		return (swiping && swipeIndex === i) || settlingIndex === i;
	}

	/** How far left this gesture must go for the release to remove the track. */
	function swipeThreshold(): number {
		return swipeWidth > 0 ? swipeWidth * SWIPE_REMOVE_RATIO : SWIPE_REMOVE_FALLBACK_PX;
	}

	/**
	 * Past the threshold: releasing now removes. The backing goes to full
	 * strength to say so, which is the only way the gesture can report that it
	 * has gone far enough BEFORE the finger lifts and the answer is final.
	 */
	function swipeArmed(i: number): boolean {
		return swipeIndex === i && -swipeX > swipeThreshold();
	}

	function onRowDown(e: PointerEvent, i: number) {
		if (e.pointerType === 'mouse') return;
		swipeIndex = i;
		swipeX = 0;
		swiping = false;
		swiped = false;
		swipeStart = { x: e.clientX, y: e.clientY };
		swipeWidth = (e.currentTarget as HTMLElement).getBoundingClientRect().width;
	}

	function onRowMove(e: PointerEvent, i: number) {
		if (swipeIndex !== i) return;
		const dx = e.clientX - swipeStart.x;
		const dy = e.clientY - swipeStart.y;
		if (!swiping) {
			// Vertical first: let go of the gesture entirely so the list can
			// scroll. Reclaiming it mid-scroll would feel like the row
			// snatching at the finger.
			if (Math.abs(dy) > SWIPE_CLAIM_PX && Math.abs(dy) >= Math.abs(dx)) {
				swipeIndex = null;
				return;
			}
			if (Math.abs(dx) <= SWIPE_CLAIM_PX || Math.abs(dx) <= Math.abs(dy)) return;
			swiping = true;
			swiped = true;
			(e.currentTarget as HTMLElement).setPointerCapture(e.pointerId);
		}
		// Left only: a right-swipe has no meaning here, and letting the row
		// travel right would imply one.
		swipeX = Math.min(0, dx);
	}

	function onRowUp(i: number) {
		if (swipeArmed(i)) {
			player.removeAt(i);
		} else if (swiping && swipeIndex === i) {
			// Keep the backing lit until the row has slid home; `onSwipeSettled`
			// turns it off when the transform transition reports it has.
			settlingIndex = i;
		}
		swipeIndex = null;
		swipeX = 0;
		swiping = false;
	}

	function onSwipeSettled(i: number) {
		if (settlingIndex === i) settlingIndex = null;
	}

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

	<ul
		class="player-queue"
		bind:this={listEl}
		aria-label={t('music.queue', 'Queue')}
		data-testid="music-queue-panel"
	>
		{#each player.queue as track, i (track.id)}
			<!--
				`data-queue-index` is how the drag finds the row under the
				pointer: the grip captures the pointer, so the events report it
				as their target and the row has to be hit-tested by coordinate.
			-->
			<!--
				The insertion line goes on the edge the track will land against,
				which depends on the DIRECTION of the drag: coming from above,
				it lands after this row (bottom edge); from below, before it
				(top edge). A fixed top line could never show "after the last
				row", so the end of the queue looked unreachable even though
				the drop worked.
			-->
			<li
				class="player-queue-row"
				class:drag-over-top={dropEdge(i) === 'top'}
				class:drag-over-bottom={dropEdge(i) === 'bottom'}
				class:dragging={dragFrom === i}
				class:swipe-open={swipeOpen(i)}
				class:swipe-armed={swipeArmed(i)}
				style:transform={dragFrom === i ? `translateY(${dragY}px)` : undefined}
				data-queue-index={i}
				onpointerdown={(e) => onRowDown(e, i)}
				onpointermove={(e) => onRowMove(e, i)}
				onpointerup={() => onRowUp(i)}
				onpointercancel={() => onRowUp(i)}
			>
				<!--
					Revealed as the row slides off it, and ONLY then: an eraser
					parked under every row would advertise a gesture on the one
					device that cannot make it, and a permanent red stripe under
					the queue would read as an error. Behind the content rather
					than beside it, so the list keeps its column widths and
					nothing reflows mid-gesture.
				-->
				{#if swipeOpen(i)}
					<span class="player-queue-swipe-hint" aria-hidden="true">
						<Icon name="eraser" />
					</span>
				{/if}
				<div
					class="player-queue-swipe"
					class:settling={swipeIndex !== i}
					style:transform={swipeIndex === i ? `translateX(${swipeX}px)` : undefined}
					ontransitionend={() => onSwipeSettled(i)}
				>
					<!--
					The drag lives on the GRIP, not the row: the row is a button
					that jumps to a track, and a drag that starts anywhere on it
					would fight that tap. Keyboard users reorder from nowhere yet
					— `removeAt` plus re-adding is the fallback — which is a gap
					worth closing separately.
				-->
					<span
						class="player-queue-grip"
						aria-hidden="true"
						onpointerdown={(e) => onGripDown(e, i)}
						onpointermove={onGripMove}
						onpointerup={onGripUp}
						onpointercancel={onGripUp}
					>
						<Icon name="grip-vertical" />
					</span>
					<button
						class="player-queue-item"
						class:active={i === player.index}
						data-testid={`music-queue-item-${track.id}`}
						onclick={() => {
							// A swipe ends with a click on the row underneath it;
							// without this, removing a track by swiping would also
							// jump to whatever took its place.
							if (swiped) {
								swiped = false;
								return;
							}
							player.jump(i);
						}}
					>
						<span class="player-queue-num">{i + 1}</span>
						<span class="player-queue-name">{player.label(track)}</span>
						<span class="player-queue-time">{fmtTime(track.duration_secs)}</span>
					</button>
					<!--
					A SIBLING of the row button, not a child: a button inside a
					button is invalid markup and the inner one would never get its
					click. It is positioned over the duration, which is idle text —
					so the row gains an action without gaining a column.
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
				</div>
			</li>
		{/each}
	</ul>
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

	/* `flex: 1` is the load-bearing part, not `overflow-y`. Without a flex
	   basis this list keeps its natural height, so a queue of any length grows
	   straight past the container and renders as a slab below it — the scroll
	   never engages because the box never gets smaller than its content. */
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
		/* The swiped row must not spill over its neighbours as it travels. */
		overflow: hidden;
		/* Vertical pans belong to the list, horizontal ones to the row. Left
		   to itself the browser decides on the first few pixels and keeps
		   whatever it chose, so a swipe that began slightly off-axis was
		   taken for a scroll and the moves stopped arriving. */
		touch-action: pan-y;
	}

	/* Everything the finger carries. Transformed rather than repositioned, so
	   the browser can move it on the compositor and the columns inside keep
	   their widths. */
	.player-queue-swipe {
		display: flex;
		align-items: center;
		flex: 1;
		min-width: 0;
		/* Positioned, so it paints ABOVE the absolute backing behind it — an
		   unpositioned sibling loses to one that is, whatever the DOM order,
		   and the red field would cover the very row it is backing. */
		position: relative;
	}

	/* Only while nothing is held: a transition during the drag would lag the
	   finger, and one on release is exactly what makes the row snap back. */
	.player-queue-swipe.settling {
		transition: transform var(--motion-fast, 0.15s) ease-out;
	}

	/* The removal backing: a red field the row slides off, uncovered from the
	   right as it goes. Lit only for the row under the finger, because the
	   gesture is destructive and this is the only warning a swipe can give
	   before the release commits it. `--color-danger-bg` + `--color-danger-text`
	   is the project's destructive fill pair, so this agrees with every delete
	   button rather than inventing a second red.

	   TWO STRENGTHS, and they carry the real information: dimmed while the
	   row is short of the threshold, full once past it. So the field answers
	   "would letting go delete this?" while the finger is still down —
	   without it the only way to find out is to lift and see.

	   The opacity lives HERE rather than on the row: the row is the content's
	   parent, so fading it would fade the title and duration travelling over
	   it, and the two strengths would read as the text dimming instead. */
	.player-queue-swipe-hint {
		position: absolute;
		inset: 0;
		display: flex;
		align-items: center;
		justify-content: flex-end;
		padding-inline-end: var(--space-3);
		border-radius: var(--radius-md);
		background: var(--color-danger-bg);
		color: var(--color-danger-text);
		/* Half strength, not two thirds: the jump to full has to be obvious at
		   arm's length, mid-gesture, with a finger over the row. */
		opacity: 0.5;
		transition: opacity var(--motion-fast, 0.15s) ease-out;
	}

	.player-queue-row.swipe-armed .player-queue-swipe-hint {
		opacity: 1;
	}

	/* The content travelling over it has to be opaque, or the red shows
	   through the title as well as beside it. Only while open, so an idle
	   queue keeps its transparent rows in both placements — the popover and
	   `/player`. */
	.player-queue-row.swipe-open .player-queue-swipe {
		background: var(--color-bg-surface);
		border-radius: var(--radius-md);
	}

	/* Where the dragged row would land — an edge rather than a filled
	   background, so it reads as an insertion point and not as a selection.
	   Which edge depends on the direction of travel; see the markup.

	   A pseudo-element with its own `z-index` rather than an inset shadow:
	   the carried row is opaque and sits at `z-index: 1`, and because it
	   follows the pointer it covers precisely the row being targeted — so a
	   shadow painted by that row went under it and the line vanished exactly
	   when it was needed. The target row has no `z-index` of its own, so this
	   child competes directly in the list's stacking context and wins. */
	.player-queue-row.drag-over-top::before,
	.player-queue-row.drag-over-bottom::after {
		content: '';
		position: absolute;
		z-index: 3;
		right: 0;
		left: 0;
		height: 2px;
		background: var(--color-accent);
	}

	.player-queue-row.drag-over-top::before {
		top: 0;
	}

	.player-queue-row.drag-over-bottom::after {
		bottom: 0;
	}

	.player-queue-grip {
		display: flex;
		flex-shrink: 0;
		align-items: center;
		/* Thumb-sized on a phone, where this is the only way to reorder. */
		padding: var(--space-2) var(--space-1);
		color: var(--color-text-faint);
		cursor: grab;
		/* Without this the browser claims the gesture as a scroll and the
		   pointermove events stop arriving mid-drag — the single thing that
		   makes touch reordering work at all. */
		touch-action: none;
	}

	.player-queue-grip:active {
		cursor: grabbing;
	}

	/* The row being carried. It FOLLOWS the pointer (see `dragY`) and lifts
	   off the list, because pointer events draw no drag image of their own —
	   without this the row stayed put while only the insertion line moved,
	   and the gesture looked like it had not taken hold.

	   `position: relative` + `z-index` so it passes over its neighbours
	   rather than under them; `overflow: visible` because the row clips its
	   own children for the swipe, which would otherwise cut the shadow. */
	.player-queue-row.dragging {
		position: relative;
		z-index: 1;
		overflow: visible;
		/* Invisible to hit-testing, which is what makes the drop work at all:
		   the row follows the pointer, so it IS the element under the finger,
		   and `elementFromPoint` kept returning the dragged row itself. The
		   target then equalled the source — no insertion line matched, and the
		   drop resolved to `move(i, i)`, a no-op. Only the top of the list
		   still worked, because that path goes through the geometry fallback
		   rather than the hit-test.

		   The gesture is unaffected: the grip holds a pointer capture, and
		   capture delivers events regardless of hit-testing. */
		pointer-events: none;
		background: var(--color-bg-surface);
		border-radius: var(--radius-md);
		box-shadow: var(--shadow-lg);
		opacity: 0.9;
	}

	/* Sits exactly over the duration and only while the row is hovered or
	   holds focus — the time is idle text, so the action costs no width. Kept
	   out of the layout entirely (absolute) rather than toggled with
	   `visibility`, so it can never nudge the row.

	   This is the POINTER-WITH-HOVER half of removal; the swipe is the touch
	   half (see the script). Hence `@media (hover: hover)`: on a phone the
	   hover state latches after a tap, so without the query the row you just
	   started playing kept an eraser where its duration should be — an
	   affordance for a gesture that device cannot make, over the one piece of
	   information the row was showing.

	   Keyboard reach is `:has(:focus-visible)` rather than `:focus-within`,
	   and outside the query: focus-within also fires when a FINGER taps the
	   row button, which brought the same eraser back on touch through the
	   other door. focus-visible is the one the browser reserves for keyboard. */
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

	@media (hover: hover) {
		.player-queue-row:hover .player-queue-remove {
			display: flex;
		}

		.player-queue-remove:hover {
			color: var(--color-error-text);
		}

		/* The duration steps aside for it, so the two never overlap visually. */
		.player-queue-row:hover .player-queue-time {
			visibility: hidden;
		}
	}

	.player-queue-row:has(:focus-visible) .player-queue-remove {
		display: flex;
	}

	.player-queue-row:has(:focus-visible) .player-queue-time {
		visibility: hidden;
	}

	/* A GRID, and one that always fills the row.

	   It used to be `display: flex; width: 100%` — but this button is itself a
	   flex item of the row, where the default `min-width: auto` lets content
	   push it wider than its siblings. So its width varied row to row, and the
	   position/title/duration inside it drifted with it: the columns never
	   lined up down the list. `flex: 1` + `min-width: 0` fixes the button's
	   width to the row, and the three tracks below put every cell at the same
	   x on every row. The title is the only `1fr`, so it is the only thing
	   that ever gives, and it crops with an ellipsis. */
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
