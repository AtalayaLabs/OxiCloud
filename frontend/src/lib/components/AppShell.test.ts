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
import { serverConfig } from '$lib/stores/serverConfig.svelte';
import AppShell from './AppShell.svelte';

const m = (fn: unknown) => fn as ReturnType<typeof vi.fn>;
const children = createRawSnippet(() => ({
	render: () => '<div data-testid="shell-child">hi</div>'
}));

// Full-fresh default so each test runs against the same baseline.
// AppShell's nav LINKS + search bar + delete-confirm copy all gate on
// `serverConfig.features.*`; without this reset a prior test's patch
// would leak across cases.
function resetFeatures() {
	serverConfig.features = {
		message_bus: true,
		trash: true,
		search: true,
		sharing: true,
		music: true,
		photos: true,
		places: true,
		faces: false,
		video_thumbnails: true,
		external_mounts: false,
		webdav: true,
		caldav: true,
		carddav: true,
		nextcloud: false,
		markdown_collab: false,
		shared_worker_bus: true
	};
}

beforeEach(() => {
	vi.clearAllMocks();
	pageState.url = new URL('http://localhost/files');
	resetFeatures();
	// Post the three-layer UserDto refactor, `session.user` is a
	// derived accessor over `session.me.full.user`; only `session.me`
	// is settable. Fixture composes the nested shape — public identity
	// (username/email/name) on `.full.user`, admin+self extras
	// (storage_*, has_password) on `.full`, self-only bag (ui_prefs,
	// dpop_bound, force_password_change, can_edit_image) at the top.
	// See docs/plan/userdto-refactor.md.
	session.me = {
		full: {
			user: {
				id: '1',
				username: 'admin',
				email: 'a@x.test',
				given_name: 'A',
				family_name: 'B',
				role: 'admin',
				is_external: false
			},
			storage_used_bytes: 10,
			storage_quota_bytes: 100
		}
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

it('logs out: clears the session and redirects to /login?source=logged_out', async () => {
	// The `?source=logged_out` query param is consumed by the login page
	// onMount branch to (a) show the "Successfully signed out" banner and
	// (b) skip the doomed post-logout /me + /refresh probes. A plain
	// `/login` navigation regresses both — the banner disappears and the
	// layout re-fires the probes on remount. See AppShell.onLogout for
	// the full comment on why the query is necessary here.
	m(logout).mockResolvedValue(undefined);
	render(AppShell, { props: { children } });
	await fireEvent.click(screen.getByTestId('appshell-user-menu-btn'));
	await fireEvent.click(await screen.findByTestId('appshell-user-menu-logout-btn'));
	await waitFor(() => expect(logout).toHaveBeenCalled());
	await waitFor(() => expect(goto).toHaveBeenCalledWith('/login?source=logged_out'));
	expect(session.user).toBeNull();
});

it('submits a search and routes to /search', async () => {
	render(AppShell, { props: { children } });
	const input = screen.getByTestId('appshell-search-input');
	await fireEvent.input(input, { target: { value: 'report' } });
	await fireEvent.click(screen.getByTestId('appshell-search-submit-btn'));
	await waitFor(() => expect(goto).toHaveBeenCalledWith('/search?q=report'));
});

// ─── Feature-flag gating ─────────────────────────────────────────────────
// Each of the following pins ONE flag flipping off and asserts the matching
// surface disappears. The nav LINKS array in AppShell is a `$derived.by`
// that filters by `serverConfig.features.*`; dropping any single gate would
// leak the stale entry past the sidebar, which these pins catch.

it('shows all gated nav links when every feature flag is on (baseline)', () => {
	render(AppShell, { props: { children } });
	expect(screen.queryByTestId('appshell-nav-shared-link')).toBeTruthy();
	expect(screen.queryByTestId('appshell-nav-shared-with-me-link')).toBeTruthy();
	expect(screen.queryByTestId('appshell-nav-photos-link')).toBeTruthy();
	expect(screen.queryByTestId('appshell-nav-music-link')).toBeTruthy();
	expect(screen.queryByTestId('appshell-nav-trash-link')).toBeTruthy();
});

it('hides /trash from the sidebar when features.trash is false', () => {
	serverConfig.features = { ...serverConfig.features, trash: false };
	render(AppShell, { props: { children } });
	expect(screen.queryByTestId('appshell-nav-trash-link')).toBeNull();
	// Non-gated siblings stay — the regression we guard against is a
	// filter that over-matches and drops /files or /recent too.
	expect(screen.queryByTestId('appshell-nav-files-link')).toBeTruthy();
	expect(screen.queryByTestId('appshell-nav-recent-link')).toBeTruthy();
});

it('hides /shared and /shared-with-me when features.sharing is false', () => {
	serverConfig.features = { ...serverConfig.features, sharing: false };
	render(AppShell, { props: { children } });
	expect(screen.queryByTestId('appshell-nav-shared-link')).toBeNull();
	expect(screen.queryByTestId('appshell-nav-shared-with-me-link')).toBeNull();
	expect(screen.queryByTestId('appshell-nav-files-link')).toBeTruthy();
});

it('hides /photos when features.photos is false', () => {
	serverConfig.features = { ...serverConfig.features, photos: false };
	render(AppShell, { props: { children } });
	expect(screen.queryByTestId('appshell-nav-photos-link')).toBeNull();
});

it('hides /music when features.music is false', () => {
	serverConfig.features = { ...serverConfig.features, music: false };
	render(AppShell, { props: { children } });
	expect(screen.queryByTestId('appshell-nav-music-link')).toBeNull();
});

it('hides the search bar when features.search is false but keeps the slot', () => {
	// The `.search-slot` div stays always-rendered as a flex-grow spacer so
	// the right-side user controls stay anchored — only the input, toggles
	// and form go away. Verify the input disappears while the user menu
	// remains reachable (the regression we guard against is a `{#if}`
	// around the slot itself, which collapsed the layout leftward and
	// re-ordered the top-bar for every features.search=false deployment).
	serverConfig.features = { ...serverConfig.features, search: false };
	render(AppShell, { props: { children } });
	expect(screen.queryByTestId('appshell-search-input')).toBeNull();
	expect(screen.queryByTestId('appshell-search-toggle-btn')).toBeNull();
	expect(screen.queryByTestId('appshell-user-menu-btn')).toBeTruthy();
});
