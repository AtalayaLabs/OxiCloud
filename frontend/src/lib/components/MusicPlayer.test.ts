import { describe, it, expect, beforeEach, vi } from 'vitest';
import { render, screen, fireEvent } from '@testing-library/svelte';
import MusicPlayer from './MusicPlayer.svelte';
import { player } from '$lib/stores/player.svelte';
import { serverConfig } from '$lib/stores/serverConfig.svelte';
import type { PlaylistItem } from '$lib/api/endpoints/music';

vi.mock('$app/paths', () => ({ resolve: (p: string) => p, base: '' }));

/**
 * jsdom has no layout — every rect is zero — and the queue drag decides where
 * a track lands by comparing the pointer against the rows' midpoints. So the
 * rows need a geometry to reason about: 40px slots stacked from y=0.
 */
function layout(rows: ArrayLike<HTMLElement>): void {
	[...Array.from(rows)].forEach((row, i) => {
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

/**
 * The swipe is touch-and-pen only — a mouse has the hover eraser instead —
 * so every swipe gesture here has to say so. jsdom leaves `pointerType` empty
 * by default, which is neither.
 */
const touch = { pointerId: 1, pointerType: 'touch' } as const;

function track(id: string): PlaylistItem {
	return {
		id,
		playlist_id: 'pl',
		file_id: `file-${id}`,
		position: 0,
		added_at: 0,
		file_name: `${id}.mp3`,
		file_size: null,
		mime_type: 'audio/mpeg',
		title: id,
		artist: 'Hawkwind',
		album: null,
		duration_secs: 222
	} as PlaylistItem;
}

beforeEach(() => {
	// jsdom implements no media playback at all: `HTMLMediaElement.play()`
	// raises "Not implemented" rather than returning a promise, which the
	// store's `.catch()` cannot swallow — it surfaced as unhandled errors
	// beside passing tests. Stubbing the two methods the store calls keeps the
	// element's API honest without pretending jsdom can decode audio.
	vi.spyOn(HTMLMediaElement.prototype, 'play').mockResolvedValue(undefined);
	vi.spyOn(HTMLMediaElement.prototype, 'pause').mockImplementation(() => {});

	// A session left behind by one test would be restored by the next one's
	// mount, which is exactly the behaviour under test here.
	localStorage.clear();
	player.queue = [];
	player.index = -1;
	player.playing = false;
	player.coverFileId = null;
	serverConfig.features = { ...serverConfig.features, music: true };
});

describe('MusicPlayer', () => {
	// THE regression this component exists to prevent. The `<audio>` element
	// used to live in `routes/music/+page.svelte`, so navigating away unmounted
	// it and cut playback mid-track. It is now rendered here, unconditionally
	// — outside the panel's `{#if open}` — and `AppShell` keeps this component
	// mounted for the session. Move the element inside that conditional and
	// closing the panel would stop the music, which is the same bug one layer
	// in.
	it('renders the audio element whether the panel is open or closed', async () => {
		const { container } = render(MusicPlayer);

		const audio = container.querySelector('audio');
		expect(audio, 'audio element must exist with the panel closed').toBeTruthy();

		await fireEvent.click(screen.getByTestId('music-player-toggle-btn'));
		expect(screen.getByTestId('music-player-empty')).toBeTruthy();
		// The SAME node, not a replacement: a remounted element loses its
		// playback position and its buffered data.
		expect(container.querySelector('audio')).toBe(audio);

		await fireEvent.click(screen.getByTestId('music-player-toggle-btn'));
		expect(screen.queryByTestId('music-player-empty')).toBeNull();
		expect(container.querySelector('audio')).toBe(audio);
	});

	// The badge reports state, and an empty player has no state worth
	// reporting — a badge that is always there is one people stop reading.
	it('shows no badge until something is queued, then reflects play/pause', async () => {
		const { container } = render(MusicPlayer);
		expect(container.querySelector('.player-trigger__badge')).toBeNull();

		player.queue = [track('a')];
		player.index = 0;
		player.playing = true;
		await Promise.resolve();
		expect(container.querySelector('.player-trigger__badge')).toBeTruthy();
	});

	it('opens to the queue when one exists, newest state first', async () => {
		player.queue = [track('a'), track('b')];
		player.index = 0;
		render(MusicPlayer);

		await fireEvent.click(screen.getByTestId('music-player-toggle-btn'));

		expect(screen.getByTestId('music-player-title').textContent).toBe('a');
		expect(screen.getByTestId('music-queue-item-a')).toBeTruthy();
		expect(screen.getByTestId('music-queue-item-b')).toBeTruthy();
		// The empty state belongs only to an empty queue.
		expect(screen.queryByTestId('music-player-empty')).toBeNull();
	});

	// The transport controls are the panel's whole purpose; a wiring slip
	// (wrong handler on a button) is invisible until someone clicks it.
	it('wires the transport buttons to the store', async () => {
		player.queue = [track('a'), track('b'), track('c')];
		player.index = 1;
		const next = vi.spyOn(player, 'next');
		const prev = vi.spyOn(player, 'prev');
		const toggle = vi.spyOn(player, 'toggle');
		render(MusicPlayer);
		await fireEvent.click(screen.getByTestId('music-player-toggle-btn'));

		await fireEvent.click(screen.getByTestId('music-player-next-btn'));
		await fireEvent.click(screen.getByTestId('music-player-prev-btn'));
		await fireEvent.click(screen.getByTestId('music-player-play-btn'));

		expect(next).toHaveBeenCalledTimes(1);
		expect(prev).toHaveBeenCalledTimes(1);
		expect(toggle).toHaveBeenCalledTimes(1);
	});

	// A deployment with `OXICLOUD_ENABLE_MUSIC=false` hides `/music` from the
	// sidebar; the player has to go with the section it belongs to, element
	// included — there is nothing to play. Gated inside the component rather
	// than at the call site so the mobile mount point coming next cannot
	// forget it.
	it('renders nothing at all when music is disabled', () => {
		serverConfig.features = { ...serverConfig.features, music: false };
		const { container } = render(MusicPlayer);

		expect(screen.queryByTestId('music-player-toggle-btn')).toBeNull();
		expect(container.querySelector('.player-wrapper')).toBeNull();
		// The element too: a hidden player that still streams would be worse
		// than a visible one.
		expect(container.querySelector('audio')).toBeNull();
	});

	// The store can persist and restore perfectly and still never be asked to.
	// These two pin the wiring: who calls it, and when.
	it('restores on mount, after attaching the element', async () => {
		const attach = vi.spyOn(player, 'attach');
		const restore = vi.spyOn(player, 'restore');

		render(MusicPlayer);
		await Promise.resolve();

		expect(restore).toHaveBeenCalled();
		// ORDER is the subtle part: restoring sets the element's source and
		// may ask it to play, so calling it before `attach` would act on
		// nothing. What restore then DOES is the store's own tests — asserting
		// the resulting queue here would depend on whether an earlier test in
		// this file had already consumed the singleton's one-shot restore.
		expect(attach.mock.invocationCallOrder[0]).toBeLessThan(restore.mock.invocationCallOrder[0]);
	});

	// A tab can vanish without ever firing `pause` — closed, navigated away
	// from, or killed by a phone OS once backgrounded. These two events are
	// the last word in each case.
	it('saves on pagehide and when the tab is hidden', async () => {
		player.queue = [track('a')];
		player.index = 0;
		render(MusicPlayer);
		const persist = vi.spyOn(player, 'persist');

		window.dispatchEvent(new Event('pagehide'));
		expect(persist).toHaveBeenCalledTimes(1);

		document.dispatchEvent(new Event('visibilitychange'));
		expect(persist).toHaveBeenCalledTimes(2);
	});

	// Reordering runs on POINTER events, not HTML5 drag-and-drop: `dragstart`
	// never fires on touch, so the grip was dead on a phone — which is where
	// this panel has a page of its own. One implementation now covers mouse,
	// touch and pen.
	it('reorders the queue by dragging a grip', async () => {
		player.queue = [track('a'), track('b'), track('c')];
		player.index = 0;
		const { container } = render(MusicPlayer);
		await fireEvent.click(screen.getByTestId('music-player-toggle-btn'));

		const rows = container.querySelectorAll<HTMLElement>('[data-queue-index]');
		layout(rows);
		const grip = rows[0].querySelector<HTMLElement>('.player-queue-grip')!;
		grip.setPointerCapture = vi.fn();

		// Past the midpoint of every other row: the first track lands last.
		await fireEvent.pointerDown(grip, { pointerId: 1 });
		await fireEvent.pointerMove(grip, { pointerId: 1, clientX: 0, clientY: 110 });
		await fireEvent.pointerUp(grip, { pointerId: 1 });

		expect(player.queue.map((t) => t.id)).toEqual(['b', 'c', 'a']);
		// The pointer was captured, so the move events target the GRIP however
		// far the finger strays; losing that is how this breaks.
		expect(grip.setPointerCapture).toHaveBeenCalledWith(1);
	});

	// Regression: a track could be moved anywhere EXCEPT the end of the queue.
	// The drop point used to be hit-tested, and below the last row the hit
	// landed on the `<ul>` rather than any row, leaving the target wherever
	// the finger last crossed one. Counting midpoints has no such gap.
	it('moves a track to the end when dropped below every row', async () => {
		player.queue = [track('a'), track('b'), track('c')];
		player.index = 0;
		const { container } = render(MusicPlayer);
		await fireEvent.click(screen.getByTestId('music-player-toggle-btn'));

		const rows = container.querySelectorAll<HTMLElement>('[data-queue-index]');
		layout(rows);
		const grip = rows[0].querySelector<HTMLElement>('.player-queue-grip')!;
		grip.setPointerCapture = vi.fn();

		// Well past the bottom of the list, as when the finger runs off it.
		await fireEvent.pointerDown(grip, { pointerId: 1 });
		await fireEvent.pointerMove(grip, { pointerId: 1, clientX: 0, clientY: 500 });
		await fireEvent.pointerUp(grip, { pointerId: 1 });

		expect(player.queue.map((t) => t.id)).toEqual(['b', 'c', 'a']);
	});

	// Regression: dropping a track back where it started was impossible —
	// the carried row is transparent to hit-testing and nothing else occupies
	// its vacated slot, so the target stayed on the last neighbour crossed
	// and the track always moved by one.
	it('leaves the queue alone when a track is dropped back on its own slot', async () => {
		player.queue = [track('a'), track('b'), track('c')];
		player.index = 0;
		const { container } = render(MusicPlayer);
		await fireEvent.click(screen.getByTestId('music-player-toggle-btn'));

		const rows = container.querySelectorAll<HTMLElement>('[data-queue-index]');
		layout(rows);
		const grip = rows[1].querySelector<HTMLElement>('.player-queue-grip')!;
		grip.setPointerCapture = vi.fn();

		// Out to a neighbour and back into its own slot (y 40–80).
		await fireEvent.pointerDown(grip, { pointerId: 1 });
		await fireEvent.pointerMove(grip, { pointerId: 1, clientX: 0, clientY: 110 });
		await fireEvent.pointerMove(grip, { pointerId: 1, clientX: 0, clientY: 50 });
		await fireEvent.pointerUp(grip, { pointerId: 1 });

		expect(player.queue.map((t) => t.id)).toEqual(['a', 'b', 'c']);
	});

	// The line marks the edge the track lands against, which depends on the
	// direction of travel. A fixed top line cannot express "after the last
	// row", so the end of the queue looked unreachable even though the drop
	// itself worked.
	it('marks the landing edge according to the drag direction', async () => {
		player.queue = [track('a'), track('b'), track('c')];
		player.index = 0;
		const { container } = render(MusicPlayer);
		await fireEvent.click(screen.getByTestId('music-player-toggle-btn'));
		const rows = container.querySelectorAll<HTMLElement>('[data-queue-index]');
		layout(rows);

		// Dragging the first row DOWN past every midpoint: it would land at
		// the end, so the LAST row carries the line on its bottom edge —
		// which is the only way to express "after the last row".
		const firstGrip = rows[0].querySelector<HTMLElement>('.player-queue-grip')!;
		firstGrip.setPointerCapture = vi.fn();
		await fireEvent.pointerDown(firstGrip, { pointerId: 1 });
		await fireEvent.pointerMove(firstGrip, { pointerId: 1, clientX: 0, clientY: 110 });
		expect(rows[2].classList.contains('drag-over-bottom')).toBe(true);
		expect(rows[2].classList.contains('drag-over-top')).toBe(false);
		await fireEvent.pointerUp(firstGrip, { pointerId: 1 });

		// And the other way: dragging the last row UP above every midpoint
		// puts the line on the first row's top edge.
		const after = container.querySelectorAll<HTMLElement>('[data-queue-index]');
		layout(after);
		const lastGrip = after[2].querySelector<HTMLElement>('.player-queue-grip')!;
		lastGrip.setPointerCapture = vi.fn();
		await fireEvent.pointerDown(lastGrip, { pointerId: 2 });
		await fireEvent.pointerMove(lastGrip, { pointerId: 2, clientX: 0, clientY: 1 });
		expect(after[0].classList.contains('drag-over-top')).toBe(true);
		expect(after[0].classList.contains('drag-over-bottom')).toBe(false);
		await fireEvent.pointerUp(lastGrip, { pointerId: 2 });
	});

	// Swipe left to remove, as Spotify and most webmail do. Three gestures
	// share a row — tap to jump, grip to reorder, swipe to remove — so the
	// swipe must claim the pointer only once the movement is clearly
	// horizontal, and must not leave a tap behind when it ends.
	it('removes a track swiped far enough to the left', async () => {
		player.queue = [track('a'), track('b'), track('c')];
		player.index = 0;
		const { container } = render(MusicPlayer);
		await fireEvent.click(screen.getByTestId('music-player-toggle-btn'));
		const row = container.querySelectorAll<HTMLElement>('[data-queue-index]')[1];
		row.setPointerCapture = vi.fn();

		await fireEvent.pointerDown(row, { ...touch, clientX: 200, clientY: 10 });
		await fireEvent.pointerMove(row, { ...touch, clientX: 80, clientY: 12 });
		await fireEvent.pointerUp(row, { ...touch });

		expect(player.queue.map((t) => t.id)).toEqual(['a', 'c']);
	});

	// The two halves of removal are split by input kind: a pointer that can
	// hover gets the eraser the row reveals under the cursor, a finger gets
	// the swipe. Both at once means a stray mouse drag across the queue —
	// plausible, the rows are buttons — flings a row and deletes a track.
	it('ignores a left drag made with a mouse, which has the eraser instead', async () => {
		player.queue = [track('a'), track('b'), track('c')];
		player.index = 0;
		const { container } = render(MusicPlayer);
		await fireEvent.click(screen.getByTestId('music-player-toggle-btn'));
		const row = container.querySelectorAll<HTMLElement>('[data-queue-index]')[1];
		row.setPointerCapture = vi.fn();

		await fireEvent.pointerDown(row, {
			pointerId: 1,
			pointerType: 'mouse',
			clientX: 200,
			clientY: 10
		});
		await fireEvent.pointerMove(row, {
			pointerId: 1,
			pointerType: 'mouse',
			clientX: 20,
			clientY: 12
		});
		await fireEvent.pointerUp(row, { pointerId: 1, pointerType: 'mouse' });

		expect(player.queue.map((t) => t.id)).toEqual(['a', 'b', 'c']);
		expect(row.setPointerCapture).not.toHaveBeenCalled();
		// And the eraser is still the way a mouse removes it.
		await fireEvent.click(screen.getByTestId('music-queue-remove-b'));
		expect(player.queue.map((t) => t.id)).toEqual(['a', 'c']);
	});

	it('snaps back a swipe that stops short, keeping the track', async () => {
		player.queue = [track('a'), track('b')];
		player.index = 0;
		const { container } = render(MusicPlayer);
		await fireEvent.click(screen.getByTestId('music-player-toggle-btn'));
		const row = container.querySelectorAll<HTMLElement>('[data-queue-index]')[1];
		row.setPointerCapture = vi.fn();

		await fireEvent.pointerDown(row, { ...touch, clientX: 200, clientY: 10 });
		await fireEvent.pointerMove(row, { ...touch, clientX: 170, clientY: 10 });
		await fireEvent.pointerUp(row, { ...touch });

		expect(player.queue.map((t) => t.id)).toEqual(['a', 'b']);
	});

	// The threshold is half the ROW, not a pixel count: the rows are 380px in
	// the popover and the full width of a phone on `/player`, so a fixed
	// distance is a different share of the gesture on every screen. Both
	// swipes here would land on the same side of a 96px threshold — which is
	// the point, since that is what this replaced.
	it('removes past half the row and keeps the track short of it', async () => {
		player.queue = [track('a'), track('b'), track('c')];
		player.index = 0;
		const { container } = render(MusicPlayer);
		await fireEvent.click(screen.getByTestId('music-player-toggle-btn'));
		const rows = container.querySelectorAll<HTMLElement>('[data-queue-index]');
		layout(rows); // 100px wide, so the threshold is 50.

		// 40 of 100: kept.
		rows[1].setPointerCapture = vi.fn();
		await fireEvent.pointerDown(rows[1], { ...touch, clientX: 90, clientY: 50 });
		await fireEvent.pointerMove(rows[1], { ...touch, clientX: 50, clientY: 50 });
		await fireEvent.pointerUp(rows[1], { ...touch });
		expect(player.queue.map((t) => t.id)).toEqual(['a', 'b', 'c']);

		// 60 of 100: removed.
		const again = container.querySelectorAll<HTMLElement>('[data-queue-index]');
		layout(again);
		again[1].setPointerCapture = vi.fn();
		await fireEvent.pointerDown(again[1], { ...touch, clientX: 90, clientY: 50 });
		await fireEvent.pointerMove(again[1], { ...touch, clientX: 30, clientY: 50 });
		await fireEvent.pointerUp(again[1], { ...touch });
		expect(player.queue.map((t) => t.id)).toEqual(['a', 'c']);
	});

	// The red backing and its eraser belong to the gesture, not to the row: at
	// rest they would advertise a swipe on the one device that cannot make one
	// (a mouse) and leave a red stripe under every track.
	it('shows the removal backing only while a row is being swiped', async () => {
		player.queue = [track('a'), track('b')];
		player.index = 0;
		const { container } = render(MusicPlayer);
		await fireEvent.click(screen.getByTestId('music-player-toggle-btn'));
		const row = container.querySelectorAll<HTMLElement>('[data-queue-index]')[1];
		row.setPointerCapture = vi.fn();

		expect(container.querySelector('.player-queue-swipe-hint')).toBeNull();
		expect(row.classList.contains('swipe-open')).toBe(false);

		await fireEvent.pointerDown(row, { ...touch, clientX: 200, clientY: 10 });
		// Down is not yet a swipe — the gesture could still be a tap or a scroll.
		expect(row.classList.contains('swipe-open')).toBe(false);

		await fireEvent.pointerMove(row, { ...touch, clientX: 170, clientY: 10 });
		expect(row.classList.contains('swipe-open')).toBe(true);
		expect(container.querySelector('.player-queue-swipe-hint')).not.toBeNull();

		// Released short of the threshold, the backing stays lit until the row
		// has finished sliding home — otherwise it blinks out and the list's
		// background shows through the gap, as if the track had gone after all.
		await fireEvent.pointerUp(row, { ...touch });
		expect(row.classList.contains('swipe-open')).toBe(true);
		await fireEvent.transitionEnd(row.querySelector('.player-queue-swipe')!);
		expect(row.classList.contains('swipe-open')).toBe(false);
		expect(container.querySelector('.player-queue-swipe-hint')).toBeNull();
	});

	// The backing has two strengths, and the dim one is not decoration: it is
	// the gesture reporting that letting go now would NOT delete the track.
	// `swipe-armed` is what the stylesheet takes to full opacity, so it has to
	// flip at the same place the release does — a backing that looks committed
	// and then snaps back is worse than no feedback at all.
	it('arms the backing exactly where the release would remove', async () => {
		player.queue = [track('a'), track('b'), track('c')];
		player.index = 0;
		const { container } = render(MusicPlayer);
		await fireEvent.click(screen.getByTestId('music-player-toggle-btn'));
		const rows = container.querySelectorAll<HTMLElement>('[data-queue-index]');
		layout(rows); // 100px wide, so the threshold is 50.
		rows[1].setPointerCapture = vi.fn();

		await fireEvent.pointerDown(rows[1], { ...touch, clientX: 90, clientY: 50 });
		await fireEvent.pointerMove(rows[1], { ...touch, clientX: 50, clientY: 50 });
		// 40 of 100 — open, but not armed.
		expect(rows[1].classList.contains('swipe-open')).toBe(true);
		expect(rows[1].classList.contains('swipe-armed')).toBe(false);

		await fireEvent.pointerMove(rows[1], { ...touch, clientX: 30, clientY: 50 });
		// 60 of 100 — armed, and a release here does remove it.
		expect(rows[1].classList.contains('swipe-armed')).toBe(true);

		// Dragged back under the threshold it disarms again, rather than
		// latching on the furthest point the finger reached.
		await fireEvent.pointerMove(rows[1], { ...touch, clientX: 60, clientY: 50 });
		expect(rows[1].classList.contains('swipe-armed')).toBe(false);
		await fireEvent.pointerUp(rows[1], { ...touch });
		expect(player.queue.map((t) => t.id)).toEqual(['a', 'b', 'c']);
	});

	// A mostly-vertical drag belongs to the list, not to the row: claiming it
	// would stop a long queue scrolling under the finger.
	it('leaves a vertical drag to the list', async () => {
		player.queue = [track('a'), track('b')];
		player.index = 0;
		const { container } = render(MusicPlayer);
		await fireEvent.click(screen.getByTestId('music-player-toggle-btn'));
		const row = container.querySelectorAll<HTMLElement>('[data-queue-index]')[1];
		row.setPointerCapture = vi.fn();

		await fireEvent.pointerDown(row, { ...touch, clientX: 200, clientY: 10 });
		await fireEvent.pointerMove(row, { ...touch, clientX: 190, clientY: 90 });
		// Even a long horizontal run afterwards must not resurrect the swipe.
		await fireEvent.pointerMove(row, { ...touch, clientX: 20, clientY: 95 });
		await fireEvent.pointerUp(row, { ...touch });

		expect(player.queue.map((t) => t.id)).toEqual(['a', 'b']);
		expect(row.setPointerCapture).not.toHaveBeenCalled();
	});

	// Carried back over its own slot, the line has to appear on a NEIGHBOUR:
	// the dragged row is translated and travels with the pointer, so a line
	// on it would travel too instead of marking the place it would land.
	it('marks the original slot when the track is carried back to it', async () => {
		player.queue = [track('a'), track('b'), track('c')];
		player.index = 0;
		const { container } = render(MusicPlayer);
		await fireEvent.click(screen.getByTestId('music-player-toggle-btn'));
		const rows = container.querySelectorAll<HTMLElement>('[data-queue-index]');
		layout(rows);
		const grip = rows[1].querySelector<HTMLElement>('.player-queue-grip')!;
		grip.setPointerCapture = vi.fn();

		await fireEvent.pointerDown(grip, { pointerId: 1 });
		// Back inside its own slot (y 40–80): past the first row's midpoint,
		// short of the last one's, which is the slot it came from.
		await fireEvent.pointerMove(grip, { pointerId: 1, clientX: 0, clientY: 50 });

		// The row BELOW shows it, not the dragged row.
		expect(rows[2].classList.contains('drag-over-top')).toBe(true);
		expect(rows[1].classList.contains('drag-over-top')).toBe(false);
		expect(rows[1].classList.contains('drag-over-bottom')).toBe(false);

		await fireEvent.pointerUp(grip, { pointerId: 1 });
		// And putting it back where it started changes nothing.
		expect(player.queue.map((t) => t.id)).toEqual(['a', 'b', 'c']);
	});

	it('removes the row it is asked to, not the current one', async () => {
		player.queue = [track('a'), track('b'), track('c')];
		player.index = 0;
		render(MusicPlayer);
		await fireEvent.click(screen.getByTestId('music-player-toggle-btn'));

		await fireEvent.click(screen.getByTestId('music-queue-remove-b'));

		expect(player.queue.map((t) => t.id)).toEqual(['a', 'c']);
		expect(player.current?.id).toBe('a');
	});
});
