/**
 * Copy-vs-move resolution for drag-and-drop, following OS convention.
 *
 * Extracted from `/files` so the sidebar drive drop reads the same
 * modifier keys the folder-row and breadcrumb drops do. A second copy of
 * this rule would mean the same gesture meaning different things in two
 * parts of the same window.
 */

/**
 * Platform sniff, for picking which modifier means "copy".
 *
 * `navigator.platform` is deprecated but still the most reliable signal
 * for the Mac/not-Mac split; `userAgent` is the fallback. Guarded for
 * SSR / prerender, where `navigator` does not exist — `adapter-static`
 * evaluates module top-level code during the build.
 */
export const IS_MAC =
	typeof navigator !== 'undefined' &&
	/Mac|iPhone|iPad|iPod/i.test(navigator.platform || navigator.userAgent || '');

/**
 * Does this drop mean COPY rather than move?
 *
 * Read the modifier keys DIRECTLY off the event — `DragEvent` extends
 * `MouseEvent`, so `altKey` / `metaKey` / `ctrlKey` are live on it. This
 * is deliberately independent of `dataTransfer.dropEffect`: that value
 * is not reliably updated by every browser for JS-initiated drags, while
 * the raw modifier bits on the drop event are. One source of truth.
 *
 * OS convention:
 *   - macOS: ⌥ Option (`altKey`) — Finder's. ⌘ (`metaKey`) accepted too
 *     as a forgiving fallback, since people reach for it.
 *   - Windows / Linux: Ctrl (`ctrlKey`) — Explorer / Nautilus.
 */
export function isCopyDrop(e: DragEvent): boolean {
	return IS_MAC ? e.altKey || e.metaKey : e.ctrlKey;
}
