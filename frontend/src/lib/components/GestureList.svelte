<!--
  A list whose rows answer to the hand: drag a grip to reorder, swipe left or
  right to act.

  It exists because three lists need the same gestures — the player queue, a
  playlist's tracks and the notification feed — and the gestures are the part
  that is hard to get right. Each consumer brings its own row content as a
  snippet and its own capabilities; nothing about a track, a notification or
  an API call leaks in here.

  WHAT IT OWNS: the `<ul>`, each `<li>`, the grip, the swipe layers, the
  insertion line, the pointer bookkeeping and the edge auto-scroll. The row
  snippet renders INSIDE the sliding layer, so the caller styles its own
  content and never has to know a gesture is in progress.

  WHAT IT DOES NOT OWN: the order. `items` is the single source of truth and
  this component keeps no copy — it reports `onmove(from, to)` and re-renders
  from whatever the caller does with it. That is not fastidiousness: the
  playlist persists a reorder through the API and re-fetches when the call
  fails, so a local copy would show an order the server rejected.
-->
<script module lang="ts">
	/**
	 * One swipe direction's action.
	 *
	 * Passing the action IS the capability — there is deliberately no
	 * `deletionCapable` flag beside it. A flag and a handler are two things
	 * that can disagree, and the failure is silent in both directions: a flag
	 * with no handler gives a gesture that swallows the row and does nothing,
	 * a handler with no flag gives dead code nobody notices for a year.
	 *
	 * It lives in the module block because a consumer declaring one as a
	 * `const` needs the type, and only this block can export it.
	 */
	export interface SwipeAction<Item> {
		/** Icon name shown on the backing the row slides off. */
		icon: string;
		/** Accessible name, already translated by the caller. */
		label: string;
		/**
		 * Which palette pair backs the gesture: `danger` (red) for anything
		 * that destroys or dismisses, `add` (blue) for anything that adds.
		 * Named rather than passed as colours, so consumers cannot invent three
		 * different reds, and so the two directions of one list read as
		 * opposites on sight, mid-gesture, with a finger over the row.
		 *
		 * BLUE, not the obvious green: red against green is the one pair that
		 * deuteranopia and protanopia collapse — roughly one man in twelve —
		 * and at half opacity both wash toward the same beige. Red against blue
		 * survives every common form of colour blindness. The icon and the
		 * label carry the meaning regardless, which is the actual requirement
		 * (WCAG 1.4.1: never colour alone); the pair is for the half-second
		 * before anyone reads them.
		 */
		tone?: 'danger' | 'add';
		/** Run on release, once the travel has passed the threshold. */
		run: (item: Item, index: number) => void | Promise<void>;
		/**
		 * The hover button's `data-testid`, built by the caller.
		 *
		 * A function rather than a prefix the component completes: the row key
		 * has to be unique per POSITION (a queue may hold one track twice), and
		 * a position-bearing test id would move whenever anything was
		 * reordered. The caller knows which of the two it wants.
		 */
		testId?: (item: Item, index: number) => string;
	}

	/** Passed to the row snippet so it can dress itself for the gesture. */
	export interface RowState {
		/** A hover-capable pointer is over the row — its action buttons show. */
		hovered: boolean;
		/** This row is being carried by the grip. */
		dragging: boolean;
		/** This row is travelling under a finger. */
		swiping: boolean;
	}
</script>

<script lang="ts" generics="T">
	import type { Snippet } from 'svelte';
	import Icon from '$lib/icons/Icon.svelte';
	import { edgeStep } from '$lib/utils/dragAutoScroll';

	interface Props {
		items: T[];
		/**
		 * Identity per ROW — unique across the list, or Svelte throws
		 * `each_key_duplicate` and takes the page with it.
		 *
		 * An item id is the obvious choice and it is wrong wherever the same
		 * item can occupy two positions: a play queue holding the same track
		 * twice, a running log of repeated events. Fold the index in when that
		 * is possible (`(item, i) => `${i}:${item.id}`).
		 */
		key: (item: T, index: number) => string;
		/**
		 * The row's content. Rendered inside the sliding layer, and handed the
		 * row's gesture state so it can step aside for the action buttons —
		 * the player queue hides its duration under them, since the time is
		 * idle text and the action then costs the row no width.
		 */
		row: Snippet<[T, number, RowState]>;
		/**
		 * Show the drag grip. False where order is not the user's to choose —
		 * the notification feed is sorted by time, so a grip there would offer
		 * a move that cannot be kept.
		 */
		reorderCapable?: boolean;
		/** Required with `reorderCapable`; `to` is an index into the lifted list. */
		onmove?: (from: number, to: number) => void | Promise<void>;
		/**
		 * The two row actions. Each reaches the user by the route its pointer
		 * has: a SWIPE on touch and pen, where there is no hover to put a
		 * button in, and a BUTTON revealed on hover for a mouse, where a drag
		 * across a list of clickable rows is a stray gesture rather than an
		 * intent. One declaration serves both, so the two routes cannot drift
		 * into doing different things — and the button is also what gives the
		 * action to the keyboard, which a swipe never can.
		 */
		swipeLeft?: SwipeAction<T>;
		swipeRight?: SwipeAction<T>;
		/**
		 * A tap — touch and pen only, and only a press that stayed a press: not
		 * a swipe, not a long press, not the start of a scroll.
		 *
		 * It exists because a mouse's primary action on these rows is often a
		 * DOUBLE click (the playlist plays a track that way), and `dblclick`
		 * never fires on touch. So the row's main action was reachable with a
		 * mouse and unreachable with a finger. Handling it here rather than in
		 * each row's own `onclick` is what keeps it from firing on the tap that
		 * ends a swipe.
		 */
		ontap?: (item: T, index: number) => void;
		/**
		 * Press and hold, touch and pen only — a mouse has the right button for
		 * this. Nothing consumes it yet; it is here because the gesture is
		 * already half-detected by the swipe's own slop and timing, and a
		 * consumer adding it later would otherwise re-implement the cancelling
		 * (and get it subtly wrong) in its own row snippet.
		 *
		 * A long press that fires swallows the click that would follow it, and
		 * the native callout is suppressed on rows that have one, so the
		 * handler can open a menu without the browser opening its own.
		 */
		onlongpress?: (item: T, index: number) => void;
		/**
		 * How far the action buttons sit from the row's trailing edge.
		 *
		 * They hover over the row's last cell — a duration, in all three lists
		 * so far — and that cell's width and the row's padding are the caller's,
		 * so only the caller can say where its centre is. Default: a plain gap
		 * from the edge.
		 */
		actionsInset?: string;
		/** Accessible name for the grip, e.g. "Reorder". */
		gripLabel?: string;
		/**
		 * Extra class on the `<ul>`, for a caller whose layout needs one. Only
		 * useful for a GLOBAL class — a Svelte component's scoped styles do not
		 * reach another component's markup — which is why the list carries its
		 * own flex-and-scroll defaults rather than expecting one.
		 */
		listClass?: string;
		/** Accessible name for the list. */
		listLabel?: string;
		/** `data-testid` for the list, so callers keep their existing pins. */
		listTestId?: string;
	}
	let {
		items,
		key,
		row,
		reorderCapable = false,
		onmove,
		swipeLeft,
		swipeRight,
		ontap,
		onlongpress,
		actionsInset,
		gripLabel = 'Reorder',
		listClass = '',
		listLabel,
		listTestId
	}: Props = $props();

	// ── Reorder by grip ───────────────────────────────────────────────────
	//
	// POINTER events, not HTML5 drag-and-drop: `dragstart` never fires on
	// touch, so a grip wired to drag-and-drop is dead on a phone. Pointer
	// events cover mouse, touch and pen with one implementation, so there is
	// no second gesture to keep in step with the first.
	//
	// The grip captures the pointer, which keeps the moves coming even when the
	// finger strays off the row, and `touch-action: none` on it stops the
	// browser treating the drag as a scroll.

	/** The row picked up, and the slot it is currently over. */
	let dragFrom = $state<number | null>(null);
	let dragOver = $state<number | null>(null);
	let listEl = $state<HTMLElement | null>(null);
	/**
	 * How far the carried row has travelled, so it follows the pointer.
	 *
	 * Drag-and-drop drew a drag image for free; pointer events draw nothing,
	 * so without this the row stays put while only the insertion line moves —
	 * and the gesture looks like it never took hold.
	 */
	let dragStartY = 0;
	let dragY = $state(0);

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
		autoScroll(e.clientY);
	}

	function onGripUp() {
		stopAutoScroll();
		if (dragFrom !== null && dragOver !== null) void onmove?.(dragFrom, dragOver);
		dragFrom = null;
		dragOver = null;
		dragY = 0;
	}

	/**
	 * Where the carried row would land, as an index into the list once that
	 * row has been lifted out of it.
	 *
	 * Counted from the STATIONARY rows rather than hit-tested. Hit-testing
	 * failed in both directions: the carried row follows the pointer, so
	 * `elementFromPoint` either returned the dragged row itself — making the
	 * target equal the source, so every drop was a no-op — or, once that row
	 * was made transparent to hit-testing, returned nothing at all over the
	 * slot it had vacated, leaving the target stuck on the last neighbour
	 * crossed. That is why a row could not be dropped back where it started.
	 *
	 * Counting midpoints has neither problem and needs no special case at
	 * either end: above everything gives 0, below everything gives the last
	 * index. It does assume every row is rendered, which rules out a
	 * virtualised list — fine for all three consumers, and the reason this is
	 * written down.
	 */
	function targetIndex(clientY: number): number {
		const rows = [...(listEl?.querySelectorAll<HTMLElement>('[data-row-index]') ?? [])];
		let index = 0;
		for (const el of rows) {
			// The dragged row's rect has moved with the pointer, so it says
			// nothing about where the gap is.
			if (Number(el.dataset.rowIndex) === dragFrom) continue;
			const rect = el.getBoundingClientRect();
			if (clientY > rect.top + rect.height / 2) index++;
		}
		return index;
	}

	/**
	 * Which edge of row `i` carries the insertion line, if any.
	 *
	 * The line marks the GAP the row would drop into, so it is drawn on the
	 * stationary row beside that gap — never on the dragged row, which is
	 * translated and would carry the line off with it. `dragOver` counts
	 * positions in the lifted list, so row `dragOver` of the remaining rows
	 * takes a top line, and the last of them takes a bottom line when the row
	 * would land at the end. A fixed top line could not express "after the
	 * last row", which made the end of the list look unreachable.
	 */
	function dropEdge(i: number): 'top' | 'bottom' | null {
		if (dragFrom === null || dragOver === null) return null;
		const remaining = items.map((_, n) => n).filter((n) => n !== dragFrom);
		if (dragOver < remaining.length) return remaining[dragOver] === i ? 'top' : null;
		return remaining[remaining.length - 1] === i ? 'bottom' : null;
	}

	/**
	 * Reorder from the keyboard, which the gestures alone leave out entirely.
	 *
	 * Plain Arrow keys on the focused grip, one step per press, rather than a
	 * pick-up-and-place mode: there is no state to explain, no way to get
	 * stranded holding a row, and it reads the same to a screen reader as it
	 * does on screen.
	 */
	function onGripKeydown(e: KeyboardEvent, i: number) {
		const to = e.key === 'ArrowUp' ? i - 1 : e.key === 'ArrowDown' ? i + 1 : null;
		if (to === null) return;
		e.preventDefault();
		if (to < 0 || to >= items.length) return;
		void onmove?.(i, to);
		// The grip travels with its row, so focus has to be put back on the one
		// that is now at the destination — otherwise a second press moves a
		// different row, and holding the key walks the focus instead of the row.
		const grips = listEl?.querySelectorAll<HTMLElement>('[data-grip-index]');
		requestAnimationFrame(() => grips?.[to]?.focus());
	}

	// ── Edge auto-scroll ──────────────────────────────────────────────────
	//
	// Browsers auto-scroll during a native drag; pointer events get nothing of
	// the sort, so without this a long list can only be reordered as far as the
	// viewport reaches. `edgeStep` is shared with the files browser's
	// drag-and-drop scroller — the ramp and the saturation past the edge are
	// the parts worth having in one place.

	let scrollStep = 0;
	let scrollFrame = 0;

	function autoScroll(clientY: number) {
		if (!listEl) return;
		const rect = listEl.getBoundingClientRect();
		scrollStep = edgeStep(clientY, rect.top, rect.bottom);
		if (scrollStep !== 0 && scrollFrame === 0) scrollFrame = requestAnimationFrame(scrollTick);
	}

	function scrollTick() {
		scrollFrame = 0;
		if (!listEl || scrollStep === 0 || dragFrom === null) return;
		const before = listEl.scrollTop;
		listEl.scrollTop = before + scrollStep;
		// Already at that end: spinning a frame loop that cannot move anything
		// is pure waste, and the next pointermove will start it again.
		if (listEl.scrollTop === before) return;
		// The rows moved under a stationary finger, so the target moved too.
		dragOver = targetIndex(dragStartY + dragY);
		scrollFrame = requestAnimationFrame(scrollTick);
	}

	function stopAutoScroll() {
		scrollStep = 0;
		if (scrollFrame !== 0) {
			cancelAnimationFrame(scrollFrame);
			scrollFrame = 0;
		}
	}

	// ── Swipe to act ──────────────────────────────────────────────────────
	//
	// TOUCH AND PEN ONLY, and the division matters: a pointer that can hover
	// has somewhere to put a button — the row can reveal one under the cursor —
	// so a mouse needs no swipe, and giving it one only means a stray drag
	// flings a row and fires a destructive action. A finger has no hover, so
	// the swipe is the whole affordance there. One route per input kind.
	//
	// Up to three gestures then share a row on touch: tap (the caller's own
	// click), drag the grip, swipe. So the swipe stays dormant until the
	// movement is clearly HORIZONTAL — until then the browser keeps the gesture
	// and the list scrolls normally, which is what anyone flicking through a
	// long list expects.

	/** Horizontal travel that claims the gesture as a swipe. */
	const SWIPE_CLAIM_PX = 8;
	/**
	 * Share of the row's own width the gesture must cross to fire on release.
	 * Half the row is a distance the hand can feel, where a fixed pixel count
	 * means something different on every screen — these rows are 380px in the
	 * player's popover and the full width of a phone on `/player`.
	 */
	const SWIPE_FIRE_RATIO = 0.5;
	/** Only for a row that reports no width, where a ratio says nothing. */
	const SWIPE_FIRE_FALLBACK_PX = 96;

	let swipeIndex = $state<number | null>(null);
	let swipeX = $state(0);
	/** Claimed: the row follows the finger and the list no longer scrolls. */
	let swiping = $state(false);
	/**
	 * The row sliding back after a release that did not fire. The backing has
	 * to stay lit for that trip: cleared with `swipeIndex` it would blink out
	 * and leave the list's own background showing through the gap, as if the
	 * row had gone after all.
	 */
	let settlingIndex = $state<number | null>(null);
	/**
	 * A gesture has claimed this press, so the click it ends with must not
	 * reach the row's own handler. Set by a swipe and by a long press — both
	 * finish with a `click` the browser synthesises from the same touch.
	 */
	let suppressClick = false;
	let swipeStart = { x: 0, y: 0 };
	/** Measured at pointerdown: mid-gesture the row is translated, not resized. */
	let swipeWidth = 0;
	/** The row under a touch or pen press, and whether it has travelled. */
	let pressIndex: number | null = null;
	let pressMoved = false;

	/** The action the current travel would fire, if any. */
	const swipeAction = $derived(swipeX < 0 ? swipeLeft : swipeX > 0 ? swipeRight : undefined);

	/**
	 * The same actions as buttons, for pointers that can hover. Destructive
	 * last, so the rightmost thing in the row is not what people reach for
	 * first and the order matches a dialog's.
	 */
	const hoverActions = $derived(
		(
			[
				{ dir: 'right', action: swipeRight },
				{ dir: 'left', action: swipeLeft }
			] as const
		).filter((slot): slot is { dir: 'left' | 'right'; action: SwipeAction<T> } => !!slot.action)
	);

	/** Which row a hover-capable pointer is over, for `RowState.hovered`. */
	let hoverIndex = $state<number | null>(null);

	/** True while row `i` shows an action backing. */
	function swipeOpen(i: number): boolean {
		return (swiping && swipeIndex === i) || settlingIndex === i;
	}

	/** How far the gesture must go for the release to fire. */
	function swipeThreshold(): number {
		return swipeWidth > 0 ? swipeWidth * SWIPE_FIRE_RATIO : SWIPE_FIRE_FALLBACK_PX;
	}

	/**
	 * Past the threshold: releasing now fires the action. The backing goes to
	 * full strength to say so, which is the only way the gesture can report
	 * that it has gone far enough BEFORE the finger lifts and the answer is
	 * final.
	 */
	function swipeArmed(i: number): boolean {
		return swipeIndex === i && Math.abs(swipeX) > swipeThreshold();
	}

	// ── Press and hold ────────────────────────────────────────────────────

	/** Held this long without travelling, and it is a long press. */
	const LONG_PRESS_MS = 500;
	/** Travel that cancels it: anything more is a swipe or a scroll. */
	const LONG_PRESS_SLOP_PX = 10;

	let longPressTimer = 0;

	function cancelLongPress() {
		if (longPressTimer !== 0) {
			clearTimeout(longPressTimer);
			longPressTimer = 0;
		}
	}

	function armLongPress(i: number) {
		if (!onlongpress) return;
		longPressTimer = window.setTimeout(() => {
			longPressTimer = 0;
			// The list may have changed under the finger while it was held.
			const item = items[i];
			if (item === undefined) return;
			// It fires mid-press, so the release still produces a click.
			suppressClick = true;
			onlongpress?.(item, i);
		}, LONG_PRESS_MS);
	}

	function onRowDown(e: PointerEvent, i: number) {
		if (e.pointerType === 'mouse') return;
		// Shared by every gesture here, so it is measured before any claims.
		swipeStart = { x: e.clientX, y: e.clientY };
		swipeWidth = (e.currentTarget as HTMLElement).getBoundingClientRect().width;
		suppressClick = false;
		pressIndex = i;
		pressMoved = false;
		armLongPress(i);
		if (!swipeLeft && !swipeRight) return;
		swipeIndex = i;
		swipeX = 0;
		swiping = false;
	}

	function onRowMove(e: PointerEvent, i: number) {
		const dx = e.clientX - swipeStart.x;
		const dy = e.clientY - swipeStart.y;
		// Travel ends a hold AND a tap, whichever way it goes — including the
		// vertical scroll that is by far the most common thing to start on a
		// row. A flick that scrolls the list must not also play the track it
		// started on.
		if (Math.abs(dx) > LONG_PRESS_SLOP_PX || Math.abs(dy) > LONG_PRESS_SLOP_PX) {
			cancelLongPress();
			pressMoved = true;
		}
		if (swipeIndex !== i) return;
		if (!swiping) {
			// Vertical first: let go of the gesture entirely so the list can
			// scroll. Reclaiming it mid-scroll would feel like the row snatching
			// at the finger.
			if (Math.abs(dy) > SWIPE_CLAIM_PX && Math.abs(dy) >= Math.abs(dx)) {
				swipeIndex = null;
				return;
			}
			if (Math.abs(dx) <= SWIPE_CLAIM_PX || Math.abs(dx) <= Math.abs(dy)) return;
			// …and only towards an action that exists: without this a list with
			// one direction wired claims the other, travels nowhere, and leaves
			// the backing lit on a gesture that can never fire.
			if (dx < 0 && !swipeLeft) return;
			if (dx > 0 && !swipeRight) return;
			swiping = true;
			suppressClick = true;
			cancelLongPress();
			(e.currentTarget as HTMLElement).setPointerCapture(e.pointerId);
		}
		swipeX = clampTravel(dx);
	}

	/** Travel is only allowed towards a direction that has an action. */
	function clampTravel(dx: number): number {
		if (dx < 0) return swipeLeft ? dx : 0;
		if (dx > 0) return swipeRight ? dx : 0;
		return 0;
	}

	function onRowUp(i: number) {
		cancelLongPress();
		const action = swipeAction;
		if (swipeArmed(i) && action) {
			void action.run(items[i], i);
			// The row may or may not go — a dismissal removes it, an "add to
			// queue" leaves it — so reset as if it stays. If it goes, the
			// `{#each}` key takes the whole row with it anyway.
			settlingIndex = null;
		} else if (swiping && swipeIndex === i && swipeX !== 0) {
			// Keep the backing lit until the row has slid home; `onSettled`
			// turns it off when the transform transition reports it has. Only
			// when there is travel to undo: with none, no transition runs, no
			// `transitionend` arrives, and the backing would stay lit for good.
			settlingIndex = i;
		}
		// A press that stayed a press. `suppressClick` is the one test that
		// covers both of the things it could have become: a swipe sets it when
		// it claims the pointer, a long press when it fires.
		if (ontap && pressIndex === i && !pressMoved && !suppressClick && items[i] !== undefined) {
			ontap(items[i], i);
		}
		pressIndex = null;
		swipeIndex = null;
		swipeX = 0;
		swiping = false;
	}

	function onSettled(i: number) {
		if (settlingIndex === i) settlingIndex = null;
	}

	/**
	 * A swipe ends with a click on the row underneath it. Swallowed HERE, in
	 * the capture phase, rather than left to each caller: every one of them
	 * puts something clickable in the row, and a swipe that also triggered it
	 * would dismiss a notification and then navigate to the resource it was
	 * about.
	 */
	function onRowClickCapture(e: MouseEvent) {
		if (!suppressClick) return;
		suppressClick = false;
		e.preventDefault();
		e.stopPropagation();
	}

	/**
	 * The native long-press menu would land on top of whatever `onlongpress`
	 * opens, so a row that has one answers for the gesture itself. Rows
	 * without a handler keep the browser's own behaviour, including
	 * press-and-hold to copy a link.
	 */
	function onRowContextMenu(e: Event) {
		if (onlongpress) e.preventDefault();
	}
</script>

<ul
	class="gesture-list {listClass}"
	bind:this={listEl}
	aria-label={listLabel}
	data-testid={listTestId}
	style:--gesture-actions-inset={actionsInset}
>
	{#each items as item, i (key(item, i))}
		<li
			class="gesture-row"
			class:drag-over-top={dropEdge(i) === 'top'}
			class:drag-over-bottom={dropEdge(i) === 'bottom'}
			class:dragging={dragFrom === i}
			class:swipe-open={swipeOpen(i)}
			class:swipe-armed={swipeArmed(i)}
			class:holdable={!!onlongpress}
			data-row-index={i}
			onpointerdown={(e) => onRowDown(e, i)}
			onpointermove={(e) => onRowMove(e, i)}
			onpointerup={() => onRowUp(i)}
			onpointercancel={() => onRowUp(i)}
			onclickcapture={onRowClickCapture}
			oncontextmenu={onRowContextMenu}
			onpointerenter={(e) => {
				if (e.pointerType === 'mouse') hoverIndex = i;
			}}
			onpointerleave={() => {
				if (hoverIndex === i) hoverIndex = null;
			}}
		>
			<!--
				The backing the row slides off, and ONLY while it is sliding: an
				icon parked under every row would advertise a gesture on the one
				device that cannot make it, and a permanent coloured stripe under
				the list would read as a warning. Behind the content rather than
				beside it, so the list keeps its column widths and nothing
				reflows mid-gesture.
			-->
			{#if swipeOpen(i) && swipeAction}
				<span
					class="gesture-backing"
					class:add={swipeAction.tone === 'add'}
					class:from-left={swipeX > 0}
					aria-hidden="true"
				>
					<Icon name={swipeAction.icon} />
					<!-- The label, not just the glyph: an icon alone asks the user to
					     remember which direction does what, and a swipe is the one
					     affordance with room to say it outright. -->
					<span class="gesture-backing-label">{swipeAction.label}</span>
				</span>
			{/if}
			<!--
				`settling` is keyed on the row sliding HOME, not on "not being
				swiped": the carried row's `--drag-y` is a transform too, so the
				broader condition put a 150ms transition on it and the row
				trailed the finger.
			-->
			<div
				class="gesture-slide"
				class:settling={settlingIndex === i}
				style:transform={swipeIndex === i ? `translateX(${swipeX}px)` : undefined}
				style:--drag-y={dragFrom === i ? `${dragY}px` : undefined}
				ontransitionend={() => onSettled(i)}
			>
				{#if reorderCapable}
					<!--
						A real button, not a decorated span: it is the only way to
						reorder without a pointer, and `aria-label` plus Arrow keys
						make it say so. The drag lives on the GRIP rather than the
						row because the row is clickable in every consumer, and a
						drag starting anywhere on it would fight that click.
					-->
					<button
						type="button"
						class="gesture-grip"
						aria-label={gripLabel}
						data-grip-index={i}
						onpointerdown={(e) => onGripDown(e, i)}
						onpointermove={onGripMove}
						onpointerup={onGripUp}
						onpointercancel={onGripUp}
						onkeydown={(e) => onGripKeydown(e, i)}
					>
						<Icon name="grip-vertical" />
					</button>
				{/if}
				{@render row(item, i, {
					hovered: hoverIndex === i,
					dragging: dragFrom === i,
					swiping: swiping && swipeIndex === i
				})}
				<!--
					The hover half of the actions. A SIBLING of the row content,
					never inside it: every consumer's row content is itself
					clickable, and a button inside a button is invalid markup
					whose inner click never arrives. Absolute, so revealing it
					cannot nudge the row's layout.
				-->
				{#if hoverActions.length > 0}
					<span class="gesture-actions">
						<!-- Keyed by the DIRECTION it belongs to: two actions can
						     legitimately share an icon and a label, and a key made
						     of those collides. -->
						{#each hoverActions as { dir, action } (dir)}
							<button
								type="button"
								class="gesture-action"
								class:add={action.tone === 'add'}
								title={action.label}
								aria-label={action.label}
								data-testid={action.testId?.(item, i)}
								onclick={() => action.run(item, i)}
							>
								<Icon name={action.icon} />
							</button>
						{/each}
					</span>
				{/if}
			</div>
		</li>
	{/each}
</ul>

<style>
	.gesture-list {
		display: flex;
		/* `1 1 auto`, NOT `flex: 1` — that shorthand means basis `0%`, so the
		   list claims none of its own content's height and whatever surrounds
		   it keeps its natural size instead. In a capped box (the player's
		   popover is `max-height: 480px`) the rows then got a sliver that
		   shrank further with every track added. With basis `auto` the list
		   asks for its content and yields it under pressure — which, with
		   `min-height: 0`, is what makes the scroll engage rather than the
		   rows compress. */
		flex: 1 1 auto;
		flex-direction: column;
		min-height: 0;
		/* The scroller is this element, so the edge auto-scroll has one box to
		   reason about wherever the list is placed. */
		overflow-y: auto;
		list-style: none;
		margin: 0;
		padding: 0;
	}

	.gesture-row {
		position: relative;
		display: flex;
		/* A row keeps its height, always.

		   The list is a flex column, and a flex item shrinks by default — so
		   wherever the list has a definite height (the player's popover caps
		   at 480px) the ROWS compressed toward their min-content instead of
		   the list overflowing. Flex resolves before overflow, so
		   `overflow-y: auto` had nothing left to scroll: every track added
		   made every row thinner. On a page, where the column is auto-height
		   and nothing pushes back, the same CSS looked perfectly fine — which
		   is why this only ever showed up in the dropdown. */
		flex-shrink: 0;
		align-items: center;
		border-radius: var(--radius-md);
		/* The swiped row must not spill over its neighbours as it travels. */
		overflow: hidden;
		/* Vertical pans belong to the list, horizontal ones to the row. Left to
		   itself the browser decides on the first few pixels and keeps whatever
		   it chose, so a swipe that began slightly off-axis was taken for a
		   scroll and the moves stopped arriving. */
		touch-action: pan-y;
	}

	/* A row that answers press-and-hold answers for the whole gesture: without
	   this iOS raises its own callout and Android starts a text selection on
	   top of whatever the handler opened. */
	.gesture-row.holdable {
		-webkit-touch-callout: none;
		user-select: none;
	}

	/* Everything the finger carries. Transformed rather than repositioned, so
	   the browser can move it on the compositor and the columns inside keep
	   their widths. */
	.gesture-slide {
		position: relative;
		display: flex;
		flex: 1;
		min-width: 0;
		align-items: center;
		/* Set by the drag, read by `.dragging` below. A custom property rather
		   than a second inline transform, because the two gestures must never
		   write the same declaration. */
		transform: translateY(var(--drag-y, 0));
	}

	/* Only while nothing is held: a transition during the drag would lag the
	   finger, and one on release is exactly what makes the row snap back. */
	.gesture-slide.settling {
		transition: transform var(--motion-fast, 0.15s) ease-out;
	}

	/* The action backing: a coloured field the row slides off, uncovered from
	   the side the gesture came from. Lit only for the row under the finger.

	   TWO STRENGTHS, and they carry the real information: dimmed while the row
	   is short of the threshold, full once past it. So the field answers "would
	   letting go do it?" while the finger is still down — without it the only
	   way to find out is to lift and see.

	   The opacity lives HERE rather than on the row: the row is the content's
	   parent, so fading it would fade the content travelling over it, and the
	   two strengths would read as the text dimming instead. */
	.gesture-backing {
		position: absolute;
		inset: 0;
		display: flex;
		align-items: center;
		justify-content: flex-end;
		gap: var(--space-2);
		padding-inline: var(--space-3);
		border-radius: var(--radius-md);
		background: var(--color-danger-bg);
		color: var(--color-danger-text);
		/* Half strength: the jump to full has to be obvious at arm's length,
		   mid-gesture, with a finger over the row. */
		opacity: 0.5;
		transition: opacity var(--motion-fast, 0.15s) ease-out;
	}

	/* Swiping right uncovers the LEFT edge, so that is where its icon waits. */
	.gesture-backing.from-left {
		justify-content: flex-start;
	}

	.gesture-backing-label {
		font-size: var(--text-sm);
		font-weight: var(--weight-semibold);
		/* The row slides over this; the label must not wrap or re-flow as the
		   uncovered strip grows. */
		white-space: nowrap;
	}

	/* Anything that ADDS rather than destroys: solid blue against the danger
	   red. Not green — see the `tone` doc for why — and not the pale
	   `--color-info-bg`, which is the tint used behind text and would read as
	   a disabled row at this size. */
	.gesture-backing.add {
		background: var(--color-info-blue);
		color: var(--color-on-accent);
	}

	.gesture-row.swipe-armed .gesture-backing {
		opacity: 1;
	}

	/* The content travelling over it has to be opaque, or the backing shows
	   through the row as well as beside it. Only while open, so an idle list
	   keeps its transparent rows. */
	.gesture-row.swipe-open .gesture-slide {
		background: var(--color-bg-surface);
		border-radius: var(--radius-md);
	}

	/* The actions as buttons, for a pointer that can hover.

	   `@media (hover: hover)` is the other half of the input-kind split: on a
	   phone `:hover` latches after a tap, so without the query the row you
	   just tapped keeps a button for the gesture that device does not use —
	   over whatever the row was showing there. Keyboard reach is
	   `:has(:focus-visible)` rather than `:focus-within`, OUTSIDE the query,
	   because focus-within also fires when a finger taps the row and brought
	   the same buttons back through the other door. */
	.gesture-actions {
		position: absolute;
		right: var(--gesture-actions-inset, var(--space-2));
		display: none;
		gap: var(--space-1);
		align-items: center;
	}

	@media (hover: hover) {
		.gesture-row:hover .gesture-actions {
			display: flex;
		}
	}

	.gesture-row:has(:focus-visible) .gesture-actions {
		display: flex;
	}

	.gesture-action {
		display: flex;
		width: 24px;
		height: 24px;
		align-items: center;
		justify-content: center;
		border: none;
		border-radius: var(--radius-sm);
		background: none;
		color: var(--color-text-muted);
		cursor: pointer;
	}

	.gesture-action:hover {
		color: var(--color-error-text);
	}

	.gesture-action.add:hover {
		color: var(--color-info-text);
	}

	/* Where the dragged row would land — an edge rather than a filled
	   background, so it reads as an insertion point and not as a selection.

	   A pseudo-element with its own `z-index` rather than an inset shadow: the
	   carried row is opaque and sits at `z-index: 1`, and because it follows
	   the pointer it covers precisely the row being targeted — so a shadow
	   painted by that row went under it and the line vanished exactly when it
	   was needed. The target row has no `z-index` of its own, so this child
	   competes directly in the list's stacking context and wins. */
	.gesture-row.drag-over-top::before,
	.gesture-row.drag-over-bottom::after {
		content: '';
		position: absolute;
		z-index: 3;
		right: 0;
		left: 0;
		height: 2px;
		background: var(--color-accent);
	}

	.gesture-row.drag-over-top::before {
		top: 0;
	}

	.gesture-row.drag-over-bottom::after {
		bottom: 0;
	}

	.gesture-grip {
		display: flex;
		flex-shrink: 0;
		align-items: center;
		/* Thumb-sized on a phone, where this is the only way to reorder. */
		padding: var(--space-2) var(--space-1);
		border: none;
		background: none;
		color: var(--color-text-faint);
		cursor: grab;
		/* Without this the browser claims the gesture as a scroll and the
		   pointermove events stop arriving mid-drag — the single thing that
		   makes touch reordering work at all. */
		touch-action: none;
	}

	.gesture-grip:active {
		cursor: grabbing;
	}

	/* The row being carried. It FOLLOWS the pointer and lifts off the list,
	   because pointer events draw no drag image of their own — without this the
	   row stays put while only the insertion line moves, and the gesture looks
	   like it never took hold.

	   `position: relative` + `z-index` so it passes over its neighbours rather
	   than under them; `overflow: visible` because the row clips its own
	   children for the swipe, which would otherwise cut the shadow. */
	.gesture-row.dragging {
		z-index: 1;
		overflow: visible;
		/* Invisible to hit-testing. The row follows the pointer, so it IS the
		   element under the finger — see `targetIndex` for the two ways that
		   broke the drop before the midpoint count replaced hit-testing. The
		   gesture is unaffected: the grip holds a pointer capture, and capture
		   delivers events regardless of hit-testing. */
		pointer-events: none;
	}

	.gesture-row.dragging .gesture-slide {
		background: var(--color-bg-surface);
		border-radius: var(--radius-md);
		box-shadow: var(--shadow-lg);
		opacity: 0.9;
	}
</style>
