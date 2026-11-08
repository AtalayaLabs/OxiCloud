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

/** Per-viewer convenience, not shared state: see `localStoragePrefs`. */
const VOLUME_KEY = 'oxi-player-volume';

class PlayerStore {
	/** The element, owned by `AppShell`. Null until it mounts. */
	#audio: HTMLAudioElement | null = null;

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
	}

	// ── Media element events, forwarded by `AppShell` ─────────────────────────

	onPlay(): void {
		this.playing = true;
		this.#publishMediaSession();
	}

	onPause(): void {
		this.playing = false;
		this.#publishMediaSession();
	}

	onTimeUpdate(): void {
		this.currentTime = this.#audio?.currentTime ?? 0;
	}

	/** Backfills a duration the server never recorded, so rows stop showing 0:00. */
	onLoadedMetadata(): { id: string; seconds: number } | null {
		this.duration = this.#audio?.duration ?? 0;
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
			artwork: cover ? [{ src: cover, sizes: '512x512' }] : []
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
