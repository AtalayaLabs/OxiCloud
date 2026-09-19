# =============================================================
# OxiCloud — CalDAV TZID support regression (#689)
# =============================================================
# Regression pin for AtalayaLabs/OxiCloud#689: CalDAV PUT rejected
# valid `DTSTART`/`DTEND` values carrying a `TZID` parameter (the
# shape DAVx⁵ produces for Europe/Paris events) with HTTP 400
# "Invalid DTSTART: Invalid datetime format: expected
# YYYYMMDDTHHMMSSZ".
#
# Post-fix:
#   1. `TZID`-anchored wall times are accepted and converted through
#      the IANA tz database (chrono-tz) so the indexed columns hold
#      the correct UTC instant — time-range queries hit the right
#      window. Paris in August is CEST = UTC+2, so 12:00 local is
#      10:00 UTC.
#   2. VTIMEZONE blocks (RRULEs and all) embedded in a PUT body are
#      preserved into the stored row and served back byte-exact on
#      GET — DST evaluation stays client-side, the server only has to
#      store and return the definition faithfully.
#
# Test shape:
#   * PUT the issue's exact reproducer → 201 (was 400).
#   * calendar-query on the correct UTC window contains the event;
#     the wall-clock-as-UTC window (pre-fix behaviour) does NOT.
#   * PUT an Auckland-anchored event with a full VTIMEZONE → 201;
#     GET the object back → VTIMEZONE + RRULEs present byte-exact.
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
# Step 2 — Provision a fresh calendar (`tz-cal`).
# ─────────────────────────────────────────────────────────────
MKCALENDAR {{base_url}}/caldav/tz-cal/
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
tz_cal_id: body regex "(?s).*/caldav/([a-f0-9]{8}-[a-f0-9]{4}-[a-f0-9]{4}-[a-f0-9]{4}-[a-f0-9]{12})/"
[Asserts]
body contains "tz-cal"


# ─────────────────────────────────────────────────────────────
# Step 4 — PUT the exact #689 reproducer: Europe/Paris wall times
# (DAVx⁵ shape). Pre-fix: HTTP 400. Post-fix: 201, and the indexed
# start_time is 2026-08-24T10:00:00Z (CEST = UTC+2).
# ─────────────────────────────────────────────────────────────
PUT {{base_url}}/caldav/{{tz_cal_id}}/tz-paris.ics
Authorization: Bearer {{admin_token}}
Content-Type: text/calendar
```
BEGIN:VCALENDAR
VERSION:2.0
PRODID:-//TZID test//EN
BEGIN:VEVENT
UID:tz-paris
DTSTAMP:20260824T094500Z
DTSTART;TZID=Europe/Paris:20260824T120000
DTEND;TZID=Europe/Paris:20260824T130000
SUMMARY:TZID test
END:VEVENT
END:VCALENDAR
```

HTTP 201


# ─────────────────────────────────────────────────────────────
# Step 5 — calendar-query on the CORRECT UTC window (10:00–11:00Z).
# The event must be found — proves the TZID was resolved through
# the tz database instead of rejected or mis-indexed.
# ─────────────────────────────────────────────────────────────
REPORT {{base_url}}/caldav/{{tz_cal_id}}/
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
      <C:comp-filter name="VEVENT">
        <C:time-range start="20260824T100000Z" end="20260824T110000Z"/>
      </C:comp-filter>
    </C:comp-filter>
  </C:filter>
</C:calendar-query>
```

HTTP 207
[Asserts]
body contains "tz-paris"
# The TZID form round-trips verbatim in calendar-data.
body contains "DTSTART;TZID=Europe/Paris:20260824T120000"


# ─────────────────────────────────────────────────────────────
# Step 6 — calendar-query on the WALL-CLOCK-as-UTC window
# (12:00–13:00Z) must return NOTHING. This is where the event
# would have landed had the server kept the pre-fix "floating =
# UTC" behaviour for TZID-anchored values.
# ─────────────────────────────────────────────────────────────
REPORT {{base_url}}/caldav/{{tz_cal_id}}/
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
      <C:comp-filter name="VEVENT">
        <C:time-range start="20260824T120000Z" end="20260824T130000Z"/>
      </C:comp-filter>
    </C:comp-filter>
  </C:filter>
</C:calendar-query>
```

HTTP 207
[Asserts]
body not contains "tz-paris"


# ─────────────────────────────────────────────────────────────
# Step 7 — PUT an Auckland-anchored event carrying its full
# VTIMEZONE definition (STANDARD + DAYLIGHT observances with
# yearly RRULEs). Auckland in January is NZDT = UTC+13, so
# 12:00 local on 2026-01-15 is 23:00 UTC on 2026-01-14.
# ─────────────────────────────────────────────────────────────
PUT {{base_url}}/caldav/{{tz_cal_id}}/tz-auckland.ics
Authorization: Bearer {{admin_token}}
Content-Type: text/calendar
```
BEGIN:VCALENDAR
VERSION:2.0
PRODID:-//TZID test//EN
BEGIN:VTIMEZONE
TZID:Pacific/Auckland
BEGIN:DAYLIGHT
TZNAME:NZDT
TZOFFSETFROM:+1200
TZOFFSETTO:+1300
DTSTART:19700927T020000
RRULE:FREQ=YEARLY;BYMONTH=9;BYDAY=-1SU
END:DAYLIGHT
BEGIN:STANDARD
TZNAME:NZST
TZOFFSETFROM:+1300
TZOFFSETTO:+1200
DTSTART:19700405T030000
RRULE:FREQ=YEARLY;BYMONTH=4;BYDAY=1SU
END:STANDARD
END:VTIMEZONE
BEGIN:VEVENT
UID:tz-auckland
DTSTAMP:20260101T100000Z
DTSTART;TZID=Pacific/Auckland:20260115T120000
DTEND;TZID=Pacific/Auckland:20260115T130000
SUMMARY:Auckland meeting
END:VEVENT
END:VCALENDAR
```

HTTP 201


# ─────────────────────────────────────────────────────────────
# Step 8 — GET the Auckland object back: the VTIMEZONE block,
# both RRULEs and the TZID-anchored DTSTART must come back
# byte-exact (DST evaluation is the client's job; the server only
# has to return the definition faithfully).
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/caldav/{{tz_cal_id}}/tz-auckland.ics
Authorization: Bearer {{admin_token}}

HTTP 200
[Asserts]
body contains "BEGIN:VTIMEZONE"
body contains "TZID:Pacific/Auckland"
body contains "RRULE:FREQ=YEARLY;BYMONTH=9;BYDAY=-1SU"
body contains "RRULE:FREQ=YEARLY;BYMONTH=4;BYDAY=1SU"
body contains "TZOFFSETTO:+1300"
body contains "DTSTART;TZID=Pacific/Auckland:20260115T120000"


# ─────────────────────────────────────────────────────────────
# Step 9 — calendar-query on the correct UTC window for the
# Auckland event (2026-01-14 23:00Z → 2026-01-15 00:00Z).
# ─────────────────────────────────────────────────────────────
REPORT {{base_url}}/caldav/{{tz_cal_id}}/
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
      <C:comp-filter name="VEVENT">
        <C:time-range start="20260114T230000Z" end="20260115T000000Z"/>
      </C:comp-filter>
    </C:comp-filter>
  </C:filter>
</C:calendar-query>
```

HTTP 207
[Asserts]
body contains "tz-auckland"
body not contains "tz-paris"


# ─────────────────────────────────────────────────────────────
# Step 10 — Whole-calendar GET: the streamed VCALENDAR must carry
# the VTIMEZONE definition alongside both events.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/caldav/{{tz_cal_id}}/
Authorization: Bearer {{admin_token}}

HTTP 200
[Asserts]
body contains "BEGIN:VTIMEZONE"
body contains "RRULE:FREQ=YEARLY;BYMONTH=9;BYDAY=-1SU"
body contains "tz-auckland"
body contains "tz-paris"


# ─────────────────────────────────────────────────────────────
# Step 11 — Cleanup: delete the calendar so downstream test files
# don't inherit an extra collection.
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/caldav/{{tz_cal_id}}/
Authorization: Bearer {{admin_token}}

HTTP 204
