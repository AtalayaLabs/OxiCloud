# =============================================================
# OxiCloud — D6 cross-drive move + drive_id cascade
# =============================================================
# Run:
#   hurl --variables-file tests/api/test.env --file-root tests \
#        --test tests/api/cross_drive_move.hurl
#
# Verifies:
#   1. File moved across drives lands in the destination drive's
#      subtree AND the file row's `drive_id` syncs to the
#      destination (observed via the per-drive quota sweep:
#      source `used_bytes` drops, target rises).
#   2. Folder moved across drives ALSO syncs `drive_id` on every
#      descendant — the cascade trigger added by migration
#      `20260807000000_cascade_drive_id_on_folder_move.sql` is
#      the load-bearing piece. Verified by moving a folder with
#      a file inside and watching the destination drive's
#      `used_bytes` jump by the descendant's size (not 0).
#
# Sweep convergence: `/api/admin/jobs/usage_reconcile/trigger` is the
# deterministic synchronisation point — it recomputes every
# drive's cached `used_bytes` from `SUM(file.size) WHERE
# drive_id = d.id`. If the file/folder move didn't update
# `drive_id`, the sweep would re-attribute size to the WRONG
# drive (or none), and the assertion below would fail.
#
# `forbid_cross_drive_move` policy refusal is covered by
# `tests/api/drive_policies.hurl` Step 11b — this scenario uses
# the policy OFF (the default) to exercise the happy path.
#
# Self-contained: provisions `dm_owner` and a fresh shared drive
# so it can run alongside the rest of the suite.
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
# Step 2 — Provision `dm_owner`.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/admin/users
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "username": "dm_owner",
  "password": "DmOwnerPwd1!",
  "email":    "dm_owner@example.com",
  "role":     "user"
}

HTTP 201

POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "dm_owner", "password": "DmOwnerPwd1!" }

HTTP 200
[Captures]
owner_token:   jsonpath "$.access_token"
owner_user_id: jsonpath "$.user.full.user.id"


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
# Step 4 — Admin creates a shared drive owned by dm_owner.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/drives
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "kind": "shared",
  "name": "dm-shared",
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


# Baseline used_bytes after the upload settles. Trigger-sweep is
# the deterministic sync point — but only after the spawn'd hook
# has had a chance to land (bug_trigger_sweep_vs_spawn_hook_race.md).
POST {{base_url}}/api/admin/jobs/usage_reconcile/trigger
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
# Step 6 — Move hello.txt across drives → shared root.
#
# Observable behaviour: after the sweep, the source drive's
# used_bytes drops to 0 and the destination's rises to 32. The
# only way this happens is if `storage.files.drive_id` was
# updated on the move (the sweep recomputes from
# `SUM(size) WHERE drive_id = d.id`). The move_file SQL already
# syncs drive_id from the destination — this asserts it still
# does post-D6.
# ─────────────────────────────────────────────────────────────
PUT {{base_url}}/api/files/{{file_id}}/move
Authorization: Bearer {{owner_token}}
Content-Type: application/json
{
  "folder_id": "{{shared_root_id}}"
}

HTTP 200

POST {{base_url}}/api/admin/jobs/usage_reconcile/trigger
Authorization: Bearer {{admin_token}}
[Options]
delay: 200ms

HTTP 200

GET {{base_url}}/api/drives
Authorization: Bearer {{owner_token}}

HTTP 200
[Asserts]
jsonpath "$[?(@.id=='{{personal_drive_id}}')].used_bytes" == 0
jsonpath "$[?(@.id=='{{shared_drive_id}}')].used_bytes"   == 32


# Confirm the file is now visible under the shared drive's root
# (cross-drive Read is fine — dm_owner is Owner on both).
GET {{base_url}}/api/folders/{{shared_root_id}}/resources?limit=50
Authorization: Bearer {{owner_token}}

HTTP 200
[Asserts]
jsonpath "$.items[?(@.resource.id=='{{file_id}}')].resource_type" == "file"


# ─────────────────────────────────────────────────────────────
# Step 7 — Folder move across drives, with a child file inside.
#          The cascade trigger MUST propagate the new drive_id
#          to the moved folder AND every descendant (folder +
#          file). Verified by moving the folder, then sweeping —
#          if the trigger doesn't fire, the descendant file's
#          drive_id stays at the source drive and the sweep
#          attributes its size to the wrong drive.
#
# First, move hello.txt back to the personal drive so the
# baseline for the next case is clean (and so the source-drive
# `used_bytes` reflects only what we're about to nest below).
# ─────────────────────────────────────────────────────────────
PUT {{base_url}}/api/files/{{file_id}}/move
Authorization: Bearer {{owner_token}}
Content-Type: application/json
{
  "folder_id": "{{personal_root_id}}"
}

HTTP 200


# Create a folder under personal root, with hello-copy.txt inside.
POST {{base_url}}/api/folders
Authorization: Bearer {{owner_token}}
Content-Type: application/json
{ "name": "dm-subtree", "parent_id": "{{personal_root_id}}" }

HTTP 201
[Captures]
subtree_id: jsonpath "$.id"

POST {{base_url}}/api/files/upload
Authorization: Bearer {{owner_token}}
[MultipartFormData]
folder_id: {{subtree_id}}
file: file,fixtures/hello-copy.txt; text/plain

HTTP 201
[Captures]
nested_file_id: jsonpath "$.id"


# Baseline post-creation. Personal holds both hello.txt (32 B) +
# nested hello-copy.txt (32 B) = 64. Shared is empty.
#
# `[Options] delay: 200ms` is the workaround for the
# trigger-sweep-vs-spawn-hook race documented in
# bug_trigger_sweep_vs_spawn_hook_race.md: the upload responds 201 as
# soon as the row is written, but the storage-usage delta hook is
# tokio::spawn'd — without the delay, trigger-sweep can run while the
# hook from THIS upload (or a prior move) is still in flight, the
# sweep then recomputes from stale numbers, the late hook adds its
# delta on top, and used_bytes ends up too high by exactly one file's
# size. Symptom: expected 64, got 96 (one extra hook landed late).
# Real fix is await'ing the hook inline server-side; until then this
# delay deflakes the test.
POST {{base_url}}/api/admin/jobs/usage_reconcile/trigger
Authorization: Bearer {{admin_token}}
[Options]
delay: 200ms

HTTP 200

GET {{base_url}}/api/drives
Authorization: Bearer {{owner_token}}

HTTP 200
[Asserts]
jsonpath "$[?(@.id=='{{personal_drive_id}}')].used_bytes" == 64
jsonpath "$[?(@.id=='{{shared_drive_id}}')].used_bytes"   == 0


# Move the SUBTREE FOLDER (with its nested file) into the shared
# drive's root.
PUT {{base_url}}/api/folders/{{subtree_id}}/move
Authorization: Bearer {{owner_token}}
Content-Type: application/json
{
  "parent_id": "{{shared_root_id}}"
}

HTTP 200


# The load-bearing assertion. After sweep:
#   personal: hello.txt remains (32)
#   shared:   nested hello-copy.txt now charged here (32)
# Anything other than (32, 32) means the descendant file's
# drive_id wasn't cascaded by the trigger.
POST {{base_url}}/api/admin/jobs/usage_reconcile/trigger
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


# Folder is visible in shared's listing.
GET {{base_url}}/api/folders/{{shared_root_id}}/resources?limit=50
Authorization: Bearer {{owner_token}}

HTTP 200
[Asserts]
jsonpath "$.items[?(@.resource.id=='{{subtree_id}}')].resource_type" == "folder"


# Descendant file is still inside the moved subtree (subtree
# integrity preserved). Drive_id sync is invisible at this
# endpoint, but the used_bytes assertion above already
# established it.
GET {{base_url}}/api/folders/{{subtree_id}}/resources?limit=50
Authorization: Bearer {{owner_token}}

HTTP 200
[Asserts]
jsonpath "$.items[?(@.resource.id=='{{nested_file_id}}')].resource_type" == "file"


# ─────────────────────────────────────────────────────────────
# Step 8 — Cleanup. Move both files back to the personal drive's
#          root + delete the subtree folder + delete the shared
#          drive (must be empty), then the test user.
# ─────────────────────────────────────────────────────────────
PUT {{base_url}}/api/files/{{nested_file_id}}/move
Authorization: Bearer {{owner_token}}
Content-Type: application/json
{
  "folder_id": "{{personal_root_id}}"
}

HTTP 200

DELETE {{base_url}}/api/folders/{{subtree_id}}
Authorization: Bearer {{owner_token}}

HTTP 204

DELETE {{base_url}}/api/drives/{{shared_drive_id}}
Authorization: Bearer {{owner_token}}

HTTP 204

DELETE {{base_url}}/api/admin/users/{{owner_user_id}}
Authorization: Bearer {{admin_token}}

HTTP 200
