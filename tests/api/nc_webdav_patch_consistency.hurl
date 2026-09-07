# =============================================================
# OxiCloud — NextCloud PATCH data-consistency + authz/lock gaps
# =============================================================
# `nc_webdav_patch.hurl` covers the PATCH contract on the NC surface.
# This file targets the specific gaps closed by the review-fix commit
# (see nextcloud/webdav_handler.rs::handle_patch):
#
#   1. AuthZ: the NC surface previously called `get_file_by_path`
#      (which performs NO authorization check) with no follow-up
#      `authz.require` at all — any caller with a valid app password
#      could learn a file's size/ETag via PATCH's precondition/range
#      responses regardless of their actual permission on that file.
#      The fix added the same `Permission::Read` check the plain
#      surface already had. That Read check is only an early
#      existence-proof gate, though — the actual write a few lines
#      later goes through `update_file_streaming_with_perms`, which
#      independently requires `Permission::Update`. So the full
#      permission chain for PATCH is: EDITOR (has Update) can PATCH;
#      VIEWER (Read only, no Update) gets past the early gate but is
#      still denied — anti-enum 404 — at the write step; a caller
#      with NO grant at all can't even establish the composite-marker
#      chroot. Tested via the multi-drive composite `{user}~{folder_id}`
#      credential shape (see `nc_multidrive_move_regression.hurl` for
#      the mechanism).
#   2. Cross-surface lock interop: a LOCK taken via the plain
#      `/webdav/` surface now also blocks PATCH via `/remote.php/dav/`
#      for the same file — proves the two surfaces share one lock
#      store, not two independent ones.
#   3. Quota/507 via the NC surface (previously missing entirely —
#      the fix added the same per-user quota check the plain surface
#      already enforced), and the failed PATCH leaves the file intact.
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
# Part A — AuthZ: Editor can PATCH; Viewer (Read only) and a
#          no-grant outsider both can't
# ═════════════════════════════════════════════════════════════


# ─────────────────────────────────────────────────────────────
# Step A1 — Provision `ncpatch_editor` (will get EDITOR),
#          `ncpatch_viewer` (will get VIEWER), and
#          `ncpatch_outsider` (gets NO grant at all).
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/admin/users
Authorization: Bearer {{admin_jwt}}
Content-Type: application/json
{
  "username": "ncpatch_editor",
  "password": "NcPatchEditorPwd1!",
  "email":    "ncpatch_editor@example.com",
  "role":     "user"
}

HTTP 201
[Captures]
editor_user_id: jsonpath "$.user.id"

POST {{base_url}}/api/admin/users
Authorization: Bearer {{admin_jwt}}
Content-Type: application/json
{
  "username": "ncpatch_viewer",
  "password": "NcPatchViewerPwd1!",
  "email":    "ncpatch_viewer@example.com",
  "role":     "user"
}

HTTP 201
[Captures]
viewer_user_id: jsonpath "$.user.id"

POST {{base_url}}/api/admin/users
Authorization: Bearer {{admin_jwt}}
Content-Type: application/json
{
  "username": "ncpatch_outsider",
  "password": "NcPatchOutsiderPwd1!",
  "email":    "ncpatch_outsider@example.com",
  "role":     "user"
}

HTTP 201
[Captures]
outsider_user_id: jsonpath "$.user.id"


# ─────────────────────────────────────────────────────────────
# Step A2 — Log all three in, mint an NC app password for each.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "ncpatch_editor", "password": "NcPatchEditorPwd1!" }

HTTP 200
[Captures]
editor_jwt: jsonpath "$.access_token"

POST {{base_url}}/api/auth/app-passwords
Authorization: Bearer {{editor_jwt}}
Content-Type: application/json
{ "label": "nc_webdav_patch_consistency (editor)" }

HTTP 200
[Captures]
editor_nc_username: jsonpath "$.username"
editor_nc_password: jsonpath "$.password"
editor_ap_id:       jsonpath "$.id"


POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "ncpatch_viewer", "password": "NcPatchViewerPwd1!" }

HTTP 200
[Captures]
viewer_jwt: jsonpath "$.access_token"

POST {{base_url}}/api/auth/app-passwords
Authorization: Bearer {{viewer_jwt}}
Content-Type: application/json
{ "label": "nc_webdav_patch_consistency (viewer)" }

HTTP 200
[Captures]
viewer_nc_username: jsonpath "$.username"
viewer_nc_password: jsonpath "$.password"
viewer_ap_id:       jsonpath "$.id"


POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "ncpatch_outsider", "password": "NcPatchOutsiderPwd1!" }

HTTP 200
[Captures]
outsider_jwt: jsonpath "$.access_token"

POST {{base_url}}/api/auth/app-passwords
Authorization: Bearer {{outsider_jwt}}
Content-Type: application/json
{ "label": "nc_webdav_patch_consistency (outsider)" }

HTTP 200
[Captures]
outsider_nc_username: jsonpath "$.username"
outsider_nc_password: jsonpath "$.password"
outsider_ap_id:       jsonpath "$.id"


# ─────────────────────────────────────────────────────────────
# Step A3 — Admin creates a shared drive, grants `ncpatch_editor`
#          EDITOR (Read + Update) and `ncpatch_viewer` VIEWER
#          (Read only). `ncpatch_outsider` gets no grant at all.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/drives
Authorization: Bearer {{admin_jwt}}
Content-Type: application/json
{
  "kind": "shared",
  "name": "ncpatch-shared",
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
PUT {{base_url}}/webdav/@drive/{{shared_drive_id}}/ncpatch-file.txt
Authorization: Bearer {{admin_jwt}}
Content-Type: text/plain
`0123456789`

HTTP 201


# ─────────────────────────────────────────────────────────────
# Step A5 — Bootstrap the composite BasicAuth usernames (Hurl's
#          [BasicAuth] parser chokes on a literal `~` split across
#          two templates — alias it via [Options] variable: first,
#          same workaround as nc_multidrive_move_regression.hurl).
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/ready
[Options]
variable: nc_basic_editor={{editor_nc_username}}~{{shared_root_id}}

HTTP 200

GET {{base_url}}/ready
[Options]
variable: nc_basic_viewer={{viewer_nc_username}}~{{shared_root_id}}

HTTP 200

GET {{base_url}}/ready
[Options]
variable: nc_basic_outsider={{outsider_nc_username}}~{{shared_root_id}}

HTTP 200


# ─────────────────────────────────────────────────────────────
# Step A6 — EDITOR (has Update via the drive grant) CAN PATCH.
#          This is the positive check: the fix's authz.require(Read)
#          gate plus the write step's Update requirement must not
#          accidentally lock out a legitimate Update-holder.
# ─────────────────────────────────────────────────────────────
PATCH {{base_url}}/remote.php/dav/files/{{nc_basic_editor}}/ncpatch-file.txt
X-Update-Range: bytes=0-2
Content-Type: text/plain
[BasicAuth]
{{nc_basic_editor}}: {{editor_nc_password}}
`XYZ`

HTTP 204


GET {{base_url}}/remote.php/dav/files/{{nc_basic_editor}}/ncpatch-file.txt
[BasicAuth]
{{nc_basic_editor}}: {{editor_nc_password}}

HTTP 200
[Asserts]
body == "XYZ3456789"


# ─────────────────────────────────────────────────────────────
# Step A7 — VIEWER (has Read via the grant, but not Update) is
#          denied. The early authz.require(Read) the fix added is
#          only an existence-proof gate; the actual write goes
#          through `update_file_streaming_with_perms`, which
#          independently requires Update. Since the Viewer CAN
#          read the file, `require`'s graduated-denial policy
#          (authorization_ports.rs::require) surfaces this as 403,
#          not the anti-enum 404 — the caller can already see the
#          resource, so hiding its existence leaks nothing new.
#          Before fixing the NC surface's error-mapping bug found
#          via this test (see nextcloud/webdav_handler.rs's PATCH
#          write-step error mapping), this denial leaked as a raw
#          500 instead of the correct 403.
# ─────────────────────────────────────────────────────────────
PATCH {{base_url}}/remote.php/dav/files/{{nc_basic_viewer}}/ncpatch-file.txt
X-Update-Range: bytes=0-2
Content-Type: text/plain
[BasicAuth]
{{nc_basic_viewer}}: {{viewer_nc_password}}
`NOP`

HTTP 403


# ─────────────────────────────────────────────────────────────
# Step A8 — OUTSIDER (no grant at all on this drive) cannot reach
#          the file — denied before PATCH's own logic ever runs.
#          Accept the broader 4xx-non-2xx shape here since the
#          denial may surface at the app-password/session boundary
#          rather than the domain authz layer.
# ─────────────────────────────────────────────────────────────
PATCH {{base_url}}/remote.php/dav/files/{{nc_basic_outsider}}/ncpatch-file.txt
X-Update-Range: bytes=0-2
Content-Type: text/plain
[BasicAuth]
{{nc_basic_outsider}}: {{outsider_nc_password}}
`NOP`

HTTP *
[Asserts]
status >= 400
status < 500


# Cleanup Part A.
DELETE {{base_url}}/webdav/@drive/{{shared_drive_id}}/ncpatch-file.txt
Authorization: Bearer {{admin_jwt}}

HTTP 204

DELETE {{base_url}}/api/auth/app-passwords/{{editor_ap_id}}
Authorization: Bearer {{editor_jwt}}
HTTP 200

DELETE {{base_url}}/api/auth/app-passwords/{{viewer_ap_id}}
Authorization: Bearer {{viewer_jwt}}
HTTP 200

DELETE {{base_url}}/api/auth/app-passwords/{{outsider_ap_id}}
Authorization: Bearer {{outsider_jwt}}
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
DELETE {{base_url}}/api/admin/users/{{editor_user_id}}
Authorization: Bearer {{admin_jwt}}

HTTP 200

DELETE {{base_url}}/api/admin/users/{{viewer_user_id}}
Authorization: Bearer {{admin_jwt}}

HTTP 200

DELETE {{base_url}}/api/drives/{{shared_drive_id}}
Authorization: Bearer {{admin_jwt}}

HTTP 204

DELETE {{base_url}}/api/admin/users/{{outsider_user_id}}
Authorization: Bearer {{admin_jwt}}

HTTP 200

DELETE {{base_url}}/api/admin/users/{{quota_owner_id}}
Authorization: Bearer {{admin_jwt}}

HTTP 200
