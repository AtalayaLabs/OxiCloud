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
vi.mock('$lib/api/endpoints/files', () => ({
	fileInlineUrl: () => '/in',
	deleteFile: vi.fn(),
	moveFile: vi.fn()
}));
vi.mock('$lib/api/endpoints/folders', () => ({ deleteFolder: vi.fn(), moveFolder: vi.fn() }));
vi.mock('$lib/api/endpoints/batch', () => ({ copyFiles: vi.fn(), copyFolders: vi.fn() }));
vi.mock('$lib/api/endpoints/favorites', () => ({ addFavorite: vi.fn() }));

// `AppShell` calls `useNotifications()` on mount, which fetches the bell's
// first page. Unmocked, that fetch fails in jsdom and the composable logs
// `notifications refresh failed` — asynchronously, once per test in this
// file. Each of those crosses Vitest's worker RPC as an `onUserConsoleLog`,
// and a run where one is still in flight when the worker starts closing dies
// with `EnvironmentTeardownError: Closing rpc while "onUserConsoleLog" was
// pending`: no test fails, the FILE does, unpredictably. Silencing the work
// is the fix; `retry` only re-rolls the dice, and a `setTimeout` flush at the
// end of a test (tried first) just loses the race less often.
//
// The whole store surface is mocked, not just what `AppShell` reads:
// `NotificationRow` and `MobileNav` import the same store and call
// `markRead` / `delete`. A partial mock here is how an unrelated test in
// this file broke once already — `session.reset()` calls `drives.reset()`,
// which the drives mock below did not have.
const { notificationsStore, markEverythingRead } = vi.hoisted(() => ({
	markEverythingRead: vi.fn(),
	notificationsStore: {
		items: [] as unknown[],
		unread: 0,
		loading: false,
		hasMore: false,
		error: null,
		refresh: vi.fn(async () => {}),
		loadOlder: vi.fn(async () => {}),
		markRead: vi.fn(async () => {}),
		markAllRead: vi.fn(async () => {}),
		delete: vi.fn(async () => {})
	}
}));
vi.mock('$lib/composables/useNotifications.svelte', () => ({
	notifications: notificationsStore,
	markEverythingRead,
	useNotifications: vi.fn(),
	summaryFor: (row: { kind?: string }) => row.kind ?? '',
	mergeById: (existing: unknown[]) => existing
}));

// Two drives so `DrivePicker` renders at all — it hides itself with a
// single drive, since there would be nowhere else to drop.
const { driveStore } = vi.hoisted(() => ({
	driveStore: {
		loaded: true,
		drives: [
			{
				id: 'drive-a',
				name: 'Personal',
				kind: 'personal',
				default_for_user: true,
				root_folder_id: 'root-a',
				used_bytes: 0,
				quota_bytes: null
			},
			{
				id: 'drive-b',
				name: 'Team',
				kind: 'shared',
				default_for_user: false,
				root_folder_id: 'root-b',
				used_bytes: 0,
				quota_bytes: null
			}
		],
		load: vi.fn(async () => []),
		findDefault: () => null,
		findByRootFolderId: () => null,
		// `session.reset()` calls this on logout. Omitting it threw inside
		// the logout path, so the pre-existing logout test failed on a
		// missing redirect rather than on anything to do with drives —
		// mock the whole surface, not just what this file reads.
		reset: vi.fn()
	}
}));
vi.mock('$lib/stores/drives.svelte', () => ({
	drives: driveStore,
	driveIcon: () => 'hdd'
}));

import { logout } from '$lib/api/endpoints/auth';
import { moveFile } from '$lib/api/endpoints/files';
import { moveFolder } from '$lib/api/endpoints/folders';
import { copyFiles, copyFolders } from '$lib/api/endpoints/batch';
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
		contacts: true,
		calendars: true,
		nextcloud: false,
		markdown_collab: false,
		shared_worker_bus: true,
		recent_changes: true
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

it('labels the language toggle with the flag and native name, not the bare code', async () => {
	// The dropdown already listed `🇬🇧 English`, so a toggle showing `EN`
	// read as a different value from the option it selects.
	render(AppShell, { props: { children } });
	await fireEvent.click(screen.getByTestId('appshell-user-menu-btn'));
	const toggle = await screen.findByTestId('appshell-lang-toggle-btn');
	expect(toggle.textContent).toContain('🇬🇧 English');
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
	expect(screen.queryByTestId('appshell-nav-calendars-link')).toBeTruthy();
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

it('drops a nav group whose every link was flagged away, and leaves no heading', () => {
	// `NAV_GROUPS` ends in a `filter(g => g.links.length > 0)`. Without it
	// an emptied group does not merely render a stray heading — the keyed
	// each reads `group.links[0].href`, so `links[0]` being undefined is a
	// TypeError that takes the WHOLE sidebar down, on every page.
	//
	// Sharing is the group that can actually empty today: both its links
	// are behind `features.sharing`, and `/contacts` behind
	// `features.contacts`. Turn both off and it has no links at all.
	serverConfig.features = { ...serverConfig.features, sharing: false, contacts: false };
	render(AppShell, { props: { children } });

	// The sidebar still rendered — this is the assertion that would have
	// caught the crash, since a throw during render leaves nothing.
	expect(screen.queryByTestId('appshell-nav-files-link')).toBeTruthy();
	expect(screen.queryByTestId('appshell-nav-recent-link')).toBeTruthy();

	// And no orphaned "Sharing" heading above an empty group.
	expect(screen.queryByText('Sharing')).toBeNull();
	// Neighbouring group headings are untouched — the regression to guard
	// against is a filter that over-matches and drops populated groups.
	expect(screen.queryByText('Library')).toBeTruthy();
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

// Both halves, because a gate that renders nothing satisfies the first one
// on its own: hidden when off, PRESENT when on. The flag gates the server
// route too, so a link left behind when ops disables the feature lands on a
// 404 (`docs/plan/recently-modified.md` §5).
it('hides /changes when features.recent_changes is false', () => {
	serverConfig.features = { ...serverConfig.features, recent_changes: false };
	render(AppShell, { props: { children } });
	expect(screen.queryByTestId('appshell-nav-changes-link')).toBeNull();
});

it('shows /changes when features.recent_changes is true', () => {
	render(AppShell, { props: { children } });
	expect(screen.queryByTestId('appshell-nav-changes-link')).toBeTruthy();
});

it('hides /contacts when features.contacts is false', () => {
	serverConfig.features = { ...serverConfig.features, contacts: false };
	render(AppShell, { props: { children } });
	expect(screen.queryByTestId('appshell-nav-contacts-link')).toBeNull();
});

it('hides /calendars when features.calendars is false', () => {
	serverConfig.features = { ...serverConfig.features, calendars: false };
	render(AppShell, { props: { children } });
	expect(screen.queryByTestId('appshell-nav-calendars-link')).toBeNull();
});

it('keeps /calendars gated on features.calendars, NOT features.caldav', () => {
	// The two are deliberately separate: `caldav` is the DAV sync
	// endpoint a desktop client talks to, `calendars` is
	// `/api/calendars` plus this page. A deployment can serve calendars
	// only over DAV, or only in the SPA. Collapsing them onto one flag
	// would silently remove one of those choices, so pin both directions.
	serverConfig.features = { ...serverConfig.features, caldav: false, calendars: true };
	render(AppShell, { props: { children } });
	expect(screen.queryByTestId('appshell-nav-calendars-link')).toBeTruthy();
});

it('keeps /contacts gated on features.contacts, NOT features.carddav', () => {
	// The two are deliberately separate: `carddav` is the DAV sync
	// endpoint a desktop client talks to, `contacts` is `/api/contacts`
	// plus this page. A deployment can serve contacts only over DAV, or
	// only in the SPA. Collapsing them onto one flag — the obvious
	// simplification — would silently remove one of those choices, so
	// pin both directions.
	serverConfig.features = { ...serverConfig.features, carddav: false, contacts: true };
	render(AppShell, { props: { children } });
	expect(screen.queryByTestId('appshell-nav-contacts-link')).toBeTruthy();
});

// ── Drop onto a drive in the sidebar picker ──────────────────────────
// The payload format, the copy-vs-move modifier rule and the batch
// calls all live in AppShell, so this is where they get pinned.
//
// `fireEvent.drop(el, { dataTransfer })` is not enough: the handlers
// read `types` / `getData` AND the live modifier bits, and jsdom does
// not give DragEvent a usable DataTransfer. So build the event by hand
// and define both onto it.
const DRAG_TYPE = 'application/x-oxi-item';

function dropOn(
	el: Element,
	items: { id: string; name: string; kind: 'file' | 'folder' }[],
	opts: { copy?: boolean } = {}
) {
	const ev = new Event('drop', { bubbles: true, cancelable: true });
	Object.defineProperty(ev, 'dataTransfer', {
		value: { types: [DRAG_TYPE], getData: () => JSON.stringify(items) }
	});
	// jsdom reports a blank `navigator.platform`, so `IS_MAC` is false and
	// Ctrl is the copy modifier — see `$lib/utils/dragModifiers`. Pinning
	// the non-Mac branch keeps this deterministic across machines.
	Object.defineProperty(ev, 'ctrlKey', { value: opts.copy === true });
	el.dispatchEvent(ev);
	return ev;
}

it('moves dropped items into a drive root, and copies them with the modifier held', async () => {
	render(AppShell, { props: { children } });
	const row = await waitFor(() => screen.getByTestId('drive-picker-row-drive-b'));

	// No modifier → move, one call per item, targeting the drive's ROOT
	// folder (not the drive id — a drive is not a folder).
	dropOn(row, [
		{ id: 'f1', name: 'a.txt', kind: 'file' },
		{ id: 'd1', name: 'sub', kind: 'folder' }
	]);
	await waitFor(() => expect(m(moveFile)).toHaveBeenCalledWith('f1', 'root-b'));
	expect(m(moveFolder)).toHaveBeenCalledWith('d1', 'root-b');
	expect(m(copyFiles)).not.toHaveBeenCalled();

	// Modifier held → copy, via the batch endpoints, one call per kind.
	dropOn(row, [{ id: 'f2', name: 'b.txt', kind: 'file' }], { copy: true });
	await waitFor(() => expect(m(copyFiles)).toHaveBeenCalledWith(['f2'], 'root-b'));
	expect(m(copyFolders)).toHaveBeenCalledWith([], 'root-b');
	// The copy must NOT also move — the two branches are exclusive.
	expect(m(moveFile)).not.toHaveBeenCalledWith('f2', 'root-b');
});

it('refuses a drop onto the drive whose root is already on screen', async () => {
	// Reported in review: dropping into the drive you are already looking
	// at reported "moved" while the server no-opped, because the target
	// folder WAS the items' current parent. `/files/root-b` means drive-b
	// is that folder, so its row must accept nothing.
	pageState.url = new URL('http://localhost/files/root-b');
	render(AppShell, { props: { children } });
	const row = await waitFor(() => screen.getByTestId('drive-picker-row-drive-b'));

	const ev = dropOn(row, [{ id: 'f1', name: 'a.txt', kind: 'file' }]);
	// Not consumed, so the browser would treat it as a non-target, and
	// nothing was called.
	expect(ev.defaultPrevented).toBe(false);
	expect(m(moveFile)).not.toHaveBeenCalled();
	expect(m(copyFiles)).not.toHaveBeenCalled();

	// …while a SUBFOLDER of that same drive is a real move to its root.
	pageState.url = new URL('http://localhost/files/root-b/deep-folder');
	render(AppShell, { props: { children } });
	const rows = await waitFor(() => screen.getAllByTestId('drive-picker-row-drive-b'));
	dropOn(rows[rows.length - 1], [{ id: 'f1', name: 'a.txt', kind: 'file' }]);
	await waitFor(() => expect(m(moveFile)).toHaveBeenCalledWith('f1', 'root-b'));
});

// The bell dropdown gained the same "mark all read" action `/notifications`
// has, through the same shared helper — the badge counts the toast history
// AND the persistent rows, so a surface that cleared one of them would leave
// it lit over a panel with nothing unread. It appears only when something is
// unread: a permanently-present control for a no-op is one people stop
// reading.
it('marks everything read from the bell, and offers it only when unread', async () => {
	notificationsStore.unread = 2;
	render(AppShell, { props: { children } });
	await fireEvent.click(screen.getByTestId('appshell-notif-bell-btn'));
	await fireEvent.click(screen.getByTestId('appshell-notif-mark-all-read-btn'));
	expect(markEverythingRead).toHaveBeenCalledOnce();
});

it('offers no mark-all-read when nothing is unread', async () => {
	notificationsStore.unread = 0;
	render(AppShell, { props: { children } });
	await fireEvent.click(screen.getByTestId('appshell-notif-bell-btn'));
	expect(screen.queryByTestId('appshell-notif-mark-all-read-btn')).toBeNull();
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
