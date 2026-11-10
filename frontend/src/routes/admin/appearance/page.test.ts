import { render, screen, fireEvent, waitFor } from '@testing-library/svelte';
import { describe, it, expect, vi, beforeEach } from 'vitest';
import Page from './+page.svelte';
import { getAppearance, selectTheme, reloadThemes } from '$lib/api/endpoints/admin';

vi.mock('$lib/api/endpoints/admin', () => ({
	getAppearance: vi.fn(),
	selectTheme: vi.fn(),
	reloadThemes: vi.fn()
}));
vi.mock('$lib/i18n/index.svelte', () => ({
	t: (_key: string, paramsOrFallback: string | Record<string, string>, fallback?: string) =>
		typeof paramsOrFallback === 'string' ? paramsOrFallback : (fallback ?? _key)
}));
const appearance = {
	available_themes: ['default', 'sober'],
	default_theme_id: 'default',
	selected_theme_id: null,
	active_theme_id: 'default',
	warning: null,
	theme_diagnostics: []
};
beforeEach(() => {
	vi.mocked(getAppearance).mockResolvedValue(appearance);
});
describe('global appearance', () => {
	it('keeps the selection and inline error when persistence fails', async () => {
		// Regression pin: a rejected save must retain the selection and show
		// an inline error instead of presenting a successful global change.
		vi.mocked(selectTheme).mockRejectedValueOnce(new Error('Storage unavailable'));
		render(Page);
		const input = await screen.findByLabelText<HTMLSelectElement>('Global theme');
		await fireEvent.change(input, { target: { value: 'sober' } });
		await fireEvent.click(screen.getByRole('button', { name: 'Save' }));
		await screen.findByRole('alert');
		expect(input.value).toBe('sober');
		expect(input.getAttribute('aria-invalid')).toBe('true');
		expect(selectTheme).toHaveBeenCalledWith('sober');
		vi.mocked(selectTheme).mockResolvedValueOnce({
			...appearance,
			selected_theme_id: 'sober',
			active_theme_id: 'sober'
		});
		await fireEvent.click(screen.getByRole('button', { name: 'Save' }));
		await waitFor(() =>
			expect(screen.getByRole('status').textContent).toContain('next full page load')
		);
		expect(screen.queryByRole('alert')).toBeNull();
	});
});

it('refreshes the catalogue and retains unsaved selection on failure', async () => {
	render(Page);
	const input = await screen.findByLabelText<HTMLSelectElement>('Global theme');
	await fireEvent.change(input, { target: { value: 'sober' } });
	vi.mocked(reloadThemes).mockResolvedValueOnce({
		...appearance,
		available_themes: ['default', 'sober', 'modern-pro']
	});
	await fireEvent.click(screen.getByRole('button', { name: 'Refresh' }));
	await screen.findByRole('option', { name: 'Modern Pro' });
	expect(input.value).toBe('sober');
	vi.mocked(reloadThemes).mockRejectedValueOnce(new Error('Unreadable directory'));
	await fireEvent.click(screen.getByRole('button', { name: 'Refresh' }));
	await screen.findByRole('alert');
	expect(input.value).toBe('sober');
	expect(screen.getByRole('option', { name: 'Modern Pro' })).toBeTruthy();
});

it('shows recovery mode and disables theme selection', async () => {
	vi.mocked(getAppearance).mockResolvedValueOnce({
		...appearance,
		warning: 'forced_default',
		selected_theme_id: 'sober'
	});
	render(Page);
	const select = await screen.findByLabelText<HTMLSelectElement>('Global theme');
	expect(select.disabled).toBe(true);
	expect((screen.getByRole('button', { name: 'Save' }) as HTMLButtonElement).disabled).toBe(true);
	expect(screen.getByRole('status').textContent).toContain('recovery');
});
