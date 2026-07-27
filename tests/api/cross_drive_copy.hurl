# =============================================================
# OxiCloud — D6 cross-drive COPY + drive_id resolution
# =============================================================
# Run:
#   hurl --variables-file tests/api/test.env --file-root tests \
#        --test tests/api/cross_drive_copy.hurl
#
# Companion to `cross_drive_move.hurl`. The MOVE path was fixed
# in D6 via the WITH dest CTE + cascade trigger; the COPY path
# was the lone holdout, fixed by migration
# `20260808000000_copy_folder_tree_cross_drive.sql` which makes
# `storage.copy_folder_tree` resolve drive_id from the
# destination once, instead of pulling source's drive_id per row.
#
# Verifies:
#   1. Single-file batch copy across drives lands in the
#      destination drive (source unchanged because copy ≠ move).
#      Already-correct path via `copy_file` SQL — guarded here
#      so a regression on the file path is caught.
#   2. Folder-tree batch copy across drives. Two layers of
#      assertion:
#        a) DIRECT — the copied folder's `drive_id` field reads
#           as the destination drive (FolderDto exposes it).
#           This is the load-bearing check for the migration.
#        b) INDIRECT — per-drive sweep totals: source unchanged,
#           destination grew by the descendant file's size.
#           Pre-fix this would have left destination = 0 and
#           the nested file's size mis-attributed to source.
#      The folder + file INSERTs in the migration share the
#      same `v_dest_drive_id` variable, so (a) passing implies
#      file rows used the same value and (b) cross-checks it.
#
# Sweep convergence: `/api/admin/jobs/storage_reconcile/trigger` is the
# deterministic synchronisation point — without it the
# fire-and-forget delta hook may not yet have updated the cached
# `used_bytes` when we read it.
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
# Step 2 — Provision `dc_owner`.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/admin/users
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "username": "dc_owner",
  "password": "DcOwnerPwd1!",
  "email":    "dc_owner@example.com",
  "role":     "user"
}

HTTP 201

POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "dc_owner", "password": "DcOwnerPwd1!" }

HTTP 200
[Captures]
owner_token:   jsonpath "$.access_token"
owner_user_id: jsonpath "$.user.id"


# ─────────────────────────────────────────────────────────────
# Step 3 — Capture the user's default Personal drive + root.
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
# Step 4 — Admin creates a shared drive owned by dc_owner.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/drives
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "kind": "shared",
  "name": "dc-shared",
  "owner": { "type": "user", "id": "{{owner_user_id}}" }
}

HTTP 201
[Captures]
shared_drive_id: jsonpath "$.id"
shared_root_id:  jsonpath "$.root_folder_id"


# ─────────────────────────────────────────────────────────────
# Step 5 — Upload hello.txt (32 B) into the personal drive root.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/files/upload
Authorization: Bearer {{owner_token}}
[MultipartFormData]
folder_id: {{personal_root_id}}
file: file,fixtures/hello.txt; text/plain

HTTP 201
[Captures]
file_id: jsonpath "$.id"

# Baseline used_bytes after the upload settles.
#
# `[Options] delay: 200ms` is the workaround for the
# trigger-sweep-vs-spawn-hook race documented in
# bug_trigger_sweep_vs_spawn_hook_race.md: the upload responds 201 as
# soon as the row lands, but the storage-usage delta hook is
# tokio::spawn'd — without the delay, trigger-sweep can run while
# that hook is still in flight, the sweep then snapshots stale
# numbers, the late hook adds its delta on top, and used_bytes ends
# up high by exactly one file's size. Symptom: expected 32, got 64.
# Real fix is await'ing the hook inline server-side.
POST {{base_url}}/api/admin/jobs/storage_reconcile/trigger
Authorization: Bearer {{admin_token}}
[Options]
delay: 200ms

HTTP 200

GET {{base_url}}/api/drives
Authorization: Bearer {{owner_token}}

HTTP 200
[Asserts]
jsonpath "$[?(@.id=='{{personal_drive_id}}')].used_bytes" == 32
jsonpath "$[?(@.id=='{{shared_drive_id}}')].used_bytes"   == 0


# ─────────────────────────────────────────────────────────────
# Step 6 — Single-file batch COPY across drives.
#
# `copy_file` SQL already binds dest drive_id via the dest_folder
# CTE; this step guards that path so a regression is caught.
# After sweep: source keeps its 32 (copy ≠ move), dest gains 32.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/batch/files/copy
Authorization: Bearer {{owner_token}}
Content-Type: application/json
{
  "file_ids":         ["{{file_id}}"],
  "target_folder_id": "{{shared_root_id}}"
}

HTTP 200
[Captures]
shared_file_id: jsonpath "$.successful[0].id"

POST {{base_url}}/api/admin/jobs/storage_reconcile/trigger
Authorization: Bearer {{admin_token}}
[Options]
delay: 200ms

HTTP 200

GET {{base_url}}/api/drives
Authorization: Bearer {{owner_token}}

HTTP 200
[Asserts]
jsonpath "$[?(@.id=='{{personal_drive_id}}')].used_bytes" == 32
jsonpath "$[?(@.id=='{{shared_drive_id}}')].used_bytes"   == 32


# Confirm the duplicate is visible under the shared drive's root.
GET {{base_url}}/api/folders/{{shared_root_id}}/resources?limit=50
Authorization: Bearer {{owner_token}}

HTTP 200
[Asserts]
jsonpath "$.items[?(@.resource_type=='file')].resource.name" contains "hello.txt"


# ─────────────────────────────────────────────────────────────
# Step 7 — Folder-tree COPY across drives, with a nested file.
#          Pre-migration this was the broken path: source's
#          drive_id leaked into every descendant of the copied
#          subtree because `storage.copy_folder_tree` used
#          `fo.drive_id` per row instead of resolving the
#          destination drive once.
#
# Create a folder under personal root with hello-copy.txt inside,
# then batch-copy the whole subtree to the shared drive.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/folders
Authorization: Bearer {{owner_token}}
Content-Type: application/json
{ "name": "dc-subtree", "parent_id": "{{personal_root_id}}" }

HTTP 201
[Captures]
subtree_id: jsonpath "$.id"

POST {{base_url}}/api/files/upload
Authorization: Bearer {{owner_token}}
[MultipartFormData]
folder_id: {{subtree_id}}
file: file,fixtures/hello-copy.txt; text/plain

HTTP 201

# Add one more level of nesting so the cascade-through-levels in
# copy_folder_tree gets exercised — the level-by-level INSERT
# loop is where the previous body's bug compounded.
POST {{base_url}}/api/folders
Authorization: Bearer {{owner_token}}
Content-Type: application/json
{ "name": "dc-subtree-inner", "parent_id": "{{subtree_id}}" }

HTTP 201
[Captures]
inner_id: jsonpath "$.id"

POST {{base_url}}/api/files/upload
Authorization: Bearer {{owner_token}}
[MultipartFormData]
folder_id: {{inner_id}}
file: file,fixtures/hello.txt; text/plain

HTTP 201


# Baseline post-creation. Personal holds:
#   - hello.txt at root (32 B)
#   - hello-copy.txt nested in dc-subtree (32 B)
#   - hello.txt nested in dc-subtree-inner (32 B)
# = 96 total. Shared still has the file-copy from Step 6 (32 B).
#
# Delay: the file-upload service fires the per-drive used_bytes
# delta via `tokio::spawn` (file_upload_service.rs ~372). With two
# uploads back-to-back the spawned hooks race the sweep: if the
# hook lands AFTER `trigger-sweep`'s recompute, the additive
# UPDATE clobbers the SUM with `used_bytes += delta`, doubling
# the file's size into the cached counter. 200 ms is well above
# the tokio task latency on any reasonable box; the deterministic
# fix would be intra-transaction hooks, deferred until D7.
POST {{base_url}}/api/admin/jobs/storage_reconcile/trigger
Authorization: Bearer {{admin_token}}
[Options]
delay: 200ms

HTTP 200

GET {{base_url}}/api/drives
Authorization: Bearer {{owner_token}}

HTTP 200
[Asserts]
jsonpath "$[?(@.id=='{{personal_drive_id}}')].used_bytes" == 96
jsonpath "$[?(@.id=='{{shared_drive_id}}')].used_bytes"   == 32


# Copy the SUBTREE FOLDER (with its nested file + nested folder
# + nested-nested file) into the shared drive's root. The
# response's `new_root_folder_id` lets us follow up with a
# direct drive_id assertion on the copy.
POST {{base_url}}/api/batch/folders/copy
Authorization: Bearer {{owner_token}}
Content-Type: application/json
{
  "folder_ids":       ["{{subtree_id}}"],
  "target_folder_id": "{{shared_root_id}}"
}

HTTP 200
[Captures]
new_root_folder_id: jsonpath "$.successful[0].new_root_folder_id"
[Asserts]
jsonpath "$.successful[0].folders_copied" == 2
jsonpath "$.successful[0].files_copied"   == 2


# ── (a) DIRECT drive_id assertion on the copied root. ──
# FolderDto exposes drive_id, so we can read it back end-to-end
# without touching SQL. Pre-fix this would equal personal_drive_id
# instead of shared_drive_id.
GET {{base_url}}/api/folders/{{new_root_folder_id}}
Authorization: Bearer {{owner_token}}

HTTP 200
[Asserts]
jsonpath "$.drive_id" == "{{shared_drive_id}}"
jsonpath "$.name"     == "dc-subtree"


# ── (a') DIRECT drive_id assertion on the descendant folder. ──
# Walk into the copied root and verify its child folder also
# inherited the destination drive_id. This is the level-by-level
# loop's correctness guard — pre-fix the inner folder would have
# kept personal_drive_id and the cascade trigger doesn't fire on
# INSERT (it only handles UPDATE OF drive_id).
#
# The `/resources` listing now surfaces the real drive_id (the
# handler used to stub Uuid::nil because the row didn't project
# drive_id; the underlying query was extended alongside this
# migration to project f.drive_id / fm.drive_id). We can assert
# directly on the listing AND cross-check via GET /api/folders/{id}.
GET {{base_url}}/api/folders/{{new_root_folder_id}}/resources?limit=50
Authorization: Bearer {{owner_token}}

HTTP 200
[Captures]
# Single-match filter — Hurl unwraps to scalar; do NOT use `nth N`
# here (see feedback_hurl_jsonpath_filter_empty.md: filters with
# nth fail on a single-match result).
new_inner_id: jsonpath "$.items[?(@.resource_type=='folder')].resource.id"
[Asserts]
jsonpath "$.items[?(@.resource_type=='folder')].resource.drive_id" == "{{shared_drive_id}}"
jsonpath "$.items[?(@.resource_type=='file')].resource.name"       == "hello-copy.txt"

GET {{base_url}}/api/folders/{{new_inner_id}}
Authorization: Bearer {{owner_token}}

HTTP 200
[Asserts]
jsonpath "$.drive_id" == "{{shared_drive_id}}"
jsonpath "$.name"     == "dc-subtree-inner"


# ── (b) INDIRECT cross-check via per-drive sweep. ──
# Source unchanged (copy ≠ move): personal still 96.
# Destination grew by the two descendant files (32 + 32 = 64) +
# the Step 6 file copy (32) = 96. Anything other than (96, 96)
# would mean the file INSERT in copy_folder_tree used the wrong
# drive_id.
POST {{base_url}}/api/admin/jobs/storage_reconcile/trigger
Authorization: Bearer {{admin_token}}
[Options]
delay: 200ms

HTTP 200

GET {{base_url}}/api/drives
Authorization: Bearer {{owner_token}}

HTTP 200
[Asserts]
jsonpath "$[?(@.id=='{{personal_drive_id}}')].used_bytes" == 96
jsonpath "$[?(@.id=='{{shared_drive_id}}')].used_bytes"   == 96


# ─────────────────────────────────────────────────────────────
# Step 8 — Cleanup. Drain the shared drive (it isn't covered by
#          the user-delete cascade), delete the shared drive,
#          drain the source subtree from the personal drive,
#          then delete the test user.
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/api/files/{{shared_file_id}}
Authorization: Bearer {{owner_token}}

HTTP 204

DELETE {{base_url}}/api/folders/{{new_root_folder_id}}
Authorization: Bearer {{owner_token}}

HTTP 204

DELETE {{base_url}}/api/drives/{{shared_drive_id}}
Authorization: Bearer {{owner_token}}

HTTP 204

DELETE {{base_url}}/api/folders/{{subtree_id}}
Authorization: Bearer {{owner_token}}

HTTP 204

DELETE {{base_url}}/api/admin/users/{{owner_user_id}}
Authorization: Bearer {{admin_token}}

HTTP 200
