import { describe, it, expect, beforeEach, vi } from 'vitest';
import { player } from './player.svelte';
import type { PlaylistItem } from '$lib/api/endpoints/music';

/** Minimal track; only `id` matters to the queue arithmetic. */
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
		artist: null,
		album: null,
		duration_secs: null
	} as PlaylistItem;
}

/**
 * Seed the queue, with a real element attached.
 *
 * The element matters: `prev()` only restarts the current track when there
 * IS one to rewind, so without this the restart branch silently falls through
 * to "step back" and the test reads as a behaviour change. jsdom implements
 * no playback, hence the stubs in `beforeEach`.
 */
function seed(ids: string[], index: number) {
	player.attach(document.createElement('audio'));
	player.queue = ids.map(track);
	player.index = index;
}

beforeEach(() => {
	vi.spyOn(HTMLMediaElement.prototype, 'play').mockResolvedValue(undefined);
	vi.spyOn(HTMLMediaElement.prototype, 'pause').mockImplementation(() => {});
});

const ids = () => player.queue.map((t) => t.id);

describe('player.move', () => {
	beforeEach(() => seed(['a', 'b', 'c', 'd'], 1));

	// `index` points at a SLOT but means a TRACK. Every case below is one way
	// a move can cross the playing position: get it wrong and nothing throws —
	// the queue just starts playing a different song at the next `next()`,
	// which is the kind of bug that gets reported as "it shuffled by itself".
	it('follows the track when the playing one is moved', () => {
		seed(['a', 'b', 'c', 'd'], 1); // playing "b"
		player.move(1, 3);
		expect(ids()).toEqual(['a', 'c', 'd', 'b']);
		expect(player.current?.id).toBe('b');
		expect(player.index).toBe(3);
	});

	it('steps back when a track is moved from before the playing one to after it', () => {
		seed(['a', 'b', 'c', 'd'], 1); // playing "b"
		player.move(0, 2); // "a" jumps past "b"
		expect(ids()).toEqual(['b', 'c', 'a', 'd']);
		expect(player.current?.id).toBe('b');
		expect(player.index).toBe(0);
	});

	it('steps forward when a track is moved from after the playing one to before it', () => {
		seed(['a', 'b', 'c', 'd'], 1); // playing "b"
		player.move(3, 0); // "d" jumps ahead of "b"
		expect(ids()).toEqual(['d', 'a', 'b', 'c']);
		expect(player.current?.id).toBe('b');
		expect(player.index).toBe(2);
	});

	it('leaves the pointer alone for a move entirely on one side', () => {
		seed(['a', 'b', 'c', 'd'], 1); // playing "b"
		player.move(2, 3); // "c" and "d" swap, both after "b"
		expect(ids()).toEqual(['a', 'b', 'd', 'c']);
		expect(player.current?.id).toBe('b');
		expect(player.index).toBe(1);
	});

	it('ignores a no-op or out-of-range move', () => {
		seed(['a', 'b', 'c', 'd'], 1);
		for (const [from, to] of [
			[1, 1],
			[-1, 2],
			[2, -1],
			[9, 0],
			[0, 9]
		]) {
			player.move(from, to);
		}
		expect(ids()).toEqual(['a', 'b', 'c', 'd']);
		expect(player.index).toBe(1);
	});
});

describe('player.removeAt', () => {
	it('keeps playing the same track when an earlier one is removed', () => {
		seed(['a', 'b', 'c'], 2); // playing "c"
		player.removeAt(0);
		expect(ids()).toEqual(['b', 'c']);
		expect(player.current?.id).toBe('c');
	});

	it('leaves the pointer alone when a later one is removed', () => {
		seed(['a', 'b', 'c'], 0); // playing "a"
		player.removeAt(2);
		expect(ids()).toEqual(['a', 'b']);
		expect(player.current?.id).toBe('a');
	});
});

describe('player transport', () => {
	beforeEach(() => {
		seed(['a', 'b', 'c'], 0);
		player.repeat = 'none';
		player.shuffle = false;
		player.currentTime = 0;
	});

	it('stops at the end of the queue unless repeat is on', () => {
		seed(['a', 'b', 'c'], 2);
		player.next();
		// Still on the last track: nothing to advance to, and wrapping without
		// being asked would restart the album at 2am.
		expect(player.index).toBe(2);

		player.repeat = 'all';
		player.next();
		expect(player.index).toBe(0);
	});

	it('restarts the track when previous is pressed mid-play', () => {
		seed(['a', 'b', 'c'], 1);
		player.currentTime = 10; // past the restart window
		player.prev();
		// The index does NOT move — this is the "start this song again"
		// gesture every physical transport has.
		expect(player.index).toBe(1);

		player.currentTime = 1; // inside the window
		player.prev();
		expect(player.index).toBe(0);
	});

	it('wraps to the end when stepping back from the first track', () => {
		seed(['a', 'b', 'c'], 0);
		player.currentTime = 0;
		player.prev();
		expect(player.index).toBe(2);
	});

	it('repeat-one replays rather than advancing', () => {
		seed(['a', 'b', 'c'], 1);
		player.repeat = 'one';
		player.onEnded();
		expect(player.index).toBe(1);

		player.repeat = 'none';
		player.onEnded();
		expect(player.index).toBe(2);
	});

	it('mutes when the volume is dragged to zero', () => {
		player.setVolume(0);
		expect(player.muted).toBe(true);
		player.setVolume(0.4);
		expect(player.muted).toBe(false);
	});

	it('cycles repeat none → all → one → none', () => {
		player.repeat = 'none';
		player.cycleRepeat();
		expect(player.repeat).toBe('all');
		player.cycleRepeat();
		expect(player.repeat).toBe('one');
		player.cycleRepeat();
		expect(player.repeat).toBe('none');
	});

	// The cover belongs to the PLAYLIST, and the queue outlives the selection:
	// capturing it at `play()` is what stops the art changing when you browse
	// to another playlist with something still playing.
	it('captures the cover with the queue', () => {
		player.play([track('a'), track('b')], 1, 'cover-1');
		expect(player.index).toBe(1);
		expect(player.coverFileId).toBe('cover-1');
		expect(player.coverUrl).toContain('cover-1');

		player.play([track('c')], 0, null);
		expect(player.coverUrl).toBeNull();
	});

	it('clear empties the queue and forgets the art', () => {
		player.play([track('a')], 0, 'cover-1');
		player.clear();
		expect(player.queue).toEqual([]);
		expect(player.index).toBe(-1);
		expect(player.current).toBeNull();
		expect(player.active).toBe(false);
		expect(player.coverFileId).toBeNull();
	});
});
