import { describe, expect, it, vi } from 'vitest';

// The bell marks rows read through the API; this file is about the local
// bookkeeping, so the network call is stubbed rather than reached.
vi.mock('$lib/api/endpoints/notifications', async (original) => ({
	...(await original<Record<string, unknown>>()),
	markAllNotificationsRead: vi.fn(async () => undefined)
}));

import { markEverythingRead, mergeById, notifications } from './useNotifications.svelte';
import { markAllNotificationsRead } from '$lib/api/endpoints/notifications';
import { ui } from '$lib/stores/ui.svelte';
import type { Notification } from '$lib/api/types';

function row(id: string, created_at: string, read_at: string | null = null): Notification {
	return {
		id,
		kind: 'share_granted',
		payload: {},
		created_at,
		read_at
	};
}

describe('mergeById — WS-push vs delta-fetch race dedup', () => {
	it('preserves existing when incoming is empty', () => {
		const existing = [row('a', '2026-09-11T10:00:00Z'), row('b', '2026-09-11T09:00:00Z')];
		expect(mergeById(existing, [])).toEqual(existing);
	});

	it('appends non-overlapping incoming and sorts newest-first', () => {
		const existing = [row('b', '2026-09-11T09:00:00Z')];
		const incoming = [row('a', '2026-09-11T10:00:00Z')];
		const merged = mergeById(existing, incoming);
		expect(merged.map((n) => n.id)).toEqual(['a', 'b']);
	});

	it('dedupes on id — same row from WS push and delta fetch appears once', () => {
		// Simulates the race: `x` was delivered live via rt.event
		// and appended locally, then the reconnect delta fetch
		// returns the same `x` again. Must not double it.
		const existing = [row('x', '2026-09-11T10:00:00Z')];
		const incoming = [row('x', '2026-09-11T10:00:00Z')];
		expect(mergeById(existing, incoming)).toHaveLength(1);
	});

	it('lets server value win — read_at flip visible in incoming', () => {
		// User marked `x` as read on another device. Local copy is
		// stale (still unread). The delta fetch returns the fresh
		// row with read_at populated — that must win.
		const existing = [row('x', '2026-09-11T10:00:00Z', null)];
		const incoming = [row('x', '2026-09-11T10:00:00Z', '2026-09-11T10:05:00Z')];
		const merged = mergeById(existing, incoming);
		expect(merged).toHaveLength(1);
		expect(merged[0].read_at).toBe('2026-09-11T10:05:00Z');
	});

	it('merges mixed overlap correctly', () => {
		const existing = [row('b', '2026-09-11T09:00:00Z'), row('a', '2026-09-11T08:00:00Z')];
		const incoming = [
			row('c', '2026-09-11T10:00:00Z'), // new
			row('b', '2026-09-11T09:00:00Z', '2026-09-11T09:30:00Z') // updated
		];
		const merged = mergeById(existing, incoming);
		expect(merged.map((n) => n.id)).toEqual(['c', 'b', 'a']);
		expect(merged[1].read_at).toBe('2026-09-11T09:30:00Z');
	});
});

// Two surfaces offer "mark all read" — the bell dropdown and
// `/notifications` — and the bell's badge counts BOTH stores. A handler that
// cleared only one of them would leave the badge lit over a panel with
// nothing unread in it, and no way to clear it from the UI. Hence one helper
// rather than the two-liner copied into each surface.
describe('markEverythingRead', () => {
	it('clears both the toast history and the persistent rows', async () => {
		ui.notifications = [
			{ id: 1, message: 'Upload finished', kind: 'success', at: Date.now(), read: false }
		];
		expect(ui.unread).toBe(1);

		markEverythingRead();

		expect(ui.unread).toBe(0);
		expect(notifications.unread).toBe(0);
		// The server has to be told, or the next fetch brings the rows back
		// unread on every other device.
		await vi.waitFor(() => expect(markAllNotificationsRead).toHaveBeenCalled());
	});
});
