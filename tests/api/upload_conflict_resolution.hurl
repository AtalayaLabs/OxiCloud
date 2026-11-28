# =============================================================
# OxiCloud — Upload preflight, replace + CAS, and on_conflict=rename
# =============================================================
# End-to-end regression suite for issue #947:
#   1. Batch preflight (`POST /api/folders/{id}/upload-preflight`):
#      - existing subfolders, existing live files (size, modified_at,
#        content_hash, can_update), file<->folder kind mismatches
#      - trashed files/folders excluded from collisions
#      - traversal / malformed paths rejected with 400
#      - unauthorized caller receives 404 (anti-enumeration)
#      - Contributor caller sees `can_update: false` and gets 403 on PUT
#   2. `on_conflict = "rename"` across all 4 create endpoints:
#      - POST /api/files/upload (multipart)
#      - POST /api/files/by-hash
#      - POST /api/files/delta/commit (whole-file fast path)
#      - POST /api/folders
#      - cross-table file<->folder collision skips occupied names
#      - compound archive extensions preserved (`archive (2).tar.gz`)
#   3. In-place overwrite (`PUT /api/files/{id}/content` & `by-hash` file_id):
#      - preserves file ID while replacing content, size, hash, and mtime
#      - optional `expected_hash` / `If-Match` rejects stale state with 412
#        without modifying the stored file
# =============================================================


# ─────────────────────────────────────────────────────────────
# Step 1 — Admin login, resolve home folder, seed helper blob + test user
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "{{username}}", "password": "{{password}}" }

HTTP 200
[Captures]
admin_token: jsonpath "$.access_token"

GET {{base_url}}/api/folders
Authorization: Bearer {{admin_token}}

HTTP 200
[Captures]
home_id: jsonpath "$[0].id"

POST {{base_url}}/api/admin/users
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "username": "ucr_contrib",
  "password": "UcrPassword1!",
  "email": "ucr_contrib@example.com",
  "role": "user"
}

HTTP 201
[Captures]
contrib_id: jsonpath "$.user.id"

POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "ucr_contrib", "password": "UcrPassword1!" }

HTTP 200
[Captures]
contrib_token: jsonpath "$.access_token"

# Seed a separate scratch folder holding `content-canary.txt` so admin owns its
# hash before we test preflight & conflict replacement inside `ucr-root`.
POST {{base_url}}/api/folders
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "name": "ucr-blobs", "parent_id": "{{home_id}}" }

HTTP 201
[Captures]
blobs_folder_id: jsonpath "$.id"

POST {{base_url}}/api/files/upload
Authorization: Bearer {{admin_token}}
[MultipartFormData]
folder_id: {{blobs_folder_id}}
file: file,fixtures/content-canary.txt; text/plain

HTTP 201
[Captures]
canary_hash: jsonpath "$.content_hash"
canary_size: jsonpath "$.size"

# Create main fixture tree under `ucr-root`.
POST {{base_url}}/api/folders
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "name": "ucr-root", "parent_id": "{{home_id}}" }

HTTP 201
[Captures]
root_id: jsonpath "$.id"

POST {{base_url}}/api/folders
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "name": "ExistingSub", "parent_id": "{{root_id}}" }

HTTP 201
[Captures]
existing_sub_id: jsonpath "$.id"

# Seed `hello.txt` inside `ExistingSub` with a rounded sub-second mtime.
POST {{base_url}}/api/files/upload
Authorization: Bearer {{admin_token}}
[MultipartFormData]
folder_id: {{existing_sub_id}}
source_modified_at: 1704067200600
file: file,fixtures/hello.txt; text/plain

HTTP 201
[Captures]
sub_hello_id: jsonpath "$.id"
hello_hash: jsonpath "$.content_hash"

# Seed a top-level file whose name blocks an incoming folder of the same name.
POST {{base_url}}/api/files/by-hash
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "name": "FileBlockingDir",
  "folder_id": "{{root_id}}",
  "hash": "{{hello_hash}}"
}

HTTP 201
[Captures]
blocking_file_id: jsonpath "$.id"

# Seed and immediately trash a file so preflight proves trashed rows are ignored.
POST {{base_url}}/api/files/upload
Authorization: Bearer {{admin_token}}
[MultipartFormData]
folder_id: {{existing_sub_id}}
file: file,fixtures/hello-trashed.txt; text/plain

HTTP 201
[Captures]
trashed_file_id: jsonpath "$.id"

DELETE {{base_url}}/api/files/{{trashed_file_id}}
Authorization: Bearer {{admin_token}}

HTTP 204


# ─────────────────────────────────────────────────────────────
# Step 2 — Preflight: matches files/folders, kind conflicts, ignores trash
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/folders/{{root_id}}/upload-preflight
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "paths": [
    "ExistingSub/hello.txt",
    "ExistingSub/brand-new.txt",
    "ExistingSub/hello-trashed.txt",
    "ExistingSub",
    "FileBlockingDir/nested.txt"
  ]
}

HTTP 200
[Asserts]
jsonpath "$.supported" == true
jsonpath "$.existing_folders" count == 1
jsonpath "$.existing_folders[0].relative_path" == "ExistingSub"
jsonpath "$.existing_folders[0].id" == "{{existing_sub_id}}"
jsonpath "$.existing_files" count == 1
jsonpath "$.existing_files[0].relative_path" == "ExistingSub/hello.txt"
jsonpath "$.existing_files[0].id" == "{{sub_hello_id}}"
jsonpath "$.existing_files[0].folder_id" == "{{existing_sub_id}}"
jsonpath "$.existing_files[0].size" == 32
jsonpath "$.existing_files[0].modified_at" == "2024-01-01T00:00:01Z"
jsonpath "$.existing_files[0].content_hash" == "{{hello_hash}}"
jsonpath "$.existing_files[0].can_update" == true
jsonpath "$.kind_conflicts" count == 2
jsonpath "$.kind_conflicts[*].relative_path" includes "ExistingSub"
jsonpath "$.kind_conflicts[*].relative_path" includes "FileBlockingDir"


# Malformed / traversal preflight paths must be rejected with 400.
POST {{base_url}}/api/folders/{{root_id}}/upload-preflight
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "paths": ["../escape.txt"] }

HTTP 400

POST {{base_url}}/api/folders/{{root_id}}/upload-preflight
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "paths": ["/abs/file.txt"] }

HTTP 400


# ─────────────────────────────────────────────────────────────
# Step 3 — AuthZ: stranger 404 anti-enum, Contributor can_update=false & 403
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/folders/{{root_id}}/upload-preflight
Authorization: Bearer {{contrib_token}}
Content-Type: application/json
{ "paths": ["ExistingSub/hello.txt"] }

HTTP 404

PUT {{base_url}}/api/files/{{sub_hello_id}}/content
Authorization: Bearer {{contrib_token}}
[MultipartFormData]
file: file,fixtures/content-canary.txt; text/plain

HTTP 404

POST {{base_url}}/api/grants
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "subject": { "type": "user", "id": "{{contrib_id}}" },
  "resource": { "type": "folder", "id": "{{root_id}}" },
  "role": "contributor"
}

HTTP 201

POST {{base_url}}/api/folders/{{root_id}}/upload-preflight
Authorization: Bearer {{contrib_token}}
Content-Type: application/json
{ "paths": ["ExistingSub/hello.txt"] }

HTTP 200
[Asserts]
jsonpath "$.existing_files" count == 1
jsonpath "$.existing_files[0].can_update" == false

# Contributor has Read + Create, NOT Update -> graduated denial returns 403.
PUT {{base_url}}/api/files/{{sub_hello_id}}/content
Authorization: Bearer {{contrib_token}}
[MultipartFormData]
file: file,fixtures/content-canary.txt; text/plain

HTTP 403


# ─────────────────────────────────────────────────────────────
# Step 4 — In-place replace (`PUT /api/files/{id}/content`, `by-hash` update)
#          + compare-and-swap `expected_hash` / `If-Match` 412 guards
# ─────────────────────────────────────────────────────────────
# Upload `hello.txt` at `root_id` to exercise full create/replace/rename chain.
POST {{base_url}}/api/files/upload
Authorization: Bearer {{admin_token}}
[MultipartFormData]
folder_id: {{root_id}}
file: file,fixtures/hello.txt; text/plain

HTTP 201
[Captures]
root_hello_id: jsonpath "$.id"

# Replace its content with `content-canary.txt` using wrong `expected_hash` -> 412.
PUT {{base_url}}/api/files/{{root_hello_id}}/content
Authorization: Bearer {{admin_token}}
[MultipartFormData]
expected_hash: {{canary_hash}}
file: file,fixtures/content-canary.txt; text/plain

HTTP 412
[Asserts]
jsonpath "$.error_type" == "PreconditionFailed"

# Verify original content stayed intact after the failed CAS PUT.
GET {{base_url}}/api/files/{{root_hello_id}}?metadata=true
Authorization: Bearer {{admin_token}}

HTTP 200
[Asserts]
jsonpath "$.content_hash" == "{{hello_hash}}"
jsonpath "$.size" == 32

# Replace with matching `If-Match` + `source_modified_at` -> 200, same file ID.
PUT {{base_url}}/api/files/{{root_hello_id}}/content
Authorization: Bearer {{admin_token}}
If-Match: "{{hello_hash}}"
[MultipartFormData]
source_modified_at: 1704067205700
file: file,fixtures/content-canary.txt; text/plain

HTTP 200
[Asserts]
jsonpath "$.id" == "{{root_hello_id}}"
jsonpath "$.name" == "hello.txt"
jsonpath "$.size" == {{canary_size}}
jsonpath "$.content_hash" == "{{canary_hash}}"
jsonpath "$.modified_at" == "2024-01-01T00:00:06Z"

# Stale `by-hash` update -> 412 PreconditionFailed.
POST {{base_url}}/api/files/by-hash
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "file_id": "{{root_hello_id}}",
  "hash": "{{hello_hash}}",
  "expected_hash": "{{hello_hash}}"
}

HTTP 412
[Asserts]
jsonpath "$.error_type" == "PreconditionFailed"

# Matching `by-hash` update -> 200 OK, swaps back to `hello.txt` bytes without transfer.
POST {{base_url}}/api/files/by-hash
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "file_id": "{{root_hello_id}}",
  "hash": "{{hello_hash}}",
  "expected_hash": "{{canary_hash}}"
}

HTTP 200
[Asserts]
jsonpath "$.id" == "{{root_hello_id}}"
jsonpath "$.content_hash" == "{{hello_hash}}"
jsonpath "$.size" == 32

# Swap `root_hello_id` back to `canary_hash` so `hello.txt` now has different
# content from `fixtures/hello.txt`, letting us test multipart `on_conflict`.
POST {{base_url}}/api/files/by-hash
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "file_id": "{{root_hello_id}}",
  "hash": "{{canary_hash}}"
}

HTTP 200


# ─────────────────────────────────────────────────────────────
# Step 5 — `on_conflict = "rename"` across upload, by-hash, delta/commit, folders
# ─────────────────────────────────────────────────────────────
# Default policy on different content still returns 409 AlreadyExists.
POST {{base_url}}/api/files/upload
Authorization: Bearer {{admin_token}}
[MultipartFormData]
folder_id: {{root_id}}
file: file,fixtures/hello.txt; text/plain

HTTP 409
[Asserts]
jsonpath "$.error_type" == "AlreadyExists"

# Multipart `on_conflict = "rename"` creates `hello (2).txt`.
POST {{base_url}}/api/files/upload
Authorization: Bearer {{admin_token}}
[MultipartFormData]
folder_id: {{root_id}}
on_conflict: rename
file: file,fixtures/hello.txt; text/plain

HTTP 201
[Asserts]
jsonpath "$.name" == "hello (2).txt"
jsonpath "$.content_hash" == "{{hello_hash}}"

# Seed a FOLDER named `hello (3).txt` so the next file rename must skip over it
# cleanly to `hello (4).txt` (cross-table file<->folder disambiguation).
POST {{base_url}}/api/folders
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "name": "hello (3).txt", "parent_id": "{{root_id}}" }

HTTP 201

# `by-hash` with `on_conflict = "rename"` -> skips `(1)`, `(2)`, and folder `(3)` -> `(4)`.
POST {{base_url}}/api/files/by-hash
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "name": "hello.txt",
  "folder_id": "{{root_id}}",
  "hash": "{{hello_hash}}",
  "on_conflict": "rename"
}

HTTP 201
[Asserts]
jsonpath "$.name" == "hello (4).txt"

# `delta/commit` whole-file fast path with `on_conflict = "rename"` -> `hello (5).txt`.
POST {{base_url}}/api/files/delta/commit
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "file_hash": "{{canary_hash}}",
  "chunks": [{ "h": "{{canary_hash}}", "s": {{canary_size}} }],
  "name": "hello.txt",
  "folder_id": "{{root_id}}",
  "on_conflict": "rename"
}

HTTP 201
[Asserts]
jsonpath "$.name" == "hello (5).txt"

# Compound archive extension preservation (`archive.tar.gz` -> `archive (2).tar.gz`).
POST {{base_url}}/api/files/by-hash
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "name": "archive.tar.gz",
  "folder_id": "{{root_id}}",
  "hash": "{{hello_hash}}",
  "on_conflict": "rename"
}

HTTP 201
[Asserts]
jsonpath "$.name" == "archive.tar.gz"

POST {{base_url}}/api/files/by-hash
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "name": "archive.tar.gz",
  "folder_id": "{{root_id}}",
  "hash": "{{canary_hash}}",
  "on_conflict": "rename"
}

HTTP 201
[Asserts]
jsonpath "$.name" == "archive (2).tar.gz"

# Folder `on_conflict = "rename"`, skipping both existing folders and existing files.
POST {{base_url}}/api/folders
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "name": "ExistingSub", "parent_id": "{{root_id}}", "on_conflict": "rename" }

HTTP 201
[Asserts]
jsonpath "$.name" == "ExistingSub (2)"

POST {{base_url}}/api/files/by-hash
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "name": "ExistingSub (3)",
  "folder_id": "{{root_id}}",
  "hash": "{{hello_hash}}"
}

HTTP 201

POST {{base_url}}/api/folders
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "name": "ExistingSub", "parent_id": "{{root_id}}", "on_conflict": "rename" }

HTTP 201
[Asserts]
jsonpath "$.name" == "ExistingSub (4)"


# ─────────────────────────────────────────────────────────────
# Step 6 — Cleanup
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/api/folders/{{root_id}}
Authorization: Bearer {{admin_token}}

HTTP 204

DELETE {{base_url}}/api/folders/{{blobs_folder_id}}
Authorization: Bearer {{admin_token}}

HTTP 204

DELETE {{base_url}}/api/admin/users/{{contrib_id}}
Authorization: Bearer {{admin_token}}

HTTP 200

DELETE {{base_url}}/api/trash/empty
Authorization: Bearer {{admin_token}}

HTTP 200
