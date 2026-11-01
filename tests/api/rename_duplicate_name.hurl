# =============================================================
# OxiCloud — rename-to-duplicate-name must surface AlreadyExists
# =============================================================
# Pins the server-side wire shape on duplicate-name renames for
# BOTH folders and files. Pre-fix (reported by Ed on 2026-11-01):
#
#   PUT /api/folders/<id>/rename → { "name": "<existing_name>" }
#   HTTP 500 Internal Server Error
#   {
#     "error_type": "InternalError",
#     "message": "An internal error occurred. Please try again later."
#   }
#
# `folder_service.rs::rename_folder_with_perms` and
# `move_folder_with_perms` both wrapped the repository's
# `AlreadyExists` DomainError into a NEW `InternalError`
# DomainError via `map_err(|e| DomainError::internal_error(...))`,
# which discarded the `kind` and collapsed every repo error to 500.
# `From<DomainError> for AppError` then saw `InternalError` and
# returned 500 instead of the correct 409 `error_type: AlreadyExists`.
#
# Fix preserves the inner `kind`:
#   DomainError::new(e.kind, "FolderStorage", format!("..."))
#
# ── The pin ──
# Two folders at the drive root — rename one to the other's name.
# Two files in a folder — rename one to the other's name.
# Both must return HTTP 409 + `error_type: AlreadyExists` so the
# SPA's `errors.ts::messageForErrorType` surfaces the friendly
# "An item with that name is already there" copy.
# =============================================================


# ─────────────────────────────────────────────────────────────
# Step 1 — Admin login.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "{{username}}", "password": "{{password}}" }

HTTP 200
[Captures]
admin_token:    jsonpath "$.access_token"
admin_user_id:  jsonpath "$.user.full.user.id"


# ─────────────────────────────────────────────────────────────
# Step 2 — Resolve the admin's home drive root — the rename test
#          lives in the admin's own personal drive to avoid
#          needing a shared-drive fixture.
#
# `GET /api/drives` returns the admin's drives default-first; the
# personal drive is `$[0]`. Same selector `drives_foundation.hurl`
# and `drive_quota.hurl` use.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/drives
Authorization: Bearer {{admin_token}}

HTTP 200
[Captures]
home_root_id: jsonpath "$[0].root_folder_id"


# ─────────────────────────────────────────────────────────────
# Step 3 — Create folder `rdn-sibling-a` under home.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/folders
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "name": "rdn-sibling-a", "parent_id": "{{home_root_id}}" }

HTTP 201
[Captures]
folder_a_id: jsonpath "$.id"


# ─────────────────────────────────────────────────────────────
# Step 4 — Create folder `rdn-sibling-b` under the same parent.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/folders
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "name": "rdn-sibling-b", "parent_id": "{{home_root_id}}" }

HTTP 201
[Captures]
folder_b_id: jsonpath "$.id"


# ─────────────────────────────────────────────────────────────
# Step 5 — THE PIN (folder): rename `rdn-sibling-b` → `rdn-sibling-a`.
#          Pre-fix: 500 InternalError. Post-fix: 409 AlreadyExists.
# ─────────────────────────────────────────────────────────────
PUT {{base_url}}/api/folders/{{folder_b_id}}/rename
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "name": "rdn-sibling-a" }

HTTP 409
[Asserts]
jsonpath "$.error_type" == "AlreadyExists"
jsonpath "$.status"     == "409 Conflict"
# Operator breadcrumb preserved — the server-side log sees the ID
# that failed; the SPA switches on `error_type` and renders the
# friendly i18n copy, so the operator-flavoured `message` is
# fine to be noisy here.
jsonpath "$.message"    contains "rdn-sibling-a"


# ─────────────────────────────────────────────────────────────
# Step 6 — Upload two files into `folder_a` so we can repeat the
#          rename-to-duplicate pin on the file surface.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/files/upload
Authorization: Bearer {{admin_token}}
[MultipartFormData]
folder_id: {{folder_a_id}}
file: file,fixtures/hello.txt; text/plain

HTTP 201
[Captures]
file_x_id: jsonpath "$.id"


POST {{base_url}}/api/files/upload
Authorization: Bearer {{admin_token}}
[MultipartFormData]
folder_id: {{folder_a_id}}
file: file,fixtures/hello-copy.txt; text/plain

HTTP 201
[Captures]
file_y_id: jsonpath "$.id"


# ─────────────────────────────────────────────────────────────
# Step 7 — THE PIN (file): rename `hello-copy.txt` → `hello.txt`.
#          Mirror of step 5 on the file surface. Not currently
#          affected by the service-layer bug (file_management
#          _service::rename_file preserves `e` unchanged in its
#          map_err), but the file surface shares the same user-
#          facing contract — if a future refactor introduces the
#          same error-wrap pattern on files, this pin trips.
# ─────────────────────────────────────────────────────────────
PUT {{base_url}}/api/files/{{file_y_id}}/rename
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "name": "hello.txt" }

HTTP 409
[Asserts]
jsonpath "$.error_type" == "AlreadyExists"
jsonpath "$.status"     == "409 Conflict"


# ─────────────────────────────────────────────────────────────
# Step 8 — Teardown. Delete the two fixture files, then the two
#          fixture folders, so the pin stays idempotent across
#          repeated runs in the shared sandbox DB
#          (feedback_hurl_teardown_shared_db).
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/api/files/{{file_x_id}}
Authorization: Bearer {{admin_token}}

HTTP 204


DELETE {{base_url}}/api/files/{{file_y_id}}
Authorization: Bearer {{admin_token}}

HTTP 204


DELETE {{base_url}}/api/folders/{{folder_a_id}}
Authorization: Bearer {{admin_token}}

HTTP 204


DELETE {{base_url}}/api/folders/{{folder_b_id}}
Authorization: Bearer {{admin_token}}

HTTP 204


# Purge trash so the folders don't bump a later `empty_trash`
# assertion's counter in a different test.
DELETE {{base_url}}/api/trash/empty
Authorization: Bearer {{admin_token}}

HTTP 200
