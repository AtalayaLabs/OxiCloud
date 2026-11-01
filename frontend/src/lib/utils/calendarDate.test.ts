import { describe, expect, it } from 'vitest';
import {
	addDays,
	dateKey,
	isSameMonth,
	monthGridDays,
	shiftMonth,
	startOfWeek,
	utcDateKey,
	weekDays
} from './calendarDate';

describe('calendarDate', () => {
	it('starts weeks on Monday by default', () => {
		// 2026-11-05 is a Thursday; the week starts on the 2nd.
		expect(dateKey(startOfWeek(new Date(2026, 10, 5)))).toBe('2026-11-02');
		expect(dateKey(startOfWeek(new Date(2026, 10, 1), 0))).toBe('2026-11-01');
	});

	it('always renders six weeks for a month grid', () => {
		const days = monthGridDays(new Date(2026, 10, 15));
		expect(days).toHaveLength(42);
		expect(dateKey(days[0])).toBe('2026-10-26');
		expect(dateKey(days[41])).toBe('2026-12-06');
	});

	it('shifts month anchors without overflowing the day', () => {
		const january = shiftMonth(new Date(2026, 0, 31), 1);
		expect(dateKey(january)).toBe('2026-02-01');
		expect(isSameMonth(january, new Date(2026, 1, 20))).toBe(true);
	});

	it('builds a seven-day week and adds days across a month boundary', () => {
		const days = weekDays(new Date(2026, 10, 30));
		expect(days).toHaveLength(7);
		expect(dateKey(days[0])).toBe('2026-11-30');
		expect(dateKey(addDays(days[0], 1))).toBe('2026-12-01');
	});

	it('keeps UTC and local keys distinct', () => {
		const d = new Date('2026-11-05T23:30:00Z');
		expect(utcDateKey(d)).toBe('2026-11-05');
		// Local key depends on the host timezone, but it must stay a
		// padded key — never throw or emit an unpadded month.
		expect(dateKey(d)).toMatch(/^\d{4}-\d{2}-\d{2}$/);
	});
});
