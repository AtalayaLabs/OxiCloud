import { describe, it, expect, vi, afterEach } from 'vitest';
import { dragAutoScroll, edgeStep } from './dragAutoScroll';
import { ITEM_DRAG_MIME } from './dragModifiers';

// Regression: a drag could not reach a drop target above the fold.
//
// Browsers auto-scroll near the WINDOW edges, which made the gap asymmetric
// and easy to miss. Dragging down, the pointer stays inside `.content-area`
// (the scroller) to the bottom of the window, so the native behaviour fires.
// Dragging up it crosses into `.top-bar`, a sibling of the scroller, well
// before the window edge — so nothing scrolled and anything above the fold
// was undroppable. Both directions are now explicit, and the listener is on
// the window precisely because the pointer is over a sibling at the moment
// the container needs to scroll.

describe('edgeStep', () => {
	// Container occupying y = 100..600.
	const top = 100;
	const bottom = 600;

	it('is zero in the middle', () => {
		expect(edgeStep(350, top, bottom)).toBe(0);
	});

	it('scrolls up near the top and down near the bottom', () => {
		expect(edgeStep(120, top, bottom)).toBeLessThan(0);
		expect(edgeStep(580, top, bottom)).toBeGreaterThan(0);
	});

	// The upward case is the one that was broken, and the pointer is usually
	// PAST the top edge when it matters — over the top bar, not over the
	// scroller. Saturating rather than going quiet is what makes that work.
	it('saturates past either edge instead of dropping to zero', () => {
		const atTop = edgeStep(top, top, bottom);
		expect(atTop).toBeLessThan(0);
		expect(edgeStep(top - 50, top, bottom)).toBe(atTop);
		const atBottom = edgeStep(bottom, top, bottom);
		expect(atBottom).toBeGreaterThan(0);
		expect(edgeStep(bottom + 50, top, bottom)).toBe(atBottom);
	});

	it('ramps with depth into the zone', () => {
		// Deeper into the zone ⇒ faster, so the speed is controllable rather
		// than all-or-nothing.
		const shallow = Math.abs(edgeStep(top + 70, top, bottom));
		const deep = Math.abs(edgeStep(top + 10, top, bottom));
		expect(deep).toBeGreaterThan(shallow);
		expect(shallow).toBeGreaterThan(0);
	});

	it('gives the nearer edge priority when the zones overlap', () => {
		// A container shorter than two zones is near BOTH edges everywhere;
		// without a tie-break the two directions would fight over the middle.
		const shortTop = 0;
		const shortBottom = 100;
		expect(edgeStep(10, shortTop, shortBottom)).toBeLessThan(0);
		expect(edgeStep(90, shortTop, shortBottom)).toBeGreaterThan(0);
	});
});

describe('dragAutoScroll action', () => {
	const cleanups: Array<() => void> = [];

	afterEach(() => {
		while (cleanups.length) cleanups.pop()!();
		vi.restoreAllMocks();
	});

	/** A scrollable node with a known rect, plus the action attached. */
	function mount(scrollHeight = 2000) {
		const node = document.createElement('div');
		document.body.appendChild(node);
		vi.spyOn(node, 'getBoundingClientRect').mockReturnValue({
			top: 100,
			bottom: 600,
			left: 0,
			right: 800,
			width: 800,
			height: 500,
			x: 0,
			y: 100,
			toJSON: () => ({})
		} as DOMRect);
		// jsdom does no layout, so model the scroll range by hand.
		let scrollTop = 500;
		Object.defineProperty(node, 'scrollTop', {
			get: () => scrollTop,
			set: (v: number) => {
				scrollTop = Math.max(0, Math.min(scrollHeight - 500, v));
			},
			configurable: true
		});
		const handle = dragAutoScroll(node);
		cleanups.push(() => {
			handle.destroy();
			node.remove();
		});
		return node;
	}

	function dragOverAt(clientY: number, types: string[] = [ITEM_DRAG_MIME]) {
		const ev = new Event('dragover', { bubbles: true }) as DragEvent;
		Object.defineProperty(ev, 'clientY', { value: clientY });
		Object.defineProperty(ev, 'dataTransfer', { value: { types } });
		window.dispatchEvent(ev);
	}

	/** Runs one animation frame. */
	function frame() {
		return new Promise<void>((r) => requestAnimationFrame(() => r()));
	}

	it('scrolls up when the drag is over the top edge', async () => {
		const node = mount();
		const before = node.scrollTop;

		// Above the container entirely — i.e. over the top bar, which is where
		// the pointer actually is during this gesture.
		dragOverAt(80);
		await frame();

		expect(node.scrollTop).toBeLessThan(before);
	});

	it('scrolls down when the drag is over the bottom edge', async () => {
		const node = mount();
		const before = node.scrollTop;

		dragOverAt(590);
		await frame();

		expect(node.scrollTop).toBeGreaterThan(before);
	});

	it('does not scroll from the middle', async () => {
		const node = mount();
		const before = node.scrollTop;

		dragOverAt(350);
		await frame();
		await frame();

		expect(node.scrollTop).toBe(before);
	});

	// Text selections, link drags and anything else dragged from another app
	// must not move the view; only our item drags and OS file drags (which can
	// be dropped onto a specific folder row) do.
	it('ignores drags it does not accept', async () => {
		const node = mount();
		const before = node.scrollTop;

		dragOverAt(590, ['text/plain']);
		await frame();
		await frame();

		expect(node.scrollTop).toBe(before);
	});

	it('accepts OS file drags', async () => {
		const node = mount();
		const before = node.scrollTop;

		dragOverAt(590, ['Files']);
		await frame();

		expect(node.scrollTop).toBeGreaterThan(before);
	});

	it('stops on drop', async () => {
		const node = mount();
		dragOverAt(590);
		await frame();
		const afterDrag = node.scrollTop;

		window.dispatchEvent(new Event('drop', { bubbles: true }));
		await frame();
		await frame();

		expect(node.scrollTop).toBe(afterDrag);
	});

	it('stops on dragend, so a cancelled drag does not keep scrolling', async () => {
		const node = mount();
		dragOverAt(590);
		await frame();
		const afterDrag = node.scrollTop;

		window.dispatchEvent(new Event('dragend', { bubbles: true }));
		await frame();
		await frame();

		expect(node.scrollTop).toBe(afterDrag);
	});

	it('detaches its listeners on destroy', async () => {
		const node = mount();
		const before = node.scrollTop;
		// `mount` registered the teardown; run it now.
		cleanups.pop()!();

		dragOverAt(590);
		await frame();

		expect(node.scrollTop).toBe(before);
		node.remove();
	});
});
