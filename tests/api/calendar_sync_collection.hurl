# =============================================================
# OxiCloud — CalDAV sync-collection (RFC 6578)
# =============================================================
# Real incremental sync for calendar events, backed by
# `caldav.calendar_sync_changes` (see
# migrations/20260911000001_calendar_sync_changes.sql,
# CalendarService::list_event_changes_with_perms,
# caldav_handler.rs::handle_report). Mirrors
# webdav_sync_collection.hurl's coverage.
#
# NOTE: CalDAV PUT-based "update" of an existing event is a
# delete+recreate at the application layer (see
# caldav_handler.rs::handle_put), not a SQL UPDATE — so it never
# exercises the change-log's `updated` branch. This test only
# covers create/delete, which is what the public CalDAV surface
# actually produces.
#
# Coverage:
#   1. Initial sync-collection (empty sync-token) → 207, the seeded
#      event present, a sync-token is returned.
#   2. sync-collection with that token and NO intervening changes →
#      empty delta.
#   3. Create a second event, then sync-collection with the prior
#      token → only the new event appears.
#   4. Delete that second event, then sync-collection with the
#      just-issued token → an RFC 6578 §3.7 404 sub-response for it.
#   5. sync-collection with a malformed sync-token → 400.
# =============================================================


# ─────────────────────────────────────────────────────────────
# Step 1 — Login, create a fresh calendar.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "{{username}}", "password": "{{password}}" }

HTTP 200
[Captures]
token: jsonpath "$.access_token"


MKCALENDAR {{base_url}}/caldav/sync-test-cal/
Authorization: Bearer {{token}}

HTTP 201


PROPFIND {{base_url}}/caldav/
Authorization: Bearer {{token}}
Depth: 1
Content-Type: application/xml
```
<?xml version="1.0" encoding="UTF-8"?>
<D:propfind xmlns:D="DAV:">
  <D:prop>
    <D:displayname/>
    <D:resourcetype/>
  </D:prop>
</D:propfind>
```

HTTP 207
[Captures]
calendar_id: body regex "/caldav/([a-f0-9]{8}-[a-f0-9]{4}-[a-f0-9]{4}-[a-f0-9]{4}-[a-f0-9]{12})/"


# ─────────────────────────────────────────────────────────────
# Step 2 — Seed one event.
# ─────────────────────────────────────────────────────────────
PUT {{base_url}}/caldav/{{calendar_id}}/sync-evt-1.ics
Authorization: Bearer {{token}}
Content-Type: text/calendar
```
BEGIN:VCALENDAR
VERSION:2.0
PRODID:-//OxiCloud Test//EN
BEGIN:VEVENT
UID:sync-evt-1
SUMMARY:Sync Event One
DTSTART:20261201T100000Z
DTEND:20261201T110000Z
END:VEVENT
END:VCALENDAR
```

HTTP 201


# ─────────────────────────────────────────────────────────────
# Step 3 — Initial sync-collection (empty sync-token).
# ─────────────────────────────────────────────────────────────
REPORT {{base_url}}/caldav/{{calendar_id}}/
Authorization: Bearer {{token}}
Content-Type: application/xml; charset=utf-8
```
<?xml version="1.0" encoding="utf-8"?>
<D:sync-collection xmlns:D="DAV:">
  <D:sync-token/>
  <D:prop>
    <D:getetag/>
  </D:prop>
</D:sync-collection>
```

HTTP 207
[Asserts]
body contains "sync-evt-1"
xpath "string(//*[local-name()='sync-token'])" != ""
[Captures]
first_sync_token: xpath "string(//*[local-name()='sync-token'])"


# ─────────────────────────────────────────────────────────────
# Step 4 — sync-collection with that token and NO intervening
#          changes → empty delta.
# ─────────────────────────────────────────────────────────────
REPORT {{base_url}}/caldav/{{calendar_id}}/
Authorization: Bearer {{token}}
Content-Type: application/xml; charset=utf-8
```
<?xml version="1.0" encoding="utf-8"?>
<D:sync-collection xmlns:D="DAV:">
  <D:sync-token>{{first_sync_token}}</D:sync-token>
  <D:prop>
    <D:getetag/>
  </D:prop>
</D:sync-collection>
```

HTTP 207
[Asserts]
body not contains "sync-evt-1"
xpath "string(//*[local-name()='sync-token'])" != ""


# ─────────────────────────────────────────────────────────────
# Step 5 — Create a second event, then sync-collection with the
#          prior token → only the new event appears.
# ─────────────────────────────────────────────────────────────
PUT {{base_url}}/caldav/{{calendar_id}}/sync-evt-2.ics
Authorization: Bearer {{token}}
Content-Type: text/calendar
```
BEGIN:VCALENDAR
VERSION:2.0
PRODID:-//OxiCloud Test//EN
BEGIN:VEVENT
UID:sync-evt-2
SUMMARY:Sync Event Two
DTSTART:20261202T100000Z
DTEND:20261202T110000Z
END:VEVENT
END:VCALENDAR
```

HTTP 201


REPORT {{base_url}}/caldav/{{calendar_id}}/
Authorization: Bearer {{token}}
Content-Type: application/xml; charset=utf-8
```
<?xml version="1.0" encoding="utf-8"?>
<D:sync-collection xmlns:D="DAV:">
  <D:sync-token>{{first_sync_token}}</D:sync-token>
  <D:prop>
    <D:getetag/>
  </D:prop>
</D:sync-collection>
```

HTTP 207
[Asserts]
body contains "sync-evt-2"
body not contains "sync-evt-1"
[Captures]
second_sync_token: xpath "string(//*[local-name()='sync-token'])"


# ─────────────────────────────────────────────────────────────
# Step 6 — Delete that second event, then sync-collection with
#          the just-issued token → RFC 6578 §3.7 404 sub-response.
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/caldav/{{calendar_id}}/sync-evt-2.ics
Authorization: Bearer {{token}}

HTTP 204


REPORT {{base_url}}/caldav/{{calendar_id}}/
Authorization: Bearer {{token}}
Content-Type: application/xml; charset=utf-8
```
<?xml version="1.0" encoding="utf-8"?>
<D:sync-collection xmlns:D="DAV:">
  <D:sync-token>{{second_sync_token}}</D:sync-token>
  <D:prop>
    <D:getetag/>
  </D:prop>
</D:sync-collection>
```

HTTP 207
[Asserts]
body contains "sync-evt-2"
xpath "string(//*[local-name()='response'][contains(., 'sync-evt-2')]/*[local-name()='status'])" contains "404"


# ─────────────────────────────────────────────────────────────
# Step 7 — Malformed sync-token → 400.
# ─────────────────────────────────────────────────────────────
REPORT {{base_url}}/caldav/{{calendar_id}}/
Authorization: Bearer {{token}}
Content-Type: application/xml; charset=utf-8
```
<?xml version="1.0" encoding="utf-8"?>
<D:sync-collection xmlns:D="DAV:">
  <D:sync-token>not-a-real-token</D:sync-token>
  <D:prop><D:getetag/></D:prop>
</D:sync-collection>
```

HTTP 400


# ─────────────────────────────────────────────────────────────
# Cleanup
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/caldav/{{calendar_id}}/
Authorization: Bearer {{token}}

HTTP 204
