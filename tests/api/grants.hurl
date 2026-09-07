# =============================================================
# OxiCloud — ReBAC grant management (POST/DELETE/GET /api/grants)
# =============================================================
# Exercises cross-user grants, cascading, roles, revoke, lifecycle
# cleanup. Uses ONLY endpoints that route through the
# AuthorizationEngine — handler-layer inline checks (e.g.
# GET /api/folders/{id}) are scheduled for cleanup separately.
#
# Runs AFTER permissions.hurl (bob already exists). Self-contained
# resources (unique names) so it doesn't depend on prior state.
# =============================================================


# ─────────────────────────────────────────────────────────────
# Step 1 — Login as admin (Alice), capture token + home folder.
# `alice_user_id` is captured for the D0 §14 provenance assertions
# that compare `created_by` / `updated_by` on resources Alice owns.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "{{username}}", "password": "{{password}}" }

HTTP 200
[Captures]
alice_token: jsonpath "$.access_token"
alice_user_id: jsonpath "$.user.full.user.id"

GET {{base_url}}/api/folders
Authorization: Bearer {{alice_token}}

HTTP 200
[Captures]
alice_home_id: jsonpath "$[0].id"


# ─────────────────────────────────────────────────────────────
# Step 2 — Create two test users specific to this file (dave + eve).
#           Avoids cross-file dependencies on bob from permissions.hurl
#           and gives us their user_id directly from the create response.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/admin/users
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{ "username": "dave", "password": "DavePassword1!", "email": "dave@example.com", "role": "user" }

HTTP 201
[Captures]
dave_user_id: jsonpath "$.user.id"

POST {{base_url}}/api/admin/users
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{ "username": "eve", "password": "EvePassword1!", "email": "eve@example.com", "role": "user" }

HTTP 201
[Captures]
eve_user_id: jsonpath "$.user.id"


# ─────────────────────────────────────────────────────────────
# Step 3 — Login dave and eve.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "dave", "password": "DavePassword1!" }

HTTP 200
[Captures]
dave_token: jsonpath "$.access_token"

POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "eve", "password": "EvePassword1!" }

HTTP 200
[Captures]
eve_token: jsonpath "$.access_token"


# ─────────────────────────────────────────────────────────────
# Step 4 — Alice creates a folder "grant-shared" + a child "grant-child".
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/folders
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{ "name": "grant-shared", "parent_id": "{{alice_home_id}}" }

HTTP 201
[Captures]
shared_folder_id: jsonpath "$.id"
[Asserts]
# D0 §14 provenance — Alice creates, so both fields stamp Alice.
jsonpath "$.created_by" == "{{alice_user_id}}"
jsonpath "$.updated_by" == "{{alice_user_id}}"

POST {{base_url}}/api/folders
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{ "name": "grant-child", "parent_id": "{{shared_folder_id}}" }

HTTP 201
[Captures]
child_folder_id: jsonpath "$.id"


# ─────────────────────────────────────────────────────────────
# Step 5 — Without any grant, bob cannot rename Alice's folder.
#           PUT /api/folders/{id}/rename goes through the engine →
#           404 (anti-enumeration).
# ─────────────────────────────────────────────────────────────
PUT {{base_url}}/api/folders/{{shared_folder_id}}/rename
Authorization: Bearer {{dave_token}}
Content-Type: application/json
{ "name": "bob-tried" }

HTTP 404


# ─────────────────────────────────────────────────────────────
# Step 6 — Alice grants Bob the Viewer role. Server expands → [read].
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/grants
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{
  "subject":  { "type": "user",   "id": "{{dave_user_id}}" },
  "resource": { "type": "folder", "id": "{{shared_folder_id}}" },
  "role": "viewer"
}

HTTP 201
# Cleanup PR: one role row per (subject, resource). `CreateGrantResponseDto`
# wraps a single role-keyed Grant in `.grants[0]`.
[Asserts]
jsonpath "$.grants" count == 1
jsonpath "$.grants[0].role" == "viewer"


# ─────────────────────────────────────────────────────────────
# Step 7 — Viewer cannot rename (no Update grant). Dave has Read
# (viewer role) → graduated denial returns 403.
# ─────────────────────────────────────────────────────────────
PUT {{base_url}}/api/folders/{{shared_folder_id}}/rename
Authorization: Bearer {{dave_token}}
Content-Type: application/json
{ "name": "bob-tried-again" }

HTTP 403


# ─────────────────────────────────────────────────────────────
# Step 8 — Bob's incoming grants list contains the new grant.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/grants/incoming
Authorization: Bearer {{dave_token}}

HTTP 200
[Asserts]
jsonpath "$[?(@.resource.id=='{{shared_folder_id}}')].role" == "viewer"


# ─────────────────────────────────────────────────────────────
# Step 9 — Promote Bob to Owner (covers comment, create, update, share,
#           delete, manage). PUT /api/grants/role replaces the role in one
#           UPSERT against `storage.role_grants`.
# ─────────────────────────────────────────────────────────────
PUT {{base_url}}/api/grants/role
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{
  "subject":  { "type": "user",   "id": "{{dave_user_id}}" },
  "resource": { "type": "folder", "id": "{{shared_folder_id}}" },
  "role": "owner"
}

HTTP 200
[Asserts]
jsonpath "$" count == 1
jsonpath "$[0].role" == "owner"


# ─────────────────────────────────────────────────────────────
# Step 10 — Bob can now rename (Owner includes update).
# ─────────────────────────────────────────────────────────────
PUT {{base_url}}/api/folders/{{shared_folder_id}}/rename
Authorization: Bearer {{dave_token}}
Content-Type: application/json
{ "name": "renamed-by-bob-as-admin" }

HTTP 200


# ─────────────────────────────────────────────────────────────
# Step 11 — Cascading: Bob can also rename the CHILD folder, because
#           his Update grant on the parent cascades via ltree to the
#           child resource — even though no direct grant on the child.
# ─────────────────────────────────────────────────────────────
PUT {{base_url}}/api/folders/{{child_folder_id}}/rename
Authorization: Bearer {{dave_token}}
Content-Type: application/json
{ "name": "renamed-child-via-cascade" }

HTTP 200


# ─────────────────────────────────────────────────────────────
# Step 12 — Bob re-shares to Carol (he has Share via Owner).
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/grants
Authorization: Bearer {{dave_token}}
Content-Type: application/json
{
  "subject":  { "type": "user",   "id": "{{eve_user_id}}" },
  "resource": { "type": "folder", "id": "{{shared_folder_id}}" },
  "role": "viewer"
}

HTTP 201
[Captures]
eve_grant_id: jsonpath "$.grants[0].id"


# ─────────────────────────────────────────────────────────────
# Step 13 — Carol can see the grant in her incoming list.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/grants/incoming
Authorization: Bearer {{eve_token}}

HTTP 200
[Asserts]
jsonpath "$[?(@.resource.id=='{{shared_folder_id}}')].role" == "viewer"


# ─────────────────────────────────────────────────────────────
# Step 14 — Bob's outgoing grants list contains the grant to Carol.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/grants/outgoing
Authorization: Bearer {{dave_token}}

HTTP 200
[Asserts]
jsonpath "$[?(@.id=='{{eve_grant_id}}')].id" == "{{eve_grant_id}}"


# ─────────────────────────────────────────────────────────────
# Step 15 — Demote Bob to Viewer; he loses update/share/etc.
# ─────────────────────────────────────────────────────────────
PUT {{base_url}}/api/grants/role
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{
  "subject":  { "type": "user",   "id": "{{dave_user_id}}" },
  "resource": { "type": "folder", "id": "{{shared_folder_id}}" },
  "role": "viewer"
}

HTTP 200
[Asserts]
jsonpath "$" count == 1
jsonpath "$[0].role" == "viewer"


# ─────────────────────────────────────────────────────────────
# Step 16 — Demoted Bob (now Viewer) can no longer rename. Read
# is still granted → graduated denial returns 403.
# ─────────────────────────────────────────────────────────────
PUT {{base_url}}/api/folders/{{shared_folder_id}}/rename
Authorization: Bearer {{dave_token}}
Content-Type: application/json
{ "name": "bob-tried-after-demote" }

HTTP 403


# ─────────────────────────────────────────────────────────────
# Step 17 — Lifecycle: Alice deletes the folder. The DB trigger
#           trg_cleanup_role_grants_folder removes both bob's and
#           carol's grants automatically (also for the cascade-deleted
#           child).
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/api/folders/{{child_folder_id}}
Authorization: Bearer {{alice_token}}

HTTP 204

DELETE {{base_url}}/api/folders/{{shared_folder_id}}
Authorization: Bearer {{alice_token}}

HTTP 204

DELETE {{base_url}}/api/trash/empty
Authorization: Bearer {{alice_token}}

HTTP 200


# ─────────────────────────────────────────────────────────────
# Step 18 — After permanent delete, Bob's incoming list no longer
#            contains the deleted folder's grant.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/grants/incoming
Authorization: Bearer {{dave_token}}

HTTP 200
[Asserts]
# Every user carries three self-owned Owner grants provisioned by
# the lifecycle hooks:
#   * personal drive         (PersonalDriveLifecycleHook, D0)
#   * default calendar       (DefaultCalendarLifecycleHook, #545)
#   * default address book   (DefaultAddressBookLifecycleHook, #545)
# The pre-lifecycle-hook assertion here was "no grants at all"
# (count == 0). D0 shifted it to "exactly the drive Owner grant"
# (count == 1). Adding the CalDAV/CardDAV defaults shifts it again
# to count == 3. Body-contains checks for each resource type are
# ordering-agnostic (the incoming feed doesn't guarantee stable
# ordering across resource types) and mirror the pattern used by
# default_caldav_carddav.hurl.
jsonpath "$" count == 3
body contains "\"type\":\"drive\""
body contains "\"type\":\"calendar\""
body contains "\"type\":\"address_book\""


# ─────────────────────────────────────────────────────────────
# Step 19 — Same for Carol.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/grants/incoming
Authorization: Bearer {{eve_token}}

HTTP 200
[Asserts]
# See Step 18 for the invariant rationale (three self-owned Owner
# grants per user from the lifecycle hooks).
jsonpath "$" count == 3
body contains "\"type\":\"drive\""
body contains "\"type\":\"calendar\""
body contains "\"type\":\"address_book\""


# ════════════════════════════════════════════════════════════════════
# PHASE 2 — Comprehensive permission coverage with fresh user "adam".
# ════════════════════════════════════════════════════════════════════
# Exercises every engine-aware endpoint at each permission tier:
#
#   no grant   → all read/write/delete operations return 404
#   Viewer     → read endpoints OK, modify/delete endpoints return 404
#   Editor     → update + create + thumbnail-push OK, delete still 404
#   Admin      → everything including delete
#
# Endpoints in scope (all routed through the AuthorizationEngine):
#   Folders:  /contents · /contents/paginated · /listing · /download (zip)
#             · POST / · PUT /{id}/rename · PUT /{id}/move · DELETE /{id}
#   Files:    GET /  · GET /{id} (download)
#             · GET /{id}/metadata · GET /{id}/thumbnail/{size}
#             · PUT /{id}/thumbnail/{size} (push, Update)
#             · PUT /{id}/rename · PUT /{id}/move · DELETE /{id}
#             · POST /upload (via folder require_permission)
#
# Listing endpoints that are still owner-scoped (GET /api/folders root,
# GET /api/folders/paginated) are NOT covered here — they don't
# reflect grants today and are tracked as separate cleanup work.


# ─────────────────────────────────────────────────────────────
# Step 20 — Create user adam (fresh, no relationship to alice's tree).
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/admin/users
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{ "username": "adam", "password": "AdamPassword1!", "email": "adam@example.com", "role": "user" }

HTTP 201
[Captures]
adam_user_id: jsonpath "$.user.id"

POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "adam", "password": "AdamPassword1!" }

HTTP 200
[Captures]
adam_token: jsonpath "$.access_token"


# ─────────────────────────────────────────────────────────────
# Step 21 — Alice creates a fresh shareable folder, sub-folder, and
#           uploads a JPEG (which the server auto-thumbnails).
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/folders
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{ "name": "perm-test-folder", "parent_id": "{{alice_home_id}}" }

HTTP 201
[Captures]
perm_folder_id: jsonpath "$.id"

POST {{base_url}}/api/folders
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{ "name": "perm-test-child", "parent_id": "{{perm_folder_id}}" }

HTTP 201
[Captures]
perm_child_id: jsonpath "$.id"

POST {{base_url}}/api/files/upload
Authorization: Bearer {{alice_token}}
[MultipartFormData]
folder_id: {{perm_folder_id}}
file: file,fixtures/oxicloud-logo.jpg; image/jpeg

HTTP 201
[Captures]
perm_file_id: jsonpath "$.id"


# ════════════════════════════════════════════════════════════════════
# Phase 2A — Adam has NO grant. Every engine-aware endpoint denies.
# ════════════════════════════════════════════════════════════════════

# ── Folder reads ─────────────────────────────────────────────
GET {{base_url}}/api/folders/{{perm_folder_id}}/resources?resource_types=folder
Authorization: Bearer {{adam_token}}

HTTP 404

GET {{base_url}}/api/folders/{{perm_folder_id}}/resources
Authorization: Bearer {{adam_token}}

HTTP 404

GET {{base_url}}/api/folders/{{perm_folder_id}}/resources
Authorization: Bearer {{adam_token}}

HTTP 404

GET {{base_url}}/api/folders/{{perm_folder_id}}/download
Authorization: Bearer {{adam_token}}

HTTP 404

# ── File reads ───────────────────────────────────────────────
GET {{base_url}}/api/files?folder_id={{perm_folder_id}}
Authorization: Bearer {{adam_token}}

HTTP 404

GET {{base_url}}/api/files/{{perm_file_id}}
Authorization: Bearer {{adam_token}}

HTTP 404

GET {{base_url}}/api/files/{{perm_file_id}}/metadata
Authorization: Bearer {{adam_token}}

HTTP 404

GET {{base_url}}/api/files/{{perm_file_id}}/thumbnail/icon
Authorization: Bearer {{adam_token}}

HTTP 404

# ── Folder mutations ─────────────────────────────────────────
POST {{base_url}}/api/folders
Authorization: Bearer {{adam_token}}
Content-Type: application/json
{ "name": "adam-attack", "parent_id": "{{perm_folder_id}}" }

HTTP 404

PUT {{base_url}}/api/folders/{{perm_folder_id}}/rename
Authorization: Bearer {{adam_token}}
Content-Type: application/json
{ "name": "adam-rename-attempt" }

HTTP 404

DELETE {{base_url}}/api/folders/{{perm_folder_id}}
Authorization: Bearer {{adam_token}}

HTTP 404

# ── File mutations ───────────────────────────────────────────
POST {{base_url}}/api/files/upload
Authorization: Bearer {{adam_token}}
[MultipartFormData]
folder_id: {{perm_folder_id}}
file: file,fixtures/hello.txt; text/plain

HTTP 404

PUT {{base_url}}/api/files/{{perm_file_id}}/rename
Authorization: Bearer {{adam_token}}
Content-Type: application/json
{ "name": "adam-file-rename" }

HTTP 404

PUT {{base_url}}/api/files/{{perm_file_id}}/thumbnail/icon
Authorization: Bearer {{adam_token}}
Content-Type: image/png
file,fixtures/blue-image.png;

HTTP 404

DELETE {{base_url}}/api/files/{{perm_file_id}}
Authorization: Bearer {{adam_token}}

HTTP 404

# ── Chunked upload: cannot start session in alice's folder ──
# create_upload_impl pre-checks Permission::Create via require_permission.
POST {{base_url}}/api/uploads
Authorization: Bearer {{adam_token}}
Content-Type: application/json
{
  "filename": "adam-chunked-attack.mp4",
  "folder_id": "{{perm_folder_id}}",
  "content_type": "video/mp4",
  "total_size": 2760653,
  "chunk_size": 3000000
}

HTTP 404


# ════════════════════════════════════════════════════════════════════
# Phase 2B — Alice grants adam Viewer. Read OK, mutate/delete denied.
# ════════════════════════════════════════════════════════════════════
POST {{base_url}}/api/grants
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{
  "subject":  { "type": "user",   "id": "{{adam_user_id}}" },
  "resource": { "type": "folder", "id": "{{perm_folder_id}}" },
  "role": "viewer"
}

HTTP 201

# ── Read endpoints now succeed ──────────────────────────────
GET {{base_url}}/api/folders/{{perm_folder_id}}/resources?resource_types=folder
Authorization: Bearer {{adam_token}}

HTTP 200
[Asserts]
jsonpath "$.items" count == 1
jsonpath "$.items[0].resource.id" == "{{perm_child_id}}"

GET {{base_url}}/api/folders/{{perm_folder_id}}/resources
Authorization: Bearer {{adam_token}}

HTTP 200

GET {{base_url}}/api/folders/{{perm_folder_id}}/resources
Authorization: Bearer {{adam_token}}

HTTP 200

GET {{base_url}}/api/folders/{{perm_folder_id}}/download
Authorization: Bearer {{adam_token}}

HTTP 200
[Asserts]
header "Content-Type" contains "zip"

GET {{base_url}}/api/files?folder_id={{perm_folder_id}}
Authorization: Bearer {{adam_token}}

HTTP 200
[Asserts]
jsonpath "$" count == 1
jsonpath "$[0].id" == "{{perm_file_id}}"

GET {{base_url}}/api/files/{{perm_file_id}}
Authorization: Bearer {{adam_token}}

HTTP 200

GET {{base_url}}/api/files/{{perm_file_id}}/metadata
Authorization: Bearer {{adam_token}}

HTTP 200

GET {{base_url}}/api/files/{{perm_file_id}}/thumbnail/icon
Authorization: Bearer {{adam_token}}

HTTP 200
[Asserts]
header "Content-Type" startsWith "image/"

# ── Cascading: child folder also readable via parent's grant ─
GET {{base_url}}/api/folders/{{perm_child_id}}/resources?resource_types=folder
Authorization: Bearer {{adam_token}}

HTTP 200

# ── Mutations still denied (Viewer has no Update/Create/Delete).
#    Viewer has Read → graduated denial returns 403 (not 404
#    anti-enum, which is reserved for Phase 2A above where Adam
#    had no Read at all).
POST {{base_url}}/api/folders
Authorization: Bearer {{adam_token}}
Content-Type: application/json
{ "name": "adam-attack-2", "parent_id": "{{perm_folder_id}}" }

HTTP 403

PUT {{base_url}}/api/folders/{{perm_folder_id}}/rename
Authorization: Bearer {{adam_token}}
Content-Type: application/json
{ "name": "adam-rename-as-viewer" }

HTTP 403

PUT {{base_url}}/api/files/{{perm_file_id}}/rename
Authorization: Bearer {{adam_token}}
Content-Type: application/json
{ "name": "adam-file-rename-as-viewer" }

HTTP 403

PUT {{base_url}}/api/files/{{perm_file_id}}/thumbnail/icon
Authorization: Bearer {{adam_token}}
Content-Type: image/png
file,fixtures/blue-image.png;

HTTP 403

POST {{base_url}}/api/files/upload
Authorization: Bearer {{adam_token}}
[MultipartFormData]
folder_id: {{perm_folder_id}}
file: file,fixtures/hello.txt; text/plain

HTTP 403

DELETE {{base_url}}/api/files/{{perm_file_id}}
Authorization: Bearer {{adam_token}}

HTTP 403

DELETE {{base_url}}/api/folders/{{perm_folder_id}}
Authorization: Bearer {{adam_token}}

HTTP 403

# ── Viewer cannot start a chunked upload (no Create grant) ──
POST {{base_url}}/api/uploads
Authorization: Bearer {{adam_token}}
Content-Type: application/json
{
  "filename": "viewer-chunked-attempt.mp4",
  "folder_id": "{{perm_folder_id}}",
  "content_type": "video/mp4",
  "total_size": 2760653,
  "chunk_size": 3000000
}

HTTP 403


# ════════════════════════════════════════════════════════════════════
# Phase 2C — Promote adam to Editor (read + comment + create + update).
#           Create + Update endpoints now succeed; Delete still denied.
# ════════════════════════════════════════════════════════════════════
PUT {{base_url}}/api/grants/role
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{
  "subject":  { "type": "user",   "id": "{{adam_user_id}}" },
  "resource": { "type": "folder", "id": "{{perm_folder_id}}" },
  "role": "editor"
}

HTTP 200

# ── Update succeeds ─────────────────────────────────────────
PUT {{base_url}}/api/folders/{{perm_folder_id}}/rename
Authorization: Bearer {{adam_token}}
Content-Type: application/json
{ "name": "renamed-by-adam-as-editor" }

HTTP 200
[Asserts]
# D0 §14 provenance — folder counterpart of the file rename below.
# Adam (Editor) mutates Alice's folder; `updated_by` becomes Adam,
# `created_by` stays Alice.
jsonpath "$.created_by" == "{{alice_user_id}}"
jsonpath "$.updated_by" == "{{adam_user_id}}"

PUT {{base_url}}/api/files/{{perm_file_id}}/rename
Authorization: Bearer {{adam_token}}
Content-Type: application/json
{ "name": "adam-renamed-logo.jpg" }

HTTP 200
[Asserts]
# D0 §14 provenance — Adam (an Editor, not the owner) mutates the
# file, so `updated_by` switches to Adam's id while `created_by`
# stays Alice (the original uploader). This is the canonical
# cross-user provenance check: distinguishes "who first put this
# here" from "who last touched it" and proves the mutator's id
# overrides the row's `user_id` (pre-D0 they were silently the
# same; post-D0 they can diverge once a non-owner mutates).
jsonpath "$.created_by" == "{{alice_user_id}}"
jsonpath "$.updated_by" == "{{adam_user_id}}"

# ── D0 §14 provenance survives on the LISTING endpoint too ──
# The rename-response asserts above cover the mutation DTO, but
# /api/folders/{id}/resources has its own DTO-build path that
# used to hardcode created_by/updated_by = None (silent bug —
# owner column rendered "—" on /files for everyone). Hit the
# listing and re-assert both the untouched folder (both = alice)
# AND the Adam-renamed file (created_by=alice, updated_by=adam)
# on the same page — two shapes, one round-trip.
#
# Fixed indices are safe because at this point perm_folder_id
# holds exactly two rows and the default order_by=name puts
# 'perm-test-child' (folder) at [0] and 'adam-renamed-logo.jpg'
# (file) at [1]. Anything appended to this folder later in the
# scenario would break these indices — hence the assertion runs
# BEFORE the subsequent thumbnail/create/upload steps.
GET {{base_url}}/api/folders/{{perm_folder_id}}/resources
Authorization: Bearer {{alice_token}}

HTTP 200
[Asserts]
jsonpath "$.items" count == 2
# [0] — untouched folder inherits Alice on both fields.
jsonpath "$.items[0].resource.name" == "perm-test-child"
jsonpath "$.items[0].resource.created_by" == "{{alice_user_id}}"
jsonpath "$.items[0].resource.updated_by" == "{{alice_user_id}}"
# [1] — file Adam renamed. created_by stays alice (original
# uploader), updated_by is adam (last mutator). Canonical
# listing-side cross-user split.
jsonpath "$.items[1].resource.name" == "adam-renamed-logo.jpg"
jsonpath "$.items[1].resource.created_by" == "{{alice_user_id}}"
jsonpath "$.items[1].resource.updated_by" == "{{adam_user_id}}"
# Caller-flag contract on the listing endpoint. Neither row
# favorited by Alice → is_favorite = false. The `perm_folder_id`
# tree carries a role_grant on the FOLDER (Adam as Editor), not
# on `perm-test-child` or `adam-renamed-logo.jpg` — so both
# child rows should carry is_shared = false; the grant on the
# parent doesn't cascade to per-child EXISTS.
jsonpath "$.items[0].resource.is_favorite" == false
jsonpath "$.items[0].resource.is_shared" == false
jsonpath "$.items[1].resource.is_favorite" == false
jsonpath "$.items[1].resource.is_shared" == false

# ── Thumbnail push (Update) succeeds ────────────────────────
PUT {{base_url}}/api/files/{{perm_file_id}}/thumbnail/preview
Authorization: Bearer {{adam_token}}
Content-Type: image/png
file,fixtures/blue-image.png;

HTTP 201

# ── Create succeeds ─────────────────────────────────────────
POST {{base_url}}/api/folders
Authorization: Bearer {{adam_token}}
Content-Type: application/json
{ "name": "adam-created-child", "parent_id": "{{perm_folder_id}}" }

HTTP 201
[Asserts]
# D0 §14 provenance — Adam (Editor on Alice's folder) creates a
# child folder inside it. Both `created_by` and `updated_by` stamp
# Adam: he's the original author AND the last toucher of this
# fresh row. The parent's owner (Alice) doesn't appear anywhere on
# the new row's provenance — content authored in a shared scope
# belongs to its author.
jsonpath "$.created_by" == "{{adam_user_id}}"
jsonpath "$.updated_by" == "{{adam_user_id}}"

POST {{base_url}}/api/files/upload
Authorization: Bearer {{adam_token}}
[MultipartFormData]
folder_id: {{perm_folder_id}}
file: file,fixtures/hello.txt; text/plain

HTTP 201
[Asserts]
# Same shape for a file upload: Adam authored, Adam touched last.
jsonpath "$.created_by" == "{{adam_user_id}}"
jsonpath "$.updated_by" == "{{adam_user_id}}"

# ── Chunked upload full lifecycle as Editor ─────────────────
# 1. Open session (server pre-checks Create on folder)
POST {{base_url}}/api/uploads
Authorization: Bearer {{adam_token}}
Content-Type: application/json
{
  "filename": "adam-chunked-video.mp4",
  "folder_id": "{{perm_folder_id}}",
  "content_type": "video/mp4",
  "total_size": 2760653,
  "chunk_size": 3000000
}

HTTP 201
[Captures]
adam_upload_id: jsonpath "$.upload_id"

# 2. Send the single chunk (chunk_size > total_size → 1 chunk).
PATCH {{base_url}}/api/uploads/{{adam_upload_id}}?chunk_index=0
Authorization: Bearer {{adam_token}}
Content-Type: application/octet-stream
file,fixtures/free_video_over_1MB.mp4;

HTTP 200

# 3. Status query: dave (different user) cannot peek at adam's session.
HEAD {{base_url}}/api/uploads/{{adam_upload_id}}
Authorization: Bearer {{dave_token}}

HTTP 404

# 4. Cancel attempt by a different user is rejected.
DELETE {{base_url}}/api/uploads/{{adam_upload_id}}
Authorization: Bearer {{dave_token}}

HTTP 404

# 5. Adam completes the upload — file is created in alice's folder.
POST {{base_url}}/api/uploads/{{adam_upload_id}}/complete
Authorization: Bearer {{adam_token}}

HTTP 201
[Captures]
adam_chunked_file_id: jsonpath "$.file_id"

# 6. The new file is visible in the folder listing (caller-of-listing is alice).
GET {{base_url}}/api/files?folder_id={{perm_folder_id}}
Authorization: Bearer {{alice_token}}

HTTP 200
[Asserts]
jsonpath "$[?(@.id=='{{adam_chunked_file_id}}')].name" == "adam-chunked-video.mp4"

# 7. A second session that adam cancels before completing — cleanup path.
POST {{base_url}}/api/uploads
Authorization: Bearer {{adam_token}}
Content-Type: application/json
{
  "filename": "adam-cancelled.mp4",
  "folder_id": "{{perm_folder_id}}",
  "content_type": "video/mp4",
  "total_size": 2760653,
  "chunk_size": 3000000
}

HTTP 201
[Captures]
adam_cancel_id: jsonpath "$.upload_id"

DELETE {{base_url}}/api/uploads/{{adam_cancel_id}}
Authorization: Bearer {{adam_token}}

HTTP 204

# ── Regression pin for AuthZ audit #17 (2026-07-12). ─────────
# The chunked-upload `complete` handler used to call plain
# `upload_file_streaming` at finalize — no `_with_perms` check.
# A grant revoked between session-open and finalize stayed
# effective until the last chunk landed (up to 24h JWT TTL,
# forever with app-passwords). Fix: swap to
# `upload_file_streaming_with_perms` so `authz.require(Create,
# Folder)` re-runs at complete time.
#
# Sequence:
#   1. Adam (Editor) opens a session — pre-check passes.
#   2. Adam PATCHes the single chunk (chunk upload is unauth'd,
#      always allowed).
#   3. Alice DEMOTES Adam to Viewer (Viewer bundle has Read but
#      no Create).
#   4. Adam POST /complete → 403 (pre-fix: 201 + file created).
#   5. Cleanup: cancel the orphaned session + re-promote Adam
#      to Editor so the following steps aren't disturbed.

# 1 — Open session while Editor.
POST {{base_url}}/api/uploads
Authorization: Bearer {{adam_token}}
Content-Type: application/json
{
  "filename": "audit17-post-revoke.mp4",
  "folder_id": "{{perm_folder_id}}",
  "content_type": "video/mp4",
  "total_size": 2760653,
  "chunk_size": 3000000
}

HTTP 201
[Captures]
audit17_upload_id: jsonpath "$.upload_id"


# 2 — Send the single chunk (session pre-authorised).
PATCH {{base_url}}/api/uploads/{{audit17_upload_id}}?chunk_index=0
Authorization: Bearer {{adam_token}}
Content-Type: application/octet-stream
file,fixtures/free_video_over_1MB.mp4;

HTTP 200


# 3 — Alice demotes Adam Editor → Viewer (Create removed).
PUT {{base_url}}/api/grants/role
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{
  "subject":  { "type": "user",   "id": "{{adam_user_id}}" },
  "resource": { "type": "folder", "id": "{{perm_folder_id}}" },
  "role":     "viewer"
}

HTTP 200


# 4 — Finalize now fails: engine re-checks Create at complete
# time. Adam still has Read (viewer role) → graduated denial
# returns 403; pre-fix returned 201 with a phantom file.
POST {{base_url}}/api/uploads/{{audit17_upload_id}}/complete
Authorization: Bearer {{adam_token}}

HTTP 403


# 5a — The session is orphaned (chunks on disk, no completion).
# Cancel it as Adam (still owns the session, so the `_with_perms`
# gate on DELETE-session lets him through).
DELETE {{base_url}}/api/uploads/{{audit17_upload_id}}
Authorization: Bearer {{adam_token}}

HTTP 204


# 5b — Restore Adam to Editor so subsequent steps behave as
# before this regression pin was inserted.
PUT {{base_url}}/api/grants/role
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{
  "subject":  { "type": "user",   "id": "{{adam_user_id}}" },
  "resource": { "type": "folder", "id": "{{perm_folder_id}}" },
  "role":     "editor"
}

HTTP 200


# ── Delete still denied (Editor excludes Delete). Editor has
#    Read → graduated denial returns 403.
DELETE {{base_url}}/api/files/{{perm_file_id}}
Authorization: Bearer {{adam_token}}

HTTP 403

DELETE {{base_url}}/api/folders/{{perm_folder_id}}
Authorization: Bearer {{adam_token}}

HTTP 403


# ════════════════════════════════════════════════════════════════════
# Phase 2D — Promote adam to Owner (full bundle, incl. delete). Delete OK.
# ════════════════════════════════════════════════════════════════════
PUT {{base_url}}/api/grants/role
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{
  "subject":  { "type": "user",   "id": "{{adam_user_id}}" },
  "resource": { "type": "folder", "id": "{{perm_folder_id}}" },
  "role": "owner"
}

HTTP 200

DELETE {{base_url}}/api/files/{{perm_file_id}}
Authorization: Bearer {{adam_token}}

HTTP 204


# ════════════════════════════════════════════════════════════════════
# Phase 2E — Lifecycle cleanup. Alice (still the owner) trashes &
#           empties; the trigger removes all role_grants rows.
# ════════════════════════════════════════════════════════════════════
DELETE {{base_url}}/api/folders/{{perm_folder_id}}
Authorization: Bearer {{alice_token}}

HTTP 204

DELETE {{base_url}}/api/trash/empty
Authorization: Bearer {{alice_token}}

HTTP 200

# Adam's incoming list holds only his three self-owned Owner grants
# (drive + calendar + address_book — provisioned by the lifecycle
# hooks). No inbound grants from other users.
GET {{base_url}}/api/grants/incoming
Authorization: Bearer {{adam_token}}

HTTP 200
[Asserts]
# See Step 18 above for the full invariant rationale — three
# self-owned Owner grants per user (drive + calendar +
# address_book). Body-contains rather than positional check
# because the incoming feed doesn't guarantee stable ordering
# across resource types.
jsonpath "$" count == 3
body contains "\"type\":\"drive\""
body contains "\"type\":\"calendar\""
body contains "\"type\":\"address_book\""


# ════════════════════════════════════════════════════════════════════
# PHASE 3 — Batch operations (/api/batch/*)
# ════════════════════════════════════════════════════════════════════
# Every batch endpoint passes caller_id through to the batch service,
# which delegates per-item to engine-aware *_with_perms methods. The
# handler aggregates results: 200 (all OK), 206 (mixed), 400 (all failed).
#
# Endpoints exercised:
#   POST   /api/batch/files/get      · /api/batch/files/move
#   POST   /api/batch/files/copy     · /api/batch/files/delete
#   POST   /api/batch/folders/get    · /api/batch/folders/create
#   POST   /api/batch/folders/move   · /api/batch/folders/copy
#   POST   /api/batch/folders/delete · /api/batch/trash
#   POST   /api/batch/download       · GET /api/batch/download (querystring)
#
# Fresh user "frank" — no grants from earlier phases.


# ─────────────────────────────────────────────────────────────
# Step P3.1 — Create frank and login.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/admin/users
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{ "username": "frank", "password": "FrankPassword1!", "email": "frank@example.com", "role": "user" }

HTTP 201
[Captures]
frank_user_id: jsonpath "$.user.id"

POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "frank", "password": "FrankPassword1!" }

HTTP 200
[Captures]
frank_token: jsonpath "$.access_token"


# ─────────────────────────────────────────────────────────────
# Step P3.2 — Alice creates a batch-test folder with 2 sub-folders
#             and 2 files (all owned by alice).
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/folders
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{ "name": "batch-test", "parent_id": "{{alice_home_id}}" }

HTTP 201
[Captures]
batch_root_id: jsonpath "$.id"

POST {{base_url}}/api/folders
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{ "name": "batch-sub-A", "parent_id": "{{batch_root_id}}" }

HTTP 201
[Captures]
batch_sub_a_id: jsonpath "$.id"

POST {{base_url}}/api/folders
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{ "name": "batch-sub-B", "parent_id": "{{batch_root_id}}" }

HTTP 201
[Captures]
batch_sub_b_id: jsonpath "$.id"

POST {{base_url}}/api/files/upload
Authorization: Bearer {{alice_token}}
[MultipartFormData]
folder_id: {{batch_root_id}}
file: file,fixtures/red-image.png; image/png

HTTP 201
[Captures]
batch_file_1_id: jsonpath "$.id"

POST {{base_url}}/api/files/upload
Authorization: Bearer {{alice_token}}
[MultipartFormData]
folder_id: {{batch_root_id}}
file: file,fixtures/green-image.png; image/png

HTTP 201
[Captures]
batch_file_2_id: jsonpath "$.id"


# ════════════════════════════════════════════════════════════════════
# Phase 3A — frank has NO grant. Every batch op returns 400 (all failed).
# ════════════════════════════════════════════════════════════════════

# Files — get
POST {{base_url}}/api/batch/files/get
Authorization: Bearer {{frank_token}}
Content-Type: application/json
{ "file_ids": ["{{batch_file_1_id}}", "{{batch_file_2_id}}"] }

HTTP 400
[Asserts]
jsonpath "$.stats.successful" == 0
jsonpath "$.stats.failed" == 2

# Files — move
POST {{base_url}}/api/batch/files/move
Authorization: Bearer {{frank_token}}
Content-Type: application/json
{ "file_ids": ["{{batch_file_1_id}}"], "target_folder_id": "{{batch_sub_a_id}}" }

HTTP 400
[Asserts]
jsonpath "$.stats.failed" == 1

# Files — copy
POST {{base_url}}/api/batch/files/copy
Authorization: Bearer {{frank_token}}
Content-Type: application/json
{ "file_ids": ["{{batch_file_1_id}}"], "target_folder_id": "{{batch_sub_a_id}}" }

HTTP 400
[Asserts]
jsonpath "$.stats.failed" == 1

# Files — delete
POST {{base_url}}/api/batch/files/delete
Authorization: Bearer {{frank_token}}
Content-Type: application/json
{ "file_ids": ["{{batch_file_1_id}}"] }

HTTP 400
[Asserts]
jsonpath "$.stats.failed" == 1

# Folders — get
POST {{base_url}}/api/batch/folders/get
Authorization: Bearer {{frank_token}}
Content-Type: application/json
{ "folder_ids": ["{{batch_sub_a_id}}", "{{batch_sub_b_id}}"] }

HTTP 400
[Asserts]
jsonpath "$.stats.failed" == 2

# Folders — create child (no Create on batch_root)
POST {{base_url}}/api/batch/folders/create
Authorization: Bearer {{frank_token}}
Content-Type: application/json
{ "folders": [{ "name": "frank-attack", "parent_id": "{{batch_root_id}}" }] }

HTTP 400

# Folders — move
POST {{base_url}}/api/batch/folders/move
Authorization: Bearer {{frank_token}}
Content-Type: application/json
{ "folder_ids": ["{{batch_sub_a_id}}"], "target_folder_id": "{{batch_sub_b_id}}" }

HTTP 400

# Folders — copy
POST {{base_url}}/api/batch/folders/copy
Authorization: Bearer {{frank_token}}
Content-Type: application/json
{ "folder_ids": ["{{batch_sub_a_id}}"], "target_folder_id": "{{batch_sub_b_id}}" }

HTTP 400

# Folders — delete
POST {{base_url}}/api/batch/folders/delete
Authorization: Bearer {{frank_token}}
Content-Type: application/json
{ "folder_ids": ["{{batch_sub_a_id}}"], "recursive": false }

HTTP 400

# Trash (mixed)
POST {{base_url}}/api/batch/trash
Authorization: Bearer {{frank_token}}
Content-Type: application/json
{ "file_ids": ["{{batch_file_1_id}}"], "folder_ids": ["{{batch_sub_a_id}}"] }

HTTP 400
[Asserts]
jsonpath "$.stats.failed" == 2

# Download POST — engine rejects each item; batch service tracks
# `items_added` and bails out with NotFound when none were authorized.
POST {{base_url}}/api/batch/download
Authorization: Bearer {{frank_token}}
Content-Type: application/json
{ "file_ids": ["{{batch_file_1_id}}"], "folder_ids": [] }

HTTP 404

# Download GET (querystring variant) — same behavior
GET {{base_url}}/api/batch/download?file_ids={{batch_file_1_id}}
Authorization: Bearer {{frank_token}}

HTTP 404


# ════════════════════════════════════════════════════════════════════
# Phase 3B — Alice grants frank Viewer. Read endpoints succeed;
#            mutating batch ops still all-fail.
# ════════════════════════════════════════════════════════════════════
POST {{base_url}}/api/grants
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{
  "subject":  { "type": "user",   "id": "{{frank_user_id}}" },
  "resource": { "type": "folder", "id": "{{batch_root_id}}" },
  "role": "viewer"
}

HTTP 201

# get_files succeeds (Read cascades to all descendants)
POST {{base_url}}/api/batch/files/get
Authorization: Bearer {{frank_token}}
Content-Type: application/json
{ "file_ids": ["{{batch_file_1_id}}", "{{batch_file_2_id}}"] }

HTTP 200
[Asserts]
jsonpath "$.stats.successful" == 2
jsonpath "$.stats.failed" == 0

# get_folders succeeds
POST {{base_url}}/api/batch/folders/get
Authorization: Bearer {{frank_token}}
Content-Type: application/json
{ "folder_ids": ["{{batch_sub_a_id}}", "{{batch_sub_b_id}}"] }

HTTP 200
[Asserts]
jsonpath "$.stats.successful" == 2

# Download POST as Viewer — succeeds (Read sufficient)
POST {{base_url}}/api/batch/download
Authorization: Bearer {{frank_token}}
Content-Type: application/json
{ "file_ids": ["{{batch_file_1_id}}", "{{batch_file_2_id}}"], "folder_ids": [] }

HTTP 200
[Asserts]
header "Content-Type" == "application/zip"

# Download GET — same
GET {{base_url}}/api/batch/download?file_ids={{batch_file_1_id}},{{batch_file_2_id}}
Authorization: Bearer {{frank_token}}

HTTP 200
[Asserts]
header "Content-Type" == "application/zip"

# Mutations still rejected
POST {{base_url}}/api/batch/files/move
Authorization: Bearer {{frank_token}}
Content-Type: application/json
{ "file_ids": ["{{batch_file_1_id}}"], "target_folder_id": "{{batch_sub_a_id}}" }

HTTP 400

POST {{base_url}}/api/batch/files/delete
Authorization: Bearer {{frank_token}}
Content-Type: application/json
{ "file_ids": ["{{batch_file_1_id}}"] }

HTTP 400


# ════════════════════════════════════════════════════════════════════
# Phase 3C — Promote frank to Editor (read + comment + create + update).
#            Move + copy + create succeed; delete still fails.
# ════════════════════════════════════════════════════════════════════
PUT {{base_url}}/api/grants/role
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{
  "subject":  { "type": "user",   "id": "{{frank_user_id}}" },
  "resource": { "type": "folder", "id": "{{batch_root_id}}" },
  "role": "editor"
}

HTTP 200

# Batch folder create (Create on parent)
POST {{base_url}}/api/batch/folders/create
Authorization: Bearer {{frank_token}}
Content-Type: application/json
{
  "folders": [
    { "name": "frank-batch-1", "parent_id": "{{batch_root_id}}" },
    { "name": "frank-batch-2", "parent_id": "{{batch_root_id}}" }
  ]
}

HTTP 201
[Asserts]
jsonpath "$.stats.successful" == 2

# Batch file move (Update on file + Create on target)
POST {{base_url}}/api/batch/files/move
Authorization: Bearer {{frank_token}}
Content-Type: application/json
{ "file_ids": ["{{batch_file_1_id}}"], "target_folder_id": "{{batch_sub_a_id}}" }

HTTP 200
[Asserts]
jsonpath "$.stats.successful" == 1

# Batch file copy (Read on src + Create on dst)
POST {{base_url}}/api/batch/files/copy
Authorization: Bearer {{frank_token}}
Content-Type: application/json
{ "file_ids": ["{{batch_file_2_id}}"], "target_folder_id": "{{batch_sub_b_id}}" }

HTTP 200
[Asserts]
jsonpath "$.stats.successful" == 1

# Batch folder move
POST {{base_url}}/api/batch/folders/move
Authorization: Bearer {{frank_token}}
Content-Type: application/json
{ "folder_ids": ["{{batch_sub_a_id}}"], "target_folder_id": "{{batch_sub_b_id}}" }

HTTP 200

# Batch folder copy — copy sub_a (now nested inside sub_b after the
# move above) back to batch_root. Avoids name collision with the
# existing sub_b at the root.
POST {{base_url}}/api/batch/folders/copy
Authorization: Bearer {{frank_token}}
Content-Type: application/json
{ "folder_ids": ["{{batch_sub_a_id}}"], "target_folder_id": "{{batch_root_id}}" }

HTTP 200

# Batch delete still denied (Editor excludes Delete)
POST {{base_url}}/api/batch/files/delete
Authorization: Bearer {{frank_token}}
Content-Type: application/json
{ "file_ids": ["{{batch_file_2_id}}"] }

HTTP 400

POST {{base_url}}/api/batch/folders/delete
Authorization: Bearer {{frank_token}}
Content-Type: application/json
{ "folder_ids": ["{{batch_sub_b_id}}"], "recursive": true }

HTTP 400


# ════════════════════════════════════════════════════════════════════
# Phase 3D — Promote frank to Admin. Delete + trash succeed.
# ════════════════════════════════════════════════════════════════════
PUT {{base_url}}/api/grants/role
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{
  "subject":  { "type": "user",   "id": "{{frank_user_id}}" },
  "resource": { "type": "folder", "id": "{{batch_root_id}}" },
  "role": "owner"
}

HTTP 200

# Frank (Owner role includes Delete) trashes batch_file_2 — item goes to
# Alice's trash because file.user_id is unchanged (Alice is still owner).
POST {{base_url}}/api/batch/trash
Authorization: Bearer {{frank_token}}
Content-Type: application/json
{ "file_ids": ["{{batch_file_2_id}}"], "folder_ids": [] }

HTTP 200
[Asserts]
jsonpath "$.stats.successful" == 1

# Alice trashes batch_file_1 (which frank moved into batch_sub_a in Phase 3C).
POST {{base_url}}/api/batch/trash
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{ "file_ids": ["{{batch_file_1_id}}"], "folder_ids": [] }

HTTP 200
[Asserts]
jsonpath "$.stats.successful" == 1

# Batch permanent-delete a folder
POST {{base_url}}/api/batch/folders/delete
Authorization: Bearer {{frank_token}}
Content-Type: application/json
{ "folder_ids": ["{{batch_sub_b_id}}"], "recursive": true }

HTTP 200


# ════════════════════════════════════════════════════════════════════
# Phase 3E — Lifecycle cleanup. Alice deletes the batch-test root.
#            Trigger removes all of frank's grants.
# ════════════════════════════════════════════════════════════════════
DELETE {{base_url}}/api/folders/{{batch_root_id}}
Authorization: Bearer {{alice_token}}

HTTP 204

DELETE {{base_url}}/api/trash/empty
Authorization: Bearer {{alice_token}}

HTTP 200

GET {{base_url}}/api/grants/incoming
Authorization: Bearer {{frank_token}}

HTTP 200
[Asserts]
# See Step 18 above for the full invariant rationale — three
# self-owned Owner grants per user (drive + calendar +
# address_book) from the lifecycle hooks. Body-contains rather
# than positional check because the incoming feed doesn't
# guarantee stable ordering across resource types.
jsonpath "$" count == 3
body contains "\"type\":\"drive\""
body contains "\"type\":\"calendar\""
body contains "\"type\":\"address_book\""
