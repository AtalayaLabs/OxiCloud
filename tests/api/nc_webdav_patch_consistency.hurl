# =============================================================
# OxiCloud — NextCloud PATCH data-consistency + authz/lock gaps
# =============================================================
# `nc_webdav_patch.hurl` covers the PATCH contract on the NC surface.
# This file targets the specific gaps closed by the review-fix commit
# (see nextcloud/webdav_handler.rs::handle_patch):
#
#   1. Cross-surface lock interop: a LOCK taken via the plain
#      `/webdav/` surface now also blocks PATCH via `/remote.php/dav/`
#      for the same file — proves the two surfaces share one lock
#      store, not two independent ones.
#   2. Quota/507 via the NC surface (previously missing entirely —
#      the fix added the same per-user quota check the plain surface
#      already enforced), and the failed PATCH leaves the file intact.
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
{ "label": "nc_webdav_patch_consistency (lock interop)" }

HTTP 200
[Captures]
nc_username: jsonpath "$.username"
nc_password: jsonpath "$.password"
lock_ap_id:  jsonpath "$.id"


# ─────────────────────────────────────────────────────────────
# Step B2 — Seed the file via the plain surface, LOCK it there.
# ─────────────────────────────────────────────────────────────
PUT {{base_url}}/webdav/nc-lock-interop-probe.txt
Authorization: Bearer {{admin_jwt}}
Content-Type: text/plain
`0123456789`

HTTP 201


LOCK {{base_url}}/webdav/nc-lock-interop-probe.txt
Authorization: Bearer {{admin_jwt}}
Content-Type: application/xml; charset=utf-8
```
<?xml version="1.0" encoding="utf-8"?>
<D:lockinfo xmlns:D="DAV:">
  <D:lockscope><D:exclusive/></D:lockscope>
  <D:locktype><D:write/></D:locktype>
  <D:owner>nc-lock-interop-test</D:owner>
</D:lockinfo>
```

HTTP 200
[Captures]
interop_lock_token: xpath "string(//*[local-name()='locktoken']/*[local-name()='href'])"


# ─────────────────────────────────────────────────────────────
# Step B3 — PATCH the SAME file via the NC surface, no lock token
#          → 423. Pre-fix, the NC surface didn't consult the
#          plain surface's lock store at all.
# ─────────────────────────────────────────────────────────────
PATCH {{base_url}}/remote.php/dav/files/{{nc_username}}/nc-lock-interop-probe.txt
X-Update-Range: bytes=0-2
Content-Type: text/plain
[BasicAuth]
{{nc_username}}: {{nc_password}}
`NOP`

HTTP 423


# Release the lock via the plain surface so cleanup below works.
UNLOCK {{base_url}}/webdav/nc-lock-interop-probe.txt
Authorization: Bearer {{admin_jwt}}
Lock-Token: <{{interop_lock_token}}>

HTTP 204


# Cleanup Part B.
DELETE {{base_url}}/webdav/nc-lock-interop-probe.txt
Authorization: Bearer {{admin_jwt}}

HTTP 204


# ═════════════════════════════════════════════════════════════
# Part C — Quota/507 via the NC surface leaves the file untouched
# ═════════════════════════════════════════════════════════════


# ─────────────────────────────────────────────────────────────
# Step C1 — Provision `ncpatch_quota_owner` with a 50-byte quota.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/admin/users
Authorization: Bearer {{admin_jwt}}
Content-Type: application/json
{
  "username": "ncpatch_quota_owner",
  "password": "NcPatchQuotaOwnerPwd1!",
  "email":    "ncpatch_quota_owner@example.com",
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
{ "username": "ncpatch_quota_owner", "password": "NcPatchQuotaOwnerPwd1!" }

HTTP 200
[Captures]
quota_owner_jwt: jsonpath "$.access_token"

POST {{base_url}}/api/auth/app-passwords
Authorization: Bearer {{quota_owner_jwt}}
Content-Type: application/json
{ "label": "nc_webdav_patch_consistency (quota)" }

HTTP 200
[Captures]
quota_nc_username: jsonpath "$.username"
quota_nc_password: jsonpath "$.password"
quota_ap_id:       jsonpath "$.id"


# ─────────────────────────────────────────────────────────────
# Step C2 — Seed a 10-byte file (under quota), then append past
#          it → 507. File must come back unchanged.
# ─────────────────────────────────────────────────────────────
PUT {{base_url}}/remote.php/dav/files/{{quota_nc_username}}/nc-quota-probe.txt
Content-Type: text/plain
[BasicAuth]
{{quota_nc_username}}: {{quota_nc_password}}
`0123456789`

HTTP 201
[Captures]
quota_probe_etag: header "ETag"


PATCH {{base_url}}/remote.php/dav/files/{{quota_nc_username}}/nc-quota-probe.txt
X-Update-Range: append
Content-Type: text/plain
[BasicAuth]
{{quota_nc_username}}: {{quota_nc_password}}
`this-is-a-100-byte-ish-payload-that-blows-past-the-fifty-byte-quota-set-for-this-throwaway-user-abc`

HTTP 507


GET {{base_url}}/remote.php/dav/files/{{quota_nc_username}}/nc-quota-probe.txt
[BasicAuth]
{{quota_nc_username}}: {{quota_nc_password}}

HTTP 200
[Asserts]
body == "0123456789"
header "ETag" contains {{quota_probe_etag}}


# Cleanup Part C.
DELETE {{base_url}}/remote.php/dav/files/{{quota_nc_username}}/nc-quota-probe.txt
[BasicAuth]
{{quota_nc_username}}: {{quota_nc_password}}

HTTP 204

DELETE {{base_url}}/api/auth/app-passwords/{{quota_ap_id}}
Authorization: Bearer {{quota_owner_jwt}}
HTTP 200

DELETE {{base_url}}/api/auth/app-passwords/{{lock_ap_id}}
Authorization: Bearer {{admin_jwt}}
HTTP 200


# ═════════════════════════════════════════════════════════════
# Teardown
# ═════════════════════════════════════════════════════════════
DELETE {{base_url}}/api/admin/users/{{quota_owner_id}}
Authorization: Bearer {{admin_jwt}}

HTTP 200
