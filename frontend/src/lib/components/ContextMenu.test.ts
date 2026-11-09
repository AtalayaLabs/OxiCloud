import { it, expect, vi } from 'vitest';
import { render, screen, fireEvent } from '@testing-library/svelte';
import ContextMenu, { type ContextMenuEntry } from './ContextMenu.svelte';

// `ContextMenu` was extracted from three hand-written copies of the same
// markup — `ResourceList`'s row menu, `/files`'s override menu and (new) the
// breadcrumb's. None of the three had a single test, so the extraction had
// nothing holding its behaviour in place; these pins are that.

function entry(over: Partial<ContextMenuEntry> = {}): ContextMenuEntry {
	return { key: 'k', label: 'Label', icon: 'eye', onselect: vi.fn(), ...over };
}

it('renders one menuitem per entry, in order, with the caller testids', () => {
	render(ContextMenu, {
		x: 10,
		y: 10,
		onclose: vi.fn(),
		testid: 'menu',
		entries: [
			entry({ key: 'open', label: 'Open', testid: 'ctx-open' }),
			entry({ key: 'rename', label: 'Rename', testid: 'ctx-rename' })
		]
	});

	const items = screen.getAllByRole('menuitem');
	expect(items.map((el) => el.textContent?.trim())).toEqual(['Open', 'Rename']);
	// Forwarded verbatim: the e2e suites select on each call site's own
	// hooks, so the shared component must not invent its own scheme.
	expect(screen.getByTestId('ctx-open')).toBe(items[0]);
	expect(screen.getByTestId('ctx-rename')).toBe(items[1]);
});

it('runs onselect for a plain entry and leaves closing to the caller', async () => {
	const onselect = vi.fn();
	const onclose = vi.fn();
	render(ContextMenu, { x: 0, y: 0, onclose, entries: [entry({ onselect })] });

	await fireEvent.click(screen.getByRole('menuitem'));

	expect(onselect).toHaveBeenCalledTimes(1);
	// The component must NOT close itself: `/files` reads its target
	// inside the handler, and closing first clears it.
	expect(onclose).not.toHaveBeenCalled();
});

// A danger entry is the one place a single click must not act. It renders a
// ConfirmButton, so the first click only arms it — losing that would turn
// "right-click, misaim" into a delete.
it('arms a danger entry instead of running it on the first click', async () => {
	const onselect = vi.fn();
	render(ContextMenu, {
		x: 0,
		y: 0,
		onclose: vi.fn(),
		entries: [entry({ key: 'del', label: 'Delete', icon: 'trash', danger: true, onselect })]
	});
	const btn = screen.getByRole('menuitem');
	expect(btn.classList.contains('menu-item--danger')).toBe(true);

	await fireEvent.click(btn);
	expect(onselect).not.toHaveBeenCalled();

	await fireEvent.click(btn);
	expect(onselect).toHaveBeenCalledTimes(1);
});

// The `/files` download entry is an `<a download>`, not a button: the browser
// performs the download from the href. Rendering it as a button would make
// the entry do nothing at all.
it('renders an href entry as a download link and still dismisses', async () => {
	const onselect = vi.fn();
	render(ContextMenu, {
		x: 0,
		y: 0,
		onclose: vi.fn(),
		entries: [entry({ key: 'dl', label: 'Download', href: '/api/files/f1/download', onselect })]
	});

	const link = screen.getByRole('menuitem');
	expect(link.tagName).toBe('A');
	expect(link.getAttribute('href')).toBe('/api/files/f1/download');
	expect(link.hasAttribute('download')).toBe(true);
	// `rel="external"` keeps SvelteKit's router from intercepting it.
	expect(link.getAttribute('rel')).toBe('external');

	await fireEvent.click(link);
	expect(onselect).toHaveBeenCalledTimes(1);
});

// Both halves matter: the attribute is what a browser enforces, and the
// handler guard is what holds in any host that delivers the click anyway
// (jsdom does, which is how this assertion first failed).
it('renders a disabled entry as non-interactive', async () => {
	const onselect = vi.fn();
	render(ContextMenu, {
		x: 0,
		y: 0,
		onclose: vi.fn(),
		entries: [entry({ disabled: true, onselect })]
	});

	const btn = screen.getByRole('menuitem') as HTMLButtonElement;
	expect(btn.disabled).toBe(true);
	await fireEvent.click(btn);
	expect(onselect).not.toHaveBeenCalled();
});

it('closes on the scrim, on Escape and on Tab', async () => {
	const onclose = vi.fn();
	render(ContextMenu, {
		x: 0,
		y: 0,
		onclose,
		testid: 'menu',
		scrimTestid: 'scrim',
		entries: [entry()]
	});

	await fireEvent.click(screen.getByTestId('scrim'));
	expect(onclose).toHaveBeenCalledTimes(1);

	await fireEvent.keyDown(screen.getByTestId('menu'), { key: 'Escape' });
	expect(onclose).toHaveBeenCalledTimes(2);

	// Tab closes rather than trapping focus — the menu is a transient
	// overlay, not a dialog.
	await fireEvent.keyDown(screen.getByTestId('menu'), { key: 'Tab' });
	expect(onclose).toHaveBeenCalledTimes(3);
});

// The scrim swallows a second right-click. Without this the browser's native
// menu opens on top of ours, which is how the three hand-written copies all
// behaved — and the one place they all agreed.
it('suppresses the native menu on the scrim', () => {
	render(ContextMenu, {
		x: 0,
		y: 0,
		onclose: vi.fn(),
		scrimTestid: 'scrim',
		entries: [entry()]
	});

	const ev = new MouseEvent('contextmenu', { bubbles: true, cancelable: true });
	screen.getByTestId('scrim').dispatchEvent(ev);
	expect(ev.defaultPrevented).toBe(true);
});

// Placement: the panel is anchored at the click, then pulled back inside the
// viewport using its MEASURED size. The old call sites each guessed that size
// instead (`entries.length * 44 + 24` in one, a flat `320` in the other), so a
// menu near the bottom edge either hung off-screen or floated well above the
// cursor depending on which copy you hit.
it('clamps the panel inside the viewport once measured', async () => {
	// jsdom reports a zero-size rect by default; give the panel a size so
	// the clamp has something to work with.
	const rect = vi
		.spyOn(HTMLElement.prototype, 'getBoundingClientRect')
		.mockReturnValue({ width: 200, height: 300, left: 0, top: 0 } as DOMRect);
	try {
		render(ContextMenu, {
			// Deep in the bottom-right corner of jsdom's 1024x768 viewport.
			x: 1000,
			y: 700,
			onclose: vi.fn(),
			testid: 'menu',
			entries: [entry()]
		});
		await vi.waitFor(() => {
			const el = screen.getByTestId('menu');
			// 1024 - 200 - 8 margin, 768 - 300 - 8 margin.
			expect(el.style.left).toBe('816px');
			expect(el.style.top).toBe('460px');
		});
	} finally {
		rect.mockRestore();
	}
});
