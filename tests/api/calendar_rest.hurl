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
# Step 8b is the cross-user denial block: a user with no grant must
# neither see the calendar in the list nor read/create/update/delete
# through any endpoint here, with the exact 404 anti-enum shape. This
# surface has its own handlers, so the DAV denials in
# dav_error_mapping.hurl do not cover it.
#
# Run:
#   hurl --variables-file tests/api/test.env --test tests/api/calendar_rest.hurl
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
# Body containment, not `jsonpath ... count`: Hurl >= 8.1 coerces a
# one-element filter result to a scalar, and `count` then fails with
# "invalid filter input type". The id appears at most once in the
# array, so containment is the same claim the count made. The regex
# stops at `}` so it cannot match across into a later calendar.
body contains "{{calendar_id}}"
body matches "\"id\":\"{{calendar_id}}\",[^}]*\"access\":\"own\""


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
body contains "\"id\":\"{{event_id}}\""


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
body contains "\"recurrence_id\":\"2026-12-22T09:00:00Z\""
body contains "\"summary\":\"Weekly sync (moved)\""


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
# Step 7b – Clearing the RRULE means the series is gone. The override
# created in Step 6b (RECURRENCE-ID Dec 22, moved to February) must not
# survive as a standalone event: the SPA renders a masterless override
# as its own calendar entry. The Dec window is empty once the rule is
# cleared.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/calendars/{{calendar_id}}/events?start=2026-12-15T00:00:00Z&end=2026-12-31T00:00:00Z
Authorization: Bearer {{token}}

HTTP 200
[Asserts]
jsonpath "$" count == 0


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
# Step 8b – A user with NO grant on the calendar cannot see or
# edit it through the SPA JSON surface.
#
# The CalDAV denials are pinned in dav_error_mapping.hurl and
# calendar.hurl; these endpoints are a SEPARATE handler stack
# (calendar_handler.rs), and every one of them must reach the
# same CalendarService gate. A gate that lives only in the DAV
# path would leave the SPA API as an unguarded second door.
#
# Refusals must be the exact 404 anti-enum shape, never 403:
# Eve holds no Read, so confirming that the calendar exists
# would defeat the policy. `error_type` is the PascalCase wire
# discriminator (`ErrorKind::NotFound` → `wire_name()` = "NotFound")
# because the REST handlers map domain errors through `AppError::from`
# and `ApiErrorBody` serializes `wire_name()`, not the Display form.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/admin/users
Authorization: Bearer {{token}}
Content-Type: application/json
{
  "username": "rest_eve",
  "password": "RestEvePassword1!",
  "email":    "rest_eve@example.com",
  "role":     "user"
}

HTTP *


POST {{base_url}}/api/auth/login
Content-Type: application/json
{
  "username": "rest_eve",
  "password": "RestEvePassword1!"
}

HTTP 200
[Captures]
eve_token: jsonpath "$.access_token"
eve_id: jsonpath "$.user.full.user.id"


# Visibility: Eve's calendar list contains her own defaults only.
GET {{base_url}}/api/calendars
Authorization: Bearer {{eve_token}}

HTTP 200
[Asserts]
body not contains "{{calendar_id}}"


GET {{base_url}}/api/calendars/{{calendar_id}}/events?start=2026-12-01T00:00:00Z&end=2027-01-01T00:00:00Z
Authorization: Bearer {{eve_token}}

HTTP 404
[Asserts]
jsonpath "$.error_type" == "NotFound"


POST {{base_url}}/api/calendars/{{calendar_id}}/events
Authorization: Bearer {{eve_token}}
Content-Type: application/json
{
  "summary": "Eve intruder",
  "start_time": "2026-12-03T09:00:00Z",
  "end_time": "2026-12-03T10:00:00Z"
}

HTTP 404
[Asserts]
jsonpath "$.error_type" == "NotFound"


PUT {{base_url}}/api/calendars/{{calendar_id}}
Authorization: Bearer {{eve_token}}
Content-Type: application/json
{
  "name": "eve-hijacked"
}

HTTP 404
[Asserts]
jsonpath "$.error_type" == "NotFound"


DELETE {{base_url}}/api/calendars/{{calendar_id}}
Authorization: Bearer {{eve_token}}

HTTP 404
[Asserts]
jsonpath "$.error_type" == "NotFound"


PUT {{base_url}}/api/calendars/{{calendar_id}}/events/{{updated_event_id}}
Authorization: Bearer {{eve_token}}
Content-Type: application/json
{
  "summary": "Eve overwrote this",
  "start_time": "2026-12-03T09:00:00Z",
  "end_time": "2026-12-03T10:00:00Z"
}

HTTP 404
[Asserts]
jsonpath "$.error_type" == "NotFound"


DELETE {{base_url}}/api/calendars/{{calendar_id}}/events/{{updated_event_id}}
Authorization: Bearer {{eve_token}}

HTTP 404
[Asserts]
jsonpath "$.error_type" == "NotFound"


# The load-bearing check: every refusal above must have happened
# BEFORE any write. Re-reading as the owner proves the event
# survived with its summary intact and that Eve's attempt never
# produced an "Eve intruder" row.
GET {{base_url}}/api/calendars/{{calendar_id}}/events?start=2026-12-01T00:00:00Z&end=2027-01-01T00:00:00Z
Authorization: Bearer {{token}}

HTTP 200
[Asserts]
body contains "\"id\":\"{{updated_event_id}}\""
body contains "\"summary\":\"Weekly sync (renamed)\""
body not contains "Eve intruder"
body not contains "Eve overwrote this"


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
# Step 9c – The SPA edit path must preserve CalDAV-only properties.
# A sync client's event carries ATTENDEE, VALARM, CATEGORIES, EXDATE
# and X-* that the JSON DTO does not model. Renaming it through the
# REST API used to rebuild a minimal VEVENT and drop all of them.
# Create a rich event, edit it via REST, then read it back through
# CalDAV (which serves the stored blob verbatim).
# ─────────────────────────────────────────────────────────────
PUT {{base_url}}/caldav/{{calendar_id}}/rich-update-guard.ics
Authorization: Bearer {{token}}
Content-Type: text/calendar; charset=utf-8
```
BEGIN:VCALENDAR
VERSION:2.0
PRODID:-//OxiCloud rich update test//EN
BEGIN:VEVENT
UID:rich-update-guard
DTSTAMP:20320501T120000Z
DTSTART;TZID=Europe/Paris:20320504T110000
DTEND;TZID=Europe/Paris:20320504T120000
SUMMARY:Rich original
DESCRIPTION:Original description
LOCATION:Room 9
RRULE:FREQ=WEEKLY;COUNT=5
EXDATE:20320511T090000Z
CATEGORIES:Work,Important
ATTENDEE;CN=Alice;ROLE=REQ-PARTICIPANT:mailto:alice@example.com
X-OXICLOUD-TEST:keep-me
BEGIN:VALARM
ACTION:DISPLAY
DESCRIPTION:Alarm text must survive
TRIGGER:-PT15M
END:VALARM
END:VEVENT
END:VCALENDAR
```

HTTP 201


GET {{base_url}}/api/calendars/{{calendar_id}}/events?start=2032-05-01T00:00:00Z&end=2032-06-01T00:00:00Z
Authorization: Bearer {{token}}

HTTP 200
[Captures]
rich_event_id: jsonpath "$[0].id"
[Asserts]
jsonpath "$" count == 1
jsonpath "$[0].ical_uid" == "rich-update-guard"


PUT {{base_url}}/api/calendars/{{calendar_id}}/events/{{rich_event_id}}
Authorization: Bearer {{token}}
Content-Type: application/json
{
  "summary": "Rich updated",
  "description": "Updated description",
  "location": null,
  "start_time": "2032-05-04T09:00:00Z",
  "end_time": "2032-05-04T10:00:00Z",
  "all_day": false,
  "rrule": "FREQ=WEEKLY;COUNT=5"
}

HTTP 200
[Captures]
rich_updated_id: jsonpath "$.id"
[Asserts]
jsonpath "$.summary" == "Rich updated"
jsonpath "$.ical_uid" == "rich-update-guard"


GET {{base_url}}/caldav/{{calendar_id}}/rich-update-guard.ics
Authorization: Bearer {{token}}

HTTP 200
[Asserts]
body contains "SUMMARY:Rich updated"
body contains "ATTENDEE;CN=Alice"
body contains "mailto:alice@example.com"
body contains "CATEGORIES:Work,Important"
body contains "EXDATE:20320511T090000Z"
body contains "X-OXICLOUD-TEST:keep-me"
body contains "BEGIN:VALARM"
body contains "DESCRIPTION:Alarm text must survive"
# The REST payload repeats the same instants (09:00Z/10:00Z), so the
# stored TZID-qualified value form must survive instead of being
# flattened to UTC by the rename.
body contains "DTSTART;TZID=Europe/Paris:20320504T110000"
body contains "DTEND;TZID=Europe/Paris:20320504T120000"
body not contains "SUMMARY:Rich original"


# Clearing the recurrence must drop the now-meaningless EXDATE while
# keeping the properties the JSON surface does not own.
PUT {{base_url}}/api/calendars/{{calendar_id}}/events/{{rich_updated_id}}
Authorization: Bearer {{token}}
Content-Type: application/json
{
  "summary": "Rich updated",
  "description": "Updated description",
  "location": null,
  "start_time": "2032-05-04T09:00:00Z",
  "end_time": "2032-05-04T10:00:00Z",
  "all_day": false,
  "rrule": null
}

HTTP 200
[Captures]
rich_single_id: jsonpath "$.id"


GET {{base_url}}/caldav/{{calendar_id}}/rich-update-guard.ics
Authorization: Bearer {{token}}

HTTP 200
[Asserts]
body contains "SUMMARY:Rich updated"
body contains "ATTENDEE;CN=Alice"
body contains "BEGIN:VALARM"
body not contains "RRULE"
body not contains "EXDATE"


DELETE {{base_url}}/api/calendars/{{calendar_id}}/events/{{rich_single_id}}
Authorization: Bearer {{token}}

HTTP 204


# ─────────────────────────────────────────────────────────────
# Step 9d – Deleting a recurring master deletes its overrides too.
# An override with a RECURRENCE-ID is not an addressable CalDAV
# resource, so a leftover row after deleting the master becomes a
# phantom standalone event in the SPA.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/calendars/{{calendar_id}}/events
Authorization: Bearer {{token}}
Content-Type: application/json
{
  "summary": "Series to delete",
  "start_time": "2031-01-06T09:00:00Z",
  "end_time": "2031-01-06T10:00:00Z",
  "rrule": "FREQ=WEEKLY;COUNT=4"
}

HTTP 201
[Captures]
series_id: jsonpath "$.id"
series_uid: jsonpath "$.ical_uid"


# Add one override via CalDAV. The master row id (`series_id`) is
# unchanged because this PUT body contains only the exception.
PUT {{base_url}}/caldav/{{calendar_id}}/{{series_uid}}.ics
Authorization: Bearer {{token}}
Content-Type: text/calendar; charset=utf-8
```
BEGIN:VCALENDAR
VERSION:2.0
PRODID:-//OxiCloud series delete test//EN
BEGIN:VEVENT
UID:{{series_uid}}
DTSTAMP:20310106T120000Z
DTSTART:20310114T090000Z
DTEND:20310114T100000Z
SUMMARY:Series moved
RECURRENCE-ID:20310113T090000Z
END:VEVENT
END:VCALENDAR
```

HTTP 201


GET {{base_url}}/api/calendars/{{calendar_id}}/events?start=2031-01-01T00:00:00Z&end=2031-02-01T00:00:00Z
Authorization: Bearer {{token}}

HTTP 200
[Asserts]
jsonpath "$" count == 2


DELETE {{base_url}}/api/calendars/{{calendar_id}}/events/{{series_id}}
Authorization: Bearer {{token}}

HTTP 204


GET {{base_url}}/api/calendars/{{calendar_id}}/events?start=2031-01-01T00:00:00Z&end=2031-02-01T00:00:00Z
Authorization: Bearer {{token}}

HTTP 200
[Asserts]
jsonpath "$" count == 0


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
body not contains "{{calendar_id}}"
body not contains "{{other_calendar_id}}"


# Drop the throwaway user so downstream scenarios don't inherit it
# (the suite shares one database for the whole run).
DELETE {{base_url}}/api/admin/users/{{eve_id}}
Authorization: Bearer {{token}}

HTTP *
[Asserts]
status < 500
