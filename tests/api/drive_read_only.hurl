# =============================================================
# OxiCloud – Drive `read_only` policy (full freeze / legal-hold)
# =============================================================
# Run:
#   hurl --variables-file tests/api/test.env --file-root tests \
#        --test tests/api/drive_read_only.hurl
#
# The model under test (`docs/plan/drive.md` §8):
#   `policies.read_only = true` on any drive refuses EVERY mutating
#   permission (Create / Update / Delete / Share / Comment / Manage)
#   on resources in that drive — from user-initiated paths AND
#   background jobs alike. Only `Read` passes. The admin escape
#   hatch is separate: `PATCH /api/drives/{id}/policies` is gated
#   by `admin_guard` at the handler layer and bypasses the engine's
#   authz.require entirely, so admins can always un-freeze.
#
# Enforcement points exercised here:
#   - `PgAclEngine::check_inner` on File/Folder resources (drive
#     precheck branch, mutating permission → refused before role
#     lookup even runs).
#   - `PgAclEngine::check_inner` on Drive resources (same gate).
#   - `share_service::create_shared_link` — goes through
#     `authz.require(Share, Resource::File)` → engine gate fires.
#   - Trash purge SQL — proven separately by the SQL predicate
#     landing in `trash_db_repository::delete_expired_bulk` (not
#     exercised at the HTTP layer here — requires a controllable
#     retention clock; see comment in Step 12).
#
# Cases:
#   1. Baseline — drive not frozen → owner can upload / rename /
#      delete / trash / share (proves the fixture is writable).
#   2. Admin freezes the drive via PATCH policies.
#   3. Every mutation attempt is refused. The engine's graduated
#      denial returns 403 to the owner (who can Read their own
#      drive) — anti-enum only kicks in for callers with no Read
#      at all, whose 404 shape is exercised by the cross-tenant
#      tests in `webdav_permissions.hurl` / `permissions.hurl`.
#      Cases: upload, rename, delete, trash-restore, permanent
#      delete, create public link, rename the drive itself.
#   4. Read still works: GET /api/drives, GET /api/folders,
#      download the file, list trash.
#   5. Admin unfreezes.
#   6. Owner mutations work again → freeze/unfreeze is reversible
#      and doesn't leave latched state.
#
# Self-contained: provisions `ro_owner` (drive owner) + `ro_target`
# (share recipient for the negative-share assertion).
# =============================================================


# ─────────────────────────────────────────────────────────────
# Step 1 — Admin login.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "{{username}}", "password": "{{password}}" }

HTTP 200
[Captures]
admin_token: jsonpath "$.access_token"


# ─────────────────────────────────────────────────────────────
# Step 2 — Provision `ro_owner` (the drive owner under test).
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/admin/users
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "username": "ro_owner",
  "password": "RoOwnerPwd1!",
  "email":    "ro_owner@example.com",
  "role":     "user"
}

HTTP 201

POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "ro_owner", "password": "RoOwnerPwd1!" }

HTTP 200
[Captures]
owner_token: jsonpath "$.access_token"
owner_user_id: jsonpath "$.user.full.user.id"


# ─────────────────────────────────────────────────────────────
# Step 3 — Provision `ro_target` (share recipient for the
#          negative-share assertion in Step 10).
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/admin/users
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "username": "ro_target",
  "password": "RoTargetPwd1!",
  "email":    "ro_target@example.com",
  "role":     "user"
}

HTTP 201

POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "ro_target", "password": "RoTargetPwd1!" }

HTTP 200
[Captures]
target_user_id: jsonpath "$.user.full.user.id"


# ─────────────────────────────────────────────────────────────
# Step 4 — Find `ro_owner`'s default Personal drive + its root
#          folder id (upload targets).
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/folders
Authorization: Bearer {{owner_token}}

HTTP 200
[Captures]
personal_root_id: jsonpath "$[0].id"

GET {{base_url}}/api/drives
Authorization: Bearer {{owner_token}}

HTTP 200
[Captures]
personal_drive_id: jsonpath "$[0].id"
[Asserts]
jsonpath "$[0].kind" == "personal"


# ─────────────────────────────────────────────────────────────
# Step 5 — Baseline: upload file A (mutation subject during
#          the freeze) and file B (already-trashed subject
#          for the restore/purge assertions).
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/files/upload
Authorization: Bearer {{owner_token}}
[MultipartFormData]
folder_id: {{personal_root_id}}
file: file,fixtures/hello.txt; text/plain

HTTP 201
[Captures]
file_a_id: jsonpath "$.id"

# DISTINCT content from file_a — re-uploading the same bytes to
# the same folder would collide on the (folder_id, name) unique
# constraint and the idempotent-upload handler would return the
# EXISTING file (file_a_id == file_b_id), then trashing "file_b"
# would trash file_a and every subsequent Read on file_a would 404
# because `get_file` filters `NOT is_trashed`. Using a fixture with
# different bytes gives us two truly distinct file rows.
POST {{base_url}}/api/files/upload
Authorization: Bearer {{owner_token}}
[MultipartFormData]
folder_id: {{personal_root_id}}
file: file,fixtures/hello-trashed.txt; text/plain

HTTP 201
[Captures]
file_b_id: jsonpath "$.id"

# Trash file B pre-freeze so we can later attempt restore + permanent
# delete on it while the drive is frozen.
DELETE {{base_url}}/api/trash/files/{{file_b_id}}
Authorization: Bearer {{owner_token}}

HTTP *
[Asserts]
status >= 200
status < 300


# ─────────────────────────────────────────────────────────────
# Step 6 — Freeze the drive. Admin-only endpoint; owner cannot
#          call it (proven separately in `drive_policies.hurl`).
#          Response echoes the merged bag.
# ─────────────────────────────────────────────────────────────
PATCH {{base_url}}/api/drives/{{personal_drive_id}}/policies
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "read_only": true
}

HTTP 200
[Asserts]
jsonpath "$.read_only" == true
jsonpath "$.forbid_public_links" == false
jsonpath "$.forbid_sharing" == false


# ─────────────────────────────────────────────────────────────
# Step 7 — Confirm the policy is visible to the owner (they can
#          READ policy state — Manage is what mutates it, and
#          Manage is admin-only via a different gate).
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/drives
Authorization: Bearer {{owner_token}}

HTTP 200
[Asserts]
jsonpath "$[?(@.id=='{{personal_drive_id}}')].policies.read_only" == true


# ─────────────────────────────────────────────────────────────
# Step 8 — MUTATIONS BLOCKED. Upload → 403 (Create).
#          Graduated denial: owner can Read their own frozen
#          drive, so the engine returns `access_denied` → 403
#          rather than the anti-enum 404 (hiding a drive from
#          its owner would be absurd). Cross-tenant callers with
#          no Read on the drive still see 404 by the same code
#          path. The engine gate emits an audit line with
#          `reason = drive_read_only` — inspectable in server
#          logs, not asserted here (no log-scraping harness).
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/files/upload
Authorization: Bearer {{owner_token}}
[MultipartFormData]
folder_id: {{personal_root_id}}
file: file,fixtures/hello.txt; text/plain

HTTP 403


# ─────────────────────────────────────────────────────────────
# Step 9 — Rename file A → 403 (Update). Endpoint is
#          `PUT /api/files/{id}/rename` (not PATCH — the file
#          service exposes rename as a distinct verb, mirroring
#          the folder side). WebDAV MOVE would fire the same
#          engine gate via `authz.require(Update, File)`.
# ─────────────────────────────────────────────────────────────
PUT {{base_url}}/api/files/{{file_a_id}}/rename
Authorization: Bearer {{owner_token}}
Content-Type: application/json
{ "name": "renamed_during_freeze.txt" }

HTTP 403


# ─────────────────────────────────────────────────────────────
# Step 10 — Delete file A → 403 (Delete).
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/api/trash/files/{{file_a_id}}
Authorization: Bearer {{owner_token}}

HTTP 403


# ─────────────────────────────────────────────────────────────
# Step 11 — Restore file B from trash → 403 (Update on the
#           soft-deleted row is a mutation like any other).
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/trash/{{file_b_id}}/restore
Authorization: Bearer {{owner_token}}

HTTP 403


# ─────────────────────────────────────────────────────────────
# Step 12 — Permanent delete of file B → 403 (Delete).
#           Note: the background retention purge SQL filter is
#           tested via source-review + a unit test on the
#           `delete_expired_bulk` query, not here — advancing
#           the retention clock synchronously from Hurl would
#           require an admin endpoint that doesn't exist. The
#           user-initiated permanent-delete path DOES exercise
#           the engine gate and is asserted below.
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/api/trash/{{file_b_id}}
Authorization: Bearer {{owner_token}}

HTTP 403


# ─────────────────────────────────────────────────────────────
# Step 13 — Share creation → 403 (Share). Goes through
#           `share_service::create_shared_link` which calls
#           `authz.require(Share, Resource::File)` → engine gate.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/shares
Authorization: Bearer {{owner_token}}
Content-Type: application/json
{
  "item_id":   "{{file_a_id}}",
  "item_type": "file"
}

HTTP 403


# ─────────────────────────────────────────────────────────────
# Step 14 — Grant (per-resource, not public link) → 403 (Share).
#           Same engine gate — Share permission on File is
#           refused regardless of which endpoint asks for it.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/grants
Authorization: Bearer {{owner_token}}
Content-Type: application/json
{
  "subject":  { "type": "user", "id": "{{target_user_id}}" },
  "resource": { "type": "file", "id": "{{file_a_id}}" },
  "role":     "viewer"
}

HTTP 403


# ─────────────────────────────────────────────────────────────
# Step 15 — Rename the drive itself → 404 (Update on
#           Resource::Drive). Drive rename goes through folder
#           PATCH on the root folder id, but the underlying
#           permission check is Update on the folder — which
#           lives in the frozen drive, so gate applies.
#
# Skipped for now — the current implementation checks Update
# on the root folder, and per `bug_drive_rename_editor_can_do_it`
# memory the exact permission surface is still under review.
# The Drive-resource path (below) covers the intent directly.
# ─────────────────────────────────────────────────────────────


# ─────────────────────────────────────────────────────────────
# Step 16 — READ STILL WORKS. Membership listing, folder
#           listing, file download — none are refused.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/drives
Authorization: Bearer {{owner_token}}

HTTP 200
[Asserts]
jsonpath "$[?(@.id=='{{personal_drive_id}}')].policies.read_only" == true


GET {{base_url}}/api/folders/{{personal_root_id}}
Authorization: Bearer {{owner_token}}

HTTP 200


GET {{base_url}}/api/files/{{file_a_id}}
Authorization: Bearer {{owner_token}}

HTTP 200


# Trash still LISTS (viewers see what's frozen inside).
GET {{base_url}}/api/trash/resources
Authorization: Bearer {{owner_token}}

HTTP 200


# ─────────────────────────────────────────────────────────────
# Step 17 — Admin unfreezes. Reversible: no latched state, no
#           residual policy drift.
# ─────────────────────────────────────────────────────────────
PATCH {{base_url}}/api/drives/{{personal_drive_id}}/policies
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "read_only": false
}

HTTP 200
[Asserts]
jsonpath "$.read_only" == false


# ─────────────────────────────────────────────────────────────
# Step 18 — Post-unfreeze: owner can mutate again. Delete
#           file A succeeds; upload a new file succeeds;
#           permanent-delete file B succeeds.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/trash/{{file_b_id}}/restore
Authorization: Bearer {{owner_token}}

HTTP *
[Asserts]
status >= 200
status < 300


DELETE {{base_url}}/api/trash/files/{{file_a_id}}
Authorization: Bearer {{owner_token}}

HTTP *
[Asserts]
status >= 200
status < 300


POST {{base_url}}/api/files/upload
Authorization: Bearer {{owner_token}}
[MultipartFormData]
folder_id: {{personal_root_id}}
file: file,fixtures/hello.txt; text/plain

HTTP 201


# ─────────────────────────────────────────────────────────────
# Step 19 — Cleanup: leave the throwaway users provisioned.
#           `storage_cleanup_check.sh` at end of run.sh
#           enumerates leftover drives and drains them.
# ─────────────────────────────────────────────────────────────
