import { it, expect, vi, beforeEach, afterEach } from 'vitest';
import { render, screen, fireEvent } from '@testing-library/svelte';
import { createRawSnippet } from 'svelte';
import Modal from './Modal.svelte';

const children = createRawSnippet(() => ({
	render: () => '<div data-testid="modal-body">content</div>'
}));

/**
 * Advance past the `requestAnimationFrame` tick that Modal's open-effect
 * uses to defer focus (so the dialog is in the DOM before the focus call
 * fires). Works in jsdom, which runs rAF via a setTimeout fallback.
 */
const nextRaf = () => new Promise<void>((resolve) => requestAnimationFrame(() => resolve()));

/**
 * jsdom has no layout engine, so `offsetParent` is null for every
 * nested element — which would cause Modal's `focusables()` filter
 * (`el.offsetParent !== null || el === document.activeElement`) to
 * drop every real control and leave the dialog div as the only
 * fallback. For the duration of a test, monkey-patch
 * `HTMLElement.prototype.offsetParent` to return something truthy so
 * `querySelectorAll(FOCUSABLE)` results pass the visibility filter
 * and the preference logic actually runs. Restored after each test
 * to keep this file's non-focus pins (which also use jsdom)
 * identical to how they ran before.
 */
let origOffsetParent: PropertyDescriptor | undefined;

beforeEach(() => {
	origOffsetParent = Object.getOwnPropertyDescriptor(HTMLElement.prototype, 'offsetParent');
	Object.defineProperty(HTMLElement.prototype, 'offsetParent', {
		get() {
			return this.parentNode ?? null;
		},
		configurable: true
	});
});

afterEach(() => {
	if (origOffsetParent) {
		Object.defineProperty(HTMLElement.prototype, 'offsetParent', origOffsetParent);
	}
});

it('renders the title and children when open', () => {
	render(Modal, { props: { open: true, title: 'My dialog', children } });
	expect(screen.getByTestId('modal')).toBeTruthy();
	expect(screen.getByText('My dialog')).toBeTruthy();
	expect(screen.getByTestId('modal-body')).toBeTruthy();
});

it('does not render when closed', () => {
	render(Modal, { props: { open: false, title: 'Hidden', children } });
	expect(screen.queryByTestId('modal')).toBeNull();
});

it('invokes onclose when the close button is clicked', async () => {
	const onclose = vi.fn();
	render(Modal, { props: { open: true, title: 'X', onclose, children } });
	await fireEvent.click(screen.getByTestId('modal-close-btn'));
	expect(onclose).toHaveBeenCalled();
});

it('closes on Escape', async () => {
	const onclose = vi.fn();
	render(Modal, { props: { open: true, title: 'X', onclose, children } });
	await fireEvent.keyDown(window, { key: 'Escape' });
	expect(onclose).toHaveBeenCalled();
});

// ── Initial-focus preference ──────────────────────────────────────────
//
// The audit on `feat/modal-error-handling` found that 14 prompt-dialog
// callers across 7 route files all fell through to the Modal's default
// first-focusable → the header's × close button. Users opening a
// rename / new-folder / "add to playlist" prompt had to click or Tab
// before typing. Modal now prefers the first text-entry control
// (`input[type=text|search|email|url|password|tel|number|date|…]`,
// `<textarea>`, or `contenteditable`) over any other focusable,
// falling back to the generic behaviour when the dialog carries no
// text input (e.g. confirm-only modals).

it('prefers a text input over the close button on open', async () => {
	const withInput = createRawSnippet(() => ({
		render: () => '<div><input data-testid="rename-input" type="text" value="report.pdf" /></div>'
	}));
	render(Modal, { props: { open: true, title: 'Rename', children: withInput } });
	await nextRaf();
	const input = screen.getByTestId('rename-input') as HTMLInputElement;
	expect(document.activeElement).toBe(input);
});

it('falls back to the first focusable when no text input exists', async () => {
	// Confirm-shape dialog: Cancel + Delete buttons, no input. The
	// header × is the first focusable in reading order, so the current
	// generic fallback still lands there — which is safe, Cancel-ish
	// (closes the dialog on Enter/Space, no destructive side effect).
	const confirmShape = createRawSnippet(() => ({
		render: () =>
			'<div><button type="button">Cancel</button><button type="button">Delete</button></div>'
	}));
	render(Modal, { props: { open: true, title: 'Delete?', children: confirmShape } });
	await nextRaf();
	const closeBtn = screen.getByTestId('modal-close-btn');
	expect(document.activeElement).toBe(closeBtn);
});

it('does not prefer non-text input types (e.g. type=checkbox)', async () => {
	// Preference matches `type` text-shapes only. A dialog whose first
	// control is a checkbox should NOT grab the auto-focus — unchecking
	// a box on open would be surprising. The × is the stable fallback.
	const withCheckbox = createRawSnippet(() => ({
		render: () =>
			'<div><input type="checkbox" data-testid="opt-check" /><button type="button">Save</button></div>'
	}));
	render(Modal, { props: { open: true, title: 'Options', children: withCheckbox } });
	await nextRaf();
	const closeBtn = screen.getByTestId('modal-close-btn');
	expect(document.activeElement).toBe(closeBtn);
});
