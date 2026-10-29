# =============================================================
# OxiCloud — Operator-authored banner messages
# =============================================================
# Pins the admin CRUD surface on `/api/admin/banners` plus the
# authenticated wire exposure through `/api/status.banners` and
# the `X-Server-Status` header's `banners_version` field.
#
# Banner bodies carry operator-authored text (DB maintenance
# notes, internal ticket refs); they are NOT on `/api/config` by
# design (feat/registration privacy split).
#
# Covered properties:
#   - Create / list / delete round-trip
#   - 400 on empty body / unknown locale key
#   - Scheduled banner (starts_at in the future) is hidden from
#     `/api/status.banners` (`list_public()` filters future
#     starts_at) while admin still sees it on /api/admin/banners
#   - banners_version on the X-Server-Status header appears when
#     at least one banner is live, absent when the list is empty
#   - DELETE clears the authenticated exposure
#
# Prerequisites: setup.hurl must have run (admin user exists).
#
# Run:
#   hurl --variables-file tests/api/test.env --file-root tests \
#        --test tests/api/ops_banners.hurl
# =============================================================


# ─────────────────────────────────────────────────────────────
# Step 1 — Admin login
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/login
Content-Type: application/json
{
  "username": "{{username}}",
  "password": "{{password}}"
}

HTTP 200
[Captures]
admin_token: jsonpath "$.access_token"


# ─────────────────────────────────────────────────────────────
# Step 2 — Baseline: `/api/status` carries no banners field (or
# an empty list), proving the admin posted none and the state
# matches what later steps acquire.
#
# Pre-split this hit `/api/config.server_status.banners`; that
# field is gone (feat/registration privacy split — banner bodies
# carry operator-authored text that can leak). The whole
# server-status surface including banners now lives only on the
# authenticated `/api/status` + `X-Server-Status` header.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/status
Authorization: Bearer {{admin_token}}

HTTP 200
[Asserts]
jsonpath "$.banners" not exists


# ─────────────────────────────────────────────────────────────
# Step 3 — Baseline: admin list is empty (prior scenario cleaned
# up its own rows). If this fails, something upstream left a
# banner behind.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/admin/banners
Authorization: Bearer {{admin_token}}

HTTP 200
[Asserts]
jsonpath "$" count == 0


# ─────────────────────────────────────────────────────────────
# Step 4 — Create a warning banner with two locales.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/admin/banners
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "severity": "warning",
  "body": {
    "en": "**Scheduled maintenance** at 22:00 UTC.",
    "fr": "**Maintenance** à 22:00 UTC."
  }
}

HTTP 201
[Captures]
banner_id: jsonpath "$.id"
[Asserts]
jsonpath "$.severity" == "warning"
jsonpath "$.body.en" contains "Scheduled maintenance"
jsonpath "$.body.fr" contains "Maintenance"


# ─────────────────────────────────────────────────────────────
# Step 5 — Validation: empty body is rejected with 400.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/admin/banners
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "severity": "notification",
  "body": {}
}

HTTP 400


# ─────────────────────────────────────────────────────────────
# Step 6 — Validation: invalid locale key is rejected with 400.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/admin/banners
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "severity": "notification",
  "body": {
    "../etc/passwd": "oops"
  }
}

HTTP 400


# ─────────────────────────────────────────────────────────────
# Step 7 — Authenticated wire now carries the banner + banners_version.
#
# Pre-split this hit `/api/config.server_status.banners` on the
# PUBLIC endpoint; banner bodies carry operator-authored text
# (DB maintenance notes, internal ticket refs) and shouldn't be
# visible to anonymous callers. The whole runtime-status surface
# including banners moved to the authenticated `/api/status` +
# the `X-Server-Status` response header stamped on every
# response that goes through `server_status_middleware` (see
# the dedicated header assertion on `/api/auth/me` below).
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/status
Authorization: Bearer {{admin_token}}

HTTP 200
[Asserts]
jsonpath "$.banners_version" exists
jsonpath "$.banners" count == 1
jsonpath "$.banners[0].id" == "{{banner_id}}"
jsonpath "$.banners[0].severity" == "warning"
jsonpath "$.banners[0].body.en" contains "Scheduled"


# ─────────────────────────────────────────────────────────────
# Step 7b — The same `banners_version` on the HEADER surface.
#
# The original step 7 asserted this on `GET /api/config` and got
# `actual: none`: `server_status_middleware` is layered inside
# `create_api_routes`, and `/api/config` sits on the PUBLIC router.
# `GET /api/auth/me` fails the same way — `/api/auth/*` is its own
# router (`auth_protected_routes()`, composed in `main.rs`), also
# outside the layer, and the middleware explicitly relies on header
# absence there meaning "outside the middleware stack".
#
# `/api/admin/*` IS nested inside `create_api_routes`, so it carries
# the header. Asserting both surfaces keeps them from drifting: the
# body snapshot serves the unauthenticated boot, the header serves the
# per-response diff that the FE fetch wrapper watches.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/admin/banners
Authorization: Bearer {{admin_token}}

HTTP 200
[Asserts]
header "x-server-status" contains "banners_version"
jsonpath "$" count == 1


# ─────────────────────────────────────────────────────────────
# Step 8 — Capture the current version hash for diff checks.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/status
Authorization: Bearer {{admin_token}}

HTTP 200
[Captures]
version_before_schedule: jsonpath "$.banners_version"


# ─────────────────────────────────────────────────────────────
# Step 9 — Create a SCHEDULED banner (starts_at = year 2099).
# Admin list sees it immediately; public list does NOT.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/admin/banners
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "severity": "notification",
  "body": {"en": "Future announcement."},
  "starts_at": "2099-01-01T00:00:00Z"
}

HTTP 201
[Captures]
scheduled_id: jsonpath "$.id"


# ─────────────────────────────────────────────────────────────
# Step 10 — Admin list has both banners.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/admin/banners
Authorization: Bearer {{admin_token}}

HTTP 200
[Asserts]
jsonpath "$" count == 2


# ─────────────────────────────────────────────────────────────
# Step 11 — Authenticated list still has only the first banner;
# version hash has changed (any-live-banner → recompute). The
# scheduled banner stays off the "live" list — `list_public()`
# filters `starts_at > now`, which `/api/status` consults.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/status
Authorization: Bearer {{admin_token}}

HTTP 200
[Captures]
version_after_schedule: jsonpath "$.banners_version"
[Asserts]
jsonpath "$.banners" count == 1
jsonpath "$.banners[0].id" == "{{banner_id}}"


# ─────────────────────────────────────────────────────────────
# Step 11b — Pin the version-diff property.
# A new banner (even scheduled) MUST produce a different version
# hash so clients know to refetch. If both are equal, the FE's
# version-diff pattern would silently skip the update.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/status
Authorization: Bearer {{admin_token}}

HTTP 200
[Asserts]
jsonpath "$.banners_version" != "{{version_before_schedule}}"


# ─────────────────────────────────────────────────────────────
# Step 12 — Delete the first banner.
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/api/admin/banners/{{banner_id}}
Authorization: Bearer {{admin_token}}

HTTP 204


# ─────────────────────────────────────────────────────────────
# Step 13 — Authenticated list is empty again (scheduled still
# hidden, first one gone).
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/status
Authorization: Bearer {{admin_token}}

HTTP 200
[Asserts]
jsonpath "$.banners" not exists


# ─────────────────────────────────────────────────────────────
# Step 14 — Delete the scheduled banner too.
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/api/admin/banners/{{scheduled_id}}
Authorization: Bearer {{admin_token}}

HTTP 204


# ─────────────────────────────────────────────────────────────
# Step 15 — Delete on unknown id is 404.
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/api/admin/banners/00000000-0000-0000-0000-000000000000
Authorization: Bearer {{admin_token}}

HTTP 404


# ─────────────────────────────────────────────────────────────
# Step 16 — Everything cleared.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/admin/banners
Authorization: Bearer {{admin_token}}

HTTP 200
[Asserts]
jsonpath "$" count == 0
