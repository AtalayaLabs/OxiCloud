import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import { describe, it, expect } from 'vitest';
import { render, screen } from '@testing-library/svelte';
import EmptyState from './EmptyState.svelte';

describe('EmptyState', () => {
	it('marks an error state assertively and tags the icon', () => {
		render(EmptyState, { icon: 'exclamation-circle', title: 'Failed to fetch', error: true });

		const el = screen.getByTestId('empty-state');
		// `role="alert"` only in the error case — a merely empty folder is not
		// worth interrupting a screen reader for.
		expect(el.getAttribute('role')).toBe('alert');
		expect(el.classList.contains('empty-state--error')).toBe(true);
		expect(el.querySelector('.empty-state__icon')).toBeTruthy();
	});

	it('is quiet for an ordinary empty state', () => {
		render(EmptyState, { icon: 'folder-open', title: 'Nothing here' });

		const el = screen.getByTestId('empty-state');
		expect(el.getAttribute('role')).toBeNull();
		expect(el.classList.contains('empty-state--error')).toBe(false);
	});

	// Regression: every error state in the app was invisible in light mode.
	//
	// The error icon was coloured `--color-danger-text`, which is `#ffffff` in
	// BOTH themes — it is the foreground for text sitting ON a red fill
	// (`.btn-danger`, the batch toolbar, the logout item, a notification
	// badge), not a standalone glyph colour. Over the bright page background
	// it painted white on white; dark mode hid the mistake, which is why it
	// survived. `--color-error-text` is the `light-dark()` pair meant for
	// standalone use: deep red on light, soft coral on dark.
	//
	// Asserted against the component source because jsdom resolves no custom
	// properties — there is no computed colour to read. Crude, but it pins the
	// exact thing that was wrong, and the trap is easy to fall back into:
	// `danger` reads like the obvious token name for an error.
	it('colours the error icon with the standalone error token, not the fill foreground', () => {
		const src = readFileSync(resolve('src/lib/components/EmptyState.svelte'), 'utf-8');

		expect(src).toContain('color: var(--color-error-text)');
		// Matched as a DECLARATION, not a substring: the comment above that
		// rule names the bad token on purpose, and a bare `includes` check
		// flagged the explanation instead of the code (it did, first run).
		expect(
			/color:\s*var\(--color-danger-text\)/.test(src),
			'`--color-danger-text` is #ffffff in both themes — white on the light ' +
				'page background. Use `--color-error-text` for a standalone glyph.'
		).toBe(false);
	});
});
