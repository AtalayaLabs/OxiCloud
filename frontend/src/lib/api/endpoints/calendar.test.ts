import { describe, expect, it } from 'vitest';
import { parseCalDavCalendarIndex, parseCalendarIcs } from './calendar';

describe('calendar agenda parsing', () => {
	it('parses calendar entries from the CalDAV root response', () => {
		const xml = `<?xml version="1.0" encoding="utf-8"?>
<D:multistatus xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav" xmlns:CS="http://calendarserver.org/ns/">
  <D:response>
    <D:href>/caldav/</D:href>
  </D:response>
  <D:response>
    <D:href>/caldav/11111111-1111-1111-1111-111111111111/</D:href>
    <D:propstat>
      <D:prop>
        <D:displayname>Work</D:displayname>
        <CS:calendar-color>#ffcc00</CS:calendar-color>
      </D:prop>
    </D:propstat>
  </D:response>
</D:multistatus>`;

		expect(parseCalDavCalendarIndex(xml)).toEqual([
			{
				id: '11111111-1111-1111-1111-111111111111',
				href: '/caldav/11111111-1111-1111-1111-111111111111/',
				name: 'Work',
				color: '#ffcc00'
			}
		]);
	});

	it('parses VEVENT blocks into agenda rows', () => {
		const calendar = {
			id: '11111111-1111-1111-1111-111111111111',
			href: '/caldav/11111111-1111-1111-1111-111111111111/',
			name: 'Work',
			color: null
		};
		const ics = `BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:alpha\r\nSUMMARY:Standup\r\nDTSTART:20260818T090000Z\r\nDTEND:20260818T093000Z\r\nLOCATION:Room 2\r\nEND:VEVENT\r\nBEGIN:VEVENT\r\nUID:beta\r\nSUMMARY:All day focus\r\nDTSTART;VALUE=DATE:20260819\r\nDTEND;VALUE=DATE:20260820\r\nEND:VEVENT\r\nEND:VCALENDAR`;

		const events = parseCalendarIcs(calendar, ics);
		expect(events).toHaveLength(2);
		expect(events[0].summary).toBe('Standup');
		expect(events[0].location).toBe('Room 2');
		expect(events[0].allDay).toBe(false);
		expect(events[1].summary).toBe('All day focus');
		expect(events[1].allDay).toBe(true);
	});
});
