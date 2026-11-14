import { it, expect, vi, beforeEach } from 'vitest';

const notify = vi.hoisted(() => vi.fn());
const goto = vi.hoisted(() => vi.fn());
const auth = vi.hoisted(() => ({ value: true }));
const owned = vi.hoisted(() => ({ value: false }));

vi.mock('$lib/stores/ui.svelte', () => ({ ui: { notify } }));
vi.mock('$lib/stores/session.svelte', () => ({
	session: {
		get isAuthenticated() {
			return auth.value;
		}
	}
}));
vi.mock('$lib/stores/osFileDrop.svelte', () => ({
	osFileDrop: {
		get claimed() {
			return owned.value;
		}
	}
}));
vi.mock('$app/navigation', () => ({ goto }));
vi.mock('$app/paths', () => ({ resolve: (p: string) => p, base: '', assets: '' }));
vi.mock('$lib/i18n/index.svelte', () => ({ t: (_k: string, fallback?: string) => fallback ?? _k }));

import { isOsFileDrag, onOsFileDragOver, onOsFileDrop } from './osFileDropGuard';

/** A DragEvent stand-in: jsdom has no DataTransfer worth constructing, and
 *  the guard only ever reads `types` and writes `dropEffect`. */
function dragEvent(types: string[], opts: { defaultPrevented?: boolean } = {}) {
	const e = {
		dataTransfer: { types, dropEffect: '' },
		defaultPrevented: opts.defaultPrevented ?? false,
		preventDefault() {
			this.defaultPrevented = true;
		}
	};
	return e as unknown as DragEvent & { dataTransfer: { dropEffect: string } };
}

beforeEach(() => {
	vi.clearAllMocks();
	auth.value = true;
	owned.value = false;
});

it('tells an OS file drag apart from a browser-internal one', () => {
	expect(isOsFileDrag(dragEvent(['Files']))).toBe(true);
	expect(isOsFileDrag(dragEvent(['application/x-oxicloud-item']))).toBe(false);
	expect(isOsFileDrag(dragEvent([]))).toBe(false);
});

// THE reason this guard exists: an un-prevented drop makes the browser
// navigate to the file, replacing the SPA and any unsaved state with it.
it('os drop: swallows an unhandled one so the browser cannot navigate to the file', () => {
	const e = dragEvent(['Files']);
	onOsFileDrop(e);
	expect(e.defaultPrevented).toBe(true);
	expect(notify).toHaveBeenCalledOnce();
});

// An in-app drag — moving an item to a folder, a drive, Favorites — must pass
// through untouched. Hijacking these would break every drag in the product.
it('browser drag: ignores an in-app item drag entirely', () => {
	const e = dragEvent(['application/x-oxicloud-item']);
	onOsFileDragOver(e);
	onOsFileDrop(e);
	expect(e.defaultPrevented).toBe(false);
	expect(notify).not.toHaveBeenCalled();
});

// `defaultPrevented` IS how a page says "mine" — `/files` and
// `ResourceList` prevent the default, so the guard must stay silent rather
// than toast over a successful upload.
it('os drop: stands down when an element already handled it', () => {
	const e = dragEvent(['Files'], { defaultPrevented: true });
	onOsFileDrop(e);
	expect(notify).not.toHaveBeenCalled();
});

// THE regression, caught by e2e and pinned here: `/files` handles drops
// anywhere on itself from a WINDOW listener, and two window listeners run in
// registration order — the layout's guard is always first, because it mounts
// first. Preventing the default here made the page's handler, running second,
// skip the upload as "already handled", and the user got "uploads only work in
// Files" while standing in Files. A claimed drop must come out of here
// completely untouched, `preventDefault` included.
it('os drop: leaves a claimed one entirely alone, including its default', () => {
	owned.value = true;
	const e = dragEvent(['Files']);
	onOsFileDrop(e);
	expect(e.defaultPrevented).toBe(false);
	expect(notify).not.toHaveBeenCalled();
});

// Same ownership rule one event earlier: the owner sets its own `dropEffect`
// (and `/files` varies it per target), so the guard must not touch the pointer.
it('os drop: leaves a claimed dragover alone too', () => {
	owned.value = true;
	const e = dragEvent(['Files']);
	onOsFileDragOver(e);
	expect(e.defaultPrevented).toBe(false);
	expect(e.dataTransfer.dropEffect).toBe('');
});

// `dropEffect = 'none'` would be the honest cursor, but it makes the browser
// reject the drop BEFORE `drop` fires — and then the explanation never shows.
it('os drop: accepts at the pointer level so the drop event still fires', () => {
	const e = dragEvent(['Files']);
	onOsFileDragOver(e);
	expect(e.defaultPrevented).toBe(true);
	expect(e.dataTransfer.dropEffect).toBe('copy');
});

// Decided 2026-11-13: "Go to Files" is meaningless on the login screen or a
// public share link. The navigation is still stopped — that is the data
// protection — but nothing is said.
it('os drop: still swallowed when logged out, but says nothing', () => {
	auth.value = false;
	const e = dragEvent(['Files']);
	onOsFileDrop(e);
	expect(e.defaultPrevented).toBe(true);
	expect(notify).not.toHaveBeenCalled();
});

// The toast's one affordance: the files cannot be forwarded (the browser
// discards the DataTransfer when the handler returns), so the offer is to
// land the user where uploads work.
it('os drop: offers Go to Files as the recovery action', () => {
	onOsFileDrop(dragEvent(['Files']));
	const opts = notify.mock.calls[0]?.[4] as { action?: { label: string; onClick: () => void } };
	expect(opts?.action?.label).toBe('Go to Files');
	opts?.action?.onClick();
	expect(goto).toHaveBeenCalledWith('/files');
});
