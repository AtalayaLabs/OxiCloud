import { it, expect, vi, beforeEach } from 'vitest';
import { render, screen, fireEvent, waitFor } from '@testing-library/svelte';
import { createRawSnippet } from 'svelte';

const { goto, pageState } = vi.hoisted(() => ({
	goto: vi.fn(),
	pageState: { url: new URL('http://localhost/files'), route: { id: '/files/[...path]' } }
}));
vi.mock('$app/navigation', () => ({ goto }));
vi.mock('$app/state', () => ({ page: pageState }));
vi.mock('$lib/api/endpoints/auth', () => ({ logout: vi.fn() }));
vi.mock('$lib/api/endpoints/search', () => ({
	searchResources: vi.fn(async () => ({ items: [], query_time_ms: 0 }))
}));
vi.mock('$lib/api/endpoints/files', () => ({ fileInlineUrl: () => '/in' }));

import { logout } from '$lib/api/endpoints/auth';
import { session } from '$lib/stores/session.svelte';
import AppShell from './AppShell.svelte';

const m = (fn: unknown) => fn as ReturnType<typeof vi.fn>;
const children = createRawSnippet(() => ({
	render: () => '<div data-testid="shell-child">hi</div>'
}));

beforeEach(() => {
	vi.clearAllMocks();
	pageState.url = new URL('http://localhost/files');
	session.user = {
		id: '1',
		username: 'admin',
		email: 'a@x.test',
		given_name: 'A',
		family_name: 'B',
		role: 'admin',
		storage_used_bytes: 10,
		storage_quota_bytes: 100,
		is_external: false
	} as never;
});

it('renders the shell chrome and its children', async () => {
	render(AppShell, { props: { children } });
	expect(screen.getByTestId('shell-child')).toBeTruthy();
	expect(screen.getByTestId('appshell-user-menu-btn')).toBeTruthy();
});

it('opens the user menu, exposing profile and admin links', async () => {
	render(AppShell, { props: { children } });
	await fireEvent.click(screen.getByTestId('appshell-user-menu-btn'));
	const profile = await screen.findByTestId('appshell-user-menu-profile-item');
	expect(profile.getAttribute('href')).toBe('/profile');
	// Admin link only shows for admin users (session.user.role === 'admin').
	expect(screen.getByTestId('appshell-user-menu-admin-item')).toBeTruthy();
});

it('logs out: clears the session and redirects to /login', async () => {
	m(logout).mockResolvedValue(undefined);
	render(AppShell, { props: { children } });
	await fireEvent.click(screen.getByTestId('appshell-user-menu-btn'));
	await fireEvent.click(await screen.findByTestId('appshell-user-menu-logout-btn'));
	await waitFor(() => expect(logout).toHaveBeenCalled());
	await waitFor(() => expect(goto).toHaveBeenCalledWith('/login'));
	expect(session.user).toBeNull();
});

it('submits a search and routes to /search', async () => {
	render(AppShell, { props: { children } });
	const input = screen.getByTestId('appshell-search-input');
	await fireEvent.input(input, { target: { value: 'report' } });
	await fireEvent.click(screen.getByTestId('appshell-search-submit-btn'));
	await waitFor(() => expect(goto).toHaveBeenCalledWith('/search?q=report'));
});
