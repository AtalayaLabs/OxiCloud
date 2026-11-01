# =============================================================
# OxiCloud – Calendar REST API (SPA JSON surface)
# =============================================================
# Covers the endpoints the SvelteKit calendar view calls:
#
#   * GET/POST /api/calendars, PUT/DELETE /api/calendars/{id}
#   * GET/POST /api/calendars/{id}/events
#   * PUT/DELETE /api/calendars/{id}/events/{event_id}
#
# The CalDAV protocol surface is covered by calendar.hurl and the
# caldav_*.hurl scenarios; this one pins the JSON contract the UI reads
# — field names, access levels, range validation, and the calendar-id
# guard on event mutations. The create/update path writes a full
# iCalendar component, so RRULE and DESCRIPTION round-trip even though
# the REST response omits the raw `ical_data` blob.
#
# Run:
#   hurl --variables-file tests/api/hurl.vars --test tests/api/calendar_rest.hurl
# =============================================================


# ─────────────────────────────────────────────────────────────
# Step 1 – Login (admin account seeded by setup.hurl)
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/login
Content-Type: application/json
{
  "username": "{{username}}",
  "password": "{{password}}"
}

HTTP 200
[Captures]
token: jsonpath "$.access_token"


# ─────────────────────────────────────────────────────────────
# Step 2 – Create a calendar. The response must carry the caller's
# access level, because the SPA gates rename/delete on it.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/calendars
Authorization: Bearer {{token}}
Content-Type: application/json
{
  "name": "rest-calendar-test",
  "description": "Created by calendar_rest.hurl",
  "color": "#3366CC"
}

HTTP 201
[Captures]
calendar_id: jsonpath "$.id"
[Asserts]
jsonpath "$.name" == "rest-calendar-test"
jsonpath "$.access" == "own"
jsonpath "$.is_readonly" == false
jsonpath "$.color" == "#3366CC"
jsonpath "$.id" isString


# ─────────────────────────────────────────────────────────────
# Step 3 – The new calendar is in the list.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/calendars
Authorization: Bearer {{token}}

HTTP 200
[Asserts]
jsonpath "$[?(@.id == '{{calendar_id}}')]" count == 1
jsonpath "$[?(@.id == '{{calendar_id}}')].access" == "own"


# ─────────────────────────────────────────────────────────────
# Step 4 – Empty event window.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/calendars/{{calendar_id}}/events?start=2026-12-01T00:00:00Z&end=2027-01-01T00:00:00Z
Authorization: Bearer {{token}}

HTTP 200
[Asserts]
jsonpath "$" isCollection
jsonpath "$" count == 0


# ─────────────────────────────────────────────────────────────
# Step 5 – Create a recurring event. Summary/description/location/
# RRULE must all survive the server-side VEVENT construction.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/calendars/{{calendar_id}}/events
Authorization: Bearer {{token}}
Content-Type: application/json
{
  "summary": "Weekly sync",
  "description": "Standing meeting",
  "location": "Room 1",
  "start_time": "2026-12-01T09:00:00Z",
  "end_time": "2026-12-01T10:00:00Z",
  "all_day": false,
  "rrule": "FREQ=WEEKLY"
}

HTTP 201
[Captures]
event_id: jsonpath "$.id"
event_uid: jsonpath "$.ical_uid"
[Asserts]
jsonpath "$.calendar_id" == "{{calendar_id}}"
jsonpath "$.summary" == "Weekly sync"
jsonpath "$.description" == "Standing meeting"
jsonpath "$.location" == "Room 1"
jsonpath "$.all_day" == false
jsonpath "$.rrule" == "FREQ=WEEKLY"
jsonpath "$.recurrence_id" == null
jsonpath "$.ical_data" not exists
jsonpath "$.ical_uid" isString


# ─────────────────────────────────────────────────────────────
# Step 6 – The event is returned in a window that starts later than
# its DTSTART, because the recurring master still overlaps it.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/calendars/{{calendar_id}}/events?start=2026-12-15T00:00:00Z&end=2026-12-31T00:00:00Z
Authorization: Bearer {{token}}

HTTP 200
[Asserts]
jsonpath "$[?(@.id == '{{event_id}}')]" count == 1


# ─────────────────────────────────────────────────────────────
# Step 6b – A CalDAV exception moves the Dec 22 occurrence out of
# the window. The row's own start is in February, but its
# RECURRENCE-ID slot (Dec 22) is inside the query window. It must
# still be returned, or the client cannot suppress the original
# occurrence and renders a phantom Dec 22 event.
# ─────────────────────────────────────────────────────────────
PUT {{base_url}}/caldav/{{calendar_id}}/{{event_uid}}.ics
Authorization: Bearer {{token}}
Content-Type: text/calendar; charset=utf-8
```
BEGIN:VCALENDAR
VERSION:2.0
PRODID:-//OxiCloud calendar-rest test//EN
BEGIN:VEVENT
UID:{{event_uid}}
DTSTAMP:20261201T120000Z
DTSTART:20270202T090000Z
DTEND:20270202T100000Z
SUMMARY:Weekly sync (moved)
RECURRENCE-ID:20261222T090000Z
END:VEVENT
END:VCALENDAR
```

HTTP 201


GET {{base_url}}/api/calendars/{{calendar_id}}/events?start=2026-12-15T00:00:00Z&end=2026-12-31T00:00:00Z
Authorization: Bearer {{token}}

HTTP 200
[Asserts]
jsonpath "$[?(@.recurrence_id == '2026-12-22T09:00:00Z')]" count == 1
jsonpath "$[?(@.recurrence_id == '2026-12-22T09:00:00Z')].summary" == "Weekly sync (moved)"


# ─────────────────────────────────────────────────────────────
# Step 7 – Update: full replacement, clearing the RRULE.
# ─────────────────────────────────────────────────────────────
PUT {{base_url}}/api/calendars/{{calendar_id}}/events/{{event_id}}
Authorization: Bearer {{token}}
Content-Type: application/json
{
  "summary": "Weekly sync (renamed)",
  "description": null,
  "location": null,
  "start_time": "2026-12-01T09:30:00Z",
  "end_time": "2026-12-01T10:30:00Z",
  "all_day": false,
  "rrule": null
}

HTTP 200
[Captures]
updated_event_id: jsonpath "$.id"
[Asserts]
jsonpath "$.summary" == "Weekly sync (renamed)"
jsonpath "$.description" == null
jsonpath "$.location" == null
jsonpath "$.rrule" == null
jsonpath "$.start_time" == "2026-12-01T09:30:00Z"
jsonpath "$.ical_uid" == "{{event_uid}}"


# ─────────────────────────────────────────────────────────────
# Step 8 – A second calendar must not be able to address the first
# calendar's event id (cross-calendar id smuggling → 404, not 204).
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/calendars
Authorization: Bearer {{token}}
Content-Type: application/json
{
  "name": "rest-calendar-other"
}

HTTP 201
[Captures]
other_calendar_id: jsonpath "$.id"


DELETE {{base_url}}/api/calendars/{{other_calendar_id}}/events/{{updated_event_id}}
Authorization: Bearer {{token}}

HTTP 404


# ─────────────────────────────────────────────────────────────
# Step 9 – Inverted ranges are a 400, not an empty 200.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/calendars/{{calendar_id}}/events?start=2027-01-01T00:00:00Z&end=2026-12-01T00:00:00Z
Authorization: Bearer {{token}}

HTTP 400
[Asserts]
jsonpath "$.error_type" == "BadRequest"


# ─────────────────────────────────────────────────────────────
# Step 9b – RRULE is written into the stored VEVENT without TEXT
# escaping. A CRLF would let a caller inject arbitrary calendar
# properties, so it must be rejected at the edge.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/calendars/{{calendar_id}}/events
Authorization: Bearer {{token}}
Content-Type: application/json
{
  "summary": "Injection attempt",
  "start_time": "2026-12-02T09:00:00Z",
  "end_time": "2026-12-02T10:00:00Z",
  "rrule": "FREQ=DAILY\r\nDESCRIPTION:pwned"
}

HTTP 400
[Asserts]
jsonpath "$.error_type" == "BadRequest"


# ─────────────────────────────────────────────────────────────
# Step 10 – Delete the event, then the calendars.
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/api/calendars/{{calendar_id}}/events/{{updated_event_id}}
Authorization: Bearer {{token}}

HTTP 204


DELETE {{base_url}}/api/calendars/{{other_calendar_id}}
Authorization: Bearer {{token}}

HTTP 204


DELETE {{base_url}}/api/calendars/{{calendar_id}}
Authorization: Bearer {{token}}

HTTP 204


GET {{base_url}}/api/calendars
Authorization: Bearer {{token}}

HTTP 200
[Asserts]
jsonpath "$[?(@.id == '{{calendar_id}}')]" count == 0
jsonpath "$[?(@.id == '{{other_calendar_id}}')]" count == 0
