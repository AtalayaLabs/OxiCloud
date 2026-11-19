# =============================================================
# OxiCloud — Baseline: OCS user-info + provisioning
# =============================================================
# Group C from BASELINE_TESTS_NC_WEBDAV.md.
#
# /ocs/v{1,2}.php/cloud/user is what NC desktop reads after
# Login Flow v2 to learn its `data.id` — and that exact string
# is then spliced into every subsequent DAV path
# (`/remote.php/dav/files/{id}/…`). A regression in this shape
# breaks 100% of subsequent syncs.
#
# Coverage:
#   C1 — GET /ocs/v1.php/cloud/user → statuscode 100 + payload
#   C2 — GET /ocs/v2.php/cloud/user → statuscode 200 + payload
#   C3 — GET /ocs/v1.php/cloud/users/admin (self provisioning)
#   C5 — GET /ocs/v2.php/apps/files_sharing/api/v1/sharees shape
#
# Deferred:
#   C4 — admin reading another user's provisioning profile.
#        Needs a second user fixture wired into setup.hurl.
#        Tracked as a TODO in BASELINE_TESTS_NC_WEBDAV.md §7.
#
# Setup pattern: this file mints its own app password inline
# (steps 1–2) so it is self-contained and resilient to test
# ordering. The mint uses the JWT-authenticated REST API
# (`POST /api/auth/app-passwords`); the NC-side username
# returned by the response is exactly the value the NC client
# would use as the HTTP Basic Auth username.
# =============================================================


# ─────────────────────────────────────────────────────────────
# Setup 1 — JWT login (gives us a Bearer token to mint the
#           app password).
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "{{username}}", "password": "{{password}}" }

HTTP 200
[Captures]
jwt: jsonpath "$.access_token"


# ─────────────────────────────────────────────────────────────
# Setup 2 — Mint an app password for this test file. The
#           response carries the plaintext password (shown
#           exactly once) and the username to use in Basic Auth.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/app-passwords
Authorization: Bearer {{jwt}}
Content-Type: application/json
{ "label": "nc_ocs_user_info hurl test" }

HTTP 200
[Captures]
nc_username: jsonpath "$.username"
nc_password: jsonpath "$.password"
ap_id:       jsonpath "$.id"
[Asserts]
jsonpath "$.password" matches "^oxicloud-"


# ─────────────────────────────────────────────────────────────
# C1 — GET /ocs/v1.php/cloud/user
#      data.id is what NC client splices into DAV URLs.
#
#      Note: `handle_user_info` returns `statuscode: 200`
#      regardless of /v1.php vs /v2.php (unlike capabilities,
#      which switches on ocs_version). C1 and C2 therefore
#      both assert 200 here — this is the actual server
#      behaviour; if either diverges in future, this is the
#      pin that catches it.
#
#      Hurl JSONPath doesn't accept `-` in dotted form, so the
#      assertion goes against `displayname` (the unhyphenated
#      alias the handler also emits) rather than `display-name`.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/ocs/v1.php/cloud/user?format=json
[BasicAuth]
{{nc_username}}: {{nc_password}}

HTTP 200
[Asserts]
jsonpath "$.ocs.meta.status"     == "ok"
jsonpath "$.ocs.meta.statuscode" == 200
jsonpath "$.ocs.meta.message"    == "OK"
jsonpath "$.ocs.data.enabled"    == true
jsonpath "$.ocs.data.id"         == "{{username}}"
jsonpath "$.ocs.data.email"      == "{{email}}"
jsonpath "$.ocs.data.displayname"    exists
jsonpath "$.ocs.data.quota.used"     exists
jsonpath "$.ocs.data.quota.total"    exists
jsonpath "$.ocs.data.quota.free"     exists
jsonpath "$.ocs.data.quota.relative" exists


# ─────────────────────────────────────────────────────────────
# C2 — GET /ocs/v2.php/cloud/user
#      Same payload shape; OCS v2 envelope reports statuscode 200.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/ocs/v2.php/cloud/user?format=json
[BasicAuth]
{{nc_username}}: {{nc_password}}

HTTP 200
[Asserts]
jsonpath "$.ocs.meta.status"     == "ok"
jsonpath "$.ocs.meta.statuscode" == 200
jsonpath "$.ocs.data.id"         == "{{username}}"
jsonpath "$.ocs.data.email"      == "{{email}}"


# ─────────────────────────────────────────────────────────────
# C3 — GET /ocs/v1.php/cloud/users/{userid} (self lookup)
#      Full provisioning profile: groups, lastLogin, backend.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/ocs/v1.php/cloud/users/{{username}}?format=json
[BasicAuth]
{{nc_username}}: {{nc_password}}

HTTP 200
[Asserts]
jsonpath "$.ocs.meta.statuscode" == 100
jsonpath "$.ocs.data.id"         == "{{username}}"
jsonpath "$.ocs.data.email"      == "{{email}}"
jsonpath "$.ocs.data.groups"     exists
jsonpath "$.ocs.data.backend"    exists
jsonpath "$.ocs.data.lastLogin"  exists


# ─────────────────────────────────────────────────────────────
# C5 — GET /ocs/v2.php/apps/files_sharing/api/v1/sharees
#      Sharees autocomplete shape — NC desktop uses this to
#      populate the share-dialog. Even with no shares set up
#      yet, the envelope + array slots must exist.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/ocs/v2.php/apps/files_sharing/api/v1/sharees?format=json&search=ad&itemType=file
[BasicAuth]
{{nc_username}}: {{nc_password}}

HTTP 200
[Asserts]
jsonpath "$.ocs.meta.status"            == "ok"
jsonpath "$.ocs.meta.statuscode"        == 200
jsonpath "$.ocs.data.exact"             exists
jsonpath "$.ocs.data.exact.users"       exists
jsonpath "$.ocs.data.exact.groups"      exists
jsonpath "$.ocs.data.users"             exists


# ─────────────────────────────────────────────────────────────
# C6 — allowed path for the #914 gate: sharee search still finds
#      another internal user (refusals are unit-pinned).
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/admin/users
Authorization: Bearer {{jwt}}
Content-Type: application/json
{
  "username": "sharee-peer",
  "password": "ShareePeerPass1!",
  "email":    "sharee-peer@example.com",
  "role":     "user"
}

# 201 on a fresh DB, 409 when a previous run already created it.
HTTP *
[Asserts]
status toString matches /^(201|409)$/

GET {{base_url}}/ocs/v2.php/apps/files_sharing/api/v1/sharees?format=json&search=sharee-pe&itemType=file
[BasicAuth]
{{nc_username}}: {{nc_password}}

HTTP 200
[Asserts]
jsonpath "$.ocs.data.users[0].value.shareWith" == "sharee-peer"


# ─────────────────────────────────────────────────────────────
# Teardown — Revoke the app password we minted (captured `ap_id`
#            from the create response, no list+lookup needed).
#            Keeps the test surface clean across re-runs.
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/api/auth/app-passwords/{{ap_id}}
Authorization: Bearer {{jwt}}

HTTP 200
