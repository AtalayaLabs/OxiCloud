# =============================================================
# OxiCloud – Cross-user permission / IDOR scenarios
# =============================================================
# Verifies the ownership checks added to FolderService::create_folder
# and FileManagementService move/copy/rename, plus the shared
# FolderDbRepository::verify_owner helper.
#
# Plan reference: /Users/ed/.claude/plans/compiled-shimmying-bonbon.md
#                 — "Verification → 2. Manual integration tests"
#
# Run via tests/api/run.sh; must be ordered LAST in the runner because
# it creates a second user (bob) and writes into admin's home folder.
# =============================================================


# ─────────────────────────────────────────────────────────────
# Step 1 – Login as admin (the user created by setup.hurl)
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/login
Content-Type: application/json
{
  "username": "{{username}}",
  "password": "{{password}}"
}

HTTP 200
[Captures]
admin_token: jsonpath "$.access_token"


# ─────────────────────────────────────────────────────────────
# Step 2 – Capture admin's home folder
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/folders
Authorization: Bearer {{admin_token}}

HTTP 200
[Captures]
admin_home_id: jsonpath "$[0].id"


# ─────────────────────────────────────────────────────────────
# Step 3 – Admin creates a private folder inside their home
#           This is the resource bob will attempt to attack.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/folders
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "name": "admin-private-folder",
  "parent_id": "{{admin_home_id}}"
}

HTTP 201
[Captures]
admin_private_id: jsonpath "$.id"
[Asserts]
jsonpath "$.name" == "admin-private-folder"
jsonpath "$.parent_id" == {{admin_home_id}}


# ─────────────────────────────────────────────────────────────
# Step 4 – Admin uploads a file into their home
#           This is the file bob will attempt to access.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/files/upload
Authorization: Bearer {{admin_token}}
[MultipartFormData]
folder_id: {{admin_home_id}}
file: file,fixtures/hello.txt; text/plain

HTTP 201
[Captures]
admin_file_id: jsonpath "$.id"


# ─────────────────────────────────────────────────────────────
# Step 5 – Admin creates user bob (via /api/admin/users)
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/admin/users
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "username": "bob",
  "password": "BobPassword1!",
  "email": "bob@example.com",
  "role": "user"
}

HTTP 201


# ─────────────────────────────────────────────────────────────
# Step 6 – Login as bob, capture his token + home folder
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/login
Content-Type: application/json
{
  "username": "bob",
  "password": "BobPassword1!"
}

HTTP 200
[Captures]
bob_token: jsonpath "$.access_token"

GET {{base_url}}/api/folders
Authorization: Bearer {{bob_token}}

HTTP 200
[Captures]
bob_home_id: jsonpath "$[0].id"
[Asserts]
jsonpath "$" count == 1
jsonpath "$[0].parent_id" == null


# ═════════════════════════════════════════════════════════════
# IDOR tests — every request below uses bob's token
# ═════════════════════════════════════════════════════════════


# ─────────────────────────────────────────────────────────────
# Step 7 – Bob attempts to create a folder inside admin's home
#           Expected: 404 (NotFound, not 403, to avoid leaking
#           the existence of admin's folder).
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/folders
Authorization: Bearer {{bob_token}}
Content-Type: application/json
{
  "name": "bob-attack-1",
  "parent_id": "{{admin_home_id}}"
}

HTTP 404
[Asserts]
jsonpath "$.error_type" == "NotFound"


# ─────────────────────────────────────────────────────────────
# Step 8 – Bob attempts to create a folder inside admin's
#           private folder. Same expectation as Step 7.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/folders
Authorization: Bearer {{bob_token}}
Content-Type: application/json
{
  "name": "bob-attack-2",
  "parent_id": "{{admin_private_id}}"
}

HTTP 404
[Asserts]
jsonpath "$.error_type" == "NotFound"


# ─────────────────────────────────────────────────────────────
# Step 9 – Bob omits parent_id (null). The REST handler
#           auto-resolves null to the caller's home folder
#           (folder_handler.rs:55-77), so the request succeeds
#           and the folder lands in bob's home — NOT at the
#           database root. The service-level validation_error
#           ("Root folder creation is reserved for registration")
#           is defense-in-depth for callers that bypass this
#           handler convenience.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/folders
Authorization: Bearer {{bob_token}}
Content-Type: application/json
{
  "name": "bob-auto-resolved",
  "parent_id": null
}

HTTP 201
[Asserts]
jsonpath "$.name" == "bob-auto-resolved"
jsonpath "$.parent_id" == {{bob_home_id}}


# ─────────────────────────────────────────────────────────────
# Step 10 – Positive control: bob CAN create a folder inside
#            his own home.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/folders
Authorization: Bearer {{bob_token}}
Content-Type: application/json
{
  "name": "bob-own-folder",
  "parent_id": "{{bob_home_id}}"
}

HTTP 201
[Captures]
bob_folder_id: jsonpath "$.id"
[Asserts]
jsonpath "$.name" == "bob-own-folder"
jsonpath "$.parent_id" == {{bob_home_id}}


# ─────────────────────────────────────────────────────────────
# Step 11 – Bob uploads a file into his own home (for the
#            file-move tests below).
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/files/upload
Authorization: Bearer {{bob_token}}
[MultipartFormData]
folder_id: {{bob_home_id}}
file: file,fixtures/hello.txt; text/plain

HTTP 201
[Captures]
bob_file_id: jsonpath "$.id"


# ─────────────────────────────────────────────────────────────
# Step 12 – Bob attempts to move his own file into admin's
#            private folder. He owns the file but not the target
#            → verify_target_folder_owner rejects with 404.
# ─────────────────────────────────────────────────────────────
PUT {{base_url}}/api/files/{{bob_file_id}}/move
Authorization: Bearer {{bob_token}}
Content-Type: application/json
{
  "folder_id": "{{admin_private_id}}"
}

HTTP 404
[Asserts]
jsonpath "$.error_type" == "NotFound"


# ─────────────────────────────────────────────────────────────
# Step 13 – Bob moves his file to folder_id: null (his root
#            namespace). storage.files.folder_id IS NULL is a
#            legitimate state — verify_target_folder_owner
#            short-circuits to Ok(()) when target is None.
# ─────────────────────────────────────────────────────────────
PUT {{base_url}}/api/files/{{bob_file_id}}/move
Authorization: Bearer {{bob_token}}
Content-Type: application/json
{
  "folder_id": null
}

HTTP 200
[Asserts]
jsonpath "$.id" == {{bob_file_id}}
jsonpath "$.folder_id" == null


# ─────────────────────────────────────────────────────────────
# Step 14 – Bob attempts to access admin's file directly.
#            verify_owner on the file (not the folder) catches
#            this — IDOR on file reads, also 404.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/files/{{admin_file_id}}
Authorization: Bearer {{bob_token}}

HTTP 404
[Asserts]
jsonpath "$.error_type" == "NotFound"


# ─────────────────────────────────────────────────────────────
# Step 15 – Admin's private folder still exists & is untouched.
#            Bob's attacks must not have polluted admin's tree.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/folders/{{admin_home_id}}/resources?resource_types=folder
Authorization: Bearer {{admin_token}}

HTTP 200
[Asserts]
jsonpath "$.items[*].resource.id" contains {{admin_private_id}}
jsonpath "$.items[*].resource.name" not contains "bob-attack-1"
jsonpath "$.items[*].resource.name" not contains "bob-attack-2"


# ═════════════════════════════════════════════════════════════
# WebDAV MKCOL — namespace isolation
# ═════════════════════════════════════════════════════════════
# WebDAV requests are isolated per-user by `resolve_webdav_path`
# (webdav_handler.rs:235). If the requested path doesn't begin
# with the caller's home folder name (the drive's root folder
# name — "Personal" by default post-D0), the handler silently
# prefixes the caller's home folder path onto the front. Effect:
# any WebDAV path a client sends is always resolved INSIDE the
# caller's own tree, regardless of what they wrote.
# The test URLs below use "My Folder - <username>" as a path
# segment that's GUARANTEED not to match any caller's home name
# (all home folders are "Personal" post-D0), so the resolver's
# prepend branch always fires.
#
# These tests assert the isolation works (regression guard) and
# that the service-level verify_owner still acts as
# defense-in-depth for the legitimate path.


# ─────────────────────────────────────────────────────────────
# Step 16 – Bob crafts a path that looks like it targets admin's
#            home. Pre-43cf4a2b the WebDAV handler silently
#            rewrote `My Folder - admin/...` into the caller's own
#            home folder, so this MKCOL succeeded with 201 but the
#            new folders landed in BOB's tree (defense via
#            redirect). 43cf4a2b made MKCOL strictly RFC 4918
#            §9.3.1 compliant: 409 when the parent collection is
#            missing, no auto-creation of ancestors. Bob's MKCOL
#            now fails because `My Folder - admin` is not a folder
#            bob can reach — defense via rejection rather than
#            silent rewrite. The 4xx range allows for 403/404/409
#            depending on which gate fires first.
# ─────────────────────────────────────────────────────────────
MKCOL {{base_url}}/webdav/My%20Folder%20-%20admin/bob-webdav-attack
Authorization: Bearer {{bob_token}}

HTTP *
[Asserts]
status >= 400
status <  500


# ─────────────────────────────────────────────────────────────
# Step 17 – Positive control: bob MKCOL inside his own home.
# Uses "Personal" — bob's home folder name post-D0
# (docs/plan/drive.md §3, the canonical default). The resolver
# detects the URL already starts with the caller's home name and
# does NOT prepend again, so the new folder lands directly in
# bob's home rather than in a fresh intermediate.
# ─────────────────────────────────────────────────────────────
MKCOL {{base_url}}/webdav/Personal/bob-webdav-own
Authorization: Bearer {{bob_token}}

HTTP 201


# ─────────────────────────────────────────────────────────────
# Step 18 – Bob's home contains "bob-webdav-own" (from Step 17's
#            legitimate MKCOL) and does NOT contain "My Folder -
#            admin". Pre-43cf4a2b the path-prefix rewrite would
#            have created that name literally as a sub-folder in
#            bob's tree (defense via redirect); post-43cf4a2b the
#            MKCOL is rejected outright (defense via rejection),
#            so no such folder exists in bob's namespace either.
#            Both are correct security outcomes — the wire signal
#            just changed from "succeeded but didn't reach admin"
#            to "didn't succeed at all."
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/folders/{{bob_home_id}}/resources?resource_types=folder
Authorization: Bearer {{bob_token}}

HTTP 200
[Asserts]
jsonpath "$.items[*].resource.name" contains "bob-webdav-own"
jsonpath "$.items[*].resource.name" not contains "My Folder - admin"


# ─────────────────────────────────────────────────────────────
# Step 19 – Admin's tree is unchanged by bob's WebDAV traffic.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/folders/{{admin_home_id}}/resources?resource_types=folder
Authorization: Bearer {{admin_token}}

HTTP 200
[Asserts]
jsonpath "$.items[*].resource.name" not contains "bob-webdav-attack"
jsonpath "$.items[*].resource.name" not contains "bob-webdav-own"
