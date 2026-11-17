import { it, expect, vi, beforeEach } from 'vitest';
import { render, screen, fireEvent, waitFor } from '@testing-library/svelte';

const { goto } = vi.hoisted(() => ({ goto: vi.fn() }));
vi.mock('$app/navigation', () => ({ goto }));
vi.mock('$lib/api/endpoints/auth', () => ({ logout: vi.fn() }));
vi.mock('$lib/api/endpoints/search', () => ({
	searchResources: vi.fn(async () => ({ items: [], query_time_ms: 0 }))
}));
vi.mock('$lib/api/endpoints/files', () => ({ fileInlineUrl: () => '/in' }));
vi.mock('$lib/stores/dialogs.svelte', () => ({ confirmDialog: vi.fn() }));

import { searchResources } from '$lib/api/endpoints/search';
import { session } from '$lib/stores/session.svelte';
import CommandPalette from './CommandPalette.svelte';

const m = (fn: unknown) => fn as ReturnType<typeof vi.fn>;

function openPalette() {
	return fireEvent.keyDown(window, { key: 'k', ctrlKey: true });
}

beforeEach(() => {
	vi.clearAllMocks();
	session.user = { id: '1', username: 'admin', role: 'admin', is_external: false } as never;
});

it('opens on Ctrl+K and shows the command panel', async () => {
	render(CommandPalette);
	await openPalette();
	await screen.findByTestId('command-palette-panel');
	expect(screen.getByTestId('command-palette-input')).toBeTruthy();
});

it('runs a navigation command', async () => {
	render(CommandPalette);
	await openPalette();
	await fireEvent.click(await screen.findByTestId('command-palette-files-item'));
	await waitFor(() => expect(goto).toHaveBeenCalledWith('/files'));
});

// #924: the debounced search had no latest-wins guard — with `rac`
// answering after `race-xyz`, the input read `race-xyz` while the list
// showed `rac`'s hits. Resolve the two searches out of order and require
// only the newest query's folder to be listed.
it('a slow earlier search cannot replace the results of a newer one', async () => {
	const pending: Array<(v: unknown) => void> = [];
	m(searchResources).mockImplementation(() => new Promise((r) => pending.push(r)));
	const hit = (name: string) => ({
		items: [{ resource_type: 'folder', resource: { id: name, name } }],
		query_time_ms: 0
	});
	render(CommandPalette);
	await openPalette();
	const input = await screen.findByTestId('command-palette-input');
	await fireEvent.input(input, { target: { value: 'rac' } });
	await waitFor(() => expect(pending).toHaveLength(1));
	await fireEvent.input(input, { target: { value: 'race-xyz' } });
	await waitFor(() => expect(pending).toHaveLength(2));
	pending[1](hit('newest-hit'));
	await screen.findByText('newest-hit');
	pending[0](hit('stale-hit'));
	await new Promise((r) => setTimeout(r, 0));
	expect(screen.queryByText('stale-hit')).toBeNull();
	expect(screen.getByText('newest-hit')).toBeTruthy();
});

it('searches files as the query is typed', async () => {
	render(CommandPalette);
	await openPalette();
	const input = await screen.findByTestId('command-palette-input');
	await fireEvent.input(input, { target: { value: 'report' } });
	await waitFor(() => expect(searchResources).toHaveBeenCalled());
	expect(m(searchResources).mock.calls[0][0]).toBe('report');
});
