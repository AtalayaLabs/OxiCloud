# =============================================================
# OxiCloud — NextCloud PUT gaps closed by bringing handle_put up to
# parity with handle_patch
# =============================================================
# `nc_webdav_patch_consistency.hurl` covers the same four gap classes
# for PATCH; this file targets the NC surface's `handle_put`
# (nextcloud/webdav_handler.rs), which had fallen behind PATCH's
# hardening across the RFC 5789 commits:
#
#   1. Error mapping: the write step mapped every `DomainError` to a
#      raw 500 (`AppError::internal_error(format!("Failed to store
#      file: {}", e))`) instead of `AppError::from(e)` — a VIEWER
#      (Read only, no Update) overwriting a file got a 500 leak
#      instead of the graduated-denial 403 the rest of the codebase
#      relies on (Read granted → visible → 403; no Read at all →
#      hidden → 404 anti-enum).
#   2. Cross-surface lock interop: PUT via `/remote.php/dav/` didn't
#      consult the lock store a LOCK taken via the plain `/webdav/`
#      surface writes to at all.
#   3. Quota/507: PUT via the NC surface bypassed
#      `check_storage_quota` entirely (PATCH already enforced it).
#   4. Existence-check depth (RFC 4918 §9.7.1): PUT to an existing
#      directory should be 400, and PUT under a missing parent folder
#      should be 409 — neither check existed on the NC surface; both
#      failure modes fell through to whatever `update_file_streaming_
#      with_perms` did internally.
#
# Self-contained: provisions its own throwaway users/drive so it can
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
# Part A — Error mapping: Editor can overwrite via PUT; Viewer
#          (Read only) gets 404, not a raw 500
# ═════════════════════════════════════════════════════════════


# ─────────────────────────────────────────────────────────────
# Step A1 — Provision `ncput_editor` (EDITOR) and `ncput_viewer`
#          (VIEWER, Read only).
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/admin/users
Authorization: Bearer {{admin_jwt}}
Content-Type: application/json
{
  "username": "ncput_editor",
  "password": "NcPutEditorPwd1!",
  "email":    "ncput_editor@example.com",
  "role":     "user"
}

HTTP 201
[Captures]
editor_user_id: jsonpath "$.user.id"

POST {{base_url}}/api/admin/users
Authorization: Bearer {{admin_jwt}}
Content-Type: application/json
{
  "username": "ncput_viewer",
  "password": "NcPutViewerPwd1!",
  "email":    "ncput_viewer@example.com",
  "role":     "user"
}

HTTP 201
[Captures]
viewer_user_id: jsonpath "$.user.id"


# ─────────────────────────────────────────────────────────────
# Step A2 — Log both in, mint an NC app password for each.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "ncput_editor", "password": "NcPutEditorPwd1!" }

HTTP 200
[Captures]
editor_jwt: jsonpath "$.access_token"

POST {{base_url}}/api/auth/app-passwords
Authorization: Bearer {{editor_jwt}}
Content-Type: application/json
{ "label": "nc_webdav_put_gaps (editor)" }

HTTP 200
[Captures]
editor_nc_username: jsonpath "$.username"
editor_nc_password: jsonpath "$.password"
editor_ap_id:       jsonpath "$.id"


POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "ncput_viewer", "password": "NcPutViewerPwd1!" }

HTTP 200
[Captures]
viewer_jwt: jsonpath "$.access_token"

POST {{base_url}}/api/auth/app-passwords
Authorization: Bearer {{viewer_jwt}}
Content-Type: application/json
{ "label": "nc_webdav_put_gaps (viewer)" }

HTTP 200
[Captures]
viewer_nc_username: jsonpath "$.username"
viewer_nc_password: jsonpath "$.password"
viewer_ap_id:       jsonpath "$.id"


# ─────────────────────────────────────────────────────────────
# Step A3 — Admin creates a shared drive, grants `ncput_editor`
#          EDITOR (Read + Update) and `ncput_viewer` VIEWER
#          (Read only).
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/drives
Authorization: Bearer {{admin_jwt}}
Content-Type: application/json
{
  "kind": "shared",
  "name": "ncput-shared",
  "owner": { "type": "user", "id": "{{admin_user_id}}" }
}

HTTP 201
[Captures]
shared_drive_id: jsonpath "$.id"
shared_root_id:  jsonpath "$.root_folder_id"


POST {{base_url}}/api/grants
Authorization: Bearer {{admin_jwt}}
Content-Type: application/json
{
  "subject":  { "type": "user",  "id": "{{editor_user_id}}" },
  "resource": { "type": "drive", "id": "{{shared_drive_id}}" },
  "role": "editor"
}

HTTP 201

POST {{base_url}}/api/grants
Authorization: Bearer {{admin_jwt}}
Content-Type: application/json
{
  "subject":  { "type": "user",  "id": "{{viewer_user_id}}" },
  "resource": { "type": "drive", "id": "{{shared_drive_id}}" },
  "role": "viewer"
}

HTTP 201


# ─────────────────────────────────────────────────────────────
# Step A4 — Admin seeds a file in the shared drive via the plain
#          WebDAV surface (`@drive/<id>/` scheme).
# ─────────────────────────────────────────────────────────────
PUT {{base_url}}/webdav/@drive/{{shared_drive_id}}/ncput-file.txt
Authorization: Bearer {{admin_jwt}}
Content-Type: text/plain
`0123456789`

HTTP 201


# ─────────────────────────────────────────────────────────────
# Step A5 — Bootstrap the composite BasicAuth usernames (see
#          nc_multidrive_move_regression.hurl for the mechanism).
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/ready
[Options]
variable: nc_basic_editor={{editor_nc_username}}~{{shared_root_id}}

HTTP 200

GET {{base_url}}/ready
[Options]
variable: nc_basic_viewer={{viewer_nc_username}}~{{shared_root_id}}

HTTP 200


# ─────────────────────────────────────────────────────────────
# Step A6 — EDITOR (has Update via the drive grant) CAN overwrite
#          via PUT.
# ─────────────────────────────────────────────────────────────
PUT {{base_url}}/remote.php/dav/files/{{nc_basic_editor}}/ncput-file.txt
Content-Type: text/plain
[BasicAuth]
{{nc_basic_editor}}: {{editor_nc_password}}
`XYZ`

HTTP 204

GET {{base_url}}/remote.php/dav/files/{{nc_basic_editor}}/ncput-file.txt
[BasicAuth]
{{nc_basic_editor}}: {{editor_nc_password}}

HTTP 200
[Asserts]
body == "XYZ"


# ─────────────────────────────────────────────────────────────
# Step A7 — VIEWER (has Read via the grant, but not Update) is
#          denied, not a raw 500. Viewer CAN read the file, so
#          the graduated-denial policy (authorization_ports.rs::
#          require) surfaces 403, not the anti-enum 404 — that
#          shape is reserved for callers with no Read at all.
#          Before the fix, `handle_put`'s write step mapped every
#          `DomainError` (including this authz denial) to
#          `AppError::internal_error(...)`, leaking a 500.
# ─────────────────────────────────────────────────────────────
PUT {{base_url}}/remote.php/dav/files/{{nc_basic_viewer}}/ncput-file.txt
Content-Type: text/plain
[BasicAuth]
{{nc_basic_viewer}}: {{viewer_nc_password}}
`NOP`

HTTP 403


# Cleanup Part A.
DELETE {{base_url}}/webdav/@drive/{{shared_drive_id}}/ncput-file.txt
Authorization: Bearer {{admin_jwt}}

HTTP 204

DELETE {{base_url}}/api/auth/app-passwords/{{editor_ap_id}}
Authorization: Bearer {{editor_jwt}}
HTTP 200

DELETE {{base_url}}/api/auth/app-passwords/{{viewer_ap_id}}
Authorization: Bearer {{viewer_jwt}}
HTTP 200


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
DELETE {{base_url}}/api/admin/users/{{editor_user_id}}
Authorization: Bearer {{admin_jwt}}

HTTP 200

DELETE {{base_url}}/api/admin/users/{{viewer_user_id}}
Authorization: Bearer {{admin_jwt}}

HTTP 200

DELETE {{base_url}}/api/drives/{{shared_drive_id}}
Authorization: Bearer {{admin_jwt}}

HTTP 204

DELETE {{base_url}}/api/admin/users/{{quota_owner_id}}
Authorization: Bearer {{admin_jwt}}

HTTP 200
