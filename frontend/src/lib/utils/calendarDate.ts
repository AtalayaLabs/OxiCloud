/** Date maths for the calendar view — no date library.
 *
 * Everything that groups or positions events uses the viewer's LOCAL
 * day, because that is the grid the user reads. Recurrence iteration
 * runs in UTC (see `calendarRecurrence.ts`) so a weekly 09:00Z event
 * does not drift an hour across a DST boundary.
 */
import { dateTimeFormatFor } from '$lib/utils/display';

export const DAY_MS = 86_400_000;

export type CalendarView = 'month' | 'week' | 'day';

const MONTH_TITLE_OPTS: Intl.DateTimeFormatOptions = { month: 'long', year: 'numeric' };
const WEEKDAY_SHORT_OPTS: Intl.DateTimeFormatOptions = { weekday: 'short' };
const DAY_NUMBER_OPTS: Intl.DateTimeFormatOptions = { day: 'numeric' };
const TIME_OPTS: Intl.DateTimeFormatOptions = { hour: '2-digit', minute: '2-digit' };
const HOUR_OPTS: Intl.DateTimeFormatOptions = { hour: 'numeric' };
const DATE_LONG_OPTS: Intl.DateTimeFormatOptions = {
	weekday: 'long',
	month: 'long',
	day: 'numeric',
	year: 'numeric'
};

export function startOfDay(value: Date): Date {
	return new Date(value.getFullYear(), value.getMonth(), value.getDate());
}

export function addDays(value: Date, days: number): Date {
	const next = new Date(value);
	next.setDate(next.getDate() + days);
	return next;
}

/** Month anchor: the first of the month `offset` months from `value`. */
export function shiftMonth(value: Date, offset: number): Date {
	return new Date(value.getFullYear(), value.getMonth() + offset, 1);
}

export function startOfMonth(value: Date): Date {
	return new Date(value.getFullYear(), value.getMonth(), 1);
}

export function startOfWeek(value: Date, weekStartsOn: 0 | 1 = 1): Date {
	const day = startOfDay(value);
	const diff = (day.getDay() - weekStartsOn + 7) % 7;
	return addDays(day, -diff);
}

/** Six fixed weeks starting on the week containing the 1st — stable
 * layout, no reflow between a 5-week and 6-week month. */
export function monthGridDays(anchor: Date, weekStartsOn: 0 | 1 = 1): Date[] {
	const first = startOfWeek(startOfMonth(anchor), weekStartsOn);
	const days: Date[] = new Array(42);
	for (let i = 0; i < 42; i++) days[i] = addDays(first, i);
	return days;
}

export function weekDays(anchor: Date, weekStartsOn: 0 | 1 = 1): Date[] {
	const first = startOfWeek(anchor, weekStartsOn);
	const days: Date[] = new Array(7);
	for (let i = 0; i < 7; i++) days[i] = addDays(first, i);
	return days;
}

export function sameDay(a: Date, b: Date): boolean {
	return (
		a.getFullYear() === b.getFullYear() &&
		a.getMonth() === b.getMonth() &&
		a.getDate() === b.getDate()
	);
}

export function isSameMonth(a: Date, b: Date): boolean {
	return a.getFullYear() === b.getFullYear() && a.getMonth() === b.getMonth();
}

/** Local `YYYY-MM-DD` key for map lookups. */
export function dateKey(value: Date): string {
	const y = value.getFullYear();
	const m = value.getMonth() + 1;
	const d = value.getDate();
	return `${y}-${m < 10 ? '0' : ''}${m}-${d < 10 ? '0' : ''}${d}`;
}

/** UTC `YYYY-MM-DD` key — all-day events are stored at UTC midnight
 * (CalDAV's VALUE=DATE model), so their calendar date is the UTC date. */
export function utcDateKey(value: Date): string {
	const y = value.getUTCFullYear();
	const m = value.getUTCMonth() + 1;
	const d = value.getUTCDate();
	return `${y}-${m < 10 ? '0' : ''}${m}-${d < 10 ? '0' : ''}${d}`;
}

export function formatMonthTitle(value: Date): string {
	return dateTimeFormatFor(undefined, MONTH_TITLE_OPTS).format(value);
}

export function formatWeekdayShort(value: Date): string {
	return dateTimeFormatFor(undefined, WEEKDAY_SHORT_OPTS).format(value);
}

export function formatDayNumber(value: Date): string {
	return dateTimeFormatFor(undefined, DAY_NUMBER_OPTS).format(value);
}

export function formatTime(value: Date): string {
	return dateTimeFormatFor(undefined, TIME_OPTS).format(value);
}

export function formatDayTitle(value: Date): string {
	return dateTimeFormatFor(undefined, DATE_LONG_OPTS).format(value);
}

/** Localised hour label ("9 AM" / "09:00") for the time grid gutter. */
export function formatHourLabel(hour: number): string {
	return dateTimeFormatFor(undefined, HOUR_OPTS).format(new Date(2000, 0, 1, hour));
}
