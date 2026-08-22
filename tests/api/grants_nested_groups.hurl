# =============================================================
# OxiCloud — ReBAC grant cascading through nested subject groups
# =============================================================
# Mirrors the comprehensive permission walk from grants.hurl Phase 2,
# but the grant target is a *parent group* and the test user reaches
# the resource via a chain:
#
#     henry  ∈  group B  ∈  group A   ←—  grant lives here
#
# Each role tier (no-grant → viewer → editor → admin) is exercised
# on the same engine-aware endpoints as grants.hurl Phase 2. Also
# verifies the listing-side group expansion (the "Shared with me"
# feed: /api/grants/incoming + /api/grants/incoming/resources) so
# the user sees folders reached via group cascade.
#
# Runs after subject_groups.hurl. Self-contained user ("henry"),
# self-contained group names ("grp-...-nested-hurl"), unique folder
# names so the test doesn't depend on or leak external state.
# =============================================================


# ─────────────────────────────────────────────────────────────
# Step 1 — Login as admin (alice) + create fresh user henry.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "{{username}}", "password": "{{password}}" }

HTTP 200
[Captures]
alice_token: jsonpath "$.access_token"

GET {{base_url}}/api/folders
Authorization: Bearer {{alice_token}}

HTTP 200
[Captures]
alice_home_id: jsonpath "$[0].id"

POST {{base_url}}/api/admin/users
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{ "username": "henry", "password": "HenryPassword1!", "email": "henry@example.com", "role": "user" }

HTTP 201
[Captures]
henry_user_id: jsonpath "$.user.id"

POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "henry", "password": "HenryPassword1!" }

HTTP 200
[Captures]
henry_token: jsonpath "$.access_token"


# ─────────────────────────────────────────────────────────────
# Step 2 — Build the nested-group chain  henry ∈ B ⊂ A.
#
# A   (grp-grant-parent-nested-hurl)
# └── B  (grp-grant-child-nested-hurl)
#     └── henry
#
# Grant is on A. The recursive CTE in `expand_user` walks
# B → A so any grant on A applies to henry.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/groups
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{ "name": "grp-grant-parent-nested-hurl", "description": "outer group A" }

HTTP 201
[Captures]
group_a_id: jsonpath "$.id"

POST {{base_url}}/api/groups
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{ "name": "grp-grant-child-nested-hurl", "description": "inner group B" }

HTTP 201
[Captures]
group_b_id: jsonpath "$.id"

# A ∋ B  (B is a sub-group of A)
POST {{base_url}}/api/groups/{{group_a_id}}/members
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{ "group_id": "{{group_b_id}}" }

HTTP 201

# B ∋ henry
POST {{base_url}}/api/groups/{{group_b_id}}/members
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{ "user_id": "{{henry_user_id}}" }

HTTP 201

# Sanity: A's direct members include B (group), not henry.
GET {{base_url}}/api/groups/{{group_a_id}}/members
Authorization: Bearer {{alice_token}}

HTTP 200
[Asserts]
jsonpath "$[*].kind" contains "group"
jsonpath "$[*].id"   contains "{{group_b_id}}"
jsonpath "$[*].id"   not contains "{{henry_user_id}}"


# ─────────────────────────────────────────────────────────────
# Step 3 — Alice creates the test resources:
#          parent folder + child folder + JPEG (auto-thumbnailed).
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/folders
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{ "name": "nested-perm-folder", "parent_id": "{{alice_home_id}}" }

HTTP 201
[Captures]
perm_folder_id: jsonpath "$.id"

POST {{base_url}}/api/folders
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{ "name": "nested-perm-child", "parent_id": "{{perm_folder_id}}" }

HTTP 201
[Captures]
perm_child_id: jsonpath "$.id"

# Use a fixture unique to this test. Sharing fixtures with grants.hurl
# would re-dedup against blob rows whose disk files were already cleaned
# up by that test's lifecycle phase (pre-existing dedup ref-count
# bookkeeping bug — see thumbnail_dedup memory). A unique fixture keeps
# this test independent of that issue.
POST {{base_url}}/api/files/upload
Authorization: Bearer {{alice_token}}
[MultipartFormData]
folder_id: {{perm_folder_id}}
file: file,fixtures/nested-groups-logo.jpg; image/jpeg

HTTP 201
[Captures]
perm_file_id: jsonpath "$.id"


# ════════════════════════════════════════════════════════════════════
# Phase A — Henry has NO grant (nothing on A, nothing on B, no
#           inheritance). Every engine-aware endpoint denies.
# ════════════════════════════════════════════════════════════════════

# ── Folder reads ─────────────────────────────────────────────
GET {{base_url}}/api/folders/{{perm_folder_id}}/resources?resource_types=folder
Authorization: Bearer {{henry_token}}

HTTP 404

GET {{base_url}}/api/folders/{{perm_folder_id}}/resources
Authorization: Bearer {{henry_token}}

HTTP 404

GET {{base_url}}/api/folders/{{perm_folder_id}}/resources
Authorization: Bearer {{henry_token}}

HTTP 404

GET {{base_url}}/api/folders/{{perm_folder_id}}/download
Authorization: Bearer {{henry_token}}

HTTP 404

# ── File reads ───────────────────────────────────────────────
GET {{base_url}}/api/files?folder_id={{perm_folder_id}}
Authorization: Bearer {{henry_token}}

HTTP 404

GET {{base_url}}/api/files/{{perm_file_id}}
Authorization: Bearer {{henry_token}}

HTTP 404

GET {{base_url}}/api/files/{{perm_file_id}}/metadata
Authorization: Bearer {{henry_token}}

HTTP 404

GET {{base_url}}/api/files/{{perm_file_id}}/thumbnail/icon
Authorization: Bearer {{henry_token}}

HTTP 404

# ── Folder mutations ─────────────────────────────────────────
POST {{base_url}}/api/folders
Authorization: Bearer {{henry_token}}
Content-Type: application/json
{ "name": "henry-attack", "parent_id": "{{perm_folder_id}}" }

HTTP 404

PUT {{base_url}}/api/folders/{{perm_folder_id}}/rename
Authorization: Bearer {{henry_token}}
Content-Type: application/json
{ "name": "henry-rename-attempt" }

HTTP 404

DELETE {{base_url}}/api/folders/{{perm_folder_id}}
Authorization: Bearer {{henry_token}}

HTTP 404

# ── File mutations ───────────────────────────────────────────
POST {{base_url}}/api/files/upload
Authorization: Bearer {{henry_token}}
[MultipartFormData]
folder_id: {{perm_folder_id}}
file: file,fixtures/hello.txt; text/plain

HTTP 404

PUT {{base_url}}/api/files/{{perm_file_id}}/rename
Authorization: Bearer {{henry_token}}
Content-Type: application/json
{ "name": "henry-file-rename" }

HTTP 404

PUT {{base_url}}/api/files/{{perm_file_id}}/thumbnail/icon
Authorization: Bearer {{henry_token}}
Content-Type: image/png
file,fixtures/blue-image.png;

HTTP 404

DELETE {{base_url}}/api/files/{{perm_file_id}}
Authorization: Bearer {{henry_token}}

HTTP 404

# ── Chunked upload: cannot start session in alice's folder ──
POST {{base_url}}/api/uploads
Authorization: Bearer {{henry_token}}
Content-Type: application/json
{
  "filename": "henry-chunked-attack.mp4",
  "folder_id": "{{perm_folder_id}}",
  "content_type": "video/mp4",
  "total_size": 2760653,
  "chunk_size": 3000000
}

HTTP 404

# ── Incoming-list expansion: nothing yet. ───────────────────
GET {{base_url}}/api/grants/incoming
Authorization: Bearer {{henry_token}}

HTTP 200
[Asserts]
jsonpath "$[*].resource.id" not contains "{{perm_folder_id}}"


# ════════════════════════════════════════════════════════════════════
# Phase B — Alice grants Viewer to GROUP A. Cascade goes:
#           grant(A, read) → henry (via B ⊂ A) → folder + child + file.
#           Read endpoints succeed; mutations still denied.
# ════════════════════════════════════════════════════════════════════
POST {{base_url}}/api/grants
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{
  "subject":  { "type": "group",  "id": "{{group_a_id}}" },
  "resource": { "type": "folder", "id": "{{perm_folder_id}}" },
  "role": "viewer"
}

HTTP 201
# PR N1: POST /api/grants now wraps results in
# `CreateGrantResponseDto { grants, notification }`.
[Asserts]
jsonpath "$.grants" count == 1
jsonpath "$.grants[0].role" == "viewer"
jsonpath "$.grants[0].subject.type" == "group"
jsonpath "$.grants[0].subject.id"   == "{{group_a_id}}"

# ── Read endpoints now succeed ──────────────────────────────
GET {{base_url}}/api/folders/{{perm_folder_id}}/resources?resource_types=folder
Authorization: Bearer {{henry_token}}

HTTP 200
[Asserts]
jsonpath "$.items" count == 1
jsonpath "$.items[0].resource.id" == "{{perm_child_id}}"

GET {{base_url}}/api/folders/{{perm_folder_id}}/resources
Authorization: Bearer {{henry_token}}

HTTP 200

GET {{base_url}}/api/folders/{{perm_folder_id}}/resources
Authorization: Bearer {{henry_token}}

HTTP 200

GET {{base_url}}/api/folders/{{perm_folder_id}}/download
Authorization: Bearer {{henry_token}}

HTTP 200
[Asserts]
header "Content-Type" contains "zip"

GET {{base_url}}/api/files?folder_id={{perm_folder_id}}
Authorization: Bearer {{henry_token}}

HTTP 200
[Asserts]
jsonpath "$" count == 1
jsonpath "$[0].id" == "{{perm_file_id}}"

GET {{base_url}}/api/files/{{perm_file_id}}
Authorization: Bearer {{henry_token}}

HTTP 200

GET {{base_url}}/api/files/{{perm_file_id}}/metadata
Authorization: Bearer {{henry_token}}

HTTP 200

GET {{base_url}}/api/files/{{perm_file_id}}/thumbnail/icon
Authorization: Bearer {{henry_token}}

HTTP 200
[Asserts]
header "Content-Type" startsWith "image/"

# ── Folder cascade through ltree: child also readable. ──────
GET {{base_url}}/api/folders/{{perm_child_id}}/resources?resource_types=folder
Authorization: Bearer {{henry_token}}

HTTP 200

# ── Incoming-list expansion: henry now sees the folder grant
#    in his "Shared with me" feed even though the grant subject
#    is group A (not henry). This validates the listing-side
#    expansion added alongside the cascade check.
GET {{base_url}}/api/grants/incoming
Authorization: Bearer {{henry_token}}

HTTP 200
[Asserts]
jsonpath "$[?(@.resource.id=='{{perm_folder_id}}')].role" == "viewer"
jsonpath "$[?(@.resource.id=='{{perm_folder_id}}')].subject.type" == "group"
jsonpath "$[?(@.resource.id=='{{perm_folder_id}}')].subject.id"   == "{{group_a_id}}"

GET {{base_url}}/api/grants/incoming/resources?limit=50
Authorization: Bearer {{henry_token}}

HTTP 200
[Asserts]
jsonpath "$.items[?(@.resource.id=='{{perm_folder_id}}')].resource_type" == "folder"
jsonpath "$.items[?(@.resource.id=='{{perm_folder_id}}')].permissions"   contains "read"

# ── Mutations still denied (Viewer has no Update/Create/Delete).
#    Henry has Read via nested-group cascade → graduated denial
#    returns 403 (see [[project_authz_require_graduated_denial]]).
#    Anti-enum 404 stays reserved for the earlier phase where the
#    cascade hadn't given Henry any Read at all.
POST {{base_url}}/api/folders
Authorization: Bearer {{henry_token}}
Content-Type: application/json
{ "name": "henry-attack-2", "parent_id": "{{perm_folder_id}}" }

HTTP 403

PUT {{base_url}}/api/folders/{{perm_folder_id}}/rename
Authorization: Bearer {{henry_token}}
Content-Type: application/json
{ "name": "henry-rename-as-viewer" }

HTTP 403

PUT {{base_url}}/api/files/{{perm_file_id}}/rename
Authorization: Bearer {{henry_token}}
Content-Type: application/json
{ "name": "henry-file-rename-as-viewer" }

HTTP 403

PUT {{base_url}}/api/files/{{perm_file_id}}/thumbnail/icon
Authorization: Bearer {{henry_token}}
Content-Type: image/png
file,fixtures/blue-image.png;

HTTP 403

POST {{base_url}}/api/files/upload
Authorization: Bearer {{henry_token}}
[MultipartFormData]
folder_id: {{perm_folder_id}}
file: file,fixtures/hello.txt; text/plain

HTTP 403

DELETE {{base_url}}/api/files/{{perm_file_id}}
Authorization: Bearer {{henry_token}}

HTTP 403

DELETE {{base_url}}/api/folders/{{perm_folder_id}}
Authorization: Bearer {{henry_token}}

HTTP 403

# Viewer cannot start a chunked upload (no Create grant).
POST {{base_url}}/api/uploads
Authorization: Bearer {{henry_token}}
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
# Phase C — Promote group A's grant to Editor (read + comment +
#           create + update). Create/Update succeed; Delete still denied.
# ════════════════════════════════════════════════════════════════════
PUT {{base_url}}/api/grants/role
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{
  "subject":  { "type": "group",  "id": "{{group_a_id}}" },
  "resource": { "type": "folder", "id": "{{perm_folder_id}}" },
  "role": "editor"
}

HTTP 200

# Update — folder + file rename succeed.
PUT {{base_url}}/api/folders/{{perm_folder_id}}/rename
Authorization: Bearer {{henry_token}}
Content-Type: application/json
{ "name": "renamed-by-henry-as-editor" }

HTTP 200

PUT {{base_url}}/api/files/{{perm_file_id}}/rename
Authorization: Bearer {{henry_token}}
Content-Type: application/json
{ "name": "henry-renamed-logo.jpg" }

HTTP 200

# Thumbnail push (Update) succeeds.
PUT {{base_url}}/api/files/{{perm_file_id}}/thumbnail/preview
Authorization: Bearer {{henry_token}}
Content-Type: image/png
file,fixtures/blue-image.png;

HTTP 201

# Create — folder + file upload + chunked upload all succeed.
POST {{base_url}}/api/folders
Authorization: Bearer {{henry_token}}
Content-Type: application/json
{ "name": "henry-created-child", "parent_id": "{{perm_folder_id}}" }

HTTP 201

POST {{base_url}}/api/files/upload
Authorization: Bearer {{henry_token}}
[MultipartFormData]
folder_id: {{perm_folder_id}}
file: file,fixtures/hello.txt; text/plain

HTTP 201

POST {{base_url}}/api/uploads
Authorization: Bearer {{henry_token}}
Content-Type: application/json
{
  "filename": "henry-chunked-video.mp4",
  "folder_id": "{{perm_folder_id}}",
  "content_type": "video/mp4",
  "total_size": 2760653,
  "chunk_size": 3000000
}

HTTP 201
[Captures]
henry_upload_id: jsonpath "$.upload_id"

PATCH {{base_url}}/api/uploads/{{henry_upload_id}}?chunk_index=0
Authorization: Bearer {{henry_token}}
Content-Type: application/octet-stream
file,fixtures/free_video_over_1MB.mp4;

HTTP 200

POST {{base_url}}/api/uploads/{{henry_upload_id}}/complete
Authorization: Bearer {{henry_token}}

HTTP 201
[Captures]
henry_chunked_file_id: jsonpath "$.file_id"

# Alice (the owner) sees the file in the folder listing.
GET {{base_url}}/api/files?folder_id={{perm_folder_id}}
Authorization: Bearer {{alice_token}}

HTTP 200
[Asserts]
jsonpath "$[?(@.id=='{{henry_chunked_file_id}}')].name" == "henry-chunked-video.mp4"

# Editor still cannot delete. Editor bundle carries Read → 403.
DELETE {{base_url}}/api/files/{{perm_file_id}}
Authorization: Bearer {{henry_token}}

HTTP 403

DELETE {{base_url}}/api/folders/{{perm_folder_id}}
Authorization: Bearer {{henry_token}}

HTTP 403


# ════════════════════════════════════════════════════════════════════
# Phase D — Promote group A's grant to Owner (full bundle).
#           Delete now succeeds for henry, still flowing through B → A.
# ════════════════════════════════════════════════════════════════════
PUT {{base_url}}/api/grants/role
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{
  "subject":  { "type": "group",  "id": "{{group_a_id}}" },
  "resource": { "type": "folder", "id": "{{perm_folder_id}}" },
  "role": "owner"
}

HTTP 200

DELETE {{base_url}}/api/files/{{perm_file_id}}
Authorization: Bearer {{henry_token}}

HTTP 204


# ════════════════════════════════════════════════════════════════════
# Phase E — Grant lives on the INNER group B (not A).
#           Same user, same chain, but the grant is one hop closer.
#           Confirms the recursive walk works for direct-membership
#           grants as well as parent-group grants.
# ════════════════════════════════════════════════════════════════════
# First demote group A to viewer so only one access_grants row remains
# for that (subject, resource) pair, capturing the id directly from
# the PUT response so we don't have to filter henry's incoming list.
PUT {{base_url}}/api/grants/role
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{
  "subject":  { "type": "group",  "id": "{{group_a_id}}" },
  "resource": { "type": "folder", "id": "{{perm_folder_id}}" },
  "role": "viewer"
}

HTTP 200
[Captures]
folder_grant_id: jsonpath "$[0].id"
[Asserts]
jsonpath "$" count == 1

# Delete that single remaining grant on group A.
DELETE {{base_url}}/api/grants/{{folder_grant_id}}
Authorization: Bearer {{alice_token}}

HTTP 204

# Confirm access is gone.
GET {{base_url}}/api/folders/{{perm_folder_id}}/resources?resource_types=folder
Authorization: Bearer {{henry_token}}

HTTP 404

# New grant targeting B directly.
POST {{base_url}}/api/grants
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{
  "subject":  { "type": "group",  "id": "{{group_b_id}}" },
  "resource": { "type": "folder", "id": "{{perm_folder_id}}" },
  "role": "viewer"
}

HTTP 201

GET {{base_url}}/api/folders/{{perm_folder_id}}/resources?resource_types=folder
Authorization: Bearer {{henry_token}}

HTTP 200

GET {{base_url}}/api/grants/incoming
Authorization: Bearer {{henry_token}}

HTTP 200
[Asserts]
jsonpath "$[?(@.resource.id=='{{perm_folder_id}}')].subject.id" == "{{group_b_id}}"


# ════════════════════════════════════════════════════════════════════
# Phase F — Lifecycle cleanup.
#           Alice (still the owner) deletes the folder; the
#           trg_cleanup_grants_folder trigger removes the group grant.
#           Then delete the groups themselves.
# ════════════════════════════════════════════════════════════════════
DELETE {{base_url}}/api/folders/{{perm_folder_id}}
Authorization: Bearer {{alice_token}}

HTTP 204

DELETE {{base_url}}/api/trash/empty
Authorization: Bearer {{alice_token}}

HTTP 200

# Henry's incoming list no longer contains this folder.
# Note: the user-groups Moka cache has a 30s TTL, but the grant row
# itself is gone (trigger fires synchronously on folder delete), so
# the listing query — which now expands subjects to include groups
# — won't find anything to match on, cache hit or miss.
GET {{base_url}}/api/grants/incoming
Authorization: Bearer {{henry_token}}

HTTP 200
[Asserts]
jsonpath "$[*].resource.id" not contains "{{perm_folder_id}}"

# Delete the groups.
DELETE {{base_url}}/api/groups/{{group_a_id}}
Authorization: Bearer {{alice_token}}

HTTP 204

DELETE {{base_url}}/api/groups/{{group_b_id}}
Authorization: Bearer {{alice_token}}

HTTP 204

GET {{base_url}}/api/groups/{{group_a_id}}
Authorization: Bearer {{alice_token}}

HTTP 404
