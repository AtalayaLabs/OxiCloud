import { base } from '$app/paths';
import { fileInlineUrl } from '$lib/api/endpoints/files';
import type { PlaylistItem } from '$lib/api/endpoints/music';

/**
 * The app's one audio player.
 *
 * It lives here, above the router, because the `<audio>` element used to sit
 * inside `routes/music/+page.svelte` — so navigating away unmounted it and
 * killed playback mid-track. The element is now rendered once by `AppShell`
 * and handed to this store; every surface that controls playback talks to the
 * store rather than owning state of its own.
 *
 * The queue is deliberately independent of whatever track list is on screen:
 * you can browse another playlist, or leave Music entirely, while the queue
 * plays on. That was already true within the page and is the whole point now.
 */

/** How the repeat button cycles. */
export type RepeatMode = 'none' | 'all' | 'one';

/** Seconds into a track beyond which "previous" restarts it instead of
 *  stepping back — the convention every physical transport uses. */
const RESTART_WINDOW_S = 3;

/** Per-viewer conveniences, not shared state: see `localStoragePrefs`. */
const VOLUME_KEY = 'oxi-player-volume';
const STATE_KEY = 'oxi-player-state';

/**
 * How often the playback position is written while a track runs.
 *
 * `timeupdate` fires about four times a second; writing localStorage that
 * often would be a synchronous disk-backed write per frame-ish for a value
 * nobody reads until the next page load. Five seconds bounds what a crash
 * can lose to five seconds of position, which is below what anyone notices
 * in a song.
 */
const POSITION_WRITE_MS = 5_000;

/** Bounds what a pathological queue can put in storage. */
const MAX_PERSISTED_TRACKS = 500;

/**
 * Shown by the OS when a track has no cover of its own.
 *
 * The maskable icon rather than `logo-plain.svg`: it is already square at
 * 512px with the safe-area padding a rounded lock-screen tile needs, and
 * several platforms decode Media Session artwork natively and drop SVG
 * entirely.
 */
const FALLBACK_ARTWORK = '/logo/maskable-512.png';

/** Shape written to storage. Versioned so a format change can be ignored
 *  rather than crashing a boot on someone's stale value. */
interface PersistedState {
	v: 1;
	queue: PlaylistItem[];
	index: number;
	position: number;
	coverFileId: string | null;
	shuffle: boolean;
	repeat: RepeatMode;
	playing: boolean;
}

/**
 * Exported for tests, which need a store that has not already restored —
 * `#restored` is a real private field, so there is no reaching in to reset
 * it, and a fresh instance is what a reload actually produces anyway.
 * Application code uses the `player` singleton below.
 */
export class PlayerStore {
	/** The element, owned by `AppShell`. Null until it mounts. */
	#audio: HTMLAudioElement | null = null;
	/** Position to seek to once the restored track reports its metadata. */
	#pendingSeek: number | null = null;
	/** Restore runs once per page, however many times `attach` is called. */
	#restored = false;
	#lastPositionWrite = 0;

	/** Playback queue — independent of any visible track list. */
	queue = $state<PlaylistItem[]>([]);
	index = $state(-1);
	playing = $state(false);
	currentTime = $state(0);
	duration = $state(0);
	volume = $state(readStoredVolume());
	muted = $state(false);
	shuffle = $state(false);
	repeat = $state<RepeatMode>('none');
	/**
	 * Cover art for the queue, captured when the queue is filled rather than
	 * read from "the selected playlist": covers hang off the playlist, and the
	 * queue outlives the selection — without this, browsing to another
	 * playlist while something plays would show the wrong art.
	 */
	coverFileId = $state<string | null>(null);

	get current(): PlaylistItem | null {
		return this.index >= 0 ? (this.queue[this.index] ?? null) : null;
	}

	/** Is there anything to show a player for? Drives the trigger's presence. */
	get active(): boolean {
		return this.queue.length > 0;
	}

	get coverUrl(): string | null {
		return this.coverFileId ? `/api/files/${encodeURIComponent(this.coverFileId)}` : null;
	}

	/** Display name, falling back the way the music page always has. */
	label(track: PlaylistItem | null = this.current): string {
		if (!track) return '';
		return track.title || track.file_name || track.file_id;
	}

	// ── Element wiring ────────────────────────────────────────────────────────

	/** Called by `AppShell` once, with the element it renders. */
	attach(el: HTMLAudioElement | null): void {
		this.#audio = el;
		this.#applyVolume();
	}

	/** `src` follows the current track; `AppShell` drives this from an effect. */
	syncSource(): void {
		const track = this.current;
		if (!this.#audio || !track) return;
		const url = fileInlineUrl(track.file_id);
		if (this.#audio.getAttribute('src') !== url) this.#audio.src = url;
	}

	// ── Transport ─────────────────────────────────────────────────────────────

	/**
	 * Replace the queue and start at `start`.
	 *
	 * `coverFileId` is the playlist's cover, passed in by the caller because
	 * tracks carry no art of their own.
	 */
	play(list: PlaylistItem[], start = 0, coverFileId: string | null = null): void {
		this.queue = [...list];
		this.coverFileId = coverFileId;
		this.playIndex(start);
		this.persist();
	}

	/**
	 * Append a track to the end of the queue, leaving what is playing alone —
	 * "add to queue" rather than "play this".
	 *
	 * Except when there is nothing playing at all: a track dropped into an
	 * empty queue with nothing to follow would just sit there, so it starts.
	 * The call always comes from a gesture or a click, which is what browsers
	 * require before `play()`.
	 */
	enqueue(track: PlaylistItem, coverFileId: string | null = null): void {
		const wasEmpty = this.queue.length === 0;
		this.queue = [...this.queue, track];
		if (wasEmpty) {
			this.coverFileId = coverFileId;
			this.playIndex(this.queue.length - 1);
		}
		this.persist();
	}

	playIndex(i: number): void {
		if (i < 0 || i >= this.queue.length) return;
		this.index = i;
		// The source swap happens in an effect; start playback after it lands.
		queueMicrotask(() => void this.#audio?.play().catch(() => {}));
	}

	toggle(): void {
		if (!this.#audio) return;
		if (this.index < 0 && this.queue.length) {
			this.playIndex(0);
			return;
		}
		if (this.playing) this.#audio.pause();
		else void this.#audio.play().catch(() => {});
	}

	next(): void {
		if (!this.queue.length) return;
		if (this.shuffle) {
			this.playIndex(Math.floor(Math.random() * this.queue.length));
			return;
		}
		if (this.index + 1 < this.queue.length) this.playIndex(this.index + 1);
		else if (this.repeat === 'all') this.playIndex(0);
	}

	prev(): void {
		if (!this.queue.length) return;
		if (this.currentTime > RESTART_WINDOW_S && this.#audio) {
			this.#audio.currentTime = 0;
			return;
		}
		// Wrap to the last track when already at the start.
		this.playIndex(this.index > 0 ? this.index - 1 : this.queue.length - 1);
	}

	seek(seconds: number): void {
		if (this.#audio) this.#audio.currentTime = seconds;
	}

	setVolume(v: number): void {
		this.volume = v;
		this.muted = v === 0;
		this.#applyVolume();
		try {
			localStorage.setItem(VOLUME_KEY, String(v));
		} catch {
			/* private mode / quota — volume just won't persist */
		}
	}

	toggleMute(): void {
		this.muted = !this.muted;
		this.#applyVolume();
	}

	cycleRepeat(): void {
		this.repeat = this.repeat === 'none' ? 'all' : this.repeat === 'all' ? 'one' : 'none';
	}

	toggleShuffle(): void {
		this.shuffle = !this.shuffle;
	}

	jump(i: number): void {
		this.playIndex(i);
	}

	/**
	 * Reorder the queue, moving the entry at `from` to `to`.
	 *
	 * `index` points at a SLOT, but what the listener cares about is the
	 * TRACK, so it has to be rewritten for any move that crosses it — drag the
	 * playing track somewhere else and playback must follow it, drag another
	 * track over it and the pointer has to step aside. Getting this wrong does
	 * not throw; it silently starts playing a different song at the next
	 * `next()`, which is why it has its own tests.
	 */
	move(from: number, to: number): void {
		const last = this.queue.length - 1;
		if (from === to || from < 0 || to < 0 || from > last || to > last) return;
		const next = [...this.queue];
		const [moved] = next.splice(from, 1);
		next.splice(to, 0, moved);
		if (this.index === from) this.index = to;
		else if (from < this.index && to >= this.index) this.index -= 1;
		else if (from > this.index && to <= this.index) this.index += 1;
		this.queue = next;
	}

	/** Drop one entry, keeping playback sensible if it was the current one. */
	removeAt(i: number): void {
		const rest = [...this.queue];
		rest.splice(i, 1);
		if (i === this.index) {
			this.queue = rest;
			if (rest.length === 0) {
				this.#audio?.pause();
				this.index = -1;
			} else {
				this.playIndex(i >= rest.length ? 0 : i);
			}
		} else {
			if (i < this.index) this.index -= 1;
			this.queue = rest;
		}
	}

	/** Stop and forget everything — the popover's "clear". */
	clear(): void {
		this.#audio?.pause();
		if (this.#audio) this.#audio.removeAttribute('src');
		this.queue = [];
		this.index = -1;
		this.playing = false;
		this.currentTime = 0;
		this.duration = 0;
		this.coverFileId = null;
		this.#pendingSeek = null;
		this.persist();
	}

	// ── Media element events, forwarded by `AppShell` ─────────────────────────

	onPlay(): void {
		this.playing = true;
		this.#publishMediaSession();
		this.persist();
	}

	onPause(): void {
		this.playing = false;
		// Pausing is the likeliest moment for a tab to be closed next.
		this.persist();
		this.#publishMediaSession();
	}

	onTimeUpdate(): void {
		this.currentTime = this.#audio?.currentTime ?? 0;
		// Throttled: see `POSITION_WRITE_MS`.
		if (Date.now() - this.#lastPositionWrite >= POSITION_WRITE_MS) this.persist();
	}

	/** Backfills a duration the server never recorded, so rows stop showing 0:00. */
	onLoadedMetadata(): { id: string; seconds: number } | null {
		this.duration = this.#audio?.duration ?? 0;
		// The restored position, now that there is a track to seek within.
		if (this.#pendingSeek !== null && this.#audio) {
			const target = Math.min(this.#pendingSeek, this.#audio.duration || this.#pendingSeek);
			if (Number.isFinite(target) && target > 0) this.#audio.currentTime = target;
			this.#pendingSeek = null;
		}
		const track = this.current;
		const seconds = this.#audio?.duration;
		if (!track || !seconds || track.duration_secs) return null;
		const rounded = Math.round(seconds);
		this.queue = this.queue.map((q) => (q.id === track.id ? { ...q, duration_secs: rounded } : q));
		// Returned so the music page can patch its own visible rows too.
		return { id: track.id, seconds: rounded };
	}

	onEnded(): void {
		if (this.repeat === 'one') {
			if (this.#audio) this.#audio.currentTime = 0;
			void this.#audio?.play().catch(() => {});
			return;
		}
		this.next();
	}

	onError(): void {
		this.playing = false;
	}

	// ── Surviving a reload ────────────────────────────────────────────────────
	//
	// Media Session does NOT help here: it describes the current page session
	// to the OS, and a refresh destroys the document, the element and the
	// session together. Resuming is ours to do, so the queue, the position and
	// the modes go to localStorage — per-viewer convenience, never shared
	// state, and nothing here is authoritative: losing it costs a resume, not
	// data.

	/** Write the current state. Cheap, but not cheap enough for `timeupdate`. */
	persist(): void {
		try {
			if (this.queue.length === 0) {
				localStorage.removeItem(STATE_KEY);
				return;
			}
			const state: PersistedState = {
				v: 1,
				queue: this.queue.slice(0, MAX_PERSISTED_TRACKS),
				index: this.index,
				position: this.currentTime,
				coverFileId: this.coverFileId,
				shuffle: this.shuffle,
				repeat: this.repeat,
				playing: this.playing
			};
			localStorage.setItem(STATE_KEY, JSON.stringify(state));
			this.#lastPositionWrite = Date.now();
		} catch {
			/* private mode / quota — the session just will not resume */
		}
	}

	/**
	 * Rehydrate from storage. Called once, by `MusicPlayer`, after the element
	 * is attached.
	 *
	 * Playback is NOT forced: a reload carries no user gesture, so most
	 * browsers refuse `play()` outright. We ask anyway when the tab was
	 * playing — Chrome allows it on an origin the viewer uses often — and
	 * otherwise stay paused at the right position, which is the part that
	 * actually matters.
	 */
	restore(): void {
		if (this.#restored) return;
		this.#restored = true;
		const state = readPersistedState();
		// A value written by a future (or broken) version is ignored rather
		// than half-applied: a player that resumes into nonsense is worse than
		// one that starts empty.
		if (!state || state.v !== 1 || !Array.isArray(state.queue) || state.queue.length === 0) {
			return;
		}
		this.queue = state.queue;
		this.index = Math.min(Math.max(state.index, 0), state.queue.length - 1);
		this.coverFileId = state.coverFileId ?? null;
		this.shuffle = !!state.shuffle;
		this.repeat = state.repeat ?? 'none';
		// `currentTime` is a display value until the element has the track;
		// the real seek waits for `loadedmetadata`, the first moment a
		// duration — and so a valid seek target — exists.
		this.currentTime = state.position ?? 0;
		this.#pendingSeek = state.position ?? 0;
		this.syncSource();
		if (state.playing) queueMicrotask(() => void this.#audio?.play().catch(() => {}));
	}

	// ── OS integration ────────────────────────────────────────────────────────

	/**
	 * Hand the current track to the OS: lock screen, keyboard media keys,
	 * headphone buttons, the macOS Now Playing widget.
	 *
	 * This matters more than usual here because the in-app transport lives
	 * behind a popover — with Media Session, the common case (pause, skip)
	 * needs no OxiCloud UI at all.
	 *
	 * Feature-detected: Safari and Firefox support differs, and a missing API
	 * must not break playback itself.
	 */
	#publishMediaSession(): void {
		if (typeof navigator === 'undefined' || !('mediaSession' in navigator)) return;
		const ms = navigator.mediaSession;
		const track = this.current;
		if (!track) {
			ms.metadata = null;
			ms.playbackState = 'none';
			return;
		}
		const cover = this.coverUrl;
		ms.metadata = new MediaMetadata({
			title: this.label(track),
			artist: track.artist ?? '',
			album: track.album ?? '',
			// The playlist cover doubles as artwork; sizes are a hint, and the
			// OS scales whatever it gets.
			//
			// With no cover, the app's own mark rather than an empty list: the
			// OS does not leave the space blank, it fills it with a generic
			// music or browser glyph, and on a lock screen next to Spotify and
			// a podcast app that is the one place the source should be obvious.
			// PNG, not the SVG: several platforms decode the artwork natively
			// and simply drop an SVG.
			artwork: cover
				? [{ src: cover, sizes: '512x512' }]
				: [{ src: `${base}${FALLBACK_ARTWORK}`, sizes: '512x512', type: 'image/png' }]
		});
		ms.playbackState = this.playing ? 'playing' : 'paused';
	}

	/** Registered once by `AppShell`; handlers stay valid for the session. */
	bindMediaKeys(): void {
		if (typeof navigator === 'undefined' || !('mediaSession' in navigator)) return;
		const ms = navigator.mediaSession;
		const set = (action: MediaSessionAction, handler: () => void) => {
			try {
				ms.setActionHandler(action, handler);
			} catch {
				/* action unsupported in this browser — the rest still work */
			}
		};
		set('play', () => this.toggle());
		set('pause', () => this.toggle());
		set('previoustrack', () => this.prev());
		set('nexttrack', () => this.next());
		set('stop', () => this.clear());
	}

	#applyVolume(): void {
		if (!this.#audio) return;
		this.#audio.volume = this.volume;
		this.#audio.muted = this.muted;
	}
}

/**
 * The saved session, or null when there is nothing usable.
 *
 * A free function rather than a method because it must survive anything the
 * stored value throws at it — absent, unparseable, or written by a version
 * that shaped it differently — and return one clean answer either way.
 */
function readPersistedState(): PersistedState | null {
	try {
		const raw = localStorage.getItem(STATE_KEY);
		return raw ? (JSON.parse(raw) as PersistedState) : null;
	} catch {
		return null;
	}
}

function readStoredVolume(): number {
	try {
		const raw = localStorage.getItem(VOLUME_KEY);
		const v = raw === null ? NaN : Number(raw);
		return Number.isFinite(v) && v >= 0 && v <= 1 ? v : 0.7;
	} catch {
		return 0.7;
	}
}

export const player = new PlayerStore();
