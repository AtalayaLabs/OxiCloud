import { describe, it, expect, beforeEach, vi } from 'vitest';
import { player, PlayerStore } from './player.svelte';
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

// Surviving Ctrl+Shift+R is the point of this group. Media Session cannot
// help — it describes the current page session to the OS, and a refresh
// destroys the document, the element and the session together — so the queue,
// the modes and the position go to localStorage and come back on boot.
describe('player persistence', () => {
	/**
	 * A store that has not restored yet — which is exactly what a reload
	 * produces. `restore()` runs once per instance and the flag behind it is a
	 * real private field, so reusing the singleton would silently return early
	 * and pass these tests without doing anything.
	 */
	function freshStore(): PlayerStore {
		const p = new PlayerStore();
		p.attach(document.createElement('audio'));
		return p;
	}

	beforeEach(() => {
		localStorage.clear();
		player.clear();
		player.shuffle = false;
		player.repeat = 'none';
	});

	it('writes the queue, the modes and the position', () => {
		player.play([track('a'), track('b')], 1, 'cover-1');
		player.shuffle = true;
		player.repeat = 'all';
		player.currentTime = 42;
		player.persist();

		const saved = JSON.parse(localStorage.getItem('oxi-player-state') ?? 'null');
		expect(saved.v).toBe(1);
		expect(saved.queue.map((t: PlaylistItem) => t.id)).toEqual(['a', 'b']);
		expect(saved.index).toBe(1);
		expect(saved.position).toBe(42);
		expect(saved.coverFileId).toBe('cover-1');
		expect(saved.shuffle).toBe(true);
		expect(saved.repeat).toBe('all');
	});

	// An empty queue is not a session worth resuming into; leaving the key
	// behind would restore a player nobody asked for on the next visit.
	it('drops the saved session when the queue is cleared', () => {
		player.play([track('a')], 0, null);
		expect(localStorage.getItem('oxi-player-state')).not.toBeNull();
		player.clear();
		expect(localStorage.getItem('oxi-player-state')).toBeNull();
	});

	// The feature itself: close the tab mid-song, come back, find the queue
	// where you left it. The POSITION is deliberately not asserted on the
	// element here — seeking waits for `loadedmetadata`, and jsdom never
	// loads anything — but it is carried and shown.
	it('brings the queue, the modes and the position back', () => {
		player.play([track('a'), track('b'), track('c')], 2, 'cover-9');
		player.shuffle = true;
		player.repeat = 'one';
		player.currentTime = 73;
		player.persist();

		const revived = freshStore();
		revived.restore();

		expect(revived.queue.map((t) => t.id)).toEqual(['a', 'b', 'c']);
		expect(revived.index).toBe(2);
		expect(revived.current?.id).toBe('c');
		expect(revived.currentTime).toBe(73);
		expect(revived.coverFileId).toBe('cover-9');
		expect(revived.shuffle).toBe(true);
		expect(revived.repeat).toBe('one');
	});

	it('clamps an index that no longer fits the saved queue', () => {
		localStorage.setItem(
			'oxi-player-state',
			JSON.stringify({ v: 1, queue: [track('a')], index: 7, position: 0 })
		);
		const revived = freshStore();
		revived.restore();
		expect(revived.index).toBe(0);
		expect(revived.current?.id).toBe('a');
	});

	it('restores at most once, however often it is called', () => {
		player.play([track('a')], 0, null);
		player.persist();
		const revived = freshStore();
		revived.restore();
		revived.queue = [];
		// A second call must not resurrect what the user has since cleared.
		revived.restore();
		expect(revived.queue).toEqual([]);
	});

	it('ignores a stored value from another format version', () => {
		localStorage.setItem(
			'oxi-player-state',
			JSON.stringify({ v: 99, queue: [track('a')], index: 0 })
		);
		const revived = freshStore();
		revived.restore();
		// Half-applying a shape we do not understand is worse than starting
		// empty.
		expect(revived.queue).toEqual([]);
		expect(revived.active).toBe(false);
	});

	it('ignores a corrupt stored value rather than failing the boot', () => {
		localStorage.setItem('oxi-player-state', '{not json');
		const revived = freshStore();
		expect(() => revived.restore()).not.toThrow();
		expect(revived.queue).toEqual([]);
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
