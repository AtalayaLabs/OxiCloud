import { describe, it, expect, vi } from 'vitest';
import { render, fireEvent } from '@testing-library/svelte';
import Harness from './__fixtures__/GestureListHarness.svelte';

/**
 * The gestures that `GestureList` owns, pinned where they live rather than
 * through one of the three lists that use it. Every one of these started as a
 * bug Ed hit in a browser on the player queue — jsdom has no layout and no
 * touch, so none of them could have been caught by the unit suite as it was.
 */

/**
 * jsdom gives every element a zero rect, and the drag decides where a row
 * lands by comparing the pointer against the rows' midpoints. So the rows need
 * a geometry to reason about: 40px slots stacked from y=0, 100px wide (which
 * puts the swipe threshold — half the row — at 50).
 */
function layout(rows: ArrayLike<HTMLElement>): void {
	Array.from(rows).forEach((row, i) => {
		row.getBoundingClientRect = () =>
			({
				top: i * 40,
				bottom: i * 40 + 40,
				height: 40,
				left: 0,
				right: 100,
				width: 100,
				x: 0,
				y: i * 40,
				toJSON: () => ({})
			}) as DOMRect;
	});
}

/** The swipe is touch-and-pen only, so every swipe here has to say so. */
const touch = { pointerId: 1, pointerType: 'touch' } as const;

const ITEMS = [{ id: 'a' }, { id: 'b' }, { id: 'c' }];

function remove(run = vi.fn()) {
	return { action: { icon: 'eraser', label: 'Remove', tone: 'danger' as const, run }, run };
}

function setup(props: Record<string, unknown> = {}) {
	const { container } = render(Harness, { initial: ITEMS, reorderCapable: true, ...props });
	const rows = container.querySelectorAll<HTMLElement>('[data-row-index]');
	layout(rows);
	// jsdom implements neither of these: `setPointerCapture` is what keeps the
	// moves coming once a gesture has claimed the pointer.
	rows.forEach((row) => (row.setPointerCapture = vi.fn()));
	return { container, rows };
}

function grip(row: HTMLElement): HTMLElement {
	const el = row.querySelector<HTMLElement>('.gesture-grip')!;
	el.setPointerCapture = vi.fn();
	return el;
}

describe('reorder by grip', () => {
	it('moves the carried row to the slot it was released over', async () => {
		const onmoved = vi.fn();
		const { rows } = setup({ onmoved });
		const handle = grip(rows[0]);

		await fireEvent.pointerDown(handle, { pointerId: 1 });
		// Past b's midpoint (20) and c's (100 would be past the end) — y 90 is
		// past b, short of c, so a lands between them.
		await fireEvent.pointerMove(handle, { pointerId: 1, clientX: 0, clientY: 90 });
		await fireEvent.pointerUp(handle, { pointerId: 1 });

		expect(onmoved).toHaveBeenCalledWith(['b', 'a', 'c']);
	});

	// Dropping past the last row was impossible while the target was
	// hit-tested: there is nothing under the pointer below the final row, so
	// the target stayed on the last one crossed and the row moved by one.
	it('moves a row to the end when released below every row', async () => {
		const onmoved = vi.fn();
		const { rows } = setup({ onmoved });
		const handle = grip(rows[0]);

		await fireEvent.pointerDown(handle, { pointerId: 1 });
		await fireEvent.pointerMove(handle, { pointerId: 1, clientX: 0, clientY: 200 });
		await fireEvent.pointerUp(handle, { pointerId: 1 });

		expect(onmoved).toHaveBeenCalledWith(['b', 'c', 'a']);
	});

	// The counterpart, and the reason hit-testing was abandoned rather than
	// patched: with the carried row transparent to hit-testing, NOTHING
	// occupies the slot it came from, so a row could not be put back where it
	// started — the target stuck to the last neighbour crossed.
	it('leaves the order alone when a row is dropped back on its own slot', async () => {
		const onmoved = vi.fn();
		const { rows } = setup({ onmoved });
		const handle = grip(rows[1]);

		await fireEvent.pointerDown(handle, { pointerId: 1 });
		await fireEvent.pointerMove(handle, { pointerId: 1, clientX: 0, clientY: 110 });
		await fireEvent.pointerMove(handle, { pointerId: 1, clientX: 0, clientY: 50 });
		await fireEvent.pointerUp(handle, { pointerId: 1 });

		expect(onmoved).toHaveBeenCalledWith(['a', 'b', 'c']);
	});

	// The line marks the edge the row lands against, which depends on the
	// direction of travel. A fixed top line cannot express "after the last
	// row", so the end of the list looked unreachable even though the drop
	// worked.
	it('marks the bottom edge when a row is dragged down past every midpoint', async () => {
		const { rows } = setup();
		const handle = grip(rows[0]);
		await fireEvent.pointerDown(handle, { pointerId: 1 });
		await fireEvent.pointerMove(handle, { pointerId: 1, clientX: 0, clientY: 110 });
		expect(rows[2].classList.contains('drag-over-bottom')).toBe(true);
		expect(rows[2].classList.contains('drag-over-top')).toBe(false);
	});

	it('marks the top edge when a row is dragged up above every midpoint', async () => {
		const { rows } = setup();
		const handle = grip(rows[2]);
		await fireEvent.pointerDown(handle, { pointerId: 1 });
		await fireEvent.pointerMove(handle, { pointerId: 1, clientX: 0, clientY: 1 });
		expect(rows[0].classList.contains('drag-over-top')).toBe(true);
		expect(rows[0].classList.contains('drag-over-bottom')).toBe(false);
	});

	// Carried back over its own slot the line has to appear on a NEIGHBOUR:
	// the dragged row travels with the pointer, so a line on it would travel
	// too instead of marking the place it would land.
	it('marks a neighbour when the row is carried back to its own slot', async () => {
		const { rows } = setup();
		const handle = grip(rows[1]);

		await fireEvent.pointerDown(handle, { pointerId: 1 });
		await fireEvent.pointerMove(handle, { pointerId: 1, clientX: 0, clientY: 50 });

		expect(rows[2].classList.contains('drag-over-top')).toBe(true);
		expect(rows[1].classList.contains('drag-over-top')).toBe(false);
		expect(rows[1].classList.contains('drag-over-bottom')).toBe(false);
		await fireEvent.pointerUp(handle, { pointerId: 1 });
	});

	// Arrow keys on the grip are the only route to reordering without a
	// pointer. The grip is a real button for exactly this reason.
	it('reorders from the keyboard, and refuses to walk off either end', async () => {
		const onmoved = vi.fn();
		const { rows } = setup({ onmoved });

		await fireEvent.keyDown(grip(rows[0]), { key: 'ArrowDown' });
		expect(onmoved).toHaveBeenLastCalledWith(['b', 'a', 'c']);

		const after = document.querySelectorAll<HTMLElement>('[data-row-index]');
		await fireEvent.keyDown(grip(after[0]), { key: 'ArrowUp' });
		// Still one call: moving the first row up has nowhere to go.
		expect(onmoved).toHaveBeenCalledTimes(1);
	});

	it('renders no grip when reordering is not offered', () => {
		const { container } = render(Harness, { initial: ITEMS, reorderCapable: false });
		expect(container.querySelector('.gesture-grip')).toBeNull();
	});
});

describe('swipe to act', () => {
	it('runs the action on a swipe past half the row', async () => {
		const { action, run } = remove();
		const { rows } = setup({ swipeLeft: action });

		await fireEvent.pointerDown(rows[1], { ...touch, clientX: 90, clientY: 50 });
		await fireEvent.pointerMove(rows[1], { ...touch, clientX: 30, clientY: 50 });
		await fireEvent.pointerUp(rows[1], { ...touch });

		// 60 of 100: fired, with the item and its index.
		expect(run).toHaveBeenCalledWith({ id: 'b' }, 1);
	});

	// Half the ROW, not a pixel count: these rows are 380px in the player's
	// popover and the full width of a phone on `/player`, so one distance is a
	// different share of the gesture on every screen.
	it('keeps the row when the swipe stops short of half', async () => {
		const { action, run } = remove();
		const { rows } = setup({ swipeLeft: action });

		await fireEvent.pointerDown(rows[1], { ...touch, clientX: 90, clientY: 50 });
		await fireEvent.pointerMove(rows[1], { ...touch, clientX: 50, clientY: 50 });
		await fireEvent.pointerUp(rows[1], { ...touch });

		// 40 of 100: not fired.
		expect(run).not.toHaveBeenCalled();
	});

	// Each direction is its own handler, so a list can offer two different
	// things — the playlist removes to the left and queues to the right.
	it('runs the right-hand action on a swipe the other way', async () => {
		const left = remove();
		const right = remove();
		const { rows } = setup({ swipeLeft: left.action, swipeRight: right.action });

		await fireEvent.pointerDown(rows[0], { ...touch, clientX: 10, clientY: 10 });
		await fireEvent.pointerMove(rows[0], { ...touch, clientX: 80, clientY: 10 });
		await fireEvent.pointerUp(rows[0], { ...touch });

		expect(right.run).toHaveBeenCalledWith({ id: 'a' }, 0);
		expect(left.run).not.toHaveBeenCalled();
	});

	// The backing names the action in words, and the two directions are told
	// apart by icon and text as well as by colour. That is the requirement
	// rather than a nicety: red against blue survives colour blindness, but
	// nothing about a colour pair survives a screen reader or a greyscale
	// print, so the label is what actually carries the meaning.
	it('names the action on the backing, per direction', async () => {
		const left = {
			icon: 'eraser',
			label: 'Remove',
			tone: 'danger' as const,
			run: vi.fn()
		};
		const right = { icon: 'plus', label: 'Add to queue', tone: 'add' as const, run: vi.fn() };
		const { rows } = setup({ swipeLeft: left, swipeRight: right });

		await fireEvent.pointerDown(rows[0], { ...touch, clientX: 50, clientY: 10 });
		await fireEvent.pointerMove(rows[0], { ...touch, clientX: 20, clientY: 10 });
		const leftBacking = rows[0].querySelector('.gesture-backing')!;
		expect(leftBacking.textContent).toContain('Remove');
		expect(leftBacking.classList.contains('add')).toBe(false);
		await fireEvent.pointerUp(rows[0], { ...touch });

		await fireEvent.pointerDown(rows[1], { ...touch, clientX: 20, clientY: 50 });
		await fireEvent.pointerMove(rows[1], { ...touch, clientX: 60, clientY: 50 });
		const rightBacking = rows[1].querySelector('.gesture-backing')!;
		expect(rightBacking.textContent).toContain('Add to queue');
		// The tone is a class, so the stylesheet owns the colour — and the two
		// directions cannot end up wearing the same one.
		expect(rightBacking.classList.contains('add')).toBe(true);
		await fireEvent.pointerUp(rows[1], { ...touch });
	});

	// A direction with no action must not claim the gesture at all: the row
	// would travel nowhere behind a lit backing, promising something that can
	// never fire.
	it('ignores a swipe towards a direction that has no action', async () => {
		const { action, run } = remove();
		const { rows } = setup({ swipeLeft: action });

		await fireEvent.pointerDown(rows[0], { ...touch, clientX: 10, clientY: 10 });
		await fireEvent.pointerMove(rows[0], { ...touch, clientX: 90, clientY: 10 });
		await fireEvent.pointerUp(rows[0], { ...touch });

		expect(run).not.toHaveBeenCalled();
		expect(rows[0].classList.contains('swipe-open')).toBe(false);
	});

	// The two routes to one action are split by input kind: a pointer that can
	// hover gets a button, a finger gets the swipe. Both at once means a stray
	// mouse drag across a list of clickable rows fires a destructive action.
	it('ignores a left drag made with a mouse, which has the button instead', async () => {
		const { action, run } = remove();
		const { rows, container } = setup({ swipeLeft: action });
		rows[1].setPointerCapture = vi.fn();

		await fireEvent.pointerDown(rows[1], {
			pointerId: 1,
			pointerType: 'mouse',
			clientX: 90,
			clientY: 50
		});
		await fireEvent.pointerMove(rows[1], {
			pointerId: 1,
			pointerType: 'mouse',
			clientX: 10,
			clientY: 50
		});
		await fireEvent.pointerUp(rows[1], { pointerId: 1, pointerType: 'mouse' });

		expect(run).not.toHaveBeenCalled();
		expect(rows[1].setPointerCapture).not.toHaveBeenCalled();

		// The button is the mouse's route to the same action, from the same
		// declaration — which is what keeps the two from drifting apart.
		const buttons = container.querySelectorAll<HTMLElement>('.gesture-action');
		expect(buttons).toHaveLength(3);
		await fireEvent.click(rows[1].querySelector<HTMLElement>('.gesture-action')!);
		expect(run).toHaveBeenCalledWith({ id: 'b' }, 1);
	});

	// A mostly-vertical drag belongs to the list, not to the row: claiming it
	// would stop a long list scrolling under the finger.
	it('leaves a vertical drag to the list', async () => {
		const { action, run } = remove();
		const { rows } = setup({ swipeLeft: action });
		rows[1].setPointerCapture = vi.fn();

		await fireEvent.pointerDown(rows[1], { ...touch, clientX: 90, clientY: 10 });
		await fireEvent.pointerMove(rows[1], { ...touch, clientX: 80, clientY: 90 });
		// Even a long horizontal run afterwards must not resurrect the swipe.
		await fireEvent.pointerMove(rows[1], { ...touch, clientX: 10, clientY: 95 });
		await fireEvent.pointerUp(rows[1], { ...touch });

		expect(run).not.toHaveBeenCalled();
		expect(rows[1].setPointerCapture).not.toHaveBeenCalled();
	});

	// The backing belongs to the gesture, not to the row: at rest it would
	// advertise a swipe on the one device that cannot make one and leave a
	// coloured stripe under every row.
	it('shows the backing only while a row is being swiped', async () => {
		const { action } = remove();
		const { rows, container } = setup({ swipeLeft: action });

		expect(container.querySelector('.gesture-backing')).toBeNull();

		await fireEvent.pointerDown(rows[1], { ...touch, clientX: 90, clientY: 50 });
		// A press is not yet a swipe — it could still be a tap or a scroll.
		expect(rows[1].classList.contains('swipe-open')).toBe(false);

		await fireEvent.pointerMove(rows[1], { ...touch, clientX: 70, clientY: 50 });
		expect(rows[1].classList.contains('swipe-open')).toBe(true);
		expect(container.querySelector('.gesture-backing')).not.toBeNull();

		// Released short of the threshold, it stays lit until the row has slid
		// home — cleared any earlier it blinks out and the list's background
		// shows through the gap, as if the row had gone after all.
		await fireEvent.pointerUp(rows[1], { ...touch });
		expect(rows[1].classList.contains('swipe-open')).toBe(true);
		await fireEvent.transitionEnd(rows[1].querySelector('.gesture-slide')!);
		expect(rows[1].classList.contains('swipe-open')).toBe(false);
	});

	// The dim-to-full jump is the gesture reporting that letting go now WOULD
	// act, while the finger is still down. So it has to flip exactly where the
	// release does: a backing that looks committed and then snaps back is
	// worse than no feedback at all.
	it('arms the backing exactly where the release would act', async () => {
		const { action, run } = remove();
		const { rows } = setup({ swipeLeft: action });

		await fireEvent.pointerDown(rows[1], { ...touch, clientX: 90, clientY: 50 });
		await fireEvent.pointerMove(rows[1], { ...touch, clientX: 50, clientY: 50 });
		expect(rows[1].classList.contains('swipe-open')).toBe(true);
		expect(rows[1].classList.contains('swipe-armed')).toBe(false);

		await fireEvent.pointerMove(rows[1], { ...touch, clientX: 30, clientY: 50 });
		expect(rows[1].classList.contains('swipe-armed')).toBe(true);

		// Dragged back under the threshold it disarms, rather than latching on
		// the furthest point the finger reached.
		await fireEvent.pointerMove(rows[1], { ...touch, clientX: 60, clientY: 50 });
		expect(rows[1].classList.contains('swipe-armed')).toBe(false);
		await fireEvent.pointerUp(rows[1], { ...touch });
		expect(run).not.toHaveBeenCalled();
	});

	// A swipe ends with a click on the row underneath it. Swallowed in the
	// component because every consumer puts something clickable in the row:
	// without this, dismissing a notification would also navigate to it.
	it('swallows the click a swipe ends with', async () => {
		const { action } = remove();
		const { rows } = setup({ swipeLeft: action });
		const onclick = vi.fn();
		rows[1].querySelector('[data-testid="row-b"]')!.addEventListener('click', onclick);

		await fireEvent.pointerDown(rows[1], { ...touch, clientX: 90, clientY: 50 });
		await fireEvent.pointerMove(rows[1], { ...touch, clientX: 30, clientY: 50 });
		await fireEvent.pointerUp(rows[1], { ...touch });
		await fireEvent.click(rows[1].querySelector<HTMLElement>('[data-testid="row-b"]')!);

		expect(onclick).not.toHaveBeenCalled();

		// …and only that one: the next tap on the row is a tap again.
		await fireEvent.click(rows[1].querySelector<HTMLElement>('[data-testid="row-b"]')!);
		expect(onclick).toHaveBeenCalledTimes(1);
	});

	it('offers no buttons and no backing when neither direction is wired', () => {
		const { container } = render(Harness, { initial: ITEMS, reorderCapable: true });
		expect(container.querySelector('.gesture-action')).toBeNull();
		expect(container.querySelector('.gesture-backing')).toBeNull();
	});
});

describe('tap', () => {
	// A finger's primary action on a row. It exists because the wide layouts
	// reach that action with a DOUBLE click — which touch never produces — so
	// without this the playlist could not be played from a phone at all.
	it('fires on a press that stayed a press', async () => {
		const ontap = vi.fn();
		const { rows } = setup({ ontap });

		await fireEvent.pointerDown(rows[1], { ...touch, clientX: 50, clientY: 50 });
		await fireEvent.pointerUp(rows[1], { ...touch });

		expect(ontap).toHaveBeenCalledWith({ id: 'b' }, 1);
	});

	// The three gestures share one press, so each of the other two has to take
	// the tap with it. A flick that scrolls the list must not also play the
	// track it started on, and a swipe that removes a row must not play the
	// one that took its place.
	it('does not fire after a scroll, a swipe or a long press', async () => {
		const ontap = vi.fn();
		const { action } = remove();
		const onlongpress = vi.fn();
		const { rows } = setup({ ontap, swipeLeft: action, onlongpress });

		// Travelled: a scroll.
		await fireEvent.pointerDown(rows[0], { ...touch, clientX: 50, clientY: 10 });
		await fireEvent.pointerMove(rows[0], { ...touch, clientX: 50, clientY: 80 });
		await fireEvent.pointerUp(rows[0], { ...touch });
		expect(ontap).not.toHaveBeenCalled();

		// Swiped far enough to fire its action.
		await fireEvent.pointerDown(rows[1], { ...touch, clientX: 90, clientY: 50 });
		await fireEvent.pointerMove(rows[1], { ...touch, clientX: 20, clientY: 50 });
		await fireEvent.pointerUp(rows[1], { ...touch });
		expect(ontap).not.toHaveBeenCalled();

		// Held.
		vi.useFakeTimers();
		try {
			await fireEvent.pointerDown(rows[2], { ...touch, clientX: 50, clientY: 100 });
			vi.advanceTimersByTime(600);
			await fireEvent.pointerUp(rows[2], { ...touch });
		} finally {
			vi.useRealTimers();
		}
		expect(onlongpress).toHaveBeenCalled();
		expect(ontap).not.toHaveBeenCalled();
	});

	// A mouse keeps its own idioms — click, double click, the hover buttons —
	// and a row that acted on mousedown-up as well would fire twice.
	it('ignores a mouse click', async () => {
		const ontap = vi.fn();
		const { rows } = setup({ ontap });

		await fireEvent.pointerDown(rows[0], { pointerId: 1, pointerType: 'mouse' });
		await fireEvent.pointerUp(rows[0], { pointerId: 1, pointerType: 'mouse' });

		expect(ontap).not.toHaveBeenCalled();
	});
});

describe('press and hold', () => {
	it('fires after the hold, with the item it was made on', async () => {
		vi.useFakeTimers();
		try {
			const onlongpress = vi.fn();
			const { rows } = setup({ onlongpress });

			await fireEvent.pointerDown(rows[2], { ...touch, clientX: 50, clientY: 100 });
			vi.advanceTimersByTime(600);
			expect(onlongpress).toHaveBeenCalledWith({ id: 'c' }, 2);
		} finally {
			vi.useRealTimers();
		}
	});

	// Travel ends a hold — including the vertical scroll that is by far the
	// most common thing to start on a row.
	it('does not fire once the finger has travelled', async () => {
		vi.useFakeTimers();
		try {
			const onlongpress = vi.fn();
			const { rows } = setup({ onlongpress });

			await fireEvent.pointerDown(rows[0], { ...touch, clientX: 50, clientY: 10 });
			await fireEvent.pointerMove(rows[0], { ...touch, clientX: 50, clientY: 60 });
			vi.advanceTimersByTime(600);
			expect(onlongpress).not.toHaveBeenCalled();
		} finally {
			vi.useRealTimers();
		}
	});

	// A mouse has the right button for this, so the hold is touch and pen
	// only — the same division as the swipe and its hover button.
	it('ignores a held mouse button', async () => {
		vi.useFakeTimers();
		try {
			const onlongpress = vi.fn();
			const { rows } = setup({ onlongpress });

			await fireEvent.pointerDown(rows[0], { pointerId: 1, pointerType: 'mouse' });
			vi.advanceTimersByTime(600);
			expect(onlongpress).not.toHaveBeenCalled();
		} finally {
			vi.useRealTimers();
		}
	});

	it('does not fire when the press ends before the hold', async () => {
		vi.useFakeTimers();
		try {
			const onlongpress = vi.fn();
			const { rows } = setup({ onlongpress });

			await fireEvent.pointerDown(rows[0], { ...touch, clientX: 50, clientY: 10 });
			vi.advanceTimersByTime(200);
			await fireEvent.pointerUp(rows[0], { ...touch });
			vi.advanceTimersByTime(600);
			expect(onlongpress).not.toHaveBeenCalled();
		} finally {
			vi.useRealTimers();
		}
	});
});
