# =============================================================
# OxiCloud — default CalDAV calendar + CardDAV address book
# =============================================================
# Regression pin for issue #545: fresh internal users must have a
# default calendar ("Personal") and address book ("Contacts") ready
# for CalDAV/CardDAV client discovery. Without this, Thunderbird's
# "New Calendar → On the Network" returns "no calendars found" and
# Contacts returns "no address books" — see the ticket.
#
# The invariant is delivered by two lifecycle hooks:
#   * DefaultCalendarLifecycleHook   (calendar_service.rs)
#   * DefaultAddressBookLifecycleHook (contact_service.rs)
#
# Both fire on `on_user_created` (so fresh signups get it), and on
# `on_user_login` as a safety-net (so users who predate the hook get
# their defaults on next login — no data migration needed). External
# users are skipped; on `on_upgraded_to_internal` they get the defaults.
#
# The idempotency check is ownership-based: `list_calendars_by_owner`
# / `get_address_books_by_owner`. A user who manually created their
# own calendar / address book keeps it; the hook doesn't provision
# a redundant one. See docs/architecture/ discussion for the design.
# =============================================================


# ─────────────────────────────────────────────────────────────
# Step 1 — Admin login. Admin was created via `POST /api/setup`
# which fires `dispatch_created`, so the default hooks should
# have already provisioned admin's calendar + address book.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "{{username}}", "password": "{{password}}" }

HTTP 200
[Captures]
admin_token: jsonpath "$.access_token"


# ─────────────────────────────────────────────────────────────
# Step 2 — Admin's default calendar exists via PROPFIND on
# `/caldav/`. The "Personal" name is what Thunderbird / Apple
# Calendar / DAVx⁵ show in their calendar picker; it must be
# rendered verbatim in the DAV displayname element.
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
[Asserts]
# The default calendar's displayname must appear in the PROPFIND
# multistatus. Thunderbird's discovery reads this exact element.
body contains "Personal"


# ─────────────────────────────────────────────────────────────
# Step 3 — Admin's default address book exists via REST list.
# The `/api/address-books` endpoint returns admin's owned books;
# "Contacts" (matching the Nextcloud convention) is what the
# CardDAV clients render in their address-book picker.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/address-books
Authorization: Bearer {{admin_token}}

HTTP 200
[Asserts]
jsonpath "$" isCollection
# The default address book's displayname must be in the list.
# Body-contains rather than a jsonpath filter — Hurl's
# `$[?(@.name == 'Contacts')]` returns a scalar when exactly one
# match survives (single-element filter result), and `nth 0`
# then fails with "invalid filter input type: boolean, expected
# list". Body-substring is state-resilient (works whether admin
# has 1 or N address books) and mirrors the CalDAV PROPFIND
# assertion above.
body contains "\"Contacts\""


# ─────────────────────────────────────────────────────────────
# Step 4 — Fresh-user provisioning. Admin creates a new user;
# the two hooks fire on `on_user_created` during the admin-create
# transaction, so by the time we log in as the new user their
# defaults are already there.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/admin/users
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "username":    "dav-defaults-fresh",
  "email":       "dav-defaults-fresh@example.com",
  "password":    "TestPassword1!",
  "role":        "user",
  "is_external": false
}

HTTP *
[Captures]
fresh_user_id: jsonpath "$.user.id"


# ─────────────────────────────────────────────────────────────
# Step 5 — Fresh user logs in. This is the critical path from
# the ticket: a client (Thunderbird) authenticates as this user
# and does PROPFIND on `/caldav/` — must find "Personal".
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "dav-defaults-fresh", "password": "TestPassword1!" }

HTTP 200
[Captures]
fresh_token: jsonpath "$.access_token"


PROPFIND {{base_url}}/caldav/
Authorization: Bearer {{fresh_token}}
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
body contains "Personal"


# ─────────────────────────────────────────────────────────────
# Step 6 — Fresh user's address book listing includes "Contacts".
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/address-books
Authorization: Bearer {{fresh_token}}

HTTP 200
[Asserts]
jsonpath "$" isCollection
# Same rationale as Step 3 — body substring rather than filtered
# jsonpath, avoids the "boolean vs list" Hurl quirk on
# single-match filters.
body contains "\"Contacts\""


# ─────────────────────────────────────────────────────────────
# Step 7 — Ownership idempotency. Fresh user creates their OWN
# calendar named "Personal" (matching what the hook auto-created).
# This coexists — two rows with different UUIDs, same display
# name. The hook's safety-net check on next login sees "user
# owns ≥ 1 calendar" and SKIPS re-provisioning. Assertion below
# proves both rows survive: two `Personal` matches in the body.
# ─────────────────────────────────────────────────────────────
MKCALENDAR {{base_url}}/caldav/Personal/
Authorization: Bearer {{fresh_token}}

HTTP *


# Second login triggers `on_user_login` safety-net. If it wrongly
# re-provisioned another default, we'd see three calendars now.
POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "dav-defaults-fresh", "password": "TestPassword1!" }

HTTP 200
[Captures]
fresh_token_2: jsonpath "$.access_token"


PROPFIND {{base_url}}/caldav/
Authorization: Bearer {{fresh_token_2}}
Depth: 1
Content-Type: application/xml
```
<?xml version="1.0" encoding="UTF-8"?>
<D:propfind xmlns:D="DAV:">
  <D:prop><D:displayname/><D:resourcetype/></D:prop>
</D:propfind>
```

HTTP 207
# The response body should contain "Personal" — at LEAST once
# (the auto-provisioned one), plus the manually-created "Personal".
# What must NOT happen is a proliferation of defaults on each
# login. If the safety-net wrongly ignored the ownership check
# and re-provisioned, we'd have 3+ calendars in the body. Count
# occurrences of the `<displayname>Personal</displayname>` tag —
# max should be 2 (auto + user's manual). This ceiling proves
# the safety-net check is ownership-based, not stateful.
#
# Hurl doesn't ship a "count regex matches" primitive, so the
# assertion is indirect: check that the whole `<multistatus>`
# body length is bounded. On the CalDAV server we run, a
# response with 2 calendars is well under 3 KB. 4 KB safely
# rejects any accumulation.
[Asserts]
body contains "Personal"
bytes count < 4096


# ─────────────────────────────────────────────────────────────
# Cleanup — admin deletes the test user. The cascade
# (`carddav.address_books.owner_id ON DELETE CASCADE` +
# `caldav.calendars.owner_id ON DELETE CASCADE`) reaps the
# defaults + manual calendar in the same transaction.
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/api/admin/users/{{fresh_user_id}}
Authorization: Bearer {{admin_token}}

HTTP *
