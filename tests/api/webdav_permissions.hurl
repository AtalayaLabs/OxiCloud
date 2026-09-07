# =============================================================
# OxiCloud — WebDAV per-role permissions + cross-drive MOVE policy
# =============================================================
# End-to-end coverage for the two WebDAV authz axes exposed by the
# `@drive` URL scheme:
#
#   1. Per-role gates through the drive-scope resolver: a Viewer on a
#      shared drive can PROPFIND/GET but cannot MKCOL/PUT/MOVE. An
#      Editor can. AuthZ denials use graduated shape: a caller with
#      Read on the target (Viewer here) gets 403 Forbidden — no point
#      hiding existence from someone already reading it. A caller with
#      no Read at all gets 404 (anti-enum), matching "no such folder".
#
#   2. Drive policy `forbid_cross_drive_move` gates MOVE at the
#      SOURCE drive (see `DrivePolicies::refuse_cross_drive_move`
#      in `src/domain/entities/drive.rs`) — even a fully-authorised
#      Editor can't move content OUT of a drive whose owner has
#      forbidden cross-drive movement. Rejection is 405
#      (`ErrorKind::UnsupportedOperation` → `METHOD_NOT_ALLOWED`).
#
# Assumes the default `OXICLOUD_WEBDAV_DRIVE_LISTING_PREFIX="@drive"` config —
# runs alongside the other tests in `tests/api/run.sh`. Uses the
# `@drive/<uuid>` selector so the paths don't collide with any
# drive-name-collision oddities.
# =============================================================


# ─────────────────────────────────────────────────────────────
# Step 1 — Login as admin (bootstrapped by `setup.hurl`).
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "{{username}}", "password": "{{password}}" }

HTTP 200
[Captures]
admin_token: jsonpath "$.access_token"
admin_user_id: jsonpath "$.user.full.user.id"


# ─────────────────────────────────────────────────────────────
# Step 2 — Create a fresh user "webdav_bob" via the admin
#          endpoint, log him in.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/admin/users
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "username": "webdav_bob",
  "password": "WebdavBobPassword1!",
  "email": "webdav_bob@example.com",
  "role": "user"
}

HTTP 201
[Captures]
bob_user_id: jsonpath "$.user.id"


POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "webdav_bob", "password": "WebdavBobPassword1!" }

HTTP 200
[Captures]
bob_token: jsonpath "$.access_token"


# Capture Bob's default personal drive id — used by the cross-drive
# MOVE scenario. Bob is not a member of any shared drive yet, so his
# `/api/drives` listing has exactly one entry (his own default).
GET {{base_url}}/api/drives
Authorization: Bearer {{bob_token}}

HTTP 200
[Captures]
bob_personal_drive_id: jsonpath "$[0].id"


# ─────────────────────────────────────────────────────────────
# Step 3 — Admin creates a shared drive owned by admin.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/drives
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "kind": "shared",
  "name": "webdav-perm-shared",
  "owner": { "type": "user", "id": "{{admin_user_id}}" }
}

HTTP 201
[Captures]
shared_drive_id: jsonpath "$.id"
shared_root_id:  jsonpath "$.root_folder_id"


# ─────────────────────────────────────────────────────────────
# Step 4 — Grant Bob VIEWER on the shared drive via /api/grants.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/grants
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "subject":  { "type": "user",  "id": "{{bob_user_id}}" },
  "resource": { "type": "drive", "id": "{{shared_drive_id}}" },
  "role": "viewer"
}

HTTP 201


# ─────────────────────────────────────────────────────────────
# Step 5 — Bob (VIEWER) CAN PROPFIND the shared drive root.
#          Depth 0 to keep the assertion minimal; a 207 with the
#          drive's own href suffices as "Bob has Read".
# ─────────────────────────────────────────────────────────────
PROPFIND {{base_url}}/webdav/@drive/{{shared_drive_id}}/
Authorization: Bearer {{bob_token}}
Depth: 0

HTTP 207


# ─────────────────────────────────────────────────────────────
# Step 6 — Bob (VIEWER) CANNOT MKCOL on the shared drive.
#          `authz.require(Create, Folder)` denies. Bob has Read
#          on the drive (viewer role) → engine's graduated denial
#          returns `DomainError::access_denied` → 403 Forbidden.
#          Anti-enum still holds for callers with no Read at all
#          (would surface as 404); this is the "you can see it,
#          but can't touch it" branch.
# ─────────────────────────────────────────────────────────────
MKCOL {{base_url}}/webdav/@drive/{{shared_drive_id}}/viewer-blocked-folder
Authorization: Bearer {{bob_token}}

HTTP 403


# ─────────────────────────────────────────────────────────────
# Step 7 — Bob (VIEWER) CANNOT PUT a file. Same 403 shape
#          (Bob has Read on the drive).
# ─────────────────────────────────────────────────────────────
PUT {{base_url}}/webdav/@drive/{{shared_drive_id}}/viewer-blocked-file.txt
Authorization: Bearer {{bob_token}}
Content-Type: text/plain
```
viewer should not upload
```

HTTP 403


# ─────────────────────────────────────────────────────────────
# Step 8 — Admin creates a probe folder in the shared drive so
#          the Editor-can-rename step below has a real target.
# ─────────────────────────────────────────────────────────────
MKCOL {{base_url}}/webdav/@drive/{{shared_drive_id}}/probe-folder
Authorization: Bearer {{admin_token}}

HTTP 201


# ─────────────────────────────────────────────────────────────
# Step 9 — Bob (VIEWER) CANNOT MOVE (rename) the probe folder.
#          MOVE requires Update on the source, which Viewer
#          doesn't have. Bob can Read the folder (viewer) → 403.
# ─────────────────────────────────────────────────────────────
MOVE {{base_url}}/webdav/@drive/{{shared_drive_id}}/probe-folder
Authorization: Bearer {{bob_token}}
Destination: {{base_url}}/webdav/@drive/{{shared_drive_id}}/probe-folder-renamed

HTTP 403


# ─────────────────────────────────────────────────────────────
# Step 9b — Bob (VIEWER) CANNOT COPY the probe folder.
#           COPY requires Create on the destination parent, which
#           Viewer doesn't have. Bob has Read on both source and
#           destination parent → 403 (graduated denial).
#
#           This is the regression pin for AuthZ audit #2
#           (2026-07-12): the COPY handler used to `map_err(|e|
#           AppError::internal_error(format!("Failed to copy folder
#           tree: {}", e)))?` on `copy_folder_tree_with_perms`,
#           collapsing the `DomainError` engine returned on denial
#           into HTTP 500 — an "exists-but-denied" oracle. Fix
#           routes through `AppError::from` so the same denial
#           surfaces as the correct 403 / 404 per graduated-denial
#           policy.
# ─────────────────────────────────────────────────────────────
COPY {{base_url}}/webdav/@drive/{{shared_drive_id}}/probe-folder
Authorization: Bearer {{bob_token}}
Destination: {{base_url}}/webdav/@drive/{{shared_drive_id}}/probe-folder-copy

HTTP 403


# ─────────────────────────────────────────────────────────────
# Step 9c — Bob (VIEWER) CANNOT DELETE the probe folder.
#           DELETE requires Delete on the target, which Viewer
#           doesn't have. Bob has Read → 403.
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/webdav/@drive/{{shared_drive_id}}/probe-folder
Authorization: Bearer {{bob_token}}

HTTP 403


# ─────────────────────────────────────────────────────────────
# Step 10 — Promote Bob from VIEWER to EDITOR.
#           `PATCH /api/drives/{id}/members/{subject-type}/{id}`
#           mutates the role in-place.
# ─────────────────────────────────────────────────────────────
PATCH {{base_url}}/api/drives/{{shared_drive_id}}/members/user/{{bob_user_id}}
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "role": "editor" }

HTTP 200


# ─────────────────────────────────────────────────────────────
# Step 11 — Bob (EDITOR) CAN MKCOL a new folder.
# ─────────────────────────────────────────────────────────────
MKCOL {{base_url}}/webdav/@drive/{{shared_drive_id}}/editor-created-folder
Authorization: Bearer {{bob_token}}

HTTP 201


# ─────────────────────────────────────────────────────────────
# Step 12 — Bob (EDITOR) CAN PUT a file.
# ─────────────────────────────────────────────────────────────
PUT {{base_url}}/webdav/@drive/{{shared_drive_id}}/editor-created-folder/hello.txt
Authorization: Bearer {{bob_token}}
Content-Type: text/plain
```
editor uploaded content
```

HTTP 201


# ─────────────────────────────────────────────────────────────
# Step 13 — Bob (EDITOR) CAN MOVE (rename) the probe folder.
# ─────────────────────────────────────────────────────────────
MOVE {{base_url}}/webdav/@drive/{{shared_drive_id}}/probe-folder
Authorization: Bearer {{bob_token}}
Destination: {{base_url}}/webdav/@drive/{{shared_drive_id}}/probe-folder-renamed

HTTP 201


# ─────────────────────────────────────────────────────────────
# Step 14 — Bob puts a file in his OWN personal drive as the
#           source for the cross-drive MOVE test below.
# ─────────────────────────────────────────────────────────────
PUT {{base_url}}/webdav/xdrive-probe.txt
Authorization: Bearer {{bob_token}}
Content-Type: text/plain
```
cross-drive probe payload
```

HTTP 201


# ─────────────────────────────────────────────────────────────
# Step 15 — Admin flips `forbid_cross_drive_move` ON for Bob's
#           PERSONAL drive. The policy sits on the SOURCE drive
#           per `DrivePolicies::refuse_cross_drive_move`; only
#           OxiCloud-admin can PATCH policies.
# ─────────────────────────────────────────────────────────────
PATCH {{base_url}}/api/drives/{{bob_personal_drive_id}}/policies
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "forbid_cross_drive_move": true }

HTTP 200
[Asserts]
jsonpath "$.forbid_cross_drive_move" == true


# ─────────────────────────────────────────────────────────────
# Step 16 — Bob tries to MOVE `xdrive-probe.txt` from his
#           PERSONAL drive to the SHARED drive. Blocked at the
#           service layer by the policy — `OperationNotSupported`
#           maps to 405 Method Not Allowed.
# ─────────────────────────────────────────────────────────────
MOVE {{base_url}}/webdav/xdrive-probe.txt
Authorization: Bearer {{bob_token}}
Destination: {{base_url}}/webdav/@drive/{{shared_drive_id}}/xdrive-probe.txt

HTTP 405


# ─────────────────────────────────────────────────────────────
# Step 17 — Admin flips the policy OFF.
# ─────────────────────────────────────────────────────────────
PATCH {{base_url}}/api/drives/{{bob_personal_drive_id}}/policies
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "forbid_cross_drive_move": false }

HTTP 200
[Asserts]
jsonpath "$.forbid_cross_drive_move" == false


# ─────────────────────────────────────────────────────────────
# Step 18 — Bob retries the same MOVE. Now the policy is off,
#           Bob has Update on source (his own personal drive) +
#           Create on dest parent (Editor on shared drive), so
#           the move succeeds. 201 on rename/move to a new URL,
#           per `handle_move`'s existing convention.
# ─────────────────────────────────────────────────────────────
MOVE {{base_url}}/webdav/xdrive-probe.txt
Authorization: Bearer {{bob_token}}
Destination: {{base_url}}/webdav/@drive/{{shared_drive_id}}/xdrive-probe.txt

HTTP 201


# ─────────────────────────────────────────────────────────────
# Step 19 — Verify the destination now exists and the source
#           is gone. Both PROPFINDs use Bob's token to also
#           re-confirm the AuthZ gates on the destination side.
# ─────────────────────────────────────────────────────────────
PROPFIND {{base_url}}/webdav/@drive/{{shared_drive_id}}/xdrive-probe.txt
Authorization: Bearer {{bob_token}}
Depth: 0

HTTP 207


PROPFIND {{base_url}}/webdav/xdrive-probe.txt
Authorization: Bearer {{bob_token}}
Depth: 0

HTTP 404
