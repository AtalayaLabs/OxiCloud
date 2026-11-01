/**
 * Calendar REST endpoints (`/api/calendars`).
 *
 * CalDAV stays the sync surface; this module is the SPA's JSON path so
 * the calendar view never has to parse XML or iCalendar. Recurring
 * masters arrive unexpanded from the server — expansion happens in
 * `$lib/utils/calendarRecurrence`.
 */
import { apiFetch, apiJson } from '$lib/api/client';
import { getCsrfHeaders } from '$lib/api/csrf';
import type {
	CalendarEventResponse,
	CalendarResponse,
	SaveEventRequest
} from '$lib/api/generated/openapi/types.gen';

const JSON_HEADERS = { 'Content-Type': 'application/json' };

/** Caller access level on a calendar. `own` is the only level that may
 * rename or delete the calendar itself. */
export type CalendarAccess = 'read' | 'contribute' | 'edit' | 'own';

/**
 * The generated DTOs mark `Option` fields as optional (`?`), but the
 * server serialises them as explicit `null`. The intersections below
 * keep the precise wire shape while still failing `npm run check` if
 * the generated field names or types move.
 */
export type CalendarInfo = Omit<CalendarResponse, 'access'> & { access: CalendarAccess };

export type CalendarEvent = CalendarEventResponse & {
	description: string | null;
	location: string | null;
	rrule: string | null;
	recurrence_id: string | null;
};

/** Full replacement payload. The update path rewrites the whole VEVENT,
 * so an omitted optional field is cleared, not left untouched. */
export type SaveEventBody = Required<
	Pick<SaveEventRequest, 'summary' | 'start_time' | 'end_time'>
> & {
	description: string | null;
	location: string | null;
	all_day: boolean;
	rrule: string | null;
};

export function listCalendars(): Promise<CalendarInfo[]> {
	return apiJson<CalendarInfo[]>('/api/calendars', { credentials: 'same-origin' });
}

export async function createCalendar(name: string, color?: string | null): Promise<CalendarInfo> {
	const res = await apiFetch('/api/calendars', {
		method: 'POST',
		credentials: 'same-origin',
		headers: { ...JSON_HEADERS, ...getCsrfHeaders() },
		body: JSON.stringify({ name, color: color ?? null })
	});
	if (!res.ok) throw new Error(`create calendar failed: ${res.status}`);
	return (await res.json()) as CalendarInfo;
}

export async function updateCalendar(
	calendarId: string,
	patch: { name?: string; description?: string | null; color?: string | null }
): Promise<CalendarInfo> {
	const res = await apiFetch(`/api/calendars/${calendarId}`, {
		method: 'PUT',
		credentials: 'same-origin',
		headers: { ...JSON_HEADERS, ...getCsrfHeaders() },
		body: JSON.stringify(patch)
	});
	if (!res.ok) throw new Error(`update calendar failed: ${res.status}`);
	return (await res.json()) as CalendarInfo;
}

export async function deleteCalendar(calendarId: string): Promise<void> {
	const res = await apiFetch(`/api/calendars/${calendarId}`, {
		method: 'DELETE',
		credentials: 'same-origin',
		headers: getCsrfHeaders()
	});
	if (!res.ok) throw new Error(`delete calendar failed: ${res.status}`);
}

/** Events overlapping `[start, end)`. Recurring masters appear once. */
export function listEvents(calendarId: string, start: Date, end: Date): Promise<CalendarEvent[]> {
	const params = new URLSearchParams({
		start: start.toISOString(),
		end: end.toISOString()
	});
	return apiJson<CalendarEvent[]>(`/api/calendars/${calendarId}/events?${params.toString()}`, {
		credentials: 'same-origin'
	});
}

export async function createEvent(calendarId: string, body: SaveEventBody): Promise<CalendarEvent> {
	const res = await apiFetch(`/api/calendars/${calendarId}/events`, {
		method: 'POST',
		credentials: 'same-origin',
		headers: { ...JSON_HEADERS, ...getCsrfHeaders() },
		body: JSON.stringify(body)
	});
	if (!res.ok) throw new Error(`create event failed: ${res.status}`);
	return (await res.json()) as CalendarEvent;
}

export async function updateEvent(
	calendarId: string,
	eventId: string,
	body: SaveEventBody
): Promise<CalendarEvent> {
	const res = await apiFetch(`/api/calendars/${calendarId}/events/${eventId}`, {
		method: 'PUT',
		credentials: 'same-origin',
		headers: { ...JSON_HEADERS, ...getCsrfHeaders() },
		body: JSON.stringify(body)
	});
	if (!res.ok) throw new Error(`update event failed: ${res.status}`);
	return (await res.json()) as CalendarEvent;
}

export async function deleteEvent(calendarId: string, eventId: string): Promise<void> {
	const res = await apiFetch(`/api/calendars/${calendarId}/events/${eventId}`, {
		method: 'DELETE',
		credentials: 'same-origin',
		headers: getCsrfHeaders()
	});
	if (!res.ok) throw new Error(`delete event failed: ${res.status}`);
}
