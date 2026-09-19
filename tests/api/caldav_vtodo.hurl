# =============================================================
# OxiCloud — CalDAV VTODO (tasks) support regression (#754)
# =============================================================
# Regression pin for AtalayaLabs/OxiCloud#754: the CalDAV surface
# only accepted VEVENT. Tasks clients (DAVx⁵ + Tasks.org / jtx
# Board, Thunderbird, Apple Reminders) sync VTODO components over
# the same collections; a VTODO PUT previously failed with
# HTTP 400 ("No VEVENT components found").
#
# Post-fix:
#   * PUT of VTODO components persists to caldav.calendar_todos
#     (master/exception model mirrors events; SUMMARY/DTSTART/DUE
#     optional per RFC 5545 §3.6.2).
#   * PROPFIND advertises VEVENT + VTODO in
#     supported-calendar-component-set (tasks clients only enable
#     task sync when VTODO is listed).
#   * GET / REPORT (calendar-query with comp-filter + time-range,
#     calendar-multiget) / DELETE cover tasks; every property —
#     PRIORITY, STATUS, PERCENT-COMPLETE, CATEGORIES, … — round-trips
#     byte-exact because the stored ical_data blob is served verbatim
#     (columns are the filter index only).
# =============================================================


# ─────────────────────────────────────────────────────────────
# Step 1 — Admin login.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "{{username}}", "password": "{{password}}" }

HTTP 200
[Captures]
admin_token: jsonpath "$.access_token"


# ─────────────────────────────────────────────────────────────
# Step 2 — Provision a fresh calendar (`vt-cal`).
# ─────────────────────────────────────────────────────────────
MKCALENDAR {{base_url}}/caldav/vt-cal/
Authorization: Bearer {{admin_token}}

HTTP 201


# ─────────────────────────────────────────────────────────────
# Step 3 — Discover the server-assigned UUID via PROPFIND.
# ─────────────────────────────────────────────────────────────
PROPFIND {{base_url}}/caldav/
Authorization: Bearer {{admin_token}}
Depth: 1
Content-Type: application/xml
```
<?xml version="1.0" encoding="UTF-8"?>
<D:propfind xmlns:D="DAV:">
  <D:prop><D:displayname/><D:resourcetype/></D:prop>
</D:propfind>
```

HTTP 207
[Captures]
vt_cal_id: body regex "(?s).*/caldav/([a-f0-9]{8}-[a-f0-9]{4}-[a-f0-9]{4}-[a-f0-9]{4}-[a-f0-9]{12})/"
[Asserts]
body contains "vt-cal"


# ─────────────────────────────────────────────────────────────
# Step 4 — PROPFIND the calendar itself: the advertised
# supported-calendar-component-set must list VEVENT AND VTODO
# (tasks clients gate task sync on this).
# ─────────────────────────────────────────────────────────────
PROPFIND {{base_url}}/caldav/{{vt_cal_id}}/
Authorization: Bearer {{admin_token}}
Depth: 0
Content-Type: application/xml
```
<?xml version="1.0" encoding="UTF-8"?>
<D:propfind xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav">
  <D:prop><C:supported-calendar-component-set/></D:prop>
</D:propfind>
```

HTTP 207
[Asserts]
body contains "supported-calendar-component-set"
body contains "VEVENT"
body contains "VTODO"


# ─────────────────────────────────────────────────────────────
# Step 5 — PUT a VTODO with the usual task properties. Pre-fix:
# HTTP 400. Post-fix: 201.
# ─────────────────────────────────────────────────────────────
PUT {{base_url}}/caldav/{{vt_cal_id}}/vt-milk.ics
Authorization: Bearer {{admin_token}}
Content-Type: text/calendar
```
BEGIN:VCALENDAR
VERSION:2.0
PRODID:-//VTODO test//EN
BEGIN:VTODO
UID:vt-milk
DTSTAMP:20260919T100000Z
SUMMARY:Buy milk
DESCRIPTION:2% or oat
STATUS:IN-PROCESS
PERCENT-COMPLETE:40
PRIORITY:1
DTSTART:20260920T090000Z
DUE:20260925T180000Z
CATEGORIES:ERRANDS,HOME
END:VTODO
END:VCALENDAR
```

HTTP 201


# ─────────────────────────────────────────────────────────────
# Step 6 — PUT a minimal VTODO (RFC 5545 §3.6.2 mandates only
# UID + DTSTAMP). Must not 400.
# ─────────────────────────────────────────────────────────────
PUT {{base_url}}/caldav/{{vt_cal_id}}/vt-minimal.ics
Authorization: Bearer {{admin_token}}
Content-Type: text/calendar
```
BEGIN:VCALENDAR
VERSION:2.0
PRODID:-//VTODO test//EN
BEGIN:VTODO
UID:vt-minimal
DTSTAMP:20260919T100000Z
END:VTODO
END:VCALENDAR
```

HTTP 201


# ─────────────────────────────────────────────────────────────
# Step 7 — Seed an event too, so the comp-filter routing below
# has something to exclude.
# ─────────────────────────────────────────────────────────────
PUT {{base_url}}/caldav/{{vt_cal_id}}/vt-event.ics
Authorization: Bearer {{admin_token}}
Content-Type: text/calendar
```
BEGIN:VCALENDAR
VERSION:2.0
PRODID:-//VTODO test//EN
BEGIN:VEVENT
UID:vt-event
DTSTAMP:20260919T100000Z
DTSTART:20260921T100000Z
DTEND:20260921T110000Z
SUMMARY:Not a task
END:VEVENT
END:VCALENDAR
```

HTTP 201


# ─────────────────────────────────────────────────────────────
# Step 8 — GET the task back: every property must round-trip
# byte-exact (PRIORITY, CATEGORIES — blob served verbatim).
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/caldav/{{vt_cal_id}}/vt-milk.ics
Authorization: Bearer {{admin_token}}

HTTP 200
[Asserts]
body contains "BEGIN:VTODO"
body contains "UID:vt-milk"
body contains "SUMMARY:Buy milk"
body contains "STATUS:IN-PROCESS"
body contains "PERCENT-COMPLETE:40"
body contains "PRIORITY:1"
body contains "DUE:20260925T180000Z"
body contains "CATEGORIES:ERRANDS,HOME"


# ─────────────────────────────────────────────────────────────
# Step 9 — calendar-query with comp-filter VTODO (no time-range):
# tasks come back, the event does NOT.
# ─────────────────────────────────────────────────────────────
REPORT {{base_url}}/caldav/{{vt_cal_id}}/
Authorization: Bearer {{admin_token}}
Content-Type: application/xml
Depth: 1
```
<?xml version="1.0" encoding="UTF-8"?>
<C:calendar-query xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav">
  <D:prop>
    <D:getetag/>
    <C:calendar-data/>
  </D:prop>
  <C:filter>
    <C:comp-filter name="VCALENDAR">
      <C:comp-filter name="VTODO"/>
    </C:comp-filter>
  </C:filter>
</C:calendar-query>
```

HTTP 207
[Asserts]
body contains "vt-milk"
body contains "vt-minimal"
body not contains "vt-event"


# ─────────────────────────────────────────────────────────────
# Step 10 — calendar-query with comp-filter VEVENT: the event
# comes back, the tasks do NOT.
# ─────────────────────────────────────────────────────────────
REPORT {{base_url}}/caldav/{{vt_cal_id}}/
Authorization: Bearer {{admin_token}}
Content-Type: application/xml
Depth: 1
```
<?xml version="1.0" encoding="UTF-8"?>
<C:calendar-query xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav">
  <D:prop>
    <D:getetag/>
    <C:calendar-data/>
  </D:prop>
  <C:filter>
    <C:comp-filter name="VCALENDAR">
      <C:comp-filter name="VEVENT"/>
    </C:comp-filter>
  </C:filter>
</C:calendar-query>
```

HTTP 207
[Asserts]
body contains "vt-event"
body not contains "vt-milk"


# ─────────────────────────────────────────────────────────────
# Step 11 — comp-filter-less query: BOTH kinds come back
# (RFC 4791 §9.7 — no component restriction).
# ─────────────────────────────────────────────────────────────
REPORT {{base_url}}/caldav/{{vt_cal_id}}/
Authorization: Bearer {{admin_token}}
Content-Type: application/xml
Depth: 1
```
<?xml version="1.0" encoding="UTF-8"?>
<C:calendar-query xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav">
  <D:prop>
    <D:getetag/>
    <C:calendar-data/>
  </D:prop>
</C:calendar-query>
```

HTTP 207
[Asserts]
body contains "vt-event"
body contains "vt-milk"


# ─────────────────────────────────────────────────────────────
# Step 12 — VTODO time-range query: window covering the DUE
# instant (2026-09-25) contains the task; a disjoint window
# (2026-10) does not. RFC 4791 §9.9 VTODO rules.
# ─────────────────────────────────────────────────────────────
REPORT {{base_url}}/caldav/{{vt_cal_id}}/
Authorization: Bearer {{admin_token}}
Content-Type: application/xml
Depth: 1
```
<?xml version="1.0" encoding="UTF-8"?>
<C:calendar-query xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav">
  <D:prop>
    <D:getetag/>
    <C:calendar-data/>
  </D:prop>
  <C:filter>
    <C:comp-filter name="VCALENDAR">
      <C:comp-filter name="VTODO">
        <C:time-range start="20260925T000000Z" end="20260926T000000Z"/>
      </C:comp-filter>
    </C:comp-filter>
  </C:filter>
</C:calendar-query>
```

HTTP 207
[Asserts]
body contains "vt-milk"


REPORT {{base_url}}/caldav/{{vt_cal_id}}/
Authorization: Bearer {{admin_token}}
Content-Type: application/xml
Depth: 1
```
<?xml version="1.0" encoding="UTF-8"?>
<C:calendar-query xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav">
  <D:prop>
    <D:getetag/>
    <C:calendar-data/>
  </D:prop>
  <C:filter>
    <C:comp-filter name="VCALENDAR">
      <C:comp-filter name="VTODO">
        <C:time-range start="20261001T000000Z" end="20261002T000000Z"/>
      </C:comp-filter>
    </C:comp-filter>
  </C:filter>
</C:calendar-query>
```

HTTP 207
[Asserts]
body not contains "vt-milk"


# ─────────────────────────────────────────────────────────────
# Step 13 — calendar-multiget on the task's href.
# ─────────────────────────────────────────────────────────────
REPORT {{base_url}}/caldav/{{vt_cal_id}}/
Authorization: Bearer {{admin_token}}
Content-Type: application/xml
Depth: 1
```
<?xml version="1.0" encoding="UTF-8"?>
<C:calendar-multiget xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav">
  <D:prop>
    <D:getetag/>
    <C:calendar-data/>
  </D:prop>
  <D:href>/caldav/{{vt_cal_id}}/vt-milk.ics</D:href>
</C:calendar-multiget>
```

HTTP 207
[Asserts]
body contains "vt-milk"
body contains "PRIORITY:1"


# ─────────────────────────────────────────────────────────────
# Step 14 — Update the task (same UID): mark COMPLETED. 204 on
# replace; GET shows the new properties.
# ─────────────────────────────────────────────────────────────
PUT {{base_url}}/caldav/{{vt_cal_id}}/vt-milk.ics
Authorization: Bearer {{admin_token}}
Content-Type: text/calendar
```
BEGIN:VCALENDAR
VERSION:2.0
PRODID:-//VTODO test//EN
BEGIN:VTODO
UID:vt-milk
DTSTAMP:20260919T110000Z
SUMMARY:Buy milk
STATUS:COMPLETED
PERCENT-COMPLETE:100
PRIORITY:1
DUE:20260925T180000Z
COMPLETED:20260919T120000Z
END:VTODO
END:VCALENDAR
```

HTTP 204


GET {{base_url}}/caldav/{{vt_cal_id}}/vt-milk.ics
Authorization: Bearer {{admin_token}}

HTTP 200
[Asserts]
body contains "STATUS:COMPLETED"
body contains "PERCENT-COMPLETE:100"
body contains "COMPLETED:20260919T120000Z"


# ─────────────────────────────────────────────────────────────
# Step 15 — depth-1 collection PROPFIND lists tasks as members
# (DAVx⁵ sync discovery).
# ─────────────────────────────────────────────────────────────
PROPFIND {{base_url}}/caldav/{{vt_cal_id}}/
Authorization: Bearer {{admin_token}}
Depth: 1
Content-Type: application/xml
```
<?xml version="1.0" encoding="UTF-8"?>
<D:propfind xmlns:D="DAV:">
  <D:prop><D:getetag/><D:resourcetype/></D:prop>
</D:propfind>
```

HTTP 207
[Asserts]
body contains "vt-milk.ics"
body contains "vt-minimal.ics"
body contains "vt-event.ics"


# ─────────────────────────────────────────────────────────────
# Step 16 — DELETE the task resource; subsequent GET is a 404.
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/caldav/{{vt_cal_id}}/vt-milk.ics
Authorization: Bearer {{admin_token}}

HTTP 204


GET {{base_url}}/caldav/{{vt_cal_id}}/vt-milk.ics
Authorization: Bearer {{admin_token}}

HTTP 404


# ─────────────────────────────────────────────────────────────
# Step 17 — Cleanup: delete the calendar so downstream test
# files don't inherit an extra collection.
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/caldav/{{vt_cal_id}}/
Authorization: Bearer {{admin_token}}

HTTP 204
