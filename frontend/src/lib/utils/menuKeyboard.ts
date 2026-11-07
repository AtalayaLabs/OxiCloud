const ITEM_SELECTOR = 'button:not([disabled]), a[href]';

export function focusFirstMenuItem(menu: HTMLElement | null): void {
	menu?.querySelector<HTMLElement>(ITEM_SELECTOR)?.focus();
}

export function clampMenuToViewport(menu: HTMLElement, margin = 8): { x: number; y: number } {
	const rect = menu.getBoundingClientRect();
	return {
		x: Math.round(Math.max(margin, Math.min(rect.left, window.innerWidth - rect.width - margin))),
		y: Math.round(Math.max(margin, Math.min(rect.top, window.innerHeight - rect.height - margin)))
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
