# =============================================================
# OxiCloud — restoring an item whose name is now taken must 409
# =============================================================
# Pin for the trash-restore-into-live-collision case: a user
# trashes "foo", creates a NEW "foo" at the same location, then
# requests restore of the original trashed "foo". The UPDATE
# `is_trashed = FALSE, folder_id = COALESCE(original_folder_id,
# folder_id)` collides on the files `(folder_id, name)` unique
# constraint (or `idx_folders_unique_name` for the folder case).
# Must surface HTTP 409 + `error_type: AlreadyExists` so the SPA
# renders the friendly "An item with that name is already there"
# copy — NOT 500 InternalError.
#
# ── History of the bugs this pin guards ──
#
# Three-layer bug, same shape as the folder rename/move chain
# reported on 2026-11-01:
#
# 1. REPO (file + folder) — `restore_from_trash` on both
#    `file_blob_write_repository.rs` and `folder_db_repository.rs`
#    was missing the PG `23505` → `AlreadyExists` translation that
#    `rename_file`/`rename_folder` had but `move_file`/`move_folder`
#    had not until commit 8fcf8310. Fix adds the same translation
#    pattern.
#
# 2. SERVICE — `trash_service.rs::restore_item` wrapped every repo
#    error as `DomainError::new(ErrorKind::InternalError, ...)`
#    for BOTH the file and folder branches, discarding the kind
#    the repo set. Same bug class as the pre-fix
#    `folder_service::rename_folder_with_perms` (commit 9043d1a9).
#    Fix preserves `e.kind`.
#
# The three layers must ALL be correct for the user to see a 409:
# a regression in any one of them re-breaks the UX.
# =============================================================


# ─────────────────────────────────────────────────────────────
# Step 1 — Admin login.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "{{username}}", "password": "{{password}}" }

HTTP 200
[Captures]
admin_token:   jsonpath "$.access_token"


# ─────────────────────────────────────────────────────────────
# Step 2 — Admin's home drive root.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/drives
Authorization: Bearer {{admin_token}}

HTTP 200
[Captures]
home_root_id: jsonpath "$[0].root_folder_id"


# ─────────────────────────────────────────────────────────────
# Step 3 — Create a working folder `trd-root` under home so the
#          test's fixtures don't collide with other hurl runs.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/folders
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "name": "trd-root", "parent_id": "{{home_root_id}}" }

HTTP 201
[Captures]
work_folder_id: jsonpath "$.id"


# =============================================================
# ── Case A: file restore vs live file of the same name ──
# =============================================================


# ─────────────────────────────────────────────────────────────
# Step 4 — Upload the original `foo` file into `trd-root`.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/files/upload
Authorization: Bearer {{admin_token}}
[MultipartFormData]
folder_id: {{work_folder_id}}
file: file,fixtures/hello.txt; text/plain

HTTP 201
[Captures]
orig_file_id: jsonpath "$.id"


# ─────────────────────────────────────────────────────────────
# Step 5 — Trash it. Soft-delete; the row survives with
#          `is_trashed = TRUE` and `original_folder_id = <work>`.
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/api/files/{{orig_file_id}}
Authorization: Bearer {{admin_token}}

HTTP 204


# ─────────────────────────────────────────────────────────────
# Step 6a — Upload a second file into the same `trd-root`. Hurl's
#           multipart `name:` field can't rewrite the uploaded
#           filename (that comes from the Content-Disposition
#           header which Hurl derives from the fixture path), so
#           we upload under the fixture's own name here, then
#           rename to collide in Step 6b. The partial unique
#           index at `migrations/20260802100002:73` is scoped on
#           `WHERE NOT is_trashed`, so the rename-to-collide-with-
#           trashed path succeeds — only the restore flip
#           `is_trashed TRUE → FALSE` is what tips the collision
#           over.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/files/upload
Authorization: Bearer {{admin_token}}
[MultipartFormData]
folder_id: {{work_folder_id}}
file: file,fixtures/hello-copy.txt; text/plain

HTTP 201
[Captures]
live_file_id: jsonpath "$.id"


# ─────────────────────────────────────────────────────────────
# Step 6b — Rename the live file to the same name as the trashed
#           one. Succeeds because the partial unique index skips
#           the trashed row — the live row is the only one indexed.
# ─────────────────────────────────────────────────────────────
PUT {{base_url}}/api/files/{{live_file_id}}/rename
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "name": "hello.txt" }

HTTP 200


# ─────────────────────────────────────────────────────────────
# Step 7 — PIN: restore the trashed `hello.txt`. Must 409
#          AlreadyExists on the (folder_id, name) collision.
#          Pre-fix: 500 InternalError.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/trash/{{orig_file_id}}/restore
Authorization: Bearer {{admin_token}}

HTTP 409
[Asserts]
jsonpath "$.error_type" == "AlreadyExists"
jsonpath "$.status"     == "409 Conflict"


# =============================================================
# ── Case B: folder restore vs live folder of the same name ──
# =============================================================


# ─────────────────────────────────────────────────────────────
# Step 8 — Create folder `bar` under `trd-root`.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/folders
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "name": "bar", "parent_id": "{{work_folder_id}}" }

HTTP 201
[Captures]
orig_bar_id: jsonpath "$.id"


# ─────────────────────────────────────────────────────────────
# Step 9 — Trash the `bar` folder.
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/api/folders/{{orig_bar_id}}
Authorization: Bearer {{admin_token}}

HTTP 204


# ─────────────────────────────────────────────────────────────
# Step 10 — Create a NEW `bar` under the same parent so the
#           unique slot the trashed folder wants back is occupied.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/folders
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "name": "bar", "parent_id": "{{work_folder_id}}" }

HTTP 201
[Captures]
live_bar_id: jsonpath "$.id"


# ─────────────────────────────────────────────────────────────
# Step 11 — PIN: restore the trashed `bar` folder. Must 409
#           AlreadyExists on `idx_folders_unique_name`. Pre-fix:
#           500 InternalError.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/trash/{{orig_bar_id}}/restore
Authorization: Bearer {{admin_token}}

HTTP 409
[Asserts]
jsonpath "$.error_type" == "AlreadyExists"
jsonpath "$.status"     == "409 Conflict"


# ─────────────────────────────────────────────────────────────
# Step 12 — Teardown. Delete the live rows, drop the work
#           folder (which cascades the two still-trashed items
#           anyway — they're descendants of `trd-root`), then
#           empty trash.
#           Note: the trashed `orig_file_id` and `orig_bar_id`
#           are already soft-deleted; the DELETE on `work_folder_id`
#           moves it + its live children to trash. `empty_trash`
#           purges everything.
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/api/files/{{live_file_id}}
Authorization: Bearer {{admin_token}}

HTTP 204


DELETE {{base_url}}/api/folders/{{live_bar_id}}
Authorization: Bearer {{admin_token}}

HTTP 204


DELETE {{base_url}}/api/folders/{{work_folder_id}}
Authorization: Bearer {{admin_token}}

HTTP 204


DELETE {{base_url}}/api/trash/empty
Authorization: Bearer {{admin_token}}

HTTP 200
