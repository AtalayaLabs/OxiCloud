/** Client-side RRULE expansion for the calendar view.
 *
 * The server stores a recurring series as a master row plus zero or
 * more RECURRENCE-ID exception rows. `GET /events` returns the rows
 * unexpanded, so the visible window is expanded here.
 *
 * Supported rule parts: `FREQ` (DAILY / WEEKLY / MONTHLY / YEARLY),
 * `INTERVAL`, `COUNT`, `UNTIL`, and `BYDAY` on weekly rules (plain
 * weekday codes or ordinals — ordinals are ignored because a weekly
 * rule's BYDAY never carries one). Unknown parts are ignored rather
 * than rejected: a rule the series came with from another client must
 * not disappear from the grid. A rule we cannot parse falls back to a
 * single occurrence at the master's own start.
 *
 * Iteration runs in the event's IANA zone when DTSTART carries a
 * `TZID` (the server exposes it as `timezone`), so a 09:00 Paris
 * weekly meeting stays 09:00 across DST. A UTC event keeps the old
 * behaviour: 09:00Z stays 09:00Z forever.
 */
import type { CalendarEvent } from '$lib/api/endpoints/calendars';
import { DAY_MS, dateKey, utcDateKey } from '$lib/utils/calendarDate';

export interface EventOccurrence {
	/** Stable per-occurrence key (series id + occurrence start). */
	key: string;
	event: CalendarEvent;
	start: Date;
	end: Date;
	allDay: boolean;
}

type Frequency = 'DAILY' | 'WEEKLY' | 'MONTHLY' | 'YEARLY';

interface RecurrenceRule {
	freq: Frequency;
	interval: number;
	count?: number;
	until?: number;
	/** JS `getUTCDay()` values (0 = Sunday). */
	byday?: number[];
}

const MAX_ITERATIONS = 20_000;

const WEEKDAY_CODES: Record<string, number> = {
	SU: 0,
	MO: 1,
	TU: 2,
	WE: 3,
	TH: 4,
	FR: 5,
	SA: 6
};

export function parseRRule(raw: string | null | undefined): RecurrenceRule | null {
	if (!raw) return null;
	const parts = new Map<string, string>();
	for (const chunk of raw.trim().split(';')) {
		const eq = chunk.indexOf('=');
		if (eq <= 0) continue;
		parts.set(chunk.slice(0, eq).toUpperCase(), chunk.slice(eq + 1));
	}
	const freq = parts.get('FREQ')?.toUpperCase();
	if (freq !== 'DAILY' && freq !== 'WEEKLY' && freq !== 'MONTHLY' && freq !== 'YEARLY') {
		return null;
	}

	const intervalRaw = Number(parts.get('INTERVAL'));
	const interval = Number.isFinite(intervalRaw) && intervalRaw > 1 ? Math.floor(intervalRaw) : 1;

	const countRaw = Number(parts.get('COUNT'));
	const count = Number.isFinite(countRaw) && countRaw > 0 ? Math.floor(countRaw) : undefined;

	const until = parseUntil(parts.get('UNTIL'));
	const byday = parseByDay(parts.get('BYDAY'));

	return {
		freq,
		interval,
		count,
		until,
		byday: byday.length > 0 ? byday : undefined
	};
}

/** RFC 5545 UNTIL: `YYYYMMDD`, `YYYYMMDDTHHMMSSZ`, or RFC 3339. Date
 * form means "end of that day" (the rule is inclusive). */
function parseUntil(value: string | undefined): number | undefined {
	if (!value) return undefined;
	if (/^\d{8}$/.test(value)) {
		const y = Number(value.slice(0, 4));
		const m = Number(value.slice(4, 6));
		const d = Number(value.slice(6, 8));
		return Date.UTC(y, m - 1, d, 23, 59, 59, 999);
	}
	if (/^\d{8}T\d{6}Z$/.test(value)) {
		const y = Number(value.slice(0, 4));
		const m = Number(value.slice(4, 6));
		const d = Number(value.slice(6, 8));
		const h = Number(value.slice(9, 11));
		const min = Number(value.slice(11, 13));
		const s = Number(value.slice(13, 15));
		return Date.UTC(y, m - 1, d, h, min, s);
	}
	const parsed = Date.parse(value);
	return Number.isNaN(parsed) ? undefined : parsed;
}

function parseByDay(value: string | undefined): number[] {
	if (!value) return [];
	const days: number[] = [];
	for (const token of value.split(',')) {
		const code = token.trim().slice(-2).toUpperCase();
		const dow = WEEKDAY_CODES[code];
		if (dow !== undefined && !days.includes(dow)) days.push(dow);
	}
	return days;
}

/** Formatter cache: constructing `Intl.DateTimeFormat` per occurrence
 * would dominate expansion time. `null` marks an unusable zone. */
const zoneFormatters = new Map<string, Intl.DateTimeFormat | null>();

function zoneFormatter(timeZone: string): Intl.DateTimeFormat | null {
	if (zoneFormatters.has(timeZone)) return zoneFormatters.get(timeZone) ?? null;
	let formatter: Intl.DateTimeFormat | null;
	try {
		formatter = new Intl.DateTimeFormat('en-US', {
			timeZone,
			hourCycle: 'h23',
			year: 'numeric',
			month: '2-digit',
			day: '2-digit',
			hour: '2-digit',
			minute: '2-digit',
			second: '2-digit'
		});
	} catch {
		formatter = null;
	}
	zoneFormatters.set(timeZone, formatter);
	return formatter;
}

function resolveTimeZone(timeZone: string | null | undefined): string | null {
	if (!timeZone) return null;
	return zoneFormatter(timeZone) ? timeZone : null;
}

/** Wall-clock components of `instant` in `timeZone`, encoded as a Date
 * whose UTC fields hold those components (a "wall date"). */
function wallDateInZone(instant: Date, timeZone: string): Date | null {
	const formatter = zoneFormatter(timeZone);
	if (!formatter) return null;
	const parts = new Map<string, number>();
	for (const part of formatter.formatToParts(instant)) {
		if (part.type !== 'literal') parts.set(part.type, Number(part.value));
	}
	const year = parts.get('year');
	const month = parts.get('month');
	const day = parts.get('day');
	const hour = parts.get('hour');
	const minute = parts.get('minute');
	const second = parts.get('second');
	if (
		year === undefined ||
		month === undefined ||
		day === undefined ||
		hour === undefined ||
		minute === undefined ||
		second === undefined
	) {
		return null;
	}
	return new Date(
		Date.UTC(year, month - 1, day, hour % 24, minute, second, instant.getUTCMilliseconds())
	);
}

function zoneOffsetMs(instant: Date, timeZone: string): number | null {
	const wall = wallDateInZone(instant, timeZone);
	return wall ? wall.getTime() - instant.getTime() : null;
}

/** Convert a wall date to its instant in the zone. A second offset
 * lookup settles the DST boundary where the first guess changes side. */
function wallDateToInstant(wall: Date, timeZone: string): Date | null {
	const firstOffset = zoneOffsetMs(wall, timeZone);
	if (firstOffset === null) return null;
	let instant = new Date(wall.getTime() - firstOffset);
	const secondOffset = zoneOffsetMs(instant, timeZone);
	if (secondOffset !== null && secondOffset !== firstOffset) {
		instant = new Date(wall.getTime() - secondOffset);
	}
	return instant;
}

/** Collect occurrence starts in the series' zone, then convert each
 * wall start to a real instant. Returns `null` when the zone cannot be
 * formatted, so the caller can fall back to the UTC path. */
function collectStartsInZone(
	masterStart: Date,
	masterEnd: Date,
	rule: RecurrenceRule,
	rangeStartMs: number,
	rangeEndMs: number,
	timeZone: string
): Date[] | null {
	const duration = Math.max(0, masterEnd.getTime() - masterStart.getTime());
	const wallStart = wallDateInZone(masterStart, timeZone);
	const rangeStartWall = wallDateInZone(new Date(rangeStartMs - duration), timeZone);
	const rangeEndWall = wallDateInZone(new Date(rangeEndMs), timeZone);
	if (!wallStart || !rangeStartWall || !rangeEndWall) return null;

	// Two days of margin covers any zone offset on either side of the
	// window; the overlap check after conversion trims the excess.
	const wallFrom = rangeStartWall.getTime() - 2 * DAY_MS;
	let wallTo = rangeEndWall.getTime() + 2 * DAY_MS;
	if (rule.until !== undefined) {
		wallTo = Math.min(wallTo, rule.until + 2 * DAY_MS);
	}
	const wallRule: RecurrenceRule = rule.until === undefined ? rule : { ...rule, until: undefined };

	const starts: Date[] = [];
	for (const wall of collectStarts(wallStart, wallRule, wallFrom, wallTo)) {
		const start = wallDateToInstant(wall, timeZone);
		if (!start) continue;
		if (rule.until !== undefined && start.getTime() > rule.until) continue;
		starts.push(start);
	}
	return starts;
}

/** Add whole months while keeping the day-of-month. `null` when the
 * target month is too short (Feb 31) — RFC 5545 skips, it does not
 * clamp. */
function addMonthsUtc(date: Date, months: number): Date | null {
	const target = new Date(
		Date.UTC(
			date.getUTCFullYear(),
			date.getUTCMonth() + months,
			date.getUTCDate(),
			date.getUTCHours(),
			date.getUTCMinutes(),
			date.getUTCSeconds(),
			date.getUTCMilliseconds()
		)
	);
	return target.getUTCDate() === date.getUTCDate() ? target : null;
}

function addYearsUtc(date: Date, years: number): Date | null {
	const target = new Date(
		Date.UTC(
			date.getUTCFullYear() + years,
			date.getUTCMonth(),
			date.getUTCDate(),
			date.getUTCHours(),
			date.getUTCMinutes(),
			date.getUTCSeconds(),
			date.getUTCMilliseconds()
		)
	);
	return target.getUTCMonth() === date.getUTCMonth() ? target : null;
}

function advance(start: Date, rule: RecurrenceRule, steps = 1): Date | null {
	switch (rule.freq) {
		case 'DAILY':
			return new Date(start.getTime() + steps * rule.interval * DAY_MS);
		case 'WEEKLY':
			return new Date(start.getTime() + steps * rule.interval * 7 * DAY_MS);
		case 'MONTHLY':
			return addMonthsUtc(start, steps * rule.interval);
		case 'YEARLY':
			return addYearsUtc(start, steps * rule.interval);
	}
}

function startOfWeekUtc(value: Date): Date {
	const day = new Date(Date.UTC(value.getUTCFullYear(), value.getUTCMonth(), value.getUTCDate()));
	const diff = (day.getUTCDay() + 6) % 7; // Monday = 0
	return new Date(day.getTime() - diff * DAY_MS);
}

function weeklyByDayStarts(
	start: Date,
	rule: RecurrenceRule,
	fromMs: number,
	toMs: number
): Date[] {
	const byday = [...(rule.byday ?? [])].sort((a, b) => ((a + 6) % 7) - ((b + 6) % 7));
	const weekStart = startOfWeekUtc(start);
	const h = start.getUTCHours();
	const m = start.getUTCMinutes();
	const s = start.getUTCSeconds();
	const ms = start.getUTCMilliseconds();
	const starts: Date[] = [];
	let produced = 0;

	for (let week = 0; week < MAX_ITERATIONS; week++) {
		const base = week * rule.interval * 7 * DAY_MS;
		let anyFuture = false;
		for (const dow of byday) {
			const offset = (dow + 6) % 7;
			const candidate = new Date(weekStart.getTime() + base + offset * DAY_MS);
			candidate.setUTCHours(h, m, s, ms);
			if (candidate.getTime() < start.getTime()) continue;
			if (rule.count !== undefined && produced >= rule.count) return starts;
			if (rule.until !== undefined && candidate.getTime() > rule.until) return starts;
			produced++;
			if (candidate.getTime() > toMs) return starts;
			anyFuture = true;
			if (candidate.getTime() >= fromMs) starts.push(candidate);
		}
		if (!anyFuture && week > 0) return starts;
	}
	return starts;
}

function collectStarts(start: Date, rule: RecurrenceRule, fromMs: number, toMs: number): Date[] {
	if (rule.freq === 'WEEKLY' && rule.byday && rule.byday.length > 0) {
		return weeklyByDayStarts(start, rule, fromMs, toMs);
	}

	let produced = 0;
	let step = 0;
	let cursor: Date | null = start;

	// Fast-forward whole steps for the fixed-length frequencies when
	// COUNT does not make the skipped occurrences observable. Monthly
	// and yearly walk from the start: a handful of hundred steps, and
	// jumping would have to model skipped short months.
	if (rule.count === undefined && (rule.freq === 'DAILY' || rule.freq === 'WEEKLY')) {
		const stepMs = (rule.freq === 'DAILY' ? DAY_MS : 7 * DAY_MS) * rule.interval;
		const skipped = Math.max(0, Math.floor((fromMs - start.getTime()) / stepMs));
		if (skipped > 0) {
			produced = skipped;
			step = skipped;
			cursor = advance(start, rule, skipped);
		}
	}

	const starts: Date[] = [];
	for (let i = 0; i < MAX_ITERATIONS; i++) {
		if (rule.count !== undefined && produced >= rule.count) break;
		if (!cursor) {
			// Short month (Feb 31): RFC 5545 skips the occurrence
			// without counting it, and later months must still run.
			step++;
			cursor = advance(start, rule, step);
			continue;
		}
		if (rule.until !== undefined && cursor.getTime() > rule.until) break;
		if (cursor.getTime() > toMs) break;
		if (cursor.getTime() >= fromMs) starts.push(cursor);
		produced++;
		step++;
		cursor = advance(start, rule, step);
	}
	return starts;
}

function occurrence(event: CalendarEvent, start: Date, end: Date): EventOccurrence {
	return {
		key: `${event.id}@${start.getTime()}`,
		event,
		start,
		end,
		allDay: event.all_day
	};
}

function overlaps(start: Date, end: Date, rangeStartMs: number, rangeEndMs: number): boolean {
	return end.getTime() > rangeStartMs && start.getTime() < rangeEndMs;
}

function pushIfOverlaps(
	out: EventOccurrence[],
	event: CalendarEvent,
	start: Date,
	end: Date,
	rangeStartMs: number,
	rangeEndMs: number
): void {
	if (overlaps(start, end, rangeStartMs, rangeEndMs)) out.push(occurrence(event, start, end));
}

/**
 * Expand every row into the occurrences that intersect `[rangeStart,
 * rangeEnd)`. Exception rows (RECURRENCE-ID set) are emitted at their
 * own moved time, and the matching generated occurrence is suppressed
 * so the series does not show it twice.
 *
 * Callers should pad the range by a day on each side and then place
 * occurrences by day: an all-day event is stored at UTC midnight, which
 * can fall on the previous local date west of Greenwich.
 */
export function expandEvents(
	events: CalendarEvent[],
	rangeStart: Date,
	rangeEnd: Date
): EventOccurrence[] {
	const rangeStartMs = rangeStart.getTime();
	const rangeEndMs = rangeEnd.getTime();
	const groups = new Map<string, CalendarEvent[]>();

	for (const event of events) {
		const key = `${event.calendar_id}\u0000${event.ical_uid}`;
		const group = groups.get(key);
		if (group) group.push(event);
		else groups.set(key, [event]);
	}

	const out: EventOccurrence[] = [];

	for (const group of groups.values()) {
		let master: CalendarEvent | null = null;
		const overrides = new Map<number, CalendarEvent>();

		for (const event of group) {
			if (event.recurrence_id) {
				overrides.set(new Date(event.recurrence_id).getTime(), event);
			} else if (!master) {
				master = event;
			}
		}

		if (!master) {
			for (const event of group) {
				pushIfOverlaps(
					out,
					event,
					new Date(event.start_time),
					new Date(event.end_time),
					rangeStartMs,
					rangeEndMs
				);
			}
			continue;
		}

		const masterStart = new Date(master.start_time);
		const masterEnd = new Date(master.end_time);
		const rule = parseRRule(master.rrule);

		if (!rule) {
			pushIfOverlaps(out, master, masterStart, masterEnd, rangeStartMs, rangeEndMs);
		} else {
			const duration = Math.max(0, masterEnd.getTime() - masterStart.getTime());
			// A TZID-qualified series steps in wall-clock time, not
			// UTC: a 09:00 Paris weekly meeting stays 09:00 after the
			// DST switch even though its UTC instant moves.
			const timeZone = master.all_day ? null : resolveTimeZone(master.timezone);
			const zonedStarts = timeZone
				? collectStartsInZone(masterStart, masterEnd, rule, rangeStartMs, rangeEndMs, timeZone)
				: null;
			const starts =
				zonedStarts ?? collectStarts(masterStart, rule, rangeStartMs - duration, rangeEndMs);
			for (const start of starts) {
				const end = new Date(start.getTime() + duration);
				if (!overlaps(start, end, rangeStartMs, rangeEndMs)) continue;
				if (overrides.has(start.getTime())) continue;
				out.push(occurrence(master, start, end));
			}
		}

		for (const event of overrides.values()) {
			pushIfOverlaps(
				out,
				event,
				new Date(event.start_time),
				new Date(event.end_time),
				rangeStartMs,
				rangeEndMs
			);
		}
	}

	out.sort((a, b) => {
		const byStart = a.start.getTime() - b.start.getTime();
		if (byStart !== 0) return byStart;
		if (a.allDay !== b.allDay) return a.allDay ? -1 : 1;
		return a.event.summary.localeCompare(b.event.summary);
	});
	return out;
}

/** Maximum days a single occurrence is spread across in the grid. A
 * malformed all-day row with a decade-long DTEND must not allocate a
 * decade of keys on every render. */
const MAX_OCCURRENCE_DAYS = 3700;

/**
 * Day keys this occurrence renders on — UTC dates for all-day rows
 * (DTEND is exclusive), local dates for timed rows.
 */
export function occurrenceDayKeys(occ: EventOccurrence): string[] {
	const keys: string[] = [];
	if (occ.allDay) {
		const start = occ.start;
		const inclusiveLast = new Date(Math.max(occ.end.getTime(), start.getTime() + 1) - 1);
		let cursor = Date.UTC(start.getUTCFullYear(), start.getUTCMonth(), start.getUTCDate());
		const last = Date.UTC(
			inclusiveLast.getUTCFullYear(),
			inclusiveLast.getUTCMonth(),
			inclusiveLast.getUTCDate()
		);
		for (let i = 0; i < MAX_OCCURRENCE_DAYS && cursor <= last; i++) {
			keys.push(utcDateKey(new Date(cursor)));
			cursor += DAY_MS;
		}
		return keys;
	}

	const inclusiveLast = new Date(Math.max(occ.end.getTime(), occ.start.getTime() + 1) - 1);
	let cursor = new Date(occ.start.getFullYear(), occ.start.getMonth(), occ.start.getDate());
	for (let i = 0; i < MAX_OCCURRENCE_DAYS; i++) {
		keys.push(dateKey(cursor));
		const next = new Date(cursor.getFullYear(), cursor.getMonth(), cursor.getDate() + 1);
		if (next.getTime() > inclusiveLast.getTime()) break;
		cursor = next;
	}
	return keys;
}

/** Bucket occurrences by the day keys they render on. */
export function groupOccurrencesByDay(
	occurrences: EventOccurrence[]
): Map<string, EventOccurrence[]> {
	const byDay = new Map<string, EventOccurrence[]>();
	for (const occ of occurrences) {
		for (const key of occurrenceDayKeys(occ)) {
			const bucket = byDay.get(key);
			if (bucket) bucket.push(occ);
			else byDay.set(key, [occ]);
		}
	}
	return byDay;
}
