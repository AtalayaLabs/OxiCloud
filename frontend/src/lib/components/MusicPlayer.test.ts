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

	// The gestures themselves — reorder, swipe, the hover button, press and
	// hold — belong to `GestureList` and are pinned in `GestureList.test.ts`.
	// What belongs HERE is the wiring: that the queue declares the right
	// capabilities and points them at the right store methods. A swap of
	// `onmove` for `removeAt`, or a remove wired to the current index rather
	// than the row's, passes every test in that file.
	it('offers reordering, and moves the track the grip belongs to', async () => {
		player.queue = [track('a'), track('b'), track('c')];
		player.index = 0;
		const { container } = render(MusicPlayer);
		await fireEvent.click(screen.getByTestId('music-player-toggle-btn'));

		const rows = container.querySelectorAll<HTMLElement>('[data-row-index]');
		layout(rows);
		const grip = rows[0].querySelector<HTMLElement>('.gesture-grip')!;
		grip.setPointerCapture = vi.fn();

		await fireEvent.pointerDown(grip, { pointerId: 1 });
		await fireEvent.pointerMove(grip, { pointerId: 1, clientX: 0, clientY: 110 });
		await fireEvent.pointerUp(grip, { pointerId: 1 });

		expect(player.queue.map((t) => t.id)).toEqual(['b', 'c', 'a']);
	});

	// Regression (Ed, in the browser): the queue was keyed by track id, so the
	// second time a track was added to a queue that already held it, Svelte
	// threw `each_key_duplicate` and took the whole player down. A queue is a
	// list of POSITIONS — the same track holding two of them is not a mistake,
	// it is what "add to queue" does — so the key carries the index.
	it('renders the same track queued twice', async () => {
		const twice = track('a');
		player.queue = [twice, track('b'), twice];
		player.index = 0;
		const { container } = render(MusicPlayer);
		await fireEvent.click(screen.getByTestId('music-player-toggle-btn'));

		expect(container.querySelectorAll('[data-row-index]')).toHaveLength(3);
		expect(screen.getAllByTestId('music-queue-item-a')).toHaveLength(2);
	});

	// …and the position is what gets removed, not the first row that happens
	// to share the id.
	it('removes the queued position, not every copy of the track', async () => {
		const twice = track('a');
		player.queue = [twice, track('b'), twice];
		player.index = 0;
		render(MusicPlayer);
		await fireEvent.click(screen.getByTestId('music-player-toggle-btn'));

		const [, secondCopy] = screen.getAllByTestId('music-queue-remove-a');
		await fireEvent.click(secondCopy);

		expect(player.queue.map((t) => t.id)).toEqual(['a', 'b']);
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
