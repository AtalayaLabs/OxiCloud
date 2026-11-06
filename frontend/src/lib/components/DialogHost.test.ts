import { it, expect, beforeEach } from 'vitest';
import { render, screen, fireEvent, waitFor } from '@testing-library/svelte';
import { dialogs, confirmDialog, promptDialog, formDialog } from '$lib/stores/dialogs.svelte';
import DialogHost from './DialogHost.svelte';

beforeEach(() => {
	while (dialogs.current && !dialogs.busy) dialogs.cancel();
});

it('confirms a dialog and resolves true', async () => {
	render(DialogHost);
	const p = confirmDialog({ title: 'Delete?', message: 'Sure?' });
	await screen.findByTestId('dialog-host-confirm-btn');
	await fireEvent.click(screen.getByTestId('dialog-host-confirm-btn'));
	await expect(p).resolves.toBe(true);
});

it('cancels a dialog and resolves false', async () => {
	render(DialogHost);
	const p = confirmDialog({ title: 'Delete?', message: 'Sure?' });
	await screen.findByTestId('dialog-host-cancel-btn');
	await fireEvent.click(screen.getByTestId('dialog-host-cancel-btn'));
	await expect(p).resolves.toBe(false);
});

it('submits a prompt dialog with the typed value', async () => {
	render(DialogHost);
	const p = promptDialog({ title: 'Rename', defaultValue: 'old' });
	const input = await screen.findByTestId('dialog-host-prompt-input');
	await fireEvent.input(input, { target: { value: 'new-name' } });
	await fireEvent.click(screen.getByTestId('dialog-host-submit-btn'));
	await expect(p).resolves.toBe('new-name');
});

it('prefills the prompt input with the default value', async () => {
	render(DialogHost);
	void promptDialog({ title: 'Rename', defaultValue: 'preset.txt' });
	const input = (await screen.findByTestId('dialog-host-prompt-input')) as HTMLInputElement;
	await waitFor(() => expect(input.value).toBe('preset.txt'));
	dialogs.cancel();
});

it('submits a form dialog with every field value', async () => {
	render(DialogHost);
	const p = formDialog({
		title: 'Edit',
		fields: [
			{ key: 'name', label: 'Name', value: 'old' },
			{ key: 'description', label: 'Description', value: 'old desc', multiline: true }
		]
	});
	const name = await screen.findByTestId('dialog-host-input-name');
	await fireEvent.input(name, { target: { value: 'new' } });
	await fireEvent.input(screen.getByTestId('dialog-host-input-description'), {
		target: { value: 'new desc' }
	});
	await fireEvent.click(screen.getByTestId('dialog-host-submit-btn'));
	await expect(p).resolves.toEqual({ name: 'new', description: 'new desc' });
});
