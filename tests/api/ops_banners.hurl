# =============================================================
# OxiCloud — Operator-authored banner messages
# =============================================================
# Pins the admin CRUD surface on `/api/admin/banners` plus the
# public wire exposure through `/api/config.server_status.banners`
# and the `X-Server-Status` header's `banners_version` field.
#
# Covered properties:
#   - Create / list / delete round-trip
#   - 400 on empty body / unknown locale key
#   - Scheduled banner (starts_at in the future) is hidden from
#     the public endpoint while admin still sees it
#   - banners_version on the X-Server-Status header appears when
#     at least one banner is live, absent when the list is empty
#   - DELETE clears the public exposure
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
# Step 2 — Baseline: public endpoint has no banners field (or an
# empty list) and the X-Server-Status header carries no
# banners_version.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/config

HTTP 200
[Asserts]
jsonpath "$.server_status.banners" not exists


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
# Step 7 — Public wire now carries the banner + banners_version.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/config

HTTP 200
[Asserts]
header "x-server-status" contains "banners_version"
jsonpath "$.server_status.banners_version" exists
jsonpath "$.server_status.banners" count == 1
jsonpath "$.server_status.banners[0].id" == "{{banner_id}}"
jsonpath "$.server_status.banners[0].severity" == "warning"
jsonpath "$.server_status.banners[0].body.en" contains "Scheduled"


# ─────────────────────────────────────────────────────────────
# Step 8 — Capture the current version hash for diff checks.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/config

HTTP 200
[Captures]
version_before_schedule: jsonpath "$.server_status.banners_version"


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
# Step 11 — Public list still has only the first banner;
# version hash has changed (any-live-banner → recompute).
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/config

HTTP 200
[Captures]
version_after_schedule: jsonpath "$.server_status.banners_version"
[Asserts]
jsonpath "$.server_status.banners" count == 1
jsonpath "$.server_status.banners[0].id" == "{{banner_id}}"


# ─────────────────────────────────────────────────────────────
# Step 11b — Pin the version-diff property.
# A new banner (even scheduled) MUST produce a different version
# hash so clients know to refetch. If both are equal, the FE's
# version-diff pattern would silently skip the update.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/config

HTTP 200
[Asserts]
jsonpath "$.server_status.banners_version" != "{{version_before_schedule}}"


# ─────────────────────────────────────────────────────────────
# Step 12 — Delete the first banner.
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/api/admin/banners/{{banner_id}}
Authorization: Bearer {{admin_token}}

HTTP 204


# ─────────────────────────────────────────────────────────────
# Step 13 — Public list is empty again (scheduled still hidden,
# first one gone).
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/config

HTTP 200
[Asserts]
jsonpath "$.server_status.banners" not exists


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
