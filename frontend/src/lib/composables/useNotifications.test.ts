import { describe, expect, it } from 'vitest';
import { mergeById } from './useNotifications.svelte';
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
