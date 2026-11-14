import { describe, expect, it } from 'vitest';
import type { CalendarEvent } from '$lib/api/endpoints/calendars';
import {
	expandEvents,
	groupOccurrencesByDay,
	occurrenceDayKeys,
	parseRRule
} from './calendarRecurrence';

let seq = 0;

function event(partial: Partial<CalendarEvent>): CalendarEvent {
	seq += 1;
	return {
		id: `event-${seq}`,
		calendar_id: 'cal-1',
		summary: 'Event',
		description: null,
		location: null,
		start_time: '2026-11-05T09:00:00Z',
		end_time: '2026-11-05T10:00:00Z',
		all_day: false,
		rrule: null,
		ical_uid: `uid-${seq}`,
		recurrence_id: null,
		timezone: null,
		created_at: '2026-11-01T00:00:00Z',
		updated_at: '2026-11-01T00:00:00Z',
		...partial
	};
}

const RANGE_START = new Date('2026-11-01T00:00:00Z');
const RANGE_END = new Date('2026-12-01T00:00:00Z');

describe('expandEvents', () => {
	it('returns a non-recurring event only when it overlaps the window', () => {
		const inside = event({});
		const outside = event({
			start_time: '2027-01-10T09:00:00Z',
			end_time: '2027-01-10T10:00:00Z'
		});
		const expanded = expandEvents([inside, outside], RANGE_START, RANGE_END);
		expect(expanded.map((o) => o.event.id)).toEqual([inside.id]);
	});

	it('expands a weekly series across the window', () => {
		const master = event({
			rrule: 'FREQ=WEEKLY',
			ical_uid: 'weekly'
		});
		const expanded = expandEvents([master], RANGE_START, RANGE_END);
		expect(expanded.map((o) => o.start.toISOString())).toEqual([
			'2026-11-05T09:00:00.000Z',
			'2026-11-12T09:00:00.000Z',
			'2026-11-19T09:00:00.000Z',
			'2026-11-26T09:00:00.000Z'
		]);
	});

	// A TZID series is anchored to local wall time: 09:00 New York
	// stays 09:00 after the March DST switch, so the UTC instant moves
	// from 14:00Z to 13:00Z. Expanding in UTC would keep 14:00Z and
	// render the meeting an hour late for the rest of the year.
	it('expands a TZID weekly series in wall-clock time across DST', () => {
		const master = event({
			start_time: '2026-03-02T14:00:00Z',
			end_time: '2026-03-02T15:00:00Z',
			rrule: 'FREQ=WEEKLY;COUNT=3',
			timezone: 'America/New_York',
			ical_uid: 'dst-weekly'
		});
		const expanded = expandEvents(
			[master],
			new Date('2026-03-01T00:00:00Z'),
			new Date('2026-03-20T00:00:00Z')
		);
		expect(expanded.map((o) => o.start.toISOString())).toEqual([
			'2026-03-02T14:00:00.000Z',
			'2026-03-09T13:00:00.000Z',
			'2026-03-16T13:00:00.000Z'
		]);
	});

	// 02:30 New York does not exist on 2026-03-08 (the clock jumps
	// 02:00 → 03:00). RFC 5545 §3.3.10 says a nonexistent generated
	// local time is omitted and does not count toward COUNT, so the
	// third occurrence must be March 22 — not March 15 after the gap
	// silently consumed an occurrence.
	it('omits a DST-gap occurrence without consuming COUNT', () => {
		const master = event({
			start_time: '2026-03-01T07:30:00Z',
			end_time: '2026-03-01T08:00:00Z',
			rrule: 'FREQ=WEEKLY;COUNT=3',
			timezone: 'America/New_York',
			ical_uid: 'dst-gap-count'
		});
		const expanded = expandEvents(
			[master],
			new Date('2026-03-01T00:00:00Z'),
			new Date('2026-04-01T00:00:00Z')
		);
		expect(expanded.map((o) => o.start.toISOString())).toEqual([
			'2026-03-01T07:30:00.000Z',
			'2026-03-15T06:30:00.000Z',
			'2026-03-22T06:30:00.000Z'
		]);
	});

	// Same gap, but through the weekly BYDAY generator rather than the
	// plain weekly step: the skipped candidate must not end the series
	// or consume COUNT there either.
	it('omits a DST-gap occurrence without consuming COUNT under BYDAY', () => {
		const master = event({
			start_time: '2026-03-01T07:30:00Z',
			end_time: '2026-03-01T08:00:00Z',
			rrule: 'FREQ=WEEKLY;BYDAY=SU;COUNT=3',
			timezone: 'America/New_York',
			ical_uid: 'dst-gap-byday'
		});
		const expanded = expandEvents(
			[master],
			new Date('2026-03-01T00:00:00Z'),
			new Date('2026-04-01T00:00:00Z')
		);
		expect(expanded.map((o) => o.start.toISOString())).toEqual([
			'2026-03-01T07:30:00.000Z',
			'2026-03-15T06:30:00.000Z',
			'2026-03-22T06:30:00.000Z'
		]);
	});

	it('applies UNTIL in UTC to a TZID series', () => {
		const master = event({
			start_time: '2026-03-02T14:00:00Z',
			end_time: '2026-03-02T15:00:00Z',
			rrule: 'FREQ=WEEKLY;UNTIL=20260316T130000Z',
			timezone: 'America/New_York',
			ical_uid: 'dst-until'
		});
		const expanded = expandEvents(
			[master],
			new Date('2026-03-01T00:00:00Z'),
			new Date('2026-04-01T00:00:00Z')
		);
		expect(expanded.map((o) => o.start.toISOString())).toEqual([
			'2026-03-02T14:00:00.000Z',
			'2026-03-09T13:00:00.000Z',
			'2026-03-16T13:00:00.000Z'
		]);
	});

	it('honours COUNT', () => {
		const master = event({ rrule: 'FREQ=DAILY;COUNT=3', ical_uid: 'counted' });
		const expanded = expandEvents([master], RANGE_START, RANGE_END);
		expect(expanded).toHaveLength(3);
	});

	it('honours UNTIL inclusively', () => {
		const master = event({
			rrule: 'FREQ=DAILY;UNTIL=20261107T090000Z',
			ical_uid: 'until'
		});
		const expanded = expandEvents([master], RANGE_START, RANGE_END);
		expect(expanded.map((o) => o.start.toISOString())).toEqual([
			'2026-11-05T09:00:00.000Z',
			'2026-11-06T09:00:00.000Z',
			'2026-11-07T09:00:00.000Z'
		]);
	});

	it('moves an overridden occurrence instead of duplicating it', () => {
		const master = event({ rrule: 'FREQ=WEEKLY', ical_uid: 'moved' });
		const override = event({
			ical_uid: 'moved',
			recurrence_id: '2026-11-12T09:00:00Z',
			start_time: '2026-11-12T14:00:00Z',
			end_time: '2026-11-12T15:00:00Z'
		});
		const expanded = expandEvents([master, override], RANGE_START, RANGE_END);
		expect(expanded).toHaveLength(4);
		const overridden = expanded.find((o) => o.start.getUTCHours() === 14);
		expect(overridden?.start.toISOString()).toBe('2026-11-12T14:00:00.000Z');
		expect(expanded.some((o) => o.start.toISOString() === '2026-11-12T09:00:00.000Z')).toBe(false);
	});

	it('supports weekly BYDAY', () => {
		const master = event({
			rrule: 'FREQ=WEEKLY;BYDAY=MO,TH',
			ical_uid: 'byday'
		});
		const expanded = expandEvents([master], RANGE_START, RANGE_END);
		expect(expanded.map((o) => o.start.toISOString())).toEqual([
			'2026-11-05T09:00:00.000Z',
			'2026-11-09T09:00:00.000Z',
			'2026-11-12T09:00:00.000Z',
			'2026-11-16T09:00:00.000Z',
			'2026-11-19T09:00:00.000Z',
			'2026-11-23T09:00:00.000Z',
			'2026-11-26T09:00:00.000Z',
			'2026-11-30T09:00:00.000Z'
		]);
	});

	// A monthly rule anchored on the 31st skips February rather than
	// clamping — and must still resume in March. (Our first pass broke
	// the whole series at the first short month.)
	it('skips short months and keeps going', () => {
		const master = event({
			start_time: '2026-01-31T09:00:00Z',
			end_time: '2026-01-31T10:00:00Z',
			rrule: 'FREQ=MONTHLY',
			ical_uid: 'monthly-31'
		});
		const expanded = expandEvents(
			[master],
			new Date('2026-01-01T00:00:00Z'),
			new Date('2026-06-01T00:00:00Z')
		);
		expect(expanded.map((o) => o.start.toISOString())).toEqual([
			'2026-01-31T09:00:00.000Z',
			'2026-03-31T09:00:00.000Z',
			'2026-05-31T09:00:00.000Z'
		]);
	});

	it('falls back to a single occurrence for an unknown rule', () => {
		const master = event({ rrule: 'FREQ=SECONDLY', ical_uid: 'weird' });
		expect(expandEvents([master], RANGE_START, RANGE_END)).toHaveLength(1);
	});
});

describe('occurrenceDayKeys', () => {
	it('uses UTC dates for all-day events and treats DTEND as exclusive', () => {
		const allDay = event({
			all_day: true,
			start_time: '2026-11-05T00:00:00Z',
			end_time: '2026-11-07T00:00:00Z'
		});
		const [occ] = expandEvents(
			[allDay],
			new Date('2026-11-01T00:00:00Z'),
			new Date('2026-12-01T00:00:00Z')
		);
		expect(occurrenceDayKeys(occ)).toEqual(['2026-11-05', '2026-11-06']);
	});

	it('spreads a timed overnight event across the local days it touches', () => {
		const start = new Date(2026, 10, 5, 22, 0);
		const end = new Date(2026, 10, 6, 1, 0);
		const timed = event({ start_time: start.toISOString(), end_time: end.toISOString() });
		const [occ] = expandEvents([timed], RANGE_START, RANGE_END);
		expect(occurrenceDayKeys(occ)).toEqual(['2026-11-05', '2026-11-06']);
	});
});

describe('groupOccurrencesByDay', () => {
	it('buckets an event on every day it covers', () => {
		const allDay = event({
			all_day: true,
			start_time: '2026-11-05T00:00:00Z',
			end_time: '2026-11-07T00:00:00Z'
		});
		const [occ] = expandEvents([allDay], RANGE_START, RANGE_END);
		const byDay = groupOccurrencesByDay([occ]);
		expect([...byDay.keys()].sort()).toEqual(['2026-11-05', '2026-11-06']);
	});
});

describe('parseRRule', () => {
	it('parses interval, count, until and byday', () => {
		expect(parseRRule('FREQ=WEEKLY;INTERVAL=2;COUNT=4;BYDAY=MO,FR')).toEqual({
			freq: 'WEEKLY',
			interval: 2,
			count: 4,
			until: undefined,
			byday: [1, 5]
		});
	});

	it('rejects a rule without a known FREQ', () => {
		expect(parseRRule('COUNT=3')).toBeNull();
		expect(parseRRule(null)).toBeNull();
	});
});
