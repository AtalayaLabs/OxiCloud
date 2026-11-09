import { it, expect, vi, beforeEach } from 'vitest';
import { render, screen, fireEvent } from '@testing-library/svelte';

const { store, markEverythingRead } = vi.hoisted(() => ({
	markEverythingRead: vi.fn(),
	store: {
		items: [] as unknown[],
		unread: 0,
		hasMore: false,
		loadingOlder: false,
		markAllRead: vi.fn(),
		markRead: vi.fn(),
		loadOlder: vi.fn(),
		delete: vi.fn()
	}
}));
vi.mock('$lib/composables/useNotifications.svelte', () => ({
	notifications: store,
	markEverythingRead
}));
vi.mock('$app/navigation', () => ({ goto: vi.fn() }));

import { ui } from '$lib/stores/ui.svelte';
import NotificationsPage from './+page.svelte';

beforeEach(() => {
	vi.clearAllMocks();
	ui.notifications = [];
	store.items = [];
	store.unread = 0;
	store.hasMore = false;
	store.loadingOlder = false;
});

it('merges transient and persistent rows newest first', () => {
	ui.notifications = [
		{
			id: 1,
			message: 'Upload finished',
			kind: 'success',
			at: Date.parse('2026-01-01T11:00:00Z'),
			read: true
		}
	];
	store.items = [
		{
			id: 'p1',
			kind: 'new_login_from_new_device',
			payload: {},
			created_at: '2026-01-01T10:00:00Z',
			read_at: null
		}
	];

	const { container } = render(NotificationsPage);
	const texts = [...container.querySelectorAll('.notif-item-text')].map((el) => el.textContent);

	expect(texts[0]).toContain('Upload finished');
	expect(texts[1]).toContain('new_login_from_new_device');
});

it('shows the empty state when both feeds are empty', () => {
	render(NotificationsPage);
	expect(screen.getByText('No notifications')).toBeTruthy();
});

// The feed's wiring into `GestureList`. Dismissal takes the ROW, not an
// index, because the two kinds leave by different doors: a toast only ever
// existed in the UI store, a persistent row has to be deleted server-side or
// it returns with the next fetch. One `run` for both, dispatching on kind.
it('dismisses each row by the route its kind needs', async () => {
	ui.notifications = [{ id: 7, message: 'Upload finished', kind: 'success', at: 2000, read: true }];
	store.items = [
		{
			id: 'p1',
			kind: 'new_login_from_new_device',
			payload: {},
			created_at: '2026-01-01T10:00:00Z',
			read_at: null
		}
	];

	render(NotificationsPage);

	await fireEvent.click(screen.getByTestId('notifications-dismiss-transient-7'));
	expect(ui.notifications.map((n) => n.id)).toEqual([]);
	expect(store.delete).not.toHaveBeenCalled();

	await fireEvent.click(screen.getByTestId('notifications-dismiss-persistent-p1'));
	expect(store.delete).toHaveBeenCalledWith('p1');
});

// The right-hand action, against the left's dismissal: blue and additive
// where that one is red and destructive. Per kind again — a toast's read flag
// lives in the UI store, a persistent row's in the API — and idempotent, so a
// row already read stays read rather than flipping back.
it('marks one row read by the route its kind needs', async () => {
	ui.notifications = [
		{ id: 9, message: 'Upload finished', kind: 'success', at: 2000, read: false }
	];
	store.items = [
		{
			id: 'p1',
			kind: 'new_login_from_new_device',
			payload: {},
			created_at: '2026-01-01T10:00:00Z',
			read_at: null
		}
	];

	render(NotificationsPage);

	await fireEvent.click(screen.getByTestId('notifications-read-persistent-p1'));
	expect(store.markRead).toHaveBeenCalledWith('p1');

	await fireEvent.click(screen.getByTestId('notifications-read-transient-9'));
	expect(ui.notifications.find((n) => n.id === 9)?.read).toBe(true);
	// The history row stays in the list — marking read is not dismissing.
	expect(ui.notifications.map((n) => n.id)).toEqual([9]);
});

// Not reorderable, and that is the point of the flag: the feed is sorted by
// time, so a grip would offer a move the next fetch undoes.
it('offers no reordering grip', () => {
	store.items = [
		{
			id: 'p1',
			kind: 'new_login_from_new_device',
			payload: {},
			created_at: '2026-01-01T10:00:00Z',
			read_at: null
		}
	];
	const { container } = render(NotificationsPage);
	expect(container.querySelector('.gesture-grip')).toBeNull();
	// …and the row's own cross is gone with it: the list's action is the one
	// route to deletion here, rather than two buttons onto the same call.
	expect(container.querySelector('.notif-item-close')).toBeNull();
});

// The button goes through the SHARED helper, which the bell dropdown also
// uses — what it does to the two stores is pinned in
// `useNotifications.test.ts`. Pinning it again here would mean two copies of
// the same expectation, and it was two copies of the IMPLEMENTATION that this
// replaced.
it('marks everything read through the shared helper', async () => {
	ui.notifications = [
		{ id: 2, message: 'Quota warning', kind: 'warning', at: Date.now(), read: false }
	];
	store.unread = 3;

	render(NotificationsPage);
	await fireEvent.click(screen.getByTestId('notifications-mark-all-read-btn'));

	expect(markEverythingRead).toHaveBeenCalledOnce();
});
