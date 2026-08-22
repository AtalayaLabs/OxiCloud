# =============================================================
# OxiCloud – CalDAV + Round-3 AuthZ end-to-end scenario
# =============================================================
# Verifies the full CalDAV surface post-Round-3:
#
#   * MKCALENDAR / PROPFIND / DELETE against `/caldav/*` all
#     route through `CalendarService`, which enforces
#     `authz.require` on every method.
#   * Cross-user access uses the 404 anti-enum shape (was 403
#     in the bespoke `check_calendar_access` era).
#   * Sharing goes through the generic `POST /api/grants` with
#     `resource.type = "calendar"` — a first-class ReBAC
#     resource variant added in Round 3 Phase 1.
#   * A shared calendar shows up in the recipient's PROPFIND
#     listing while the grant is live and disappears again
#     after revoke.
#
# The `calendar_id` is server-assigned at MKCALENDAR time and
# surfaces in the PROPFIND response as `/caldav/<uuid>/`. We
# extract it with a regex on the response body — the fresh CI
# database (`tests/webdav/run.sh` spawns a private Postgres)
# guarantees admin has zero pre-existing calendars, so the
# first-match regex is unambiguous.
#
# CalDAV auth is JWT via the same middleware the REST API uses
# (`/caldav/*` and `/carddav/*` are both wrapped in
# `auth_middleware + require_internal_user_layer` in main.rs).
# =============================================================


# ─────────────────────────────────────────────────────────────
# Step 1 – Alice (admin) logs in.
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
alice_user_id: jsonpath "$.user.full.user.id"


# ─────────────────────────────────────────────────────────────
# Step 2 – MKCALENDAR: create a fresh calendar for the test.
# Empty body → the CalDAV handler derives the display name
# from the last path segment ("round3-cal" here). The response
# is 201 with an empty body — CalDAV convention. The
# server-assigned UUID is captured in Step 3 via PROPFIND.
# ─────────────────────────────────────────────────────────────
MKCALENDAR {{base_url}}/caldav/round3-cal/
Authorization: Bearer {{alice_token}}

HTTP 201


# ─────────────────────────────────────────────────────────────
# Step 3 – Alice PROPFIND at Depth 1 lists her calendars.
# The response is a `<D:multistatus>` — each calendar surfaces
# as `<D:href>/caldav/<uuid>/</D:href>`. Since
# `DefaultCalendarLifecycleHook` provisions a "Personal" default
# on first login, Alice has TWO calendars here: her default
# "Personal" (first) and the round3-cal created in Step 2
# (second, later `created_at`). Anchor the regex with `(?s).*`
# so it matches the LAST `/caldav/<uuid>/` in the body — that's
# round3-cal, which is what the rest of the test grants/shares
# against.
# ─────────────────────────────────────────────────────────────
PROPFIND {{base_url}}/caldav/
Authorization: Bearer {{alice_token}}
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
calendar_id: body regex "(?s).*/caldav/([a-f0-9]{8}-[a-f0-9]{4}-[a-f0-9]{4}-[a-f0-9]{4}-[a-f0-9]{12})/"
[Asserts]
# Sanity: both calendars visible in the same response.
body contains "Personal"
body contains "round3-cal"


# ─────────────────────────────────────────────────────────────
# Step 4 – Provision Bob. Idempotent: `HTTP *` accepts 201
# on the first run and 409 on subsequent ones. Login is the
# actual precondition.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/admin/users
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{
  "username": "caldav_bob",
  "password": "CaldavBobPassword1!",
  "email":    "caldav_bob@example.com",
  "role":     "user"
}

HTTP *


POST {{base_url}}/api/auth/login
Content-Type: application/json
{
  "username": "caldav_bob",
  "password": "CaldavBobPassword1!"
}

HTTP 200
[Captures]
bob_token: jsonpath "$.access_token"
bob_user_id: jsonpath "$.user.full.user.id"


# ─────────────────────────────────────────────────────────────
# Step 5 – Cross-user PROPFIND. Bob has no grant on Alice's
# calendar; his listing does NOT include the calendar's UUID.
# (Bob's OWN response body will list his lifecycle-provisioned
# calendars — none of them collide with Alice's UUID.)
# ─────────────────────────────────────────────────────────────
PROPFIND {{base_url}}/caldav/
Authorization: Bearer {{bob_token}}
Depth: 1
Content-Type: application/xml
```
<?xml version="1.0" encoding="UTF-8"?>
<D:propfind xmlns:D="DAV:">
  <D:prop><D:displayname/><D:resourcetype/></D:prop>
</D:propfind>
```

HTTP 207
[Asserts]
body not contains "{{calendar_id}}"


# ─────────────────────────────────────────────────────────────
# Step 6 – Cross-user direct PROPFIND on Alice's calendar
# → 404. `authz.require(Read)` denies with `NotFound` for
# anti-enumeration parity with files/folders/drives.
# ─────────────────────────────────────────────────────────────
PROPFIND {{base_url}}/caldav/{{calendar_id}}/
Authorization: Bearer {{bob_token}}
Depth: 0
Content-Type: application/xml
```
<?xml version="1.0" encoding="UTF-8"?>
<D:propfind xmlns:D="DAV:">
  <D:prop><D:displayname/></D:prop>
</D:propfind>
```

HTTP *
[Asserts]
status >= 400
status < 500


# ─────────────────────────────────────────────────────────────
# Step 7 – Alice shares the calendar with Bob as Viewer via
# the generic ReBAC grant endpoint. `resource.type = "calendar"`
# is a first-class variant post-Round-3.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/grants
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{
  "subject":  { "type": "user", "id": "{{bob_user_id}}" },
  "resource": { "type": "calendar", "id": "{{calendar_id}}" },
  "role":     "viewer"
}

HTTP 201
[Captures]
share_grant_id: jsonpath "$.grants[0].id"
[Asserts]
jsonpath "$.grants[0].role" == "viewer"
jsonpath "$.grants[0].resource.type" == "calendar"
jsonpath "$.grants[0].resource.id" == "{{calendar_id}}"


# ─────────────────────────────────────────────────────────────
# Step 8 – Bob PROPFIND now includes Alice's calendar. The
# `list_my_calendars` service method reads
# `authz.list_incoming_grants(user)` and unions across
# owned + shared, replacing the pre-Round-3 owner-only query.
# ─────────────────────────────────────────────────────────────
PROPFIND {{base_url}}/caldav/
Authorization: Bearer {{bob_token}}
Depth: 1
Content-Type: application/xml
```
<?xml version="1.0" encoding="UTF-8"?>
<D:propfind xmlns:D="DAV:">
  <D:prop><D:displayname/><D:resourcetype/></D:prop>
</D:propfind>
```

HTTP 207
[Asserts]
body contains "{{calendar_id}}"


# ─────────────────────────────────────────────────────────────
# Step 8b – Unified list-on-resource: Alice queries
# `GET /api/grants?resource_type=calendar&resource_id=…`. The
# handler requires `Share` on the resource (Alice's Owner grant
# satisfies it) and returns the raw `role_grants` rows including
# the Owner self-grant. Confirms `ResourceTypeDto::Calendar` is
# admitted at the query-string boundary.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/grants?resource_type=calendar&resource_id={{calendar_id}}
Authorization: Bearer {{alice_token}}

HTTP 200
[Asserts]
jsonpath "$[*].subject.id" contains "{{bob_user_id}}"
jsonpath "$[*].subject.id" contains "{{alice_user_id}}"
jsonpath "$[?(@.subject.id == '{{bob_user_id}}')].role" == "viewer"
jsonpath "$[?(@.subject.id == '{{alice_user_id}}')].role" == "owner"
jsonpath "$[?(@.subject.id == '{{bob_user_id}}')].resource.type" == "calendar"


# ─────────────────────────────────────────────────────────────
# Step 8c – Viewer Bob is denied on the unified list endpoint —
# `Share` is required, Viewer's bundle excludes it. Bob has Read
# on the calendar → graduated denial returns 403 (see
# [[project_authz_require_graduated_denial]]).
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/grants?resource_type=calendar&resource_id={{calendar_id}}
Authorization: Bearer {{bob_token}}

HTTP 403


# ─────────────────────────────────────────────────────────────
# Step 9 – Alice revokes the grant. `DELETE /api/grants/{id}`
# maps to a single `role_grants` row delete.
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/api/grants/{{share_grant_id}}
Authorization: Bearer {{alice_token}}

HTTP 204


# ─────────────────────────────────────────────────────────────
# Step 10 – Bob PROPFIND no longer includes Alice's calendar.
# The role_grants row is gone, so `list_incoming_grants` won't
# surface it and `list_my_calendars` collapses back to Bob's
# own.
# ─────────────────────────────────────────────────────────────
PROPFIND {{base_url}}/caldav/
Authorization: Bearer {{bob_token}}
Depth: 1
Content-Type: application/xml
```
<?xml version="1.0" encoding="UTF-8"?>
<D:propfind xmlns:D="DAV:">
  <D:prop><D:displayname/><D:resourcetype/></D:prop>
</D:propfind>
```

HTTP 207
[Asserts]
body not contains "{{calendar_id}}"


# ─────────────────────────────────────────────────────────────
# Step 11 – Cleanup: Alice deletes the calendar. The service
# runs `authz.require(Delete)` (owner passes via the seeded
# Owner grant), then `revoke_all_for_resource` wipes any
# remaining grants on the calendar in case a share slipped
# through.
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/caldav/{{calendar_id}}/
Authorization: Bearer {{alice_token}}

HTTP *
[Asserts]
status >= 200
status < 300
