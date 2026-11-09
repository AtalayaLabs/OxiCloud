const ITEM_SELECTOR = 'button:not([disabled]), a[href]';

export function focusFirstMenuItem(menu: HTMLElement | null): void {
	menu?.querySelector<HTMLElement>(ITEM_SELECTOR)?.focus();
}

/**
 * Where to place a menu anchored at (`x`, `y`) — the click coords — so it
 * stays inside the viewport, given the panel's measured size.
 *
 * Reads the element's SIZE, never its current position, so the result is
 * a pure function of the anchor and that size. Callers can therefore
 * assign it back to the style that positions the element without the
 * value drifting on every pass: feeding an already-clamped position back
 * in returns the same answer.
 */
export function clampMenuToViewport(
	menu: HTMLElement,
	x: number,
	y: number,
	margin = 8
): { x: number; y: number } {
	const rect = menu.getBoundingClientRect();
	return {
		x: Math.round(Math.max(margin, Math.min(x, window.innerWidth - rect.width - margin))),
		y: Math.round(Math.max(margin, Math.min(y, window.innerHeight - rect.height - margin)))
	};
}

export function handleMenuKeydown(
	e: KeyboardEvent,
	menu: HTMLElement | null,
	close: () => void
): void {
	if (e.key === 'Escape') {
		e.preventDefault();
		close();
		return;
	}
	if (e.key === 'Tab') {
		close();
		return;
	}
	if (!menu || !['ArrowDown', 'ArrowUp', 'Home', 'End'].includes(e.key)) return;
	const items = [...menu.querySelectorAll<HTMLElement>(ITEM_SELECTOR)];
	if (!items.length) return;
	e.preventDefault();
	const current = items.indexOf(document.activeElement as HTMLElement);
	let next: number;
	if (current < 0) next = e.key === 'ArrowUp' || e.key === 'End' ? items.length - 1 : 0;
	else if (e.key === 'ArrowDown') next = (current + 1) % items.length;
	else if (e.key === 'ArrowUp') next = (current - 1 + items.length) % items.length;
	else if (e.key === 'End') next = items.length - 1;
	else next = 0;
	items[next].focus();
}
