import { it, expect, vi } from 'vitest';
import { render, screen, fireEvent } from '@testing-library/svelte';
import LanguageSelect from './LanguageSelect.svelte';

// Regression pin: the closed select used to render bare locale codes while the
// menu listed them, so the visible value ("en") and the option the user picked
// could be read as different things. The selected value is now the option's own
// label (flag + endonym), which is what the user sees before opening the menu.
it('shows the selected locale as its flag + endonym, matching the option label', () => {
	render(LanguageSelect, { value: 'fr', onchange: vi.fn() });
	const select = screen.getByRole('combobox') as HTMLSelectElement;
	expect(select.value).toBe('fr');
	expect(select.selectedOptions[0].textContent?.trim()).toBe('🇫🇷 Français');
});

it('prepends the automatic option only when a label is given', () => {
	render(LanguageSelect, { value: '', automaticLabel: 'Automatic', onchange: vi.fn() });
	const select = screen.getByRole('combobox') as HTMLSelectElement;
	expect(select.value).toBe('');
	expect(select.selectedOptions[0].textContent?.trim()).toBe('Automatic');
	expect(screen.getAllByRole('option')).toHaveLength(17);
});

it('reports the picked locale code', async () => {
	const onchange = vi.fn();
	render(LanguageSelect, { value: 'en', onchange });
	await fireEvent.change(screen.getByRole('combobox'), { target: { value: 'ja' } });
	expect(onchange).toHaveBeenCalledWith('ja');
});
