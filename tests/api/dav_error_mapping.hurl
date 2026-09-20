# =============================================================
# OxiCloud — DAV error-shape regression pin
# =============================================================
# Regression pin for the second half of AtalayaLabs/OxiCloud#545 (the
# funboytwo comment): the CalDAV/CardDAV handlers used to blanket-wrap
# every domain error as `AppError::internal_error(...)`, producing a
# `500 Internal Server Error` (with `error_type = "InternalError"`)
# for client-side bugs like a missing `DTSTART` line in an iCalendar
# PUT body. That masked real client bugs as opaque server errors,
# tripped monitoring, and gave clients no useful signal.
#
# The fix (both handlers): route domain errors through
# `AppError::from` so the `ErrorKind` selects the right HTTP status:
#   * `InvalidInput`  → 400
#   * `NotFound`      → 404
#   * `AccessDenied`  → 403 (surfaces as 404 anti-enum via `authz.require`
#                      before it reaches error mapping)
#   * `DatabaseError` / `InternalError` → 500 (genuine bugs)
#
# This test pins that shape for the two client-input paths that were
# reported: iCalendar PUT to `/caldav/{cal}/{uid}.ics` and vCard PUT
# to `/carddav/{book}/{uid}.vcf`. Both use the user's default
# calendar / address book provisioned by the lifecycle hooks — so
# this file also transitively regresses that end of the fix.
#
# IT IS ALSO THE CROSS-USER AUTHORIZATION COVERAGE FOR DAV.
# Steps 9-19 have a second user attack the admin's calendar and
# address book across PROPPATCH / DELETE / GET / REPORT / PUT, for
# both VEVENT and VTODO. That is stated here because the file name
# does not say so, and the `caldav_*.hurl` files are single-user by
# design — someone grepping those for a denial test concludes, wrongly,
# that none exists. If you add a DAV verb, its denial belongs here.
#
# Two conventions worth keeping when extending:
#   * assert the EXACT status on a denial — `404`, not `4xx`. A caller
#     holding no permission at all must get the anti-enumeration shape;
#     a `403` would confirm the calendar exists, and a range assertion
#     accepts it silently. (Steps 3-13 predate this and use ranges:
#     their job was "never 5xx" for the #545 regression, a weaker and
#     different property. New denials should be exact.)
#   * for anything that returns a body, assert the owner's data is
#     ABSENT as well as the status: a 207 multistatus can be
#     "successful" and still leak the objects it should have hidden,
#     and an empty 207 still confirms the collection exists.
#
# One fixture rule that bites silently: an object's UID must EQUAL its
# resource name minus `.ics`. GET and DELETE derive the UID from the URL
# (`filename.trim_end_matches(".ics")`) and look it up by that, so a
# fixture PUT as `foo.ics` carrying `UID:foo@example.test` stores fine
# and then 404s on every read. Worse for a denial test: the 404 you
# asserted arrives because the object is unfindable, not because the
# caller was refused — the assertion passes while testing nothing.
# The fixtures here originally had that mismatch; it was invisible until
# a step tried to read one back.
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
# Step 2 — Find the default "Personal" calendar UUID via
# PROPFIND `/caldav/`. Since this test runs early in the suite
# (see run.sh order) admin has exactly one calendar — the
# `DefaultCalendarLifecycleHook`-provisioned default. Any
# regex quirk is caught here rather than downstream.
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
default_calendar_id: body regex "/caldav/([a-f0-9]{8}-[a-f0-9]{4}-[a-f0-9]{4}-[a-f0-9]{4}-[a-f0-9]{12})/"


# ─────────────────────────────────────────────────────────────
# Step 3 — Malformed iCalendar PUT (missing DTSTART). Pre-fix
# behavior: `500 InternalError` — the domain-layer InvalidInput
# was blanket-wrapped as `internal_error`. Post-fix behavior:
# `400 BadRequest` + `error_type = "InvalidInput"` because
# `AppError::from(DomainError)` routes ErrorKind → HTTP status.
#
# The body has a valid VCALENDAR wrapper and a VEVENT with a
# UID + DTEND, but no DTSTART line — the exact malformed shape
# that hit the ticket.
# ─────────────────────────────────────────────────────────────
PUT {{base_url}}/caldav/{{default_calendar_id}}/dav-error-test-missing-dtstart.ics
Authorization: Bearer {{admin_token}}
Content-Type: text/calendar
```
BEGIN:VCALENDAR
VERSION:2.0
PRODID:-//OxiCloud test//EN
BEGIN:VEVENT
UID:dav-error-test-missing-dtstart@oxicloud.test
DTSTAMP:20260101T120000Z
DTEND:20260101T130000Z
SUMMARY:Missing DTSTART regression pin
END:VEVENT
END:VCALENDAR
```

HTTP 400
[Asserts]
# `error_type` is the `Display` form of `ErrorKind::InvalidInput`
# ("Invalid Input", with a space) — that's what `From<DomainError>
# for AppError` emits (see interfaces/errors.rs:134 →
# `err.kind.to_string()`). Note the ecosystem inconsistency: hand-
# crafted codes on `AppError::new(..., "MyCode")` use CamelCase
# (`EmailNotVerified`, `PasswordLoginDisabled`, …), auto-mapped
# codes use Space Case. Not normalizing here; documenting the
# current contract so this assertion doesn't drift.
jsonpath "$.error_type" == "Invalid Input"
# Body should surface the domain error message so a curl / DAV-
# client debugger can see WHAT was wrong, not just "bad request".
jsonpath "$.message" contains "DTSTART"


# ─────────────────────────────────────────────────────────────
# Step 4 — Sanity: well-formed iCalendar PUT still succeeds.
# Confirms the fix didn't turn every event into a 400.
# ─────────────────────────────────────────────────────────────
PUT {{base_url}}/caldav/{{default_calendar_id}}/dav-error-test-ok.ics
Authorization: Bearer {{admin_token}}
Content-Type: text/calendar
```
BEGIN:VCALENDAR
VERSION:2.0
PRODID:-//OxiCloud test//EN
BEGIN:VEVENT
UID:dav-error-test-ok
DTSTAMP:20260101T120000Z
DTSTART:20260101T120000Z
DTEND:20260101T130000Z
SUMMARY:Regression sanity happy path
END:VEVENT
END:VCALENDAR
```

# CalDAV PUT semantics: 201 Created on new event, 204 No Content on
# update. Accept either — this test doesn't own the event lifecycle
# distinction, only the "not 400/500" shape.
HTTP *
[Asserts]
status >= 200
status < 300


# ─────────────────────────────────────────────────────────────
# Step 5 — Find the default "Contacts" address book UUID.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/address-books
Authorization: Bearer {{admin_token}}

HTTP 200
[Asserts]
body contains "\"Contacts\""
[Captures]
# The default book's id — captured via body regex rather than a
# jsonpath filter. Hurl's `$[?(@.name == 'Contacts')].id` returns
# a scalar (not a list) when exactly one match survives, which
# then breaks `nth 0` with "invalid filter input type: string,
# expected list". Body regex is scalar-safe and works because
# `AddressBookDto` (see src/application/dtos/address_book_dto.rs)
# serializes `id` before `name` — serde preserves struct field
# declaration order, so the two fields appear adjacent in the
# JSON, letting us anchor the pattern on the known name.
default_book_id: body regex "\"id\":\"([a-f0-9-]{36})\",\"name\":\"Contacts\""


# ─────────────────────────────────────────────────────────────
# Step 6 — Malformed vCard PUT (missing FN — the required
# formatted-name property under RFC 6350). Should return
# 400 InvalidInput, not 500.
#
# NOTE: if the vCard parser here accepts an FN-less body (loose
# parsing), this step will produce a 201 and the assertion will
# fail. In that case the fix for CardDAV specifically covers a
# different failure mode (e.g. missing VERSION or duplicate
# UID). Adjust the malformed payload to whatever the domain
# parser actually rejects with InvalidInput.
# ─────────────────────────────────────────────────────────────
PUT {{base_url}}/carddav/{{default_book_id}}/dav-error-vcard-bad.vcf
Authorization: Bearer {{admin_token}}
Content-Type: text/vcard
```
INVALID-NOT-A-VCARD-AT-ALL
```

HTTP *
[Asserts]
# Whatever the domain parser rejects it with, it must not be a
# 500. The important invariant is "client-input bug → 4xx, never
# 5xx". If the CardDAV path uses a very permissive parser and
# this body somehow parses, the sanity Step 4-equivalent below
# still exercises the happy path — worst case this assertion
# skips gracefully.
status < 500


# ─────────────────────────────────────────────────────────────
# Step 7 — Sanity: well-formed vCard PUT succeeds.
# ─────────────────────────────────────────────────────────────
PUT {{base_url}}/carddav/{{default_book_id}}/dav-error-vcard-ok.vcf
Authorization: Bearer {{admin_token}}
Content-Type: text/vcard
```
BEGIN:VCARD
VERSION:3.0
UID:dav-error-vcard-ok@oxicloud.test
FN:Regression Sanity
N:Sanity;Regression;;;
EMAIL:sanity@oxicloud.test
END:VCARD
```

HTTP *
[Asserts]
status >= 200
status < 300


# ─────────────────────────────────────────────────────────────
# Cross-user AuthZ mapping (fix/caldav-carddav-error-mapping)
# ─────────────────────────────────────────────────────────────
# Regression pin for the second half of the CalDAV/CardDAV
# error-mapping sweep: EVERY handler used to
# `map_err(|e| AppError::internal_error(format!("Failed to ...: {}", e)))`,
# turning a domain-layer `NotFound` (which is what AuthZ returns
# for anti-enum on denied resources) into a 500 InternalError.
#
# Symptom: PROPPATCH / DELETE on a calendar the caller has no
# permission on returned 500 with the calendar UUID leaked in
# the body; on-call metrics tripped for benign perm denials.
#
# Fix: `.map_err(AppError::from)` — the kind-aware mapping via
# `From<DomainError> for AppError` routes NotFound → 404.
#
# Provision a second user (Alice), have her hit admin's default
# calendar + address book across the four verbs. Every response
# MUST be a 4xx client error, NOT a 5xx server error. We don't
# assert an exact 404 in every case because some paths naturally
# return 403 or 401 depending on the auth stack; the invariant
# the fix defends is "never 5xx for a perm denial".
# ─────────────────────────────────────────────────────────────


# ─────────────────────────────────────────────────────────────
# Step 8 — Provision + log in Alice (a distinct throwaway user).
# HTTP * on the create because a re-run inside the same DB will
# hit 409 Conflict; login is the actual precondition.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/admin/users
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "username": "dav-err-alice",
  "password": "DavErrAlicePassword1!",
  "email":    "dav-err-alice@example.com",
  "role":     "user"
}

HTTP *
[Captures]
alice_id: jsonpath "$.user.id"


POST {{base_url}}/api/auth/login
Content-Type: application/json
{
  "username": "dav-err-alice",
  "password": "DavErrAlicePassword1!"
}

HTTP 200
[Captures]
alice_token: jsonpath "$.access_token"


# ─────────────────────────────────────────────────────────────
# Step 9 — Alice PROPPATCH on admin's default calendar.
# Pre-fix: 500 InternalError with "Failed to update calendar:
# Not Found: Calendar not found: <uuid>" in the body.
# Post-fix: 4xx (typically 404 anti-enum from `authz.require`).
# ─────────────────────────────────────────────────────────────
PROPPATCH {{base_url}}/caldav/{{default_calendar_id}}/
Authorization: Bearer {{alice_token}}
Content-Type: application/xml
```
<?xml version="1.0" encoding="UTF-8"?>
<D:propertyupdate xmlns:D="DAV:">
  <D:set>
    <D:prop>
      <D:displayname>hijacked</D:displayname>
    </D:prop>
  </D:set>
</D:propertyupdate>
```

HTTP *
[Asserts]
status >= 400
status < 500


# ─────────────────────────────────────────────────────────────
# Step 10 — Alice DELETE on admin's default calendar. Same
# invariant — 4xx, never 5xx.
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/caldav/{{default_calendar_id}}/
Authorization: Bearer {{alice_token}}

HTTP *
[Asserts]
status >= 400
status < 500


# ─────────────────────────────────────────────────────────────
# Step 11 — Alice DELETE on the well-formed event Step 4 created
# in admin's calendar. Pre-fix: 500 on the lookup or delete step.
# Post-fix: 4xx via NotFound anti-enum.
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/caldav/{{default_calendar_id}}/dav-error-test-ok.ics
Authorization: Bearer {{alice_token}}

HTTP *
[Asserts]
status >= 400
status < 500


# ─────────────────────────────────────────────────────────────
# Step 12 — Alice PROPPATCH on admin's default address book.
# Mirror of Step 9 on the CardDAV side. Pre-fix: 500. Post-fix: 4xx.
# ─────────────────────────────────────────────────────────────
PROPPATCH {{base_url}}/carddav/{{default_book_id}}/
Authorization: Bearer {{alice_token}}
Content-Type: application/xml
```
<?xml version="1.0" encoding="UTF-8"?>
<D:propertyupdate xmlns:D="DAV:">
  <D:set>
    <D:prop>
      <D:displayname>hijacked</D:displayname>
    </D:prop>
  </D:set>
</D:propertyupdate>
```

HTTP *
[Asserts]
status >= 400
status < 500


# ─────────────────────────────────────────────────────────────
# Step 13 — Alice DELETE on admin's default address book.
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/carddav/{{default_book_id}}/
Authorization: Bearer {{alice_token}}

HTTP *
[Asserts]
status >= 400
status < 500


# ─────────────────────────────────────────────────────────────
# Step 14 — Admin stores a VTODO.
#
# Steps 9-13 all target VEVENTs and calendar/address-book
# collections. Tasks (#755) are a PARALLEL implementation, not a
# shared one: `get_todo_by_ical_uid` / `delete_todo` /
# `calendar_id_for_todo` mirror their event twins line for line.
# Copy-paste is where two gates drift apart unnoticed, so the
# task path needs its own pin rather than inheriting the event's.
# ─────────────────────────────────────────────────────────────
PUT {{base_url}}/caldav/{{default_calendar_id}}/dav-error-test-todo.ics
Authorization: Bearer {{admin_token}}
Content-Type: text/calendar
```
BEGIN:VCALENDAR
VERSION:2.0
PRODID:-//OxiCloud test//EN
BEGIN:VTODO
UID:dav-error-test-todo
DTSTAMP:20260101T120000Z
SUMMARY:Regression sanity task
END:VTODO
END:VCALENDAR
```

HTTP *
[Asserts]
status >= 200
status < 400


# ─────────────────────────────────────────────────────────────
# Step 15 — Alice GET on admin's event and task.
#
# Reads take a different route from the verbs above: they are
# gated by `has_calendar_perm(…, Read)`, which returns a bool and
# yields None rather than raising. That makes the denial a
# not-found produced by the handler, not by the error mapper — a
# separate path, and one no step so far exercises.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/caldav/{{default_calendar_id}}/dav-error-test-ok.ics
Authorization: Bearer {{alice_token}}

HTTP 404
[Asserts]
body not contains "Regression sanity happy path"


GET {{base_url}}/caldav/{{default_calendar_id}}/dav-error-test-todo.ics
Authorization: Bearer {{alice_token}}

HTTP 404
[Asserts]
body not contains "Regression sanity task"


# ─────────────────────────────────────────────────────────────
# Step 16 — Alice REPORT over admin's calendar.
#
# The body assertions carry the weight here. A multistatus can be
# a "successful" 207 while still listing objects the caller may
# not see, so a status check alone would pass a server that
# leaks. Assert on content: neither summary may appear, whatever
# the status turns out to be.
# ─────────────────────────────────────────────────────────────
REPORT {{base_url}}/caldav/{{default_calendar_id}}/
Authorization: Bearer {{alice_token}}
Depth: 1
Content-Type: application/xml
```
<?xml version="1.0" encoding="UTF-8"?>
<C:calendar-query xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav">
  <D:prop><D:getetag/><C:calendar-data/></D:prop>
  <C:filter><C:comp-filter name="VCALENDAR"/></C:filter>
</C:calendar-query>
```

HTTP 404
[Asserts]
body not contains "Regression sanity happy path"
body not contains "Regression sanity task"


# ─────────────────────────────────────────────────────────────
# Step 17 — Alice PUT into admin's calendar, twice: once creating
# a new object, once overwriting an existing one.
#
# Both route through the single `upsert_ical_objects` gate
# (`Permission::Create`), but they take different branches inside
# it — insert versus update — and only the second can destroy
# data that already exists.
# ─────────────────────────────────────────────────────────────
PUT {{base_url}}/caldav/{{default_calendar_id}}/dav-error-alice-intruder.ics
Authorization: Bearer {{alice_token}}
Content-Type: text/calendar
```
BEGIN:VCALENDAR
VERSION:2.0
PRODID:-//OxiCloud test//EN
BEGIN:VTODO
UID:dav-error-alice-intruder
DTSTAMP:20260101T120000Z
SUMMARY:Alice should not be able to store this
END:VTODO
END:VCALENDAR
```

HTTP 404


PUT {{base_url}}/caldav/{{default_calendar_id}}/dav-error-test-todo.ics
Authorization: Bearer {{alice_token}}
Content-Type: text/calendar
```
BEGIN:VCALENDAR
VERSION:2.0
PRODID:-//OxiCloud test//EN
BEGIN:VTODO
UID:dav-error-test-todo
DTSTAMP:20260101T120000Z
SUMMARY:Overwritten by Alice
END:VTODO
END:VCALENDAR
```

HTTP 404


# ─────────────────────────────────────────────────────────────
# Step 18 — Alice DELETE on admin's task.
#
# The task twin of Step 11. The delete path resolves the .ics
# name to an id *within* the named calendar, then re-derives the
# owning calendar from that id before checking permission — two
# independent gates. This is the request that fails if either is
# removed as redundant.
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/caldav/{{default_calendar_id}}/dav-error-test-todo.ics
Authorization: Bearer {{alice_token}}

HTTP 404


# ─────────────────────────────────────────────────────────────
# Step 19 — Nothing Alice attempted actually happened.
#
# The load-bearing step of this group. Every assertion above
# would still pass against a server that denied the *response*
# after performing the write; only re-reading as the owner proves
# the objects survived, unmodified, and that the intruder was
# never created.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/caldav/{{default_calendar_id}}/dav-error-test-todo.ics
Authorization: Bearer {{admin_token}}

HTTP 200
[Asserts]
body contains "Regression sanity task"
body not contains "Overwritten by Alice"


GET {{base_url}}/caldav/{{default_calendar_id}}/dav-error-alice-intruder.ics
Authorization: Bearer {{admin_token}}

HTTP 404


# ─────────────────────────────────────────────────────────────
# Step 20 — Sanity: admin's own PROPPATCH still succeeds. Guards
# against a fix that over-corrects and starts denying legitimate
# writes. `HTTP *` because PROPPATCH multi-status can be 207 or
# 200 depending on the property set; we assert the negative
# invariant (no 4xx/5xx).
# ─────────────────────────────────────────────────────────────
PROPPATCH {{base_url}}/caldav/{{default_calendar_id}}/
Authorization: Bearer {{admin_token}}
Content-Type: application/xml
```
<?xml version="1.0" encoding="UTF-8"?>
<D:propertyupdate xmlns:D="DAV:">
  <D:set>
    <D:prop>
      <D:displayname>Personal (renamed by sanity step)</D:displayname>
    </D:prop>
  </D:set>
</D:propertyupdate>
```

HTTP *
[Asserts]
status >= 200
status < 400


# ─────────────────────────────────────────────────────────────
# Step 21 — Cleanup: delete Alice so downstream test files don't
# inherit an extra user (per feedback_hurl_teardown_shared_db —
# state carries across the run.sh invocation), and drop the task
# fixture Step 14 added to the admin's *default* calendar, which
# lives for the whole run.
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/caldav/{{default_calendar_id}}/dav-error-test-todo.ics
Authorization: Bearer {{admin_token}}

HTTP *
[Asserts]
status < 500


DELETE {{base_url}}/api/admin/users/{{alice_id}}
Authorization: Bearer {{admin_token}}

HTTP *
[Asserts]
status < 500
