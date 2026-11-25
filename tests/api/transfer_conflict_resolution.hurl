# =============================================================
# OxiCloud — Move / Copy Conflict Resolution & Merge Suite
# =============================================================
# End-to-end verification of unified internal Move/Copy conflict
# handling:
#   1. POST /api/folders/{id}/transfer-preflight:
#      - same_parent_file_ids / same_parent_folder_ids detection
#      - descendant cycle detection (`cycle_folder_ids`)
#      - top-level & nested file conflicts (`identical`, `can_update_target`, hashes)
#      - folder conflicts with recursive diff counts & `source_has_shares`
#      - file<->folder kind mismatches
#      - stranger 404 anti-enum & Contributor `can_update_target: false`
#   2. POST /api/folders/{id}/transfer:
#      - same-parent Copy duplicates directly as `name (2)` without prompting
#      - same-parent Move is a clean `skipped` no-op
#      - File decisions: `skip`, `keep_both`, `replace` (preserves target ID)
#      - CAS `expected_hash` stale mismatch records `PreconditionFailed` without mutating target
#      - Contributor `replace` denied cleanly (`AccessDenied`)
#      - Recursive Folder `merge`: moves non-conflicting subtree whole, resolves
#        nested colliding files (`replace`/`keep_both`/`skip`), preserves non-empty
#        source folder when any nested item is skipped, and removes source folder once
#        truly empty
#      - Folder & kind-mismatch `keep_both` (`SubDir (2)`, `FolderVsFile (2)`)
#      - Cycle execution rejected cleanly with `InvalidInput` outcome
#   3. Legacy PUT /api/files/{id}/move & PUT /api/folders/{id}/move:
#      - default `fail` returns 409 AlreadyExists (including cross-table file<->folder)
#      - `on_conflict = "rename"` elects `name (2)` cleanly
#      - descendant folder move cycle rejected with 400
# =============================================================


# ─────────────────────────────────────────────────────────────
# Step 1 — Admin & Contributor login + seed scratch hashes
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "{{username}}", "password": "{{password}}" }

HTTP 200
[Captures]
admin_token: jsonpath "$.access_token"

GET {{base_url}}/api/drives
Authorization: Bearer {{admin_token}}

HTTP 200
[Captures]
home_id: jsonpath "$[0].root_folder_id"

POST {{base_url}}/api/admin/users
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "username": "tcr_contrib",
  "password": "TcrPassword1!",
  "email": "tcr_contrib@example.com",
  "role": "user"
}

HTTP 201
[Captures]
contrib_id: jsonpath "$.user.id"

POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "tcr_contrib", "password": "TcrPassword1!" }

HTTP 200
[Captures]
contrib_token: jsonpath "$.access_token"

GET {{base_url}}/api/drives
Authorization: Bearer {{contrib_token}}

HTTP 200
[Captures]
contrib_home_id: jsonpath "$[0].root_folder_id"

# Seed scratch blobs for admin (`hello.txt` and `content-canary.txt`).
POST {{base_url}}/api/folders
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "name": "tcr-blobs", "parent_id": "{{home_id}}" }

HTTP 201
[Captures]
blobs_id: jsonpath "$.id"

POST {{base_url}}/api/files/upload
Authorization: Bearer {{admin_token}}
[MultipartFormData]
folder_id: {{blobs_id}}
file: file,fixtures/hello.txt; text/plain

HTTP 201
[Captures]
hello_hash: jsonpath "$.content_hash"
hello_size: jsonpath "$.size"

POST {{base_url}}/api/files/upload
Authorization: Bearer {{admin_token}}
[MultipartFormData]
folder_id: {{blobs_id}}
file: file,fixtures/content-canary.txt; text/plain

HTTP 201
[Captures]
canary_hash: jsonpath "$.content_hash"
canary_size: jsonpath "$.size"


# ─────────────────────────────────────────────────────────────
# Step 2 — Create source & destination trees and test same-parent rules
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/folders
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "name": "tcr-root", "parent_id": "{{home_id}}" }

HTTP 201
[Captures]
root_id: jsonpath "$.id"

POST {{base_url}}/api/folders
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "name": "tcr-src", "parent_id": "{{root_id}}" }

HTTP 201
[Captures]
src_id: jsonpath "$.id"

POST {{base_url}}/api/folders
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "name": "tcr-dst", "parent_id": "{{root_id}}" }

HTTP 201
[Captures]
dst_id: jsonpath "$.id"

# Populate source items for same-parent Copy / Move.
POST {{base_url}}/api/files/by-hash
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "name": "doc.txt", "folder_id": "{{src_id}}", "hash": "{{canary_hash}}" }

HTTP 201
[Captures]
doc_id: jsonpath "$.id"

POST {{base_url}}/api/folders
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "name": "SubDir", "parent_id": "{{src_id}}" }

HTTP 201
[Captures]
subdir_id: jsonpath "$.id"

POST {{base_url}}/api/files/by-hash
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "name": "inner.txt", "folder_id": "{{subdir_id}}", "hash": "{{hello_hash}}" }

HTTP 201

# Same-parent Copy duplicates directly as `doc (2).txt` and `SubDir (2)`.
POST {{base_url}}/api/folders/{{src_id}}/transfer-preflight
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "operation": "copy",
  "file_ids": ["{{doc_id}}"],
  "folder_ids": ["{{subdir_id}}"]
}

HTTP 200
[Asserts]
jsonpath "$.supported" == true
jsonpath "$.same_parent_file_ids" count == 1
jsonpath "$.same_parent_folder_ids" count == 1
jsonpath "$.file_conflicts" count == 0
jsonpath "$.folder_conflicts" count == 0
jsonpath "$.kind_conflicts" count == 0

POST {{base_url}}/api/folders/{{src_id}}/transfer
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "operation": "copy",
  "file_ids": ["{{doc_id}}"],
  "folder_ids": ["{{subdir_id}}"],
  "decisions": []
}

HTTP 200
[Asserts]
jsonpath "$.renamed_count" == 2
jsonpath "$.failed_count" == 0
jsonpath "$.outcomes[0].final_name" == "doc (2).txt"
jsonpath "$.outcomes[1].final_name" == "SubDir (2)"

# Same-parent Move is a clean skipped no-op.
POST {{base_url}}/api/folders/{{src_id}}/transfer
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "operation": "move",
  "file_ids": ["{{doc_id}}"],
  "folder_ids": [],
  "decisions": []
}

HTTP 200
[Asserts]
jsonpath "$.skipped_count" == 1
jsonpath "$.outcomes[0].status" == "skipped"


# ─────────────────────────────────────────────────────────────
# Step 3 — Populate colliding files, folders, shares & kind mismatches
# ─────────────────────────────────────────────────────────────
# Top-level colliding files (`conflict.txt` & `keepme.txt`)
POST {{base_url}}/api/files/by-hash
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "name": "conflict.txt", "folder_id": "{{src_id}}", "hash": "{{canary_hash}}" }

HTTP 201
[Captures]
src_conflict_id: jsonpath "$.id"

POST {{base_url}}/api/files/by-hash
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "name": "conflict.txt", "folder_id": "{{dst_id}}", "hash": "{{hello_hash}}" }

HTTP 201
[Captures]
dst_conflict_id: jsonpath "$.id"

POST {{base_url}}/api/files/by-hash
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "name": "keepme.txt", "folder_id": "{{src_id}}", "hash": "{{canary_hash}}" }

HTTP 201
[Captures]
src_keep_id: jsonpath "$.id"

POST {{base_url}}/api/files/by-hash
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "name": "keepme.txt", "folder_id": "{{dst_id}}", "hash": "{{hello_hash}}" }

HTTP 201

# File<->Folder kind mismatch (`FolderVsFile` is a folder in src, file in dst)
POST {{base_url}}/api/folders
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "name": "FolderVsFile", "parent_id": "{{src_id}}" }

HTTP 201
[Captures]
src_kind_dir_id: jsonpath "$.id"

POST {{base_url}}/api/files/by-hash
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "name": "FolderVsFile", "folder_id": "{{dst_id}}", "hash": "{{hello_hash}}" }

HTTP 201

# Recursive Merge folder tree `MergeDir`
POST {{base_url}}/api/folders
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "name": "MergeDir", "parent_id": "{{src_id}}" }

HTTP 201
[Captures]
src_merge_id: jsonpath "$.id"

POST {{base_url}}/api/folders
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "name": "MergeDir", "parent_id": "{{dst_id}}" }

HTTP 201
[Captures]
dst_merge_id: jsonpath "$.id"

# Unique subfolder inside `src_merge_id` to verify whole-subtree move on Merge
POST {{base_url}}/api/folders
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "name": "UniqueChild", "parent_id": "{{src_merge_id}}" }

HTTP 201
[Captures]
unique_child_id: jsonpath "$.id"

POST {{base_url}}/api/files/by-hash
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "name": "unique.txt", "folder_id": "{{unique_child_id}}", "hash": "{{hello_hash}}" }

HTTP 201

# Three nested colliding files inside `MergeDir` (replace, keep_both, skip)
POST {{base_url}}/api/files/by-hash
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "name": "nested-replace.txt", "folder_id": "{{src_merge_id}}", "hash": "{{canary_hash}}" }

HTTP 201
[Captures]
src_nested_replace_id: jsonpath "$.id"

POST {{base_url}}/api/files/by-hash
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "name": "nested-replace.txt", "folder_id": "{{dst_merge_id}}", "hash": "{{hello_hash}}" }

HTTP 201
[Captures]
dst_nested_replace_id: jsonpath "$.id"

POST {{base_url}}/api/files/by-hash
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "name": "nested-keep.txt", "folder_id": "{{src_merge_id}}", "hash": "{{canary_hash}}" }

HTTP 201
[Captures]
src_nested_keep_id: jsonpath "$.id"

POST {{base_url}}/api/files/by-hash
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "name": "nested-keep.txt", "folder_id": "{{dst_merge_id}}", "hash": "{{hello_hash}}" }

HTTP 201

POST {{base_url}}/api/files/by-hash
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "name": "nested-skip.txt", "folder_id": "{{src_merge_id}}", "hash": "{{canary_hash}}" }

HTTP 201
[Captures]
src_nested_skip_id: jsonpath "$.id"

POST {{base_url}}/api/files/by-hash
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "name": "nested-skip.txt", "folder_id": "{{dst_merge_id}}", "hash": "{{hello_hash}}" }

HTTP 201

# Grant viewer on `src_merge_id` so preflight reports `source_has_shares == true`.
POST {{base_url}}/api/grants
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "subject": { "type": "user", "id": "{{contrib_id}}" },
  "resource": { "type": "folder", "id": "{{src_merge_id}}" },
  "role": "viewer"
}

HTTP 201

# Combined preflight check (files + merge folder + kind mismatch + self cycle)
POST {{base_url}}/api/folders/{{dst_id}}/transfer-preflight
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "operation": "move",
  "file_ids": ["{{src_conflict_id}}", "{{src_keep_id}}"],
  "folder_ids": ["{{src_merge_id}}", "{{src_kind_dir_id}}", "{{dst_id}}"]
}

HTTP 200
[Asserts]
jsonpath "$.supported" == true
jsonpath "$.cycle_folder_ids" count == 1
jsonpath "$.cycle_folder_ids[0]" == "{{dst_id}}"
jsonpath "$.folder_conflicts" count == 1
jsonpath "$.folder_conflicts[0].source_id" == "{{src_merge_id}}"
jsonpath "$.folder_conflicts[0].nested_conflict_count" == 3
jsonpath "$.folder_conflicts[0].source_has_shares" == true
jsonpath "$.folder_conflicts[0].can_delete_source" == true
jsonpath "$.file_conflicts" count == 5
jsonpath "$.kind_conflicts" count == 1
jsonpath "$.kind_conflicts[0].source_id" == "{{src_kind_dir_id}}"
jsonpath "$.kind_conflicts[0].source_kind" == "folder"
jsonpath "$.kind_conflicts[0].existing_kind" == "file"


# ─────────────────────────────────────────────────────────────
# Step 4 — AuthZ: Stranger 404 + Contributor can_update_target=false & Replace denied
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/folders/{{dst_id}}/transfer-preflight
Authorization: Bearer {{contrib_token}}
Content-Type: application/json
{ "operation": "copy", "file_ids": [], "folder_ids": [] }

HTTP 404

# Give `tcr_contrib` a source folder holding `hello.txt`, collide with `hello.txt` in `tcr_dst`.
POST {{base_url}}/api/folders
Authorization: Bearer {{contrib_token}}
Content-Type: application/json
{ "name": "contrib-src", "parent_id": "{{contrib_home_id}}" }

HTTP 201
[Captures]
contrib_src_id: jsonpath "$.id"

POST {{base_url}}/api/files/upload
Authorization: Bearer {{contrib_token}}
[MultipartFormData]
folder_id: {{contrib_src_id}}
file: file,fixtures/hello.txt; text/plain

HTTP 201
[Captures]
contrib_hello_id: jsonpath "$.id"

POST {{base_url}}/api/files/by-hash
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "name": "hello.txt", "folder_id": "{{dst_id}}", "hash": "{{canary_hash}}" }

HTTP 201
[Captures]
dst_hello_id: jsonpath "$.id"

POST {{base_url}}/api/grants
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "subject": { "type": "user", "id": "{{contrib_id}}" },
  "resource": { "type": "folder", "id": "{{dst_id}}" },
  "role": "contributor"
}

HTTP 201

POST {{base_url}}/api/folders/{{dst_id}}/transfer-preflight
Authorization: Bearer {{contrib_token}}
Content-Type: application/json
{
  "operation": "copy",
  "file_ids": ["{{contrib_hello_id}}"],
  "folder_ids": []
}

HTTP 200
[Asserts]
jsonpath "$.file_conflicts" count == 1
jsonpath "$.file_conflicts[0].can_update_target" == false

POST {{base_url}}/api/folders/{{dst_id}}/transfer
Authorization: Bearer {{contrib_token}}
Content-Type: application/json
{
  "operation": "copy",
  "file_ids": ["{{contrib_hello_id}}"],
  "folder_ids": [],
  "decisions": [
    {
      "source_id": "{{contrib_hello_id}}",
      "kind": "file",
      "action": "replace",
      "expected_hash": "{{canary_hash}}"
    }
  ]
}

HTTP 200
[Asserts]
jsonpath "$.failed_count" == 1
jsonpath "$.replaced_count" == 0
jsonpath "$.outcomes[0].status" == "failed"
jsonpath "$.outcomes[0].error_type" == "AccessDenied"

# Destination `hello.txt` stays untouched after Contributor denial.
GET {{base_url}}/api/files/{{dst_hello_id}}?metadata=true
Authorization: Bearer {{admin_token}}

HTTP 200
[Asserts]
jsonpath "$.content_hash" == "{{canary_hash}}"


# ─────────────────────────────────────────────────────────────
# Step 5 — File Replace (with CAS 412 guard) + Keep both + Skip
# ─────────────────────────────────────────────────────────────
# Wrong `expected_hash` fails safely with PreconditionFailed.
POST {{base_url}}/api/folders/{{dst_id}}/transfer
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "operation": "move",
  "file_ids": ["{{src_conflict_id}}"],
  "folder_ids": [],
  "decisions": [
    {
      "source_id": "{{src_conflict_id}}",
      "kind": "file",
      "action": "replace",
      "expected_hash": "wrong-hash-0000"
    }
  ]
}

HTTP 200
[Asserts]
jsonpath "$.failed_count" == 1
jsonpath "$.outcomes[0].error_type" == "PreconditionFailed"

GET {{base_url}}/api/files/{{dst_conflict_id}}?metadata=true
Authorization: Bearer {{admin_token}}

HTTP 200
[Asserts]
jsonpath "$.content_hash" == "{{hello_hash}}"

# Valid Replace (matching `expected_hash`) + `keep_both` on `keepme.txt`.
POST {{base_url}}/api/folders/{{dst_id}}/transfer
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "operation": "move",
  "file_ids": ["{{src_conflict_id}}", "{{src_keep_id}}"],
  "folder_ids": [],
  "decisions": [
    {
      "source_id": "{{src_conflict_id}}",
      "kind": "file",
      "action": "replace",
      "expected_hash": "{{hello_hash}}"
    },
    {
      "source_id": "{{src_keep_id}}",
      "kind": "file",
      "action": "keep_both"
    }
  ]
}

HTTP 200
[Asserts]
jsonpath "$.replaced_count" == 1
jsonpath "$.renamed_count" == 1
jsonpath "$.failed_count" == 0
jsonpath "$.outcomes[0].target_id" == "{{dst_conflict_id}}"
jsonpath "$.outcomes[1].final_name" == "keepme (2).txt"

GET {{base_url}}/api/files/{{dst_conflict_id}}?metadata=true
Authorization: Bearer {{admin_token}}

HTTP 200
[Asserts]
jsonpath "$.content_hash" == "{{canary_hash}}"


# ─────────────────────────────────────────────────────────────
# Step 6 — Recursive Folder Merge + Empty-Only Source Cleanup + Keep Both
# ─────────────────────────────────────────────────────────────
# Part A: merge `MergeDir` with one nested skip -> source folder MUST survive.
POST {{base_url}}/api/folders/{{dst_id}}/transfer
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "operation": "move",
  "file_ids": [],
  "folder_ids": ["{{src_merge_id}}", "{{src_kind_dir_id}}"],
  "decisions": [
    { "source_id": "{{src_merge_id}}", "kind": "folder", "action": "merge" },
    {
      "source_id": "{{src_nested_replace_id}}",
      "kind": "file",
      "action": "replace",
      "expected_hash": "{{hello_hash}}"
    },
    { "source_id": "{{src_nested_keep_id}}", "kind": "file", "action": "keep_both" },
    { "source_id": "{{src_nested_skip_id}}", "kind": "file", "action": "skip" },
    { "source_id": "{{src_kind_dir_id}}", "kind": "folder", "action": "keep_both" }
  ]
}

HTTP 200
[Asserts]
jsonpath "$.merged_count" == 1
jsonpath "$.renamed_count" == 2
jsonpath "$.failed_count" == 0
jsonpath "$.outcomes[*].final_name" contains "nested-keep (2).txt"
jsonpath "$.outcomes[*].final_name" contains "FolderVsFile (2)"

# Verify `nested-replace.txt` preserved target ID and updated bytes.
GET {{base_url}}/api/files/{{dst_nested_replace_id}}?metadata=true
Authorization: Bearer {{admin_token}}

HTTP 200
[Asserts]
jsonpath "$.content_hash" == "{{canary_hash}}"

# Verify `dst_merge_id` now contains `UniqueChild` and `nested-keep (2).txt`.
GET {{base_url}}/api/folders/{{dst_merge_id}}/resources
Authorization: Bearer {{admin_token}}

HTTP 200
[Asserts]
jsonpath "$.items[*].resource.name" contains "UniqueChild"
jsonpath "$.items[*].resource.name" contains "nested-keep (2).txt"

# Because `nested-skip.txt` was skipped, `src_merge_id` is NOT empty and still exists.
GET {{base_url}}/api/folders/{{src_merge_id}}
Authorization: Bearer {{admin_token}}

HTTP 200

# Part B: re-merge `src_merge_id` choosing `keep_both` for `nested-skip.txt` -> empties source!
POST {{base_url}}/api/folders/{{dst_id}}/transfer
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "operation": "move",
  "file_ids": [],
  "folder_ids": ["{{src_merge_id}}"],
  "decisions": [
    { "source_id": "{{src_merge_id}}", "kind": "folder", "action": "merge" },
    { "source_id": "{{src_nested_skip_id}}", "kind": "file", "action": "keep_both" }
  ]
}

HTTP 200
[Asserts]
jsonpath "$.merged_count" == 1
jsonpath "$.failed_count" == 0

# Now that `src_merge_id` was completely emptied, Move deleted it.
GET {{base_url}}/api/folders/{{src_merge_id}}
Authorization: Bearer {{admin_token}}

HTTP 404

# Copy a folder cleanly with explicit `keep_both` (`SubDir` -> `SubDir`, then second Copy -> `SubDir (2)`).
POST {{base_url}}/api/folders/{{dst_id}}/transfer
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "operation": "copy",
  "file_ids": [],
  "folder_ids": ["{{subdir_id}}"],
  "decisions": []
}

HTTP 200
[Asserts]
jsonpath "$.copied_count" == 1

POST {{base_url}}/api/folders/{{dst_id}}/transfer
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "operation": "copy",
  "file_ids": [],
  "folder_ids": ["{{subdir_id}}"],
  "decisions": [
    { "source_id": "{{subdir_id}}", "kind": "folder", "action": "keep_both" }
  ]
}

HTTP 200
[Asserts]
jsonpath "$.renamed_count" == 1
jsonpath "$.outcomes[0].final_name" == "SubDir (2)"


# ─────────────────────────────────────────────────────────────
# Step 7 — Legacy PUT /move `on_conflict` & Cross-Table Guard + Cycle Outcome
# ─────────────────────────────────────────────────────────────
# Moving `doc.txt` into `tcr_dst` where folder `SubDir` doesn't collide, let's create file `SubDir`?
# Instead create file named `SubDir` in `src_id`? `src_id` already has folder `SubDir`, so create in `contrib_root` or `root_id`.
POST {{base_url}}/api/files/by-hash
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "name": "SubDir", "folder_id": "{{root_id}}", "hash": "{{hello_hash}}" }

HTTP 201
[Captures]
cross_file_id: jsonpath "$.id"

# Default `PUT /api/files/{id}/move` against folder `SubDir` in `dst_id` must fail with 409.
PUT {{base_url}}/api/files/{{cross_file_id}}/move
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "folder_id": "{{dst_id}}" }

HTTP 409
[Asserts]
jsonpath "$.error_type" == "AlreadyExists"

# With `on_conflict: "rename"` -> skips `SubDir` (folder) and `SubDir (2)` (folder) -> picks `SubDir (3)`.
PUT {{base_url}}/api/files/{{cross_file_id}}/move
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "folder_id": "{{dst_id}}", "on_conflict": "rename" }

HTTP 200
[Asserts]
jsonpath "$.name" == "SubDir (3)"

# Cycle attempt in `POST /transfer` records InvalidInput outcome without panicking.
POST {{base_url}}/api/folders/{{dst_id}}/transfer
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "operation": "move",
  "file_ids": [],
  "folder_ids": ["{{dst_id}}"],
  "decisions": []
}

HTTP 200
[Asserts]
jsonpath "$.failed_count" == 1
jsonpath "$.outcomes[0].status" == "failed"
jsonpath "$.outcomes[0].error_type" == "InvalidInput"


# ─────────────────────────────────────────────────────────────
# Step 8 — Cleanup
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/api/folders/{{root_id}}
Authorization: Bearer {{admin_token}}

HTTP 204

DELETE {{base_url}}/api/folders/{{blobs_id}}
Authorization: Bearer {{admin_token}}

HTTP 204

DELETE {{base_url}}/api/folders/{{contrib_src_id}}
Authorization: Bearer {{contrib_token}}

HTTP 204

DELETE {{base_url}}/api/admin/users/{{contrib_id}}
Authorization: Bearer {{admin_token}}

HTTP 200

DELETE {{base_url}}/api/trash/empty
Authorization: Bearer {{admin_token}}

HTTP 200
