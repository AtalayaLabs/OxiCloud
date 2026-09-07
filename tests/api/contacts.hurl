# =============================================================
# OxiCloud – Contacts API end-to-end scenario
# =============================================================
# Run:
#   hurl --variables-file tests/api/hurl.vars --test tests/api/contacts.hurl
#
# Variables required (see hurl.vars):
#   base_url   – e.g. http://localhost:8087
#   username   – OxiCloud username
#   password   – OxiCloud password
# =============================================================


# ─────────────────────────────────────────────────────────────
# Step 1 – Login and capture the JWT token
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
admin_user_id: jsonpath "$.user.full.user.id"
[Asserts]
jsonpath "$.access_token" isString
jsonpath "$.token_type" == "Bearer"


# ─────────────────────────────────────────────────────────────
# Step 2 – List address books (at least the system book)
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/address-books
Authorization: Bearer {{token}}

HTTP 200
[Asserts]
jsonpath "$" isCollection
# System address book must always be present
jsonpath "$[?(@.id == 'system')].is_system" == true
jsonpath "$[?(@.id == 'system')].is_readonly" == true


# ─────────────────────────────────────────────────────────────
# Step 3 – Create a personal address book
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/address-books
Authorization: Bearer {{token}}
Content-Type: application/json
{
  "name": "Personal",
  "description": "Personal address book created by Hurl tests",
  "is_public": false
}

HTTP 201
[Captures]
book_id: jsonpath "$.id"
[Asserts]
jsonpath "$.id" isString
jsonpath "$.name" == "Personal"
jsonpath "$.description" == "Personal address book created by Hurl tests"
jsonpath "$.is_public" == false
jsonpath "$.is_system" == false
jsonpath "$.is_readonly" == false


# ─────────────────────────────────────────────────────────────
# Step 4 – List contacts in the new book – must be empty
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/address-books/{{book_id}}/contacts
Authorization: Bearer {{token}}

HTTP 200
[Asserts]
jsonpath "$" isCollection
jsonpath "$" count == 0


# ─────────────────────────────────────────────────────────────
# Step 5 – Create contact John Doe
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/address-books/{{book_id}}/contacts
Authorization: Bearer {{token}}
Content-Type: application/json
{
  "first_name": "John",
  "last_name": "Doe",
  "full_name": "John Doe",
  "email": [
    {
      "email": "john.doe@example.com",
      "type": "work",
      "is_primary": true
    }
  ],
  "phone": [
    {
      "number": "+1-555-0100",
      "type": "mobile",
      "is_primary": true
    }
  ]
}

HTTP 201
[Captures]
contact_id: jsonpath "$.id"
[Asserts]
jsonpath "$.id" isString
jsonpath "$.address_book_id" == {{book_id}}
jsonpath "$.first_name" == "John"
jsonpath "$.last_name" == "Doe"
jsonpath "$.full_name" == "John Doe"
jsonpath "$.email" count == 1
jsonpath "$.email[0].email" == "john.doe@example.com"
jsonpath "$.email[0].type" == "work"
jsonpath "$.email[0].is_primary" == true
jsonpath "$.phone" count == 1
jsonpath "$.phone[0].number" == "+1-555-0100"
jsonpath "$.phone[0].type" == "mobile"
jsonpath "$.phone[0].is_primary" == true


# ─────────────────────────────────────────────────────────────
# Step 6 – List contacts – exactly 1 result, must be John Doe
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/address-books/{{book_id}}/contacts
Authorization: Bearer {{token}}

HTTP 200
[Asserts]
jsonpath "$" count == 1
jsonpath "$[*].id" contains {{contact_id}}
jsonpath "$[0].first_name" == "John"
jsonpath "$[0].last_name" == "Doe"
jsonpath "$[0].full_name" == "John Doe"


# ─────────────────────────────────────────────────────────────
# Step 7 – Get John Doe by id and verify all fields
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/address-books/{{book_id}}/contacts/{{contact_id}}
Authorization: Bearer {{token}}

HTTP 200
[Captures]
etag: header "ETag"
[Asserts]
header "ETag" exists
jsonpath "$.id" == {{contact_id}}
jsonpath "$.address_book_id" == {{book_id}}
jsonpath "$.first_name" == "John"
jsonpath "$.last_name" == "Doe"
jsonpath "$.full_name" == "John Doe"
jsonpath "$.email[0].email" == "john.doe@example.com"
jsonpath "$.email[0].type" == "work"
jsonpath "$.email[0].is_primary" == true
jsonpath "$.phone[0].number" == "+1-555-0100"
jsonpath "$.phone[0].type" == "mobile"


# ─────────────────────────────────────────────────────────────
# Step 8 – Update John Doe: add nickname, notes, and organisation
#           Uses If-Match for optimistic concurrency
#           Captures the refreshed ETag into etag_updated so that
#           the original etag remains available as a stale value.
# ─────────────────────────────────────────────────────────────
PUT {{base_url}}/api/address-books/{{book_id}}/contacts/{{contact_id}}
Authorization: Bearer {{token}}
Content-Type: application/json
If-Match: {{etag}}
{
  "first_name": "John",
  "last_name": "Doe",
  "full_name": "John Doe",
  "nickname": "JD",
  "organization": "ACME Corp",
  "notes": "Updated via Hurl test",
  "email": [
    {
      "email": "john.doe@example.com",
      "type": "work",
      "is_primary": true
    }
  ],
  "phone": [
    {
      "number": "+1-555-0100",
      "type": "mobile",
      "is_primary": true
    }
  ]
}

HTTP 200
[Captures]
etag_updated: header "ETag"
[Asserts]
header "ETag" exists
jsonpath "$.id" == {{contact_id}}
jsonpath "$.first_name" == "John"
jsonpath "$.last_name" == "Doe"
jsonpath "$.nickname" == "JD"
jsonpath "$.organization" == "ACME Corp"
jsonpath "$.notes" == "Updated via Hurl test"


# ─────────────────────────────────────────────────────────────
# Step 9 – Stale ETag rejection: update with the pre-step-8 ETag
#           The contact was already modified so this must return 412
# ─────────────────────────────────────────────────────────────
PUT {{base_url}}/api/address-books/{{book_id}}/contacts/{{contact_id}}
Authorization: Bearer {{token}}
Content-Type: application/json
If-Match: {{etag}}
{
  "first_name": "John",
  "last_name": "Doe",
  "full_name": "John Doe",
  "nickname": "should-not-be-saved"
}

HTTP 412


# ─────────────────────────────────────────────────────────────
# Step 10 – Delete John Doe (uses refreshed ETag from step 8)
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/api/address-books/{{book_id}}/contacts/{{contact_id}}
Authorization: Bearer {{token}}
If-Match: {{etag_updated}}

HTTP 204


# ─────────────────────────────────────────────────────────────
# Step 11 – Address book must be empty again
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/address-books/{{book_id}}/contacts
Authorization: Bearer {{token}}

HTTP 200
[Asserts]
jsonpath "$" count == 0


# ─────────────────────────────────────────────────────────────
# Step 12 – Delete the personal address book
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/api/address-books/{{book_id}}
Authorization: Bearer {{token}}

HTTP 204


# ─────────────────────────────────────────────────────────────
# Step 13 – Verify the address book is gone from the list
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/address-books
Authorization: Bearer {{token}}

HTTP 200
[Asserts]
jsonpath "$[*].id" not contains {{book_id}}


# ─────────────────────────────────────────────────────────────
# Step 14 – List the system address book (OxiCloud users)
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/address-books/system/contacts
Authorization: Bearer {{token}}

HTTP 200
[Asserts]
jsonpath "$" isCollection


# ═════════════════════════════════════════════════════════════
# Round 3 — CardDAV/AddressBook AuthZ regression
# ═════════════════════════════════════════════════════════════
# Post-Round-3, address-book access + sharing routes through
# `AuthorizationEngine` and `storage.role_grants`. The dedicated
# `carddav.address_book_shares` table stopped being consulted;
# the generic `POST /api/grants` endpoint accepts
# `resource.type = "address_book"` as a first-class ReBAC
# resource.
#
# Coverage:
#   15. Fresh book owned by admin (Alice).
#   16. Non-member user (Bob) doesn't see the book.
#   17. Bob's direct GET on the book → 404 (anti-enum, was 403
#       pre-Round-3).
#   18. Alice shares with Bob as Viewer via `POST /api/grants`.
#   19. Bob's listing includes the book with is_readonly=true.
#   20. Viewer role's bundle has no Create — Bob's contact
#       write → 404 (anti-enum).
#   21. Alice revokes via `DELETE /api/grants/{id}`.
#   22. Bob no longer sees the book.
#   23. Cleanup.
# =============================================================


# Step 15 — Alice creates a fresh book for the share regression.
POST {{base_url}}/api/address-books
Authorization: Bearer {{token}}
Content-Type: application/json
{
  "name": "Round3 Share Book",
  "description": "Book for the multi-user share regression",
  "is_public": false
}

HTTP 201
[Captures]
share_book_id: jsonpath "$.id"


# Step 16 — Provision Bob. Idempotent: accept 201 on first run,
# 409 on subsequent runs; login is the actual precondition.
POST {{base_url}}/api/admin/users
Authorization: Bearer {{token}}
Content-Type: application/json
{
  "username": "carddav_bob",
  "password": "CarddavBobPassword1!",
  "email":    "carddav_bob@example.com",
  "role":     "user"
}

HTTP *


POST {{base_url}}/api/auth/login
Content-Type: application/json
{
  "username": "carddav_bob",
  "password": "CarddavBobPassword1!"
}

HTTP 200
[Captures]
bob_token: jsonpath "$.access_token"
bob_user_id: jsonpath "$.user.full.user.id"


# Step 17 — Bob's book listing does NOT include Alice's book.
GET {{base_url}}/api/address-books
Authorization: Bearer {{bob_token}}

HTTP 200
[Asserts]
jsonpath "$[*].id" not contains {{share_book_id}}


# Step 18a — Direct GET on Alice's book: 404 (anti-enum).
GET {{base_url}}/api/address-books/{{share_book_id}}/contacts
Authorization: Bearer {{bob_token}}

HTTP 404


# Step 18b — Contact-write into Alice's book: 404. Bob has no
# grant, so authz.require(Create) rejects with NotFound.
# Body is minimal on purpose — the endpoint's wire DTO
# (`CreateContactRequest`) marks every collection field
# `#[serde(default)]`, so `full_name` alone deserialises
# fine and lets the request reach the authz gate. Any
# body-side 422 here would mask the AuthZ regression the
# step is meant to verify.
POST {{base_url}}/api/address-books/{{share_book_id}}/contacts
Authorization: Bearer {{bob_token}}
Content-Type: application/json
{
  "full_name": "Sneaky Insert"
}

HTTP 404


# Step 19 — Alice shares the book with Bob as Viewer via the
# generic ReBAC grant endpoint. `resource.type = "address_book"`
# is a first-class variant post-Round-3.
POST {{base_url}}/api/grants
Authorization: Bearer {{token}}
Content-Type: application/json
{
  "subject":  { "type": "user", "id": "{{bob_user_id}}" },
  "resource": { "type": "address_book", "id": "{{share_book_id}}" },
  "role":     "viewer"
}

HTTP 201
[Captures]
share_grant_id: jsonpath "$.grants[0].id"
[Asserts]
jsonpath "$.grants[0].role" == "viewer"
jsonpath "$.grants[0].resource.type" == "address_book"
jsonpath "$.grants[0].resource.id" == "{{share_book_id}}"


# Step 20 — Bob's listing now includes the book, marked readonly
# because he's not the owner.
GET {{base_url}}/api/address-books
Authorization: Bearer {{bob_token}}

HTTP 200
[Asserts]
jsonpath "$[?(@.id == '{{share_book_id}}')].is_readonly" == true


# Step 21 — Viewer bundle has no Create permission. Bob has Read
# on the address book (viewer role) so graduated denial returns
# 403, not 404 (see [[project_authz_require_graduated_denial]]).
# Same minimal-body reasoning as Step 18b: keep the request valid
# at the wire layer so any rejection has to come from the AuthZ
# engine.
POST {{base_url}}/api/address-books/{{share_book_id}}/contacts
Authorization: Bearer {{bob_token}}
Content-Type: application/json
{
  "full_name": "Viewer Cannot Write"
}

HTTP 403


# Step 21b — Unified list-on-resource: Alice queries
# `GET /api/grants?resource_type=address_book&resource_id=…`.
# `Share` is required (Alice's Owner grant satisfies it) and the
# response includes the Owner self-grant that the per-domain
# UI hides. Confirms `ResourceTypeDto::AddressBook` is admitted
# at the query-string boundary.
GET {{base_url}}/api/grants?resource_type=address_book&resource_id={{share_book_id}}
Authorization: Bearer {{token}}

HTTP 200
[Asserts]
jsonpath "$[*].subject.id" contains "{{bob_user_id}}"
jsonpath "$[*].subject.id" contains "{{admin_user_id}}"
jsonpath "$[?(@.subject.id == '{{bob_user_id}}')].role" == "viewer"
jsonpath "$[?(@.subject.id == '{{admin_user_id}}')].role" == "owner"
jsonpath "$[?(@.subject.id == '{{bob_user_id}}')].resource.type" == "address_book"


# Step 21c — Viewer Bob is denied on the unified list endpoint —
# `Share` isn't in the Viewer bundle. Bob has Read → graduated
# denial returns 403 (see [[project_authz_require_graduated_denial]]).
GET {{base_url}}/api/grants?resource_type=address_book&resource_id={{share_book_id}}
Authorization: Bearer {{bob_token}}

HTTP 403


# ─────────────────────────────────────────────────────────────
# Step 21d–21g — Regression pin for AuthZ audit #13 (2026-07-12).
#
# `ContactService::delete_contact` used to `authz.require(Update)`
# on the address book instead of `Delete`. Editor role bundle
# (Read + Comment + Create + Update) satisfies Update → any
# Editor grantee on a shared address book could delete individual
# contacts. Fix: swap the required Permission on delete_contact
# + delete_group to `Delete`. Sibling `CalendarService::delete_event`
# was the ground-truth pattern.
#
# The pin promotes Bob to Editor (so his bundle includes Update
# but NOT Delete — exactly the pre-fix bypass condition), seeds a
# canary contact as Alice, has Bob attempt DELETE, then confirms
# Alice still sees the contact. Pre-fix would 204; post-fix 403.
# ─────────────────────────────────────────────────────────────

# 21d — Promote Bob from Viewer to Editor.
PUT {{base_url}}/api/grants/role
Authorization: Bearer {{token}}
Content-Type: application/json
{
  "subject":  { "type": "user",         "id": "{{bob_user_id}}" },
  "resource": { "type": "address_book", "id": "{{share_book_id}}" },
  "role":     "editor"
}

HTTP 200


# 21e — Alice seeds a canary contact in the shared book.
POST {{base_url}}/api/address-books/{{share_book_id}}/contacts
Authorization: Bearer {{token}}
Content-Type: application/json
{
  "full_name": "audit-13 delete-permission canary"
}

HTTP 201
[Captures]
audit13_contact_id: jsonpath "$.id"


# 21f — Bob (Editor) DELETE the canary → 403. Editor has Read
# so graduated denial fires with `visibility=visible`. Pre-fix
# this returned 204 because `require(Update)` succeeded on the
# Editor bundle.
DELETE {{base_url}}/api/address-books/{{share_book_id}}/contacts/{{audit13_contact_id}}
Authorization: Bearer {{bob_token}}

HTTP 403


# 21g — Alice re-fetches to confirm the canary is still there
# (Bob's DELETE really was refused, not just responded to).
GET {{base_url}}/api/address-books/{{share_book_id}}/contacts/{{audit13_contact_id}}
Authorization: Bearer {{token}}

HTTP 200
[Asserts]
jsonpath "$.id" == "{{audit13_contact_id}}"


# ─────────────────────────────────────────────────────────────
# Step 21h–21i — Regression pin for AuthZ audit #19 (2026-07-12).
#
# `ContactService::create_contact` + `create_contact_from_vcard`
# + `create_group` used to `authz.require(Update)` on the address
# book, which the Contributor bundle (Read + Create) does NOT
# satisfy — so Contributor grantees were blocked from adding
# contacts via REST or CardDAV PUT despite holding the intended
# Create permission. Not a bypass, an over-restrictive gate.
# Fix: `Permission::Create`. Sibling `#13` above closed the
# mirror bug on the delete verbs.
#
# The pin demotes Bob from Editor (Step 21d) to Contributor —
# Contributor is the minimal role that MUST succeed post-fix and
# FAILED pre-fix. Bob then POSTs a contact via REST; pre-fix this
# 403'd, post-fix returns 201.
# ─────────────────────────────────────────────────────────────

# 21h — Demote Bob from Editor to Contributor.
PUT {{base_url}}/api/grants/role
Authorization: Bearer {{token}}
Content-Type: application/json
{
  "subject":  { "type": "user",         "id": "{{bob_user_id}}" },
  "resource": { "type": "address_book", "id": "{{share_book_id}}" },
  "role":     "contributor"
}

HTTP 200


# 21i — Bob (Contributor) creates a contact → 201. Pre-fix, the
# service required Update which Contributor's bundle doesn't hold,
# so this 403'd and the CardDAV surface was equally blocked.
POST {{base_url}}/api/address-books/{{share_book_id}}/contacts
Authorization: Bearer {{bob_token}}
Content-Type: application/json
{
  "full_name": "audit-19 contributor-can-create canary"
}

HTTP 201
[Captures]
audit19_contact_id: jsonpath "$.id"


# Step 22 — Alice revokes the grant.
DELETE {{base_url}}/api/grants/{{share_grant_id}}
Authorization: Bearer {{token}}

HTTP 204


# Step 23 — Bob's listing no longer includes the book.
GET {{base_url}}/api/address-books
Authorization: Bearer {{bob_token}}

HTTP 200
[Asserts]
jsonpath "$[*].id" not contains {{share_book_id}}


# Step 24 — Cleanup: Alice deletes the book.
DELETE {{base_url}}/api/address-books/{{share_book_id}}
Authorization: Bearer {{token}}

HTTP 204


# ═════════════════════════════════════════════════════════════
# Round 3 — CardDAV protocol coverage
# ═════════════════════════════════════════════════════════════
# Verifies the CardDAV surface end-to-end:
#
#   * MKCOL creates an address book via the CardDAV protocol
#     (`ContactService::create_address_book` seeds an Owner
#     role_grant on the caller so the engine's cache warms).
#   * PROPFIND lists it in the caller's address-book home.
#   * A non-member's PROPFIND doesn't include the book.
#   * `POST /api/grants` with `resource.type = "address_book"`
#     grants Read to the non-member.
#   * The recipient's PROPFIND now includes the book.
#   * Revoke → book vanishes.
#   * DELETE cleans up.
#
# Book UUID is server-assigned at MKCOL time and appears in the
# PROPFIND multistatus as `<D:href>/carddav/<uuid>/</D:href>`.
# Regex-capture is unambiguous only if admin has zero
# pre-existing CardDAV books — true on the CI DB (fresh from
# `tests/webdav/run.sh`'s private Postgres), false in a
# populated dev DB.
# =============================================================


# Step 25 — Alice creates a fresh book via CardDAV MKCOL.
# Empty body — `handle_mkcol` derives the display name from the
# path's last segment.
MKCOL {{base_url}}/carddav/round3-carddav-book/
Authorization: Bearer {{token}}

HTTP 201


# Step 26 — Alice PROPFIND at Depth 1 lists her books. Capture
# the server-assigned UUID with a regex on the `<D:href>` value.
PROPFIND {{base_url}}/carddav/
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
carddav_book_id: body regex "/carddav/([a-f0-9]{8}-[a-f0-9]{4}-[a-f0-9]{4}-[a-f0-9]{4}-[a-f0-9]{12})/"


# Step 27 — Bob PROPFIND: the book UUID is NOT in his response.
# (Bob's lifecycle-provisioned books, if any, get their own
# UUIDs — no collision.)
PROPFIND {{base_url}}/carddav/
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
body not contains "{{carddav_book_id}}"


# Step 28 — Alice shares the book with Bob as Viewer via the
# generic ReBAC grant endpoint (same wire format as the
# calendar test, only the resource type differs).
POST {{base_url}}/api/grants
Authorization: Bearer {{token}}
Content-Type: application/json
{
  "subject":  { "type": "user", "id": "{{bob_user_id}}" },
  "resource": { "type": "address_book", "id": "{{carddav_book_id}}" },
  "role":     "viewer"
}

HTTP 201
[Captures]
carddav_grant_id: jsonpath "$.grants[0].id"


# Step 29 — Bob PROPFIND now includes the shared book. The
# CardDAV handler routes through the same
# `list_user_address_books` as the REST API, so the shared
# book flows in via the role_grants union.
PROPFIND {{base_url}}/carddav/
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
body contains "{{carddav_book_id}}"


# Step 30 — Alice revokes the grant.
DELETE {{base_url}}/api/grants/{{carddav_grant_id}}
Authorization: Bearer {{token}}

HTTP 204


# Step 31 — Bob PROPFIND no longer includes the book.
PROPFIND {{base_url}}/carddav/
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
body not contains "{{carddav_book_id}}"


# Step 32 — Cleanup: Alice deletes the book via CardDAV DELETE.
DELETE {{base_url}}/carddav/{{carddav_book_id}}/
Authorization: Bearer {{token}}

HTTP *
[Asserts]
status >= 200
status < 300
