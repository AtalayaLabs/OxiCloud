# =============================================================
# OxiCloud – CalDAV role mapping on shared calendars (#776)
# =============================================================
# A calendar shared through `POST /api/grants` must behave over
# CalDAV the way its role says:
#
#   | Role        | current-user-privilege-set                |
#   |-------------|-------------------------------------------|
#   | Viewer      | read                                      |
#   | Contributor | read, bind                                |
#   | Editor      | read, write-content, bind, unbind         |
#   | Owner       | all                                       |
#
# and the service must enforce the same split:
#
#   * PUT of a NEW UID needs `Create` (Contributor and up).
#   * PUT over an EXISTING UID needs `Update` (Editor and up) —
#     a Contributor may add entries, never overwrite someone's.
#   * DELETE of an object needs `Update` (Editor and up); `Delete`
#     on a calendar is reserved for deleting the calendar itself.
#   * PROPPATCH (rename / recolour) needs `Manage` (Owner only).
#
# Refusals to a caller who can read the calendar are 403 (graduated
# denial), not the 404 anti-enum shape.
# =============================================================


# ─────────────────────────────────────────────────────────────
# Setup – Alice (admin) creates the calendar and three members.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/login
Content-Type: application/json
{
  "username": "{{username}}",
  "password": "{{password}}"
}

HTTP 200
[Captures]
alice_token: jsonpath "$.access_token"


MKCALENDAR {{base_url}}/caldav/roles-cal/
Authorization: Bearer {{alice_token}}

HTTP 201


# Pick the calendar out by display name rather than position —
# listing order is not guaranteed.
PROPFIND {{base_url}}/caldav/
Authorization: Bearer {{alice_token}}
Depth: 1
Content-Type: application/xml
```
<?xml version="1.0" encoding="UTF-8"?>
<D:propfind xmlns:D="DAV:">
  <D:prop><D:displayname/><D:current-user-privilege-set/></D:prop>
</D:propfind>
```

HTTP 207
[Captures]
calendar_id: xpath "string(//*[local-name()='response'][.//*[local-name()='displayname']='roles-cal']/*[local-name()='href'])" regex "/caldav/([a-f0-9-]{36})/"
[Asserts]
# The owner's own calendar, through the grants-derived listing path.
xpath "count(//*[local-name()='response'][.//*[local-name()='displayname']='roles-cal']//*[local-name()='privilege']/*[local-name()='all'])" == 1.0


POST {{base_url}}/api/admin/users
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{
  "username": "roles_viewer",
  "password": "RolesViewerPassword1!",
  "email":    "roles_viewer@example.com",
  "role":     "user"
}

HTTP *


POST {{base_url}}/api/admin/users
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{
  "username": "roles_contrib",
  "password": "RolesContribPassword1!",
  "email":    "roles_contrib@example.com",
  "role":     "user"
}

HTTP *


POST {{base_url}}/api/admin/users
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{
  "username": "roles_editor",
  "password": "RolesEditorPassword1!",
  "email":    "roles_editor@example.com",
  "role":     "user"
}

HTTP *


POST {{base_url}}/api/auth/login
Content-Type: application/json
{
  "username": "roles_viewer",
  "password": "RolesViewerPassword1!"
}

HTTP 200
[Captures]
viewer_token: jsonpath "$.access_token"
viewer_id: jsonpath "$.user.full.user.id"


POST {{base_url}}/api/auth/login
Content-Type: application/json
{
  "username": "roles_contrib",
  "password": "RolesContribPassword1!"
}

HTTP 200
[Captures]
contrib_token: jsonpath "$.access_token"
contrib_id: jsonpath "$.user.full.user.id"


POST {{base_url}}/api/auth/login
Content-Type: application/json
{
  "username": "roles_editor",
  "password": "RolesEditorPassword1!"
}

HTTP 200
[Captures]
editor_token: jsonpath "$.access_token"
editor_id: jsonpath "$.user.full.user.id"


POST {{base_url}}/api/grants
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{
  "subject":  { "type": "user", "id": "{{viewer_id}}" },
  "resource": { "type": "calendar", "id": "{{calendar_id}}" },
  "role":     "viewer"
}

HTTP 201


POST {{base_url}}/api/grants
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{
  "subject":  { "type": "user", "id": "{{contrib_id}}" },
  "resource": { "type": "calendar", "id": "{{calendar_id}}" },
  "role":     "contributor"
}

HTTP 201


POST {{base_url}}/api/grants
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{
  "subject":  { "type": "user", "id": "{{editor_id}}" },
  "resource": { "type": "calendar", "id": "{{calendar_id}}" },
  "role":     "editor"
}

HTTP 201


# Alice seeds an event the members will try to touch.
PUT {{base_url}}/caldav/{{calendar_id}}/roles-owner-evt.ics
Authorization: Bearer {{alice_token}}
Content-Type: text/calendar
```
BEGIN:VCALENDAR
VERSION:2.0
PRODID:-//OxiCloud//roles test//EN
BEGIN:VEVENT
UID:roles-owner-evt
DTSTAMP:20260101T090000Z
DTSTART:20260105T090000Z
DTEND:20260105T100000Z
SUMMARY:Owner original
END:VEVENT
END:VCALENDAR
```

HTTP 201


# ─────────────────────────────────────────────────────────────
# Advertised privileges – collection PROPFIND (engine-checked path).
# ─────────────────────────────────────────────────────────────
PROPFIND {{base_url}}/caldav/{{calendar_id}}/
Authorization: Bearer {{viewer_token}}
Depth: 0
Content-Type: application/xml
```
<?xml version="1.0" encoding="UTF-8"?>
<D:propfind xmlns:D="DAV:">
  <D:prop><D:current-user-privilege-set/></D:prop>
</D:propfind>
```

HTTP 207
[Asserts]
body contains "<D:read/>"
body not contains "<D:bind/>"
body not contains "<D:write-content/>"
body not contains "<D:unbind/>"
body not contains "<D:all/>"


PROPFIND {{base_url}}/caldav/{{calendar_id}}/
Authorization: Bearer {{contrib_token}}
Depth: 0
Content-Type: application/xml
```
<?xml version="1.0" encoding="UTF-8"?>
<D:propfind xmlns:D="DAV:">
  <D:prop><D:current-user-privilege-set/></D:prop>
</D:propfind>
```

HTTP 207
[Asserts]
body contains "<D:read/>"
body contains "<D:bind/>"
body not contains "<D:write-content/>"
body not contains "<D:unbind/>"
body not contains "<D:all/>"


PROPFIND {{base_url}}/caldav/{{calendar_id}}/
Authorization: Bearer {{editor_token}}
Depth: 0
Content-Type: application/xml
```
<?xml version="1.0" encoding="UTF-8"?>
<D:propfind xmlns:D="DAV:">
  <D:prop><D:current-user-privilege-set/></D:prop>
</D:propfind>
```

HTTP 207
[Asserts]
body contains "<D:read/>"
body contains "<D:bind/>"
body contains "<D:write-content/>"
body contains "<D:unbind/>"
body not contains "<D:all/>"
body not contains "<D:write-properties/>"


PROPFIND {{base_url}}/caldav/{{calendar_id}}/
Authorization: Bearer {{alice_token}}
Depth: 0
Content-Type: application/xml
```
<?xml version="1.0" encoding="UTF-8"?>
<D:propfind xmlns:D="DAV:">
  <D:prop><D:current-user-privilege-set/></D:prop>
</D:propfind>
```

HTTP 207
[Asserts]
body contains "<D:all/>"


# Same level through the calendar-home listing (grants-derived path).
PROPFIND {{base_url}}/caldav/
Authorization: Bearer {{editor_token}}
Depth: 1
Content-Type: application/xml
```
<?xml version="1.0" encoding="UTF-8"?>
<D:propfind xmlns:D="DAV:">
  <D:prop><D:displayname/><D:current-user-privilege-set/></D:prop>
</D:propfind>
```

HTTP 207
[Asserts]
xpath "count(//*[local-name()='response'][.//*[local-name()='displayname']='roles-cal']//*[local-name()='privilege']/*[local-name()='write-content'])" == 1.0
xpath "count(//*[local-name()='response'][.//*[local-name()='displayname']='roles-cal']//*[local-name()='privilege']/*[local-name()='all'])" == 0.0


# ─────────────────────────────────────────────────────────────
# Viewer – may read, may not add.
# ─────────────────────────────────────────────────────────────
PUT {{base_url}}/caldav/{{calendar_id}}/roles-viewer-evt.ics
Authorization: Bearer {{viewer_token}}
Content-Type: text/calendar
```
BEGIN:VCALENDAR
VERSION:2.0
PRODID:-//OxiCloud//roles test//EN
BEGIN:VEVENT
UID:roles-viewer-evt
DTSTAMP:20260101T090000Z
DTSTART:20260106T090000Z
DTEND:20260106T100000Z
SUMMARY:Viewer attempt
END:VEVENT
END:VCALENDAR
```

HTTP 403


# ─────────────────────────────────────────────────────────────
# Contributor – may add a new UID…
# ─────────────────────────────────────────────────────────────
PUT {{base_url}}/caldav/{{calendar_id}}/roles-contrib-evt.ics
Authorization: Bearer {{contrib_token}}
Content-Type: text/calendar
```
BEGIN:VCALENDAR
VERSION:2.0
PRODID:-//OxiCloud//roles test//EN
BEGIN:VEVENT
UID:roles-contrib-evt
DTSTAMP:20260101T090000Z
DTSTART:20260107T090000Z
DTEND:20260107T100000Z
SUMMARY:Contributor added
END:VEVENT
END:VCALENDAR
```

HTTP 201


# …but not overwrite an existing one (not even their own)…
PUT {{base_url}}/caldav/{{calendar_id}}/roles-owner-evt.ics
Authorization: Bearer {{contrib_token}}
Content-Type: text/calendar
```
BEGIN:VCALENDAR
VERSION:2.0
PRODID:-//OxiCloud//roles test//EN
BEGIN:VEVENT
UID:roles-owner-evt
DTSTAMP:20260101T090000Z
DTSTART:20260105T090000Z
DTEND:20260105T100000Z
SUMMARY:Contributor overwrite
END:VEVENT
END:VCALENDAR
```

HTTP 403


# …nor add an override to someone else's series — the refusal
# must land before anything is written.
PUT {{base_url}}/caldav/{{calendar_id}}/roles-owner-evt.ics
Authorization: Bearer {{contrib_token}}
Content-Type: text/calendar
```
BEGIN:VCALENDAR
VERSION:2.0
PRODID:-//OxiCloud//roles test//EN
BEGIN:VEVENT
UID:roles-owner-evt
RECURRENCE-ID:20260112T090000Z
DTSTAMP:20260101T090000Z
DTSTART:20260112T110000Z
DTEND:20260112T120000Z
SUMMARY:Contributor override
END:VEVENT
END:VCALENDAR
```

HTTP 403


# The owner's event is untouched.
GET {{base_url}}/caldav/{{calendar_id}}/roles-owner-evt.ics
Authorization: Bearer {{alice_token}}

HTTP 200
[Asserts]
body contains "SUMMARY:Owner original"
body not contains "Contributor"


# …and may not delete.
DELETE {{base_url}}/caldav/{{calendar_id}}/roles-contrib-evt.ics
Authorization: Bearer {{contrib_token}}

HTTP 403


# ─────────────────────────────────────────────────────────────
# Editor – may modify and delete objects…
# ─────────────────────────────────────────────────────────────
PUT {{base_url}}/caldav/{{calendar_id}}/roles-owner-evt.ics
Authorization: Bearer {{editor_token}}
Content-Type: text/calendar
```
BEGIN:VCALENDAR
VERSION:2.0
PRODID:-//OxiCloud//roles test//EN
BEGIN:VEVENT
UID:roles-owner-evt
DTSTAMP:20260101T090000Z
DTSTART:20260105T090000Z
DTEND:20260105T100000Z
SUMMARY:Editor edited
END:VEVENT
END:VCALENDAR
```

HTTP 204


GET {{base_url}}/caldav/{{calendar_id}}/roles-owner-evt.ics
Authorization: Bearer {{alice_token}}

HTTP 200
[Asserts]
body contains "SUMMARY:Editor edited"


DELETE {{base_url}}/caldav/{{calendar_id}}/roles-contrib-evt.ics
Authorization: Bearer {{editor_token}}

HTTP 204


# …but not rename the calendar for everyone…
PROPPATCH {{base_url}}/caldav/{{calendar_id}}/
Authorization: Bearer {{editor_token}}
Content-Type: application/xml
```
<?xml version="1.0" encoding="UTF-8"?>
<D:propertyupdate xmlns:D="DAV:">
  <D:set><D:prop><D:displayname>editor-renamed</D:displayname></D:prop></D:set>
</D:propertyupdate>
```

HTTP 403


# …nor delete it.
DELETE {{base_url}}/caldav/{{calendar_id}}/
Authorization: Bearer {{editor_token}}

HTTP 403


# ─────────────────────────────────────────────────────────────
# Owner – may rename, then clean up.
# ─────────────────────────────────────────────────────────────
PROPPATCH {{base_url}}/caldav/{{calendar_id}}/
Authorization: Bearer {{alice_token}}
Content-Type: application/xml
```
<?xml version="1.0" encoding="UTF-8"?>
<D:propertyupdate xmlns:D="DAV:">
  <D:set><D:prop><D:displayname>roles-cal-renamed</D:displayname></D:prop></D:set>
</D:propertyupdate>
```

HTTP 207


PROPFIND {{base_url}}/caldav/{{calendar_id}}/
Authorization: Bearer {{viewer_token}}
Depth: 0
Content-Type: application/xml
```
<?xml version="1.0" encoding="UTF-8"?>
<D:propfind xmlns:D="DAV:">
  <D:prop><D:displayname/></D:prop>
</D:propfind>
```

HTTP 207
[Asserts]
body contains "roles-cal-renamed"


DELETE {{base_url}}/caldav/{{calendar_id}}/
Authorization: Bearer {{alice_token}}

HTTP *
[Asserts]
status >= 200
status < 300
