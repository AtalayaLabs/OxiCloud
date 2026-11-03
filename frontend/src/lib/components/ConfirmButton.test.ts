import { it, expect, vi } from 'vitest';
import { render, screen, fireEvent } from '@testing-library/svelte';
import { createRawSnippet } from 'svelte';
import ConfirmButton from './ConfirmButton.svelte';

const label = () => createRawSnippet(() => ({ render: () => '<span>Delete permanently</span>' }));

// Regression pin: the confirm label used to be an absolutely positioned pill
// laid over the button, so a narrow caller (icon cell, `.btn-action`, the
// playlist trash button) showed the label spilling past its edges. It then hid
// the caller's content wholesale, which dropped the delete icon. The armed face
// is now the same icon with "Confirm" in place of the caller's text.
it('keeps the icon and swaps the caller text for the confirm label', async () => {
	render(ConfirmButton, { onconfirm: vi.fn(), icon: 'trash', title: 'Delete', children: label() });
	const btn = screen.getByRole('button');

	expect(btn.querySelector('svg')).toBeTruthy();
	expect(btn.textContent).toContain('Delete permanently');

	await fireEvent.click(btn);

	const confirmLabel = btn.querySelector('.confirm-btn__label');
	expect(confirmLabel?.textContent).toBe('Really?');
	expect(btn.lastElementChild).toBe(confirmLabel);
	expect(btn.querySelector('svg')).toBeTruthy();
	expect(btn.textContent?.trim()).toBe('Really?');
	expect(btn.classList.contains('confirm-btn--armed')).toBe(true);
});

it('fires onconfirm on the second click and disarms', async () => {
	const onconfirm = vi.fn();
	render(ConfirmButton, { onconfirm, icon: 'trash', title: 'Delete', children: label() });
	const btn = screen.getByRole('button');

	await fireEvent.click(btn);
	expect(onconfirm).not.toHaveBeenCalled();

	await fireEvent.click(btn);
	expect(onconfirm).toHaveBeenCalledTimes(1);
	expect(btn.classList.contains('confirm-btn--armed')).toBe(false);
	expect(btn.textContent).toContain('Delete permanently');
});
