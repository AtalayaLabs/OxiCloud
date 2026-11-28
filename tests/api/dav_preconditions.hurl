# =============================================================
# OxiCloud — CalDAV / CardDAV conditional writes (RFC 7232)
# =============================================================
# PUT and DELETE on `.ics` / `.vcf` objects ignored `If-Match` and
# `If-None-Match`. A DAVx⁵ / Tasks.org client replaying stale VTODOs
# with `If-Match: "<old ETag>"` overwrote the newer server copies
# (completed tasks reopened, titles and due dates reset) instead of
# getting `412 Precondition Failed` (RFC 4791 §5.3.2, RFC 6352 §6.3.2).
#
# Post-fix:
#   * If-Match with the current ETag → write proceeds.
#   * If-Match with a stale ETag, or on a missing object → 412,
#     nothing written / deleted.
#   * If-None-Match: * on an existing object → 412.
#   * No conditional header → unchanged behaviour.
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
# Step 2 — Provision a calendar and discover its id.
# ─────────────────────────────────────────────────────────────
MKCALENDAR {{base_url}}/caldav/pre-cal/
Authorization: Bearer {{admin_token}}

HTTP 201

PROPFIND {{base_url}}/caldav/
Authorization: Bearer {{admin_token}}
Depth: 1
Content-Type: application/xml
```
<?xml version="1.0" encoding="UTF-8"?>
<D:propfind xmlns:D="DAV:">
  <D:prop><D:displayname/></D:prop>
</D:propfind>
```

HTTP 207
[Captures]
pre_cal_id: xpath "string(//*[local-name()='response'][.//*[local-name()='displayname' and text()='pre-cal']]/*[local-name()='href'])" regex "/caldav/([a-f0-9-]{36})/"


# ─────────────────────────────────────────────────────────────
# Step 3 — Create a task with If-None-Match: * (fresh UID → 201).
# ─────────────────────────────────────────────────────────────
PUT {{base_url}}/caldav/{{pre_cal_id}}/pre-task-1.ics
Authorization: Bearer {{admin_token}}
Content-Type: text/calendar
If-None-Match: *
```
BEGIN:VCALENDAR
VERSION:2.0
PRODID:-//OxiCloud Tests//EN
BEGIN:VTODO
UID:pre-task-1
SUMMARY:Version 1
STATUS:NEEDS-ACTION
END:VTODO
END:VCALENDAR
```

HTTP 201
[Captures]
etag_v1: header "ETag"


# ─────────────────────────────────────────────────────────────
# Step 4 — Second create attempt with If-None-Match: * → 412.
# ─────────────────────────────────────────────────────────────
PUT {{base_url}}/caldav/{{pre_cal_id}}/pre-task-1.ics
Authorization: Bearer {{admin_token}}
Content-Type: text/calendar
If-None-Match: *
```
BEGIN:VCALENDAR
VERSION:2.0
PRODID:-//OxiCloud Tests//EN
BEGIN:VTODO
UID:pre-task-1
SUMMARY:Clobbered by create
END:VTODO
END:VCALENDAR
```

HTTP 412


# ─────────────────────────────────────────────────────────────
# Step 5 — Update with the current ETag → 204, new ETag.
# ─────────────────────────────────────────────────────────────
PUT {{base_url}}/caldav/{{pre_cal_id}}/pre-task-1.ics
Authorization: Bearer {{admin_token}}
Content-Type: text/calendar
If-Match: {{etag_v1}}
```
BEGIN:VCALENDAR
VERSION:2.0
PRODID:-//OxiCloud Tests//EN
BEGIN:VTODO
UID:pre-task-1
SUMMARY:Version 2
STATUS:COMPLETED
END:VTODO
END:VCALENDAR
```

HTTP 204
[Captures]
etag_v2: header "ETag"
[Asserts]
header "ETag" != "{{etag_v1}}"


# ─────────────────────────────────────────────────────────────
# Step 6 — The reported bug: replay of the stale copy with the
# old ETag → 412, server copy untouched.
# ─────────────────────────────────────────────────────────────
PUT {{base_url}}/caldav/{{pre_cal_id}}/pre-task-1.ics
Authorization: Bearer {{admin_token}}
Content-Type: text/calendar
If-Match: {{etag_v1}}
```
BEGIN:VCALENDAR
VERSION:2.0
PRODID:-//OxiCloud Tests//EN
BEGIN:VTODO
UID:pre-task-1
SUMMARY:Version 1
STATUS:NEEDS-ACTION
END:VTODO
END:VCALENDAR
```

HTTP 412

GET {{base_url}}/caldav/{{pre_cal_id}}/pre-task-1.ics
Authorization: Bearer {{admin_token}}

HTTP 200
[Asserts]
header "ETag" == "{{etag_v2}}"
body contains "SUMMARY:Version 2"
body contains "STATUS:COMPLETED"


# ─────────────────────────────────────────────────────────────
# Step 7 — The ETag compared is the one REPORT / PROPFIND serve.
# ─────────────────────────────────────────────────────────────
PROPFIND {{base_url}}/caldav/{{pre_cal_id}}/
Authorization: Bearer {{admin_token}}
Depth: 1
Content-Type: application/xml
```
<?xml version="1.0" encoding="UTF-8"?>
<D:propfind xmlns:D="DAV:">
  <D:prop><D:getetag/></D:prop>
</D:propfind>
```

HTTP 207
[Asserts]
xpath "string(//*[local-name()='response'][contains(*[local-name()='href'], 'pre-task-1.ics')]//*[local-name()='getetag'])" == "{{etag_v2}}"


# ─────────────────────────────────────────────────────────────
# Step 8 — If-Match on a missing object → 412 (PUT and DELETE).
# ─────────────────────────────────────────────────────────────
PUT {{base_url}}/caldav/{{pre_cal_id}}/pre-task-missing.ics
Authorization: Bearer {{admin_token}}
Content-Type: text/calendar
If-Match: *
```
BEGIN:VCALENDAR
VERSION:2.0
PRODID:-//OxiCloud Tests//EN
BEGIN:VTODO
UID:pre-task-missing
SUMMARY:Should not exist
END:VTODO
END:VCALENDAR
```

HTTP 412

GET {{base_url}}/caldav/{{pre_cal_id}}/pre-task-missing.ics
Authorization: Bearer {{admin_token}}

HTTP 404


# ─────────────────────────────────────────────────────────────
# Step 9 — No conditional header: unconditional overwrite as
# before.
# ─────────────────────────────────────────────────────────────
PUT {{base_url}}/caldav/{{pre_cal_id}}/pre-task-1.ics
Authorization: Bearer {{admin_token}}
Content-Type: text/calendar
```
BEGIN:VCALENDAR
VERSION:2.0
PRODID:-//OxiCloud Tests//EN
BEGIN:VTODO
UID:pre-task-1
SUMMARY:Version 3
END:VTODO
END:VCALENDAR
```

HTTP 204
[Captures]
etag_v3: header "ETag"


# ─────────────────────────────────────────────────────────────
# Step 10 — DELETE with a stale ETag → 412; with the current
# one → 204.
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/caldav/{{pre_cal_id}}/pre-task-1.ics
Authorization: Bearer {{admin_token}}
If-Match: {{etag_v2}}

HTTP 412

DELETE {{base_url}}/caldav/{{pre_cal_id}}/pre-task-1.ics
Authorization: Bearer {{admin_token}}
If-Match: {{etag_v3}}

HTTP 204

DELETE {{base_url}}/caldav/{{pre_cal_id}}/pre-task-1.ics
Authorization: Bearer {{admin_token}}
If-Match: {{etag_v3}}

HTTP 412


# ─────────────────────────────────────────────────────────────
# Step 11 — Events go through the same path.
# ─────────────────────────────────────────────────────────────
PUT {{base_url}}/caldav/{{pre_cal_id}}/pre-event-1.ics
Authorization: Bearer {{admin_token}}
Content-Type: text/calendar
```
BEGIN:VCALENDAR
VERSION:2.0
PRODID:-//OxiCloud Tests//EN
BEGIN:VEVENT
UID:pre-event-1
DTSTART:20260601T100000Z
DTEND:20260601T110000Z
SUMMARY:Event v1
END:VEVENT
END:VCALENDAR
```

HTTP 201
[Captures]
ev_etag_v1: header "ETag"

PUT {{base_url}}/caldav/{{pre_cal_id}}/pre-event-1.ics
Authorization: Bearer {{admin_token}}
Content-Type: text/calendar
If-Match: "stale-etag"
```
BEGIN:VCALENDAR
VERSION:2.0
PRODID:-//OxiCloud Tests//EN
BEGIN:VEVENT
UID:pre-event-1
DTSTART:20260601T100000Z
DTEND:20260601T110000Z
SUMMARY:Event stale
END:VEVENT
END:VCALENDAR
```

HTTP 412

DELETE {{base_url}}/caldav/{{pre_cal_id}}/pre-event-1.ics
Authorization: Bearer {{admin_token}}
If-Match: {{ev_etag_v1}}

HTTP 204


# ─────────────────────────────────────────────────────────────
# Step 12 — CardDAV: a dedicated address book. Not the default
# "Contacts" book: earlier scenarios may have removed it.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/address-books
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "name": "pre-book", "description": "" }

HTTP 201
[Captures]
pre_book_id: jsonpath "$.id"


# ─────────────────────────────────────────────────────────────
# Step 13 — CardDAV create / If-None-Match: * conflict.
# ─────────────────────────────────────────────────────────────
PUT {{base_url}}/carddav/{{pre_book_id}}/pre-card-1.vcf
Authorization: Bearer {{admin_token}}
Content-Type: text/vcard
If-None-Match: *
```
BEGIN:VCARD
VERSION:3.0
UID:pre-card-1
FN:Card Version 1
N:One;Card;;;
END:VCARD
```

HTTP 201
[Captures]
card_etag_v1: header "ETag"

PUT {{base_url}}/carddav/{{pre_book_id}}/pre-card-1.vcf
Authorization: Bearer {{admin_token}}
Content-Type: text/vcard
If-None-Match: *
```
BEGIN:VCARD
VERSION:3.0
UID:pre-card-1
FN:Card Clobbered
N:Clobbered;Card;;;
END:VCARD
```

HTTP 412


# ─────────────────────────────────────────────────────────────
# Step 14 — CardDAV If-Match: current → 204, stale → 412.
# ─────────────────────────────────────────────────────────────
PUT {{base_url}}/carddav/{{pre_book_id}}/pre-card-1.vcf
Authorization: Bearer {{admin_token}}
Content-Type: text/vcard
If-Match: {{card_etag_v1}}
```
BEGIN:VCARD
VERSION:3.0
UID:pre-card-1
FN:Card Version 2
N:Two;Card;;;
END:VCARD
```

HTTP 204
[Captures]
card_etag_v2: header "ETag"

PUT {{base_url}}/carddav/{{pre_book_id}}/pre-card-1.vcf
Authorization: Bearer {{admin_token}}
Content-Type: text/vcard
If-Match: {{card_etag_v1}}
```
BEGIN:VCARD
VERSION:3.0
UID:pre-card-1
FN:Card Version 1
N:One;Card;;;
END:VCARD
```

HTTP 412

GET {{base_url}}/carddav/{{pre_book_id}}/pre-card-1.vcf
Authorization: Bearer {{admin_token}}

HTTP 200
[Asserts]
header "ETag" == "{{card_etag_v2}}"
body contains "FN:Card Version 2"


# ─────────────────────────────────────────────────────────────
# Step 15 — CardDAV DELETE: stale → 412, current → 204.
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/carddav/{{pre_book_id}}/pre-card-1.vcf
Authorization: Bearer {{admin_token}}
If-Match: {{card_etag_v1}}

HTTP 412

DELETE {{base_url}}/carddav/{{pre_book_id}}/pre-card-1.vcf
Authorization: Bearer {{admin_token}}
If-Match: {{card_etag_v2}}

HTTP 204


# ─────────────────────────────────────────────────────────────
# Step 16 — Cleanup.
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/caldav/{{pre_cal_id}}/
Authorization: Bearer {{admin_token}}

HTTP 204

DELETE {{base_url}}/api/address-books/{{pre_book_id}}
Authorization: Bearer {{admin_token}}

HTTP 204
