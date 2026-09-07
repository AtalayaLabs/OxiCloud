# =============================================================
# OxiCloud — D-Prep: role_grants dual-write + new wire format
# =============================================================
# Pins the D-Prep refactor behaviours that don't fit naturally into
# the existing `grants.hurl` (which is API-shape-focused). Specifically:
#
#   1. Wire-format role names:
#        - "owner" is accepted on POST and emitted on response
#        - "admin" is REJECTED with 422 (compat alias retired in the
#          cleanup PR — see Step 6a)
#
#   2. Role-keyed write proof: granting a role and then exercising a
#      permission from its bundle works → proves the row landed in
#      `storage.role_grants` and the engine read path expands the
#      bundle correctly (see `folder_cascade_grant_exists`).
#
#   3. Atomic role updates via PUT /api/grants/role — the role flips
#      in a single SQL update (no DELETE+INSERT race window).
#
#   4. Clean revoke: DELETE /api/grants/{id} clears role_grants too.
#
# Self-contained: creates its own users, folders, and files so it
# can run in any position relative to other test files.
# =============================================================


# ─────────────────────────────────────────────────────────────
# Step 1 — admin login + home folder lookup
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "{{username}}", "password": "{{password}}" }

HTTP 200
[Captures]
admin_token: jsonpath "$.access_token"
admin_user_id: jsonpath "$.user.full.user.id"

GET {{base_url}}/api/folders
Authorization: Bearer {{admin_token}}

HTTP 200
[Captures]
admin_home_id: jsonpath "$[0].id"


# ─────────────────────────────────────────────────────────────
# Step 2 — Create two fresh test users (renee, sam) so this file
#           doesn't depend on cross-file fixtures. Use the
#           legacy-compat path that POSTs to /api/admin/users.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/admin/users
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "username": "renee", "password": "ReneePassword1!", "email": "renee@example.com", "role": "user" }

HTTP 201
[Captures]
renee_user_id: jsonpath "$.user.id"


POST {{base_url}}/api/admin/users
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "username": "sam", "password": "SamPassword1!", "email": "sam@example.com", "role": "user" }

HTTP 201
[Captures]
sam_user_id: jsonpath "$.user.id"


POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "renee", "password": "ReneePassword1!" }

HTTP 200
[Captures]
renee_token: jsonpath "$.access_token"


POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "sam", "password": "SamPassword1!" }

HTTP 200
[Captures]
sam_token: jsonpath "$.access_token"


# ─────────────────────────────────────────────────────────────
# Step 3 — admin creates a folder "role-grants-test" + a file
#          inside it to use as the authz target throughout the file.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/folders
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "name": "role-grants-test", "parent_id": "{{admin_home_id}}" }

HTTP 201
[Captures]
test_folder_id: jsonpath "$.id"


POST {{base_url}}/api/files/upload
Authorization: Bearer {{admin_token}}
[MultipartFormData]
folder_id: {{test_folder_id}}
file: file,fixtures/hello.txt; text/plain

HTTP 201
[Captures]
test_file_id: jsonpath "$.id"

# Rename immediately so subsequent throwaway uploads of `hello.txt`
# to the same folder don't 409. Each throwaway upload below applies
# the same pattern (upload → rename → use) to keep the namespace
# clean for the next one.
PUT {{base_url}}/api/files/{{test_file_id}}/rename
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "name": "step3-anchor.txt" }

HTTP 200


# ─────────────────────────────────────────────────────────────
# Step 4 — Grant renee role="owner" on the folder. New wire format.
#          Response should echo back the canonical name.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/grants
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "subject": { "type": "user", "id": "{{renee_user_id}}" },
  "resource": { "type": "folder", "id": "{{test_folder_id}}" },
  "role": "owner"
}

HTTP 201


# ─────────────────────────────────────────────────────────────
# Step 5 — Dual-write proof: renee (now Owner of the folder) can
#          DELETE a file inside it. Owner's bundle includes Delete;
#          the engine reads from role_grants → if dual-write didn't
#          land the row, the cascade query returns empty and the
#          delete is refused.
#
#          We upload + delete a throwaway file to avoid removing the
#          test_file we'll need for later steps.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/files/upload
Authorization: Bearer {{admin_token}}
[MultipartFormData]
folder_id: {{test_folder_id}}
file: file,fixtures/hello.txt; text/plain

HTTP 201
[Captures]
throwaway_file_id: jsonpath "$.id"

# Rename so subsequent uploads in this folder don't 409 on "hello.txt".
PUT {{base_url}}/api/files/{{throwaway_file_id}}/rename
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "name": "renee-throwaway.txt" }

HTTP 200


DELETE {{base_url}}/api/files/{{throwaway_file_id}}
Authorization: Bearer {{renee_token}}

HTTP 204


# ─────────────────────────────────────────────────────────────
# Step 6a — Reject the legacy "admin" string. The cleanup PR
#          retired the `#[serde(alias = "admin")]` compat shim on
#          `RoleDto::Owner`; the deserialiser now refuses it with 422.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/grants
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "subject": { "type": "user", "id": "{{sam_user_id}}" },
  "resource": { "type": "folder", "id": "{{test_folder_id}}" },
  "role": "admin"
}

HTTP 422


# ─────────────────────────────────────────────────────────────
# Step 6b — Grant sam Owner with the canonical role string.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/grants
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "subject": { "type": "user", "id": "{{sam_user_id}}" },
  "resource": { "type": "folder", "id": "{{test_folder_id}}" },
  "role": "owner"
}

HTTP 201
[Captures]
# The single role-keyed Grant returned in `.grants[0]` is the
# `storage.role_grants` row id. Step 10's revoke uses it to
# `clear_role` and wipe the row.
sam_grant_id: jsonpath "$.grants[0].id"


# Sam should now have Owner-equivalent access — Delete works.
POST {{base_url}}/api/files/upload
Authorization: Bearer {{admin_token}}
[MultipartFormData]
folder_id: {{test_folder_id}}
file: file,fixtures/hello.txt; text/plain

HTTP 201
[Captures]
sam_throwaway_id: jsonpath "$.id"

PUT {{base_url}}/api/files/{{sam_throwaway_id}}/rename
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "name": "sam-throwaway.txt" }

HTTP 200


DELETE {{base_url}}/api/files/{{sam_throwaway_id}}
Authorization: Bearer {{sam_token}}

HTTP 204


# ─────────────────────────────────────────────────────────────
# Step 7 — Atomic role update: demote renee from Owner to Viewer
#          via PUT /api/grants/role. Single SQL UPDATE on
#          role_grants — no DELETE+INSERT race.
#
#          After: renee can still Read but should be refused Delete.
# ─────────────────────────────────────────────────────────────
PUT {{base_url}}/api/grants/role
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "subject": { "type": "user", "id": "{{renee_user_id}}" },
  "resource": { "type": "folder", "id": "{{test_folder_id}}" },
  "role": "viewer"
}

HTTP 200


# Renee can still read the file (Viewer's bundle includes Read).
GET {{base_url}}/api/files/{{test_file_id}}
Authorization: Bearer {{renee_token}}

HTTP 200


# Renee CANNOT delete — Viewer's bundle excludes Delete; the
# folder_cascade_grant_exists query for Delete returns empty.
POST {{base_url}}/api/files/upload
Authorization: Bearer {{admin_token}}
[MultipartFormData]
folder_id: {{test_folder_id}}
file: file,fixtures/hello.txt; text/plain

HTTP 201
[Captures]
post_demote_file_id: jsonpath "$.id"

PUT {{base_url}}/api/files/{{post_demote_file_id}}/rename
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "name": "post-demote.txt" }

HTTP 200


DELETE {{base_url}}/api/files/{{post_demote_file_id}}
Authorization: Bearer {{renee_token}}

HTTP *
[Asserts]
status >= 400
status <  500


# ─────────────────────────────────────────────────────────────
# Step 8 — Promote renee back to Editor (one role change, atomic)
# ─────────────────────────────────────────────────────────────
PUT {{base_url}}/api/grants/role
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "subject": { "type": "user", "id": "{{renee_user_id}}" },
  "resource": { "type": "folder", "id": "{{test_folder_id}}" },
  "role": "editor"
}

HTTP 200


# Editor's bundle includes Update — renaming a file should work.
PUT {{base_url}}/api/files/{{post_demote_file_id}}/rename
Authorization: Bearer {{renee_token}}
Content-Type: application/json
{ "name": "renamed-by-renee.txt" }

HTTP 200


# ─────────────────────────────────────────────────────────────
# Step 9 — My Shares response shape: the outgoing-resources endpoint
#          must emit role strings from the new roster ("viewer" /
#          "editor" / "owner"), never the legacy "admin".
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/grants/outgoing/resources
Authorization: Bearer {{admin_token}}

HTTP 200
[Asserts]
# The response body MUST contain "owner" (sam's role) and "editor"
# (renee's current role after the promote). It MUST NOT contain the
# legacy "admin" role string for any grant emitted by the server.
body contains "\"role\":\"owner\""
body contains "\"role\":\"editor\""
body not contains "\"role\":\"admin\""


# ─────────────────────────────────────────────────────────────
# Step 10 — Revoke: removing sam's grant. `engine.revoke()` DELETEs
#           the single `storage.role_grants` row by id.
#
#           After: sam's Delete attempt should be refused (proof
#           the role_grants row is gone — the cascade query for
#           Delete returns empty because sam has no row pointing
#           at this folder).
# ─────────────────────────────────────────────────────────────

# sam_grant_id was captured at Step 6b from the create response.
DELETE {{base_url}}/api/grants/{{sam_grant_id}}
Authorization: Bearer {{admin_token}}

HTTP 204


# Post-revoke: sam can no longer Delete in this folder.
POST {{base_url}}/api/files/upload
Authorization: Bearer {{admin_token}}
[MultipartFormData]
folder_id: {{test_folder_id}}
file: file,fixtures/hello.txt; text/plain

HTTP 201
[Captures]
post_revoke_file_id: jsonpath "$.id"

PUT {{base_url}}/api/files/{{post_revoke_file_id}}/rename
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "name": "post-revoke.txt" }

HTTP 200


DELETE {{base_url}}/api/files/{{post_revoke_file_id}}
Authorization: Bearer {{sam_token}}

HTTP *
[Asserts]
status >= 400
status <  500


# ─────────────────────────────────────────────────────────────
# Step 11 — Teardown: clean up users + folder so this file
#            leaves no residue for the storage_cleanup_check.
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/api/folders/{{test_folder_id}}
Authorization: Bearer {{admin_token}}
HTTP 204

DELETE {{base_url}}/api/admin/users/{{renee_user_id}}
Authorization: Bearer {{admin_token}}
HTTP 200

DELETE {{base_url}}/api/admin/users/{{sam_user_id}}
Authorization: Bearer {{admin_token}}
HTTP 200
