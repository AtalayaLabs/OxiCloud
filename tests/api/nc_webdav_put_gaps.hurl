# =============================================================
# OxiCloud — NextCloud PUT gaps closed by bringing handle_put up to
# parity with handle_patch
# =============================================================
# `nc_webdav_patch_consistency.hurl` covers the same gap classes
# for PATCH; this file targets the NC surface's `handle_put`
# (nextcloud/webdav_handler.rs), which had fallen behind PATCH's
# hardening across the RFC 5789 commits:
#
#   1. Cross-surface lock interop: PUT via `/remote.php/dav/` didn't
#      consult the lock store a LOCK taken via the plain `/webdav/`
#      surface writes to at all.
#   2. Quota/507: PUT via the NC surface bypassed
#      `check_storage_quota` entirely (PATCH already enforced it).
#   3. Existence-check depth (RFC 4918 §9.7.1): PUT to an existing
#      directory should be 400, and PUT under a missing parent folder
#      should be 409 — neither check existed on the NC surface; both
#      failure modes fell through to whatever `update_file_streaming_
#      with_perms` did internally.
#
# Self-contained: provisions its own throwaway users so it can
# run alongside the rest of the suite.
# =============================================================


# ─────────────────────────────────────────────────────────────
# Setup — Admin JWT login.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "{{username}}", "password": "{{password}}" }

HTTP 200
[Captures]
admin_jwt: jsonpath "$.access_token"
admin_user_id: jsonpath "$.user.full.user.id"


# ═════════════════════════════════════════════════════════════
# Part B — Cross-surface lock interop
# ═════════════════════════════════════════════════════════════


# ─────────────────────────────────────────────────────────────
# Step B1 — Mint admin's own NC app password (bare-username
#          surface — admin's personal drive, same file tree as
#          `/webdav/`).
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/app-passwords
Authorization: Bearer {{admin_jwt}}
Content-Type: application/json
{ "label": "nc_webdav_put_gaps (lock interop)" }

HTTP 200
[Captures]
nc_username: jsonpath "$.username"
nc_password: jsonpath "$.password"
lock_ap_id:  jsonpath "$.id"


# ─────────────────────────────────────────────────────────────
# Step B2 — Seed the file via the plain surface, LOCK it there.
# ─────────────────────────────────────────────────────────────
PUT {{base_url}}/webdav/nc-put-lock-interop-probe.txt
Authorization: Bearer {{admin_jwt}}
Content-Type: text/plain
`0123456789`

HTTP 201


LOCK {{base_url}}/webdav/nc-put-lock-interop-probe.txt
Authorization: Bearer {{admin_jwt}}
Content-Type: application/xml; charset=utf-8
```
<?xml version="1.0" encoding="utf-8"?>
<D:lockinfo xmlns:D="DAV:">
  <D:lockscope><D:exclusive/></D:lockscope>
  <D:locktype><D:write/></D:locktype>
  <D:owner>nc-put-lock-interop-test</D:owner>
</D:lockinfo>
```

HTTP 200
[Captures]
interop_lock_token: xpath "string(//*[local-name()='locktoken']/*[local-name()='href'])"


# ─────────────────────────────────────────────────────────────
# Step B3 — PUT the SAME file via the NC surface, no lock token
#          → 423. Pre-fix, the NC surface's `handle_put` didn't
#          consult the plain surface's lock store at all.
# ─────────────────────────────────────────────────────────────
PUT {{base_url}}/remote.php/dav/files/{{nc_username}}/nc-put-lock-interop-probe.txt
Content-Type: text/plain
[BasicAuth]
{{nc_username}}: {{nc_password}}
`NOP`

HTTP 423


# Release the lock via the plain surface so cleanup below works.
UNLOCK {{base_url}}/webdav/nc-put-lock-interop-probe.txt
Authorization: Bearer {{admin_jwt}}
Lock-Token: <{{interop_lock_token}}>

HTTP 204


# Cleanup Part B.
DELETE {{base_url}}/webdav/nc-put-lock-interop-probe.txt
Authorization: Bearer {{admin_jwt}}

HTTP 204


# ═════════════════════════════════════════════════════════════
# Part C — Quota/507 via the NC surface leaves the file untouched
# ═════════════════════════════════════════════════════════════


# ─────────────────────────────────────────────────────────────
# Step C1 — Provision `ncput_quota_owner` with a 50-byte quota.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/admin/users
Authorization: Bearer {{admin_jwt}}
Content-Type: application/json
{
  "username": "ncput_quota_owner",
  "password": "NcPutQuotaOwnerPwd1!",
  "email":    "ncput_quota_owner@example.com",
  "role":     "user"
}

HTTP 201
[Captures]
quota_owner_id: jsonpath "$.user.id"


PUT {{base_url}}/api/admin/users/{{quota_owner_id}}/quota
Authorization: Bearer {{admin_jwt}}
Content-Type: application/json
{ "quota_bytes": 50 }

HTTP 200


POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "ncput_quota_owner", "password": "NcPutQuotaOwnerPwd1!" }

HTTP 200
[Captures]
quota_owner_jwt: jsonpath "$.access_token"

POST {{base_url}}/api/auth/app-passwords
Authorization: Bearer {{quota_owner_jwt}}
Content-Type: application/json
{ "label": "nc_webdav_put_gaps (quota)" }

HTTP 200
[Captures]
quota_nc_username: jsonpath "$.username"
quota_nc_password: jsonpath "$.password"
quota_ap_id:       jsonpath "$.id"


# ─────────────────────────────────────────────────────────────
# Step C2 — Seed a 10-byte file (under quota), then overwrite it
#          with a payload that blows past the 50-byte quota → 507.
#          File must come back unchanged.
# ─────────────────────────────────────────────────────────────
PUT {{base_url}}/remote.php/dav/files/{{quota_nc_username}}/nc-put-quota-probe.txt
Content-Type: text/plain
[BasicAuth]
{{quota_nc_username}}: {{quota_nc_password}}
`0123456789`

HTTP 201
[Captures]
quota_probe_etag: header "ETag"


PUT {{base_url}}/remote.php/dav/files/{{quota_nc_username}}/nc-put-quota-probe.txt
Content-Type: text/plain
[BasicAuth]
{{quota_nc_username}}: {{quota_nc_password}}
`this-is-a-100-byte-ish-payload-that-blows-past-the-fifty-byte-quota-set-for-this-throwaway-user-abc`

HTTP 507


GET {{base_url}}/remote.php/dav/files/{{quota_nc_username}}/nc-put-quota-probe.txt
[BasicAuth]
{{quota_nc_username}}: {{quota_nc_password}}

HTTP 200
[Asserts]
body == "0123456789"
header "ETag" contains {{quota_probe_etag}}


# Cleanup Part C.
DELETE {{base_url}}/remote.php/dav/files/{{quota_nc_username}}/nc-put-quota-probe.txt
[BasicAuth]
{{quota_nc_username}}: {{quota_nc_password}}

HTTP 204

DELETE {{base_url}}/api/auth/app-passwords/{{quota_ap_id}}
Authorization: Bearer {{quota_owner_jwt}}
HTTP 200


# ═════════════════════════════════════════════════════════════
# Part D — Existence-check depth (RFC 4918 §9.7.1): folder-collision
#          and missing-parent, previously unchecked on the NC surface
# ═════════════════════════════════════════════════════════════


# ─────────────────────────────────────────────────────────────
# Step D1 — PUT to an existing directory → 400 (not whatever the
#          write step's internals happened to produce).
# ─────────────────────────────────────────────────────────────
MKCOL {{base_url}}/remote.php/dav/files/{{nc_username}}/nc-put-probe-dir/
[BasicAuth]
{{nc_username}}: {{nc_password}}

HTTP 201


PUT {{base_url}}/remote.php/dav/files/{{nc_username}}/nc-put-probe-dir/
Content-Type: text/plain
[BasicAuth]
{{nc_username}}: {{nc_password}}
`NOP`

HTTP 400


DELETE {{base_url}}/remote.php/dav/files/{{nc_username}}/nc-put-probe-dir/
[BasicAuth]
{{nc_username}}: {{nc_password}}

HTTP 204


# ─────────────────────────────────────────────────────────────
# Step D2 — PUT under a nonexistent parent folder → 409 Conflict
#          (RFC 4918 §9.7.1), not a generic error from further down
#          the write path.
# ─────────────────────────────────────────────────────────────
PUT {{base_url}}/remote.php/dav/files/{{nc_username}}/nc-put-missing-parent/probe.txt
Content-Type: text/plain
[BasicAuth]
{{nc_username}}: {{nc_password}}
`NOP`

HTTP 409


DELETE {{base_url}}/api/auth/app-passwords/{{lock_ap_id}}
Authorization: Bearer {{admin_jwt}}
HTTP 200


# ═════════════════════════════════════════════════════════════
# Teardown
# ═════════════════════════════════════════════════════════════
DELETE {{base_url}}/api/admin/users/{{quota_owner_id}}
Authorization: Bearer {{admin_jwt}}

HTTP 200
