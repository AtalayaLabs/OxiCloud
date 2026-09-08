import { apiFetch } from '$lib/api/client';

const XML_HEADERS = { 'Content-Type': 'application/xml; charset=utf-8' };

export interface CalendarAgendaSource {
	id: string;
	href: string;
	name: string;
	color?: string | null;
}

export interface CalendarAgendaEvent {
	id: string;
	calendarId: string;
	calendarName: string;
	href: string;
	summary: string;
	description: string | null;
	location: string | null;
	start: Date;
	end: Date;
	allDay: boolean;
	uid: string;
	recurrenceId: string | null;
}

export interface CalendarAgendaResult {
	calendars: CalendarAgendaSource[];
	events: CalendarAgendaEvent[];
	errors: string[];
}

interface IcsProperty {
	name: string;
	params: string[];
	value: string;
}

interface ParsedIcsEvent {
	id: string;
	calendarId: string;
	calendarName: string;
	href: string;
	summary: string;
	description: string | null;
	location: string | null;
	start: Date;
	end: Date;
	allDay: boolean;
	uid: string;
	recurrenceId: string | null;
}

function propfindBody(): string {
	return [
		'<?xml version="1.0" encoding="utf-8"?>',
		'<propfind xmlns="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav" xmlns:CS="http://calendarserver.org/ns/">',
		'  <prop>',
		'    <displayname />',
		'    <CS:calendar-color />',
		'  </prop>',
		'</propfind>'
	].join('');
}

function normalizeHref(href: string): string {
	return href.endsWith('/') ? href : `${href}/`;
}

function joinHref(base: string, suffix: string): string {
	return `${normalizeHref(base)}${suffix}`;
}

function trimTrailingSlash(value: string): string {
	return value.replace(/\/+$/, '');
}

function parseElementText(element: Element | null): string {
	return element?.textContent?.trim() ?? '';
}

function elementByLocalName(root: Element | Document, localName: string): Element | null {
	for (const child of Array.from(root.children)) {
		if (child.localName === localName) return child;
		const nested = elementByLocalName(child, localName);
		if (nested) return nested;
	}
	return null;
}

function childrenByLocalName(root: Element | Document, localName: string): Element[] {
	const matches: Element[] = [];
	for (const child of Array.from(root.children)) {
		if (child.localName === localName) matches.push(child);
		matches.push(...childrenByLocalName(child, localName));
	}
	return matches;
}

function appendProp(map: Map<string, IcsProperty[]>, prop: IcsProperty): void {
	const existing = map.get(prop.name);
	if (existing) existing.push(prop);
	else map.set(prop.name, [prop]);
}

function firstProp(map: Map<string, IcsProperty[]>, name: string): IcsProperty | undefined {
	return map.get(name)?.[0];
}

function unfoldIcsLines(ical: string): string[] {
	const lines = ical.replace(/\r\n?/g, '\n').split('\n');
	const unfolded: string[] = [];
	for (const line of lines) {
		if ((line.startsWith(' ') || line.startsWith('\t')) && unfolded.length > 0) {
			unfolded[unfolded.length - 1] += line.slice(1);
		} else {
			unfolded.push(line);
		}
	}
	return unfolded;
}

function parseIcsProperty(line: string): IcsProperty | null {
	const colonIndex = line.indexOf(':');
	if (colonIndex < 0) return null;
	const lhs = line.slice(0, colonIndex);
	const value = line.slice(colonIndex + 1);
	const [name, ...params] = lhs.split(';');
	return { name: name.toUpperCase(), params, value };
}

function parseIcsDate(property: IcsProperty | undefined): { date: Date; allDay: boolean } | null {
	if (!property) return null;
	const isDateOnly =
		property.params.some((param) => /^VALUE=DATE$/i.test(param)) || /^\d{8}$/.test(property.value);
	if (isDateOnly) {
		const year = Number(property.value.slice(0, 4));
		const month = Number(property.value.slice(4, 6)) - 1;
		const day = Number(property.value.slice(6, 8));
		return { date: new Date(year, month, day), allDay: true };
	}

	const raw = property.value;
	if (/^\d{8}T\d{6}Z$/.test(raw)) {
		const year = Number(raw.slice(0, 4));
		const month = Number(raw.slice(4, 6)) - 1;
		const day = Number(raw.slice(6, 8));
		const hour = Number(raw.slice(9, 11));
		const minute = Number(raw.slice(11, 13));
		const second = Number(raw.slice(13, 15));
		return { date: new Date(Date.UTC(year, month, day, hour, minute, second)), allDay: false };
	}

	if (/^\d{8}T\d{6}$/.test(raw)) {
		const year = Number(raw.slice(0, 4));
		const month = Number(raw.slice(4, 6)) - 1;
		const day = Number(raw.slice(6, 8));
		const hour = Number(raw.slice(9, 11));
		const minute = Number(raw.slice(11, 13));
		const second = Number(raw.slice(13, 15));
		return { date: new Date(year, month, day, hour, minute, second), allDay: false };
	}

	return { date: new Date(raw), allDay: false };
}

function eventIdentity(
	calendarId: string,
	uid: string,
	recurrenceId: string | null,
	start: Date
): string {
	return [calendarId, uid, recurrenceId ?? '', String(start.getTime())].join(':');
}

function parseIcsEvent(
	calendar: CalendarAgendaSource,
	props: Map<string, IcsProperty[]>
): ParsedIcsEvent | null {
	const startProp = firstProp(props, 'DTSTART');
	const startInfo = parseIcsDate(startProp);
	if (!startInfo) return null;
	const endProp = firstProp(props, 'DTEND');
	const endInfo = parseIcsDate(endProp) ?? startInfo;
	const summary = firstProp(props, 'SUMMARY')?.value.trim() || 'Untitled event';
	const description = firstProp(props, 'DESCRIPTION')?.value.trim() || null;
	const location = firstProp(props, 'LOCATION')?.value.trim() || null;
	const uid =
		firstProp(props, 'UID')?.value.trim() ||
		eventIdentity(calendar.id, summary, null, startInfo.date);
	const recurrenceId = firstProp(props, 'RECURRENCE-ID')?.value.trim() || null;
	const allDay = startInfo.allDay || endInfo.allDay;
	const end = allDay && !endProp ? new Date(startInfo.date.getTime() + 86_400_000) : endInfo.date;

	return {
		id: eventIdentity(calendar.id, uid, recurrenceId, startInfo.date),
		calendarId: calendar.id,
		calendarName: calendar.name,
		href: joinHref(calendar.href, `${encodeURIComponent(uid)}.ics`),
		summary,
		description,
		location,
		start: startInfo.date,
		end,
		allDay,
		uid,
		recurrenceId
	};
}

export function parseCalDavCalendarIndex(xml: string): CalendarAgendaSource[] {
	const doc = new DOMParser().parseFromString(xml, 'application/xml');
	const responseNodes = childrenByLocalName(doc, 'response');
	const calendars: CalendarAgendaSource[] = [];

	for (const response of responseNodes) {
		const href = parseElementText(elementByLocalName(response, 'href'));
		if (!href) continue;
		const trimmedHref = trimTrailingSlash(href);
		const parts = trimmedHref.split('/').filter(Boolean);
		if (parts.length < 2) continue;
		const id = parts[parts.length - 1];
		if (id === 'caldav' || id === 'principals') continue;
		const displayName = parseElementText(elementByLocalName(response, 'displayname')) || id;
		const color = parseElementText(elementByLocalName(response, 'calendar-color')) || null;
		calendars.push({ id, href: normalizeHref(href), name: displayName, color });
	}

	return calendars;
}

export function parseCalendarIcs(
	calendar: CalendarAgendaSource,
	ical: string
): CalendarAgendaEvent[] {
	const events: CalendarAgendaEvent[] = [];
	const lines = unfoldIcsLines(ical);
	let inEvent = false;
	let props = new Map<string, IcsProperty[]>();

	const flush = () => {
		if (!inEvent) return;
		const parsed = parseIcsEvent(calendar, props);
		if (parsed) {
			events.push(parsed);
		}
		props = new Map<string, IcsProperty[]>();
	};

	for (const rawLine of lines) {
		const line = rawLine.trimEnd();
		if (/^BEGIN:VEVENT$/i.test(line)) {
			inEvent = true;
			props = new Map<string, IcsProperty[]>();
			continue;
		}
		if (/^END:VEVENT$/i.test(line)) {
			flush();
			inEvent = false;
			continue;
		}
		if (!inEvent) continue;
		const prop = parseIcsProperty(line);
		if (!prop) continue;
		appendProp(props, prop);
	}

	return events;
}

function eventInWindow(event: CalendarAgendaEvent, startMs: number, endMs: number): boolean {
	return event.end.getTime() > startMs && event.start.getTime() < endMs;
}

export async function fetchCalendarAgenda(): Promise<CalendarAgendaResult> {
	const calendarsResponse = await apiFetch('/caldav/', {
		method: 'PROPFIND',
		credentials: 'same-origin',
		headers: { Depth: '1', ...XML_HEADERS },
		body: propfindBody()
	});
	if (!calendarsResponse.ok) {
		throw new Error(`calendar index failed: ${calendarsResponse.status}`);
	}

	const calendars = parseCalDavCalendarIndex(await calendarsResponse.text());
	const windowStart = new Date();
	windowStart.setHours(0, 0, 0, 0);
	windowStart.setDate(windowStart.getDate() - 1);
	const windowEnd = new Date(windowStart);
	windowEnd.setDate(windowEnd.getDate() + 31);

	const settled = await Promise.allSettled(
		calendars.map(async (calendar) => {
			const response = await apiFetch(calendar.href, {
				credentials: 'same-origin',
				cache: 'no-store'
			});
			if (!response.ok) {
				throw new Error(`${calendar.name}: ${response.status}`);
			}
			const ics = await response.text();
			return parseCalendarIcs(calendar, ics).filter((event) =>
				eventInWindow(event, windowStart.getTime(), windowEnd.getTime())
			);
		})
	);

	const events: CalendarAgendaEvent[] = [];
	const errors: string[] = [];
	for (const result of settled) {
		if (result.status === 'fulfilled') {
			events.push(...result.value);
		} else {
			errors.push(result.reason instanceof Error ? result.reason.message : String(result.reason));
		}
	}

	events.sort((left, right) => {
		const byStart = left.start.getTime() - right.start.getTime();
		if (byStart !== 0) return byStart;
		return (
			left.summary.localeCompare(right.summary) ||
			left.calendarName.localeCompare(right.calendarName)
		);
	});

	return { calendars, events, errors };
}
