import { it, expect, vi, beforeEach } from 'vitest';
import { render, screen, fireEvent } from '@testing-library/svelte';

const { store } = vi.hoisted(() => ({
	store: {
		items: [] as unknown[],
		unread: 0,
		hasMore: false,
		loadingOlder: false,
		markAllRead: vi.fn(),
		loadOlder: vi.fn()
	}
}));
vi.mock('$lib/composables/useNotifications.svelte', () => ({ notifications: store }));
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

it('marks transient and persistent rows read with one action', async () => {
	ui.notifications = [
		{ id: 2, message: 'Quota warning', kind: 'warning', at: Date.now(), read: false }
	];
	store.unread = 3;

	render(NotificationsPage);
	await fireEvent.click(screen.getByTestId('notifications-mark-all-read-btn'));

	expect(store.markAllRead).toHaveBeenCalledOnce();
	expect(ui.unread).toBe(0);
});
