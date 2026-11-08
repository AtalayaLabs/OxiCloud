/**
 * Auto-scroll a container while something is dragged near its edges, so a
 * drop target below or above the fold can be reached without letting go.
 *
 * Browsers do this natively near the WINDOW edges, which is why the gap was
 * asymmetric here and easy to miss: dragging down, the pointer stays inside
 * `.content-area` (the scroller) all the way to the bottom of the window, so
 * the native behaviour fires. Dragging up, it crosses into `.top-bar` — a
 * sibling of the scroller, not part of it — long before reaching the top of
 * the window, so nothing scrolled and anything above the fold was simply
 * undroppable (Ed, 2026-11-08: "scroll down works right now but not scroll
 * up").
 *
 * Hence listening on `window` rather than on the container: the pointer is
 * routinely over something else (the top bar, the sidebar, a sticky header)
 * at the moment the container needs to scroll.
 */
import { ITEM_DRAG_MIME } from './dragModifiers';

/** How far from an edge auto-scroll begins, in px. */
const EDGE_ZONE = 80;
/** Scroll per frame at the very edge, in px (~60fps → ~1100px/s). */
const MAX_STEP = 18;
/**
 * Give up if no `dragover` has been seen for this long. Browsers fire the
 * event repeatedly while a drag hovers, even without movement, so silence
 * means the drag ended in a way we did not observe — a drop into another
 * window, or an `Escape` the page never saw. Without this the rAF loop would
 * keep scrolling on its own.
 */
const STALE_MS = 250;

/**
 * Scroll step for a pointer at `y`, given the container's top and bottom.
 * Negative scrolls up. Zero outside both edge zones.
 *
 * Ramps linearly with depth into the zone, and saturates once the pointer
 * passes the edge entirely — which is the normal case here, since the
 * pointer spends that part of the gesture over the top bar or past the
 * bottom of the window.
 *
 * Exported for its own tests: the arithmetic is the part worth pinning, and
 * jsdom cannot exercise it through a real drag.
 */
export function edgeStep(y: number, top: number, bottom: number, zone = EDGE_ZONE): number {
	// A container shorter than two zones would have them overlap, making every
	// position both "near the top" and "near the bottom"; let the nearer edge
	// win rather than fighting over the middle.
	const fromTop = y - top;
	const fromBottom = bottom - y;
	if (fromTop < fromBottom) {
		if (fromTop >= zone) return 0;
		return -MAX_STEP * ramp(fromTop, zone);
	}
	if (fromBottom >= zone) return 0;
	return MAX_STEP * ramp(fromBottom, zone);
}

/** 0 at the far side of the zone, 1 at the edge and beyond. */
function ramp(distance: number, zone: number): number {
	return Math.min(1, Math.max(0, (zone - distance) / zone));
}

interface Options {
	/**
	 * Drag types that should scroll. Defaults to in-app item drags plus OS
	 * file drags — an uploaded file can be dropped onto a specific folder
	 * row, so it needs to reach one just as much.
	 */
	accept?: string[];
}

/**
 * Svelte action. Put it on the scrolling element:
 * `<main class="content-area" use:dragAutoScroll>`.
 */
export function dragAutoScroll(node: HTMLElement, options: Options = {}) {
	const accept = options.accept ?? [ITEM_DRAG_MIME, 'Files'];

	let step = 0;
	let frame = 0;
	let lastSeen = 0;

	function tick() {
		if (step === 0 || performance.now() - lastSeen > STALE_MS) {
			frame = 0;
			step = 0;
			return;
		}
		const before = node.scrollTop;
		node.scrollTop = before + step;
		// Already at the end in that direction: nothing more to do until the
		// pointer moves to the other edge, and spinning a frame loop that
		// cannot move anything is pure waste.
		if (node.scrollTop === before) {
			frame = 0;
			step = 0;
			return;
		}
		frame = requestAnimationFrame(tick);
	}

	function onDragOver(e: DragEvent) {
		const types = e.dataTransfer?.types;
		if (!types || !accept.some((t) => types.includes(t))) return;
		lastSeen = performance.now();
		const rect = node.getBoundingClientRect();
		step = edgeStep(e.clientY, rect.top, rect.bottom);
		if (step !== 0 && frame === 0) frame = requestAnimationFrame(tick);
	}

	function stop() {
		step = 0;
		if (frame !== 0) {
			cancelAnimationFrame(frame);
			frame = 0;
		}
	}

	// `dragover` bubbles to the window from wherever the pointer is, which is
	// the whole point. `drop` and `dragend` both end a drag — `dragend` fires
	// on the source element, so it also covers a cancelled drag.
	window.addEventListener('dragover', onDragOver);
	window.addEventListener('drop', stop);
	window.addEventListener('dragend', stop);

	return {
		destroy() {
			stop();
			window.removeEventListener('dragover', onDragOver);
			window.removeEventListener('drop', stop);
			window.removeEventListener('dragend', stop);
		}
	};
}
