# =============================================================
# OxiCloud — duplicate-name must surface 409 + AlreadyExists
# =============================================================
# One pin covering all four duplicate-name mutations the SPA can
# issue — rename × {folder, file} and move × {folder, file}. Each
# must return HTTP 409 + `error_type: AlreadyExists` so the SPA's
# `errors.ts::messageForErrorType` renders the friendly i18n copy
# "An item with that name is already there" instead of the opaque
# "An internal error occurred" fallback.
#
# ── History of the bugs this pin guards ──
#
# Reported by Ed on 2026-11-01, in order of discovery:
#
# 1. Folder RENAME to a duplicate sibling name returned 500. The
#    service layer `folder_service.rs::rename_folder_with_perms`
#    wrapped every repo error as InternalError via
#    `map_err(|e| DomainError::internal_error(...))`, discarding
#    the `AlreadyExists` kind the repo already set. The repo's
#    `rename_folder` DID translate PG `23505` →
#    `DomainError::already_exists`, but the service threw the kind
#    away before the handler ran `From<DomainError> for AppError`.
#    Fix: `DomainError::new(e.kind, "FolderStorage", ...)`.
#
# 2. Folder MOVE to a parent that already holds a sibling of that
#    name ALSO returned 500 — but for a DIFFERENT reason. The
#    repo's `move_folder` wasn't translating `23505` at all
#    (unlike `rename_folder`, which was), so the inner DomainError
#    was ALREADY `InternalError`. The service-layer fix above
#    preserved the kind faithfully — but the kind had already been
#    lost one layer deeper. Fix: added the `23505` → `AlreadyExists`
#    translation to `folder_db_repository.rs::move_folder`.
#
# 3. File MOVE to a folder that already holds a file of that name:
#    same bug class as (2). The file repo's `rename_file` DID
#    translate `23505`, but `move_file` didn't. Fix mirrored.
#
# ── The pin ──
#
# Fixtures sit under the admin's own personal drive root to avoid
# needing a shared-drive setup. Four assertions, each a distinct
# code path through the service + repo layers:
#
#   Step 5  — folder rename to sibling name     → 409 AlreadyExists
#   Step 7  — file rename to sibling name       → 409 AlreadyExists
#   Step 8  — folder move into dup-holding dest → 409 AlreadyExists
#   Step 9  — file move into dup-holding dest   → 409 AlreadyExists
#
# A regression that reinstates ANY of the InternalError-swallowing
# patterns above would trip the matching step with HTTP 500.
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
# Step 2 — Admin's home drive root. Same selector pattern
#          `drives_foundation.hurl` and `drive_quota.hurl` use.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/drives
Authorization: Bearer {{admin_token}}

HTTP 200
[Captures]
home_root_id: jsonpath "$[0].root_folder_id"


# ─────────────────────────────────────────────────────────────
# Step 3 — Create folder `dnc-sibling-a` under home.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/folders
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "name": "dnc-sibling-a", "parent_id": "{{home_root_id}}" }

HTTP 201
[Captures]
folder_a_id: jsonpath "$.id"


# ─────────────────────────────────────────────────────────────
# Step 4 — Create folder `dnc-sibling-b` under the same parent.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/folders
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "name": "dnc-sibling-b", "parent_id": "{{home_root_id}}" }

HTTP 201
[Captures]
folder_b_id: jsonpath "$.id"


# ─────────────────────────────────────────────────────────────
# Step 5 — PIN (folder rename): rename B → "dnc-sibling-a".
#          Guards fix #1 (service-layer kind preservation).
# ─────────────────────────────────────────────────────────────
PUT {{base_url}}/api/folders/{{folder_b_id}}/rename
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "name": "dnc-sibling-a" }

HTTP 409
[Asserts]
jsonpath "$.error_type" == "AlreadyExists"
jsonpath "$.status"     == "409 Conflict"
jsonpath "$.message"    contains "dnc-sibling-a"


# ─────────────────────────────────────────────────────────────
# Step 6 — Upload two files into `folder_a` so we can repeat the
#          duplicate-name assertions on the file surface. The
#          byte-size suffixes stop them colliding with other
#          hurl runs that touch the same drive.
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
# Step 7 — PIN (file rename): rename `hello-copy.txt` → `hello.txt`.
#          Guards the `rename_file` 23505 translation that was
#          already in place pre-regression (defensive pin — if a
#          future refactor copies the `move_file` pattern over the
#          rename site, this trips).
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
# Step 8 — PIN (folder move): create a child of `folder_b` named
#          `shared-child`, then create a child of `folder_a` also
#          named `shared-child`, then try to move the `folder_a`
#          version INTO `folder_b` — would collide on the PG
#          `idx_folders_unique_name` constraint. Pre-fix: 500.
#          Post-fix: 409 AlreadyExists.
#          Guards fix #2 (repo-layer `move_folder` 23505 translation).
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/folders
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "name": "shared-child", "parent_id": "{{folder_b_id}}" }

HTTP 201


POST {{base_url}}/api/folders
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "name": "shared-child", "parent_id": "{{folder_a_id}}" }

HTTP 201
[Captures]
movable_folder_id: jsonpath "$.id"


PUT {{base_url}}/api/folders/{{movable_folder_id}}/move
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "parent_id": "{{folder_b_id}}" }

HTTP 409
[Asserts]
jsonpath "$.error_type" == "AlreadyExists"
jsonpath "$.status"     == "409 Conflict"


# ─────────────────────────────────────────────────────────────
# Step 9 — PIN (file move): upload `hello.txt` into `folder_b`,
#          then try to move `folder_a`'s `hello.txt` into
#          `folder_b` — collides on `(folder_id, name)` unique.
#          Pre-fix: 500. Post-fix: 409 AlreadyExists.
#          Guards fix #3 (repo-layer `move_file` 23505 translation).
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/files/upload
Authorization: Bearer {{admin_token}}
[MultipartFormData]
folder_id: {{folder_b_id}}
file: file,fixtures/hello.txt; text/plain

HTTP 201
[Captures]
file_b_hello_id: jsonpath "$.id"


PUT {{base_url}}/api/files/{{file_x_id}}/move
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "folder_id": "{{folder_b_id}}" }

HTTP 409
[Asserts]
jsonpath "$.error_type" == "AlreadyExists"
jsonpath "$.status"     == "409 Conflict"


# ─────────────────────────────────────────────────────────────
# Step 10 — Teardown. Delete files + folders, empty trash. Order
#           matters: files first (so the folders are empty), then
#           folders (so the drive subtree is clean), then trash
#           purge. Idempotent-across-runs per
#           `feedback_hurl_teardown_shared_db`.
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/api/files/{{file_x_id}}
Authorization: Bearer {{admin_token}}

HTTP 204


DELETE {{base_url}}/api/files/{{file_y_id}}
Authorization: Bearer {{admin_token}}

HTTP 204


DELETE {{base_url}}/api/files/{{file_b_hello_id}}
Authorization: Bearer {{admin_token}}

HTTP 204


DELETE {{base_url}}/api/folders/{{folder_a_id}}
Authorization: Bearer {{admin_token}}

HTTP 204


DELETE {{base_url}}/api/folders/{{folder_b_id}}
Authorization: Bearer {{admin_token}}

HTTP 204


DELETE {{base_url}}/api/trash/empty
Authorization: Bearer {{admin_token}}

HTTP 200
