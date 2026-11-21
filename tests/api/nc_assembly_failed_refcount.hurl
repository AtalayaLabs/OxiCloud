# =============================================================
# OxiCloud – failed NC chunk assembly must not release a reference
#            it does not own (#916)
# =============================================================
# A failed assembly MOVE released the dedup reference twice, deleting the
# manifest another file with the same bytes still used. Trigger: missing
# parent folder (400). The fixture is unique to this file — don't reuse it.
# =============================================================

POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "{{username}}", "password": "{{password}}" }

HTTP 200
[Captures]
token: jsonpath "$.access_token"
admin_id: jsonpath "$.user.full.user.id"


POST {{base_url}}/api/auth/app-passwords
Authorization: Bearer {{token}}
Content-Type: application/json
{ "label": "nc_assembly_failed_refcount hurl test" }

HTTP 200
[Captures]
nc_username: jsonpath "$.username"
nc_password: jsonpath "$.password"
ap_id:       jsonpath "$.id"


# Step 1 — The surviving file: REST upload of the fixture, which
# hands back the server-side BLAKE3 content hash.
GET {{base_url}}/api/folders
Authorization: Bearer {{token}}

HTTP 200
[Captures]
home_folder_id: jsonpath "$[0].id"


POST {{base_url}}/api/files/upload
Authorization: Bearer {{token}}
[MultipartFormData]
folder_id: {{home_folder_id}}
file: file,fixtures/nc-assembly-refcount.txt; text/plain

HTTP 201
[Captures]
survivor_id: jsonpath "$.id"
survivor_hash: jsonpath "$.content_hash"


# Step 2 — The surviving file's manifest holds exactly one reference.
GET {{base_url}}/api/admin/dedup/check/{{survivor_hash}}
Authorization: Bearer {{token}}

HTTP 200
[Asserts]
jsonpath "$.ref_count" == 1


# Step 3 — Same bytes as a chunked upload, assembled into a folder
# that does not exist → the commit fails.
MKCOL {{base_url}}/remote.php/dav/uploads/{{nc_username}}/nc-916-session
[BasicAuth]
{{nc_username}}: {{nc_password}}

HTTP 201


PUT {{base_url}}/remote.php/dav/uploads/{{nc_username}}/nc-916-session/00001
[BasicAuth]
{{nc_username}}: {{nc_password}}
file,fixtures/nc-assembly-refcount.txt;

HTTP 201


MOVE {{base_url}}/remote.php/dav/uploads/{{nc_username}}/nc-916-session/.file
Destination: {{base_url}}/remote.php/dav/files/{{nc_username}}/nc-916-does-not-exist/x.txt
[BasicAuth]
{{nc_username}}: {{nc_password}}

HTTP 400


# Step 4 — The pin: the failed assembly left the survivor's
# reference alone. Before the fix the manifest was deleted (404).
GET {{base_url}}/api/admin/dedup/check/{{survivor_hash}}
Authorization: Bearer {{token}}

HTTP 200
[Asserts]
jsonpath "$.ref_count" == 1


# Step 5 — The same bytes PUT by a viewer of a shared drive: refused before
# the write, so the service must release the reference it was handed
# (WebDAV/PATCH/WOPI share that path). Without it the count leaks to 2.
POST {{base_url}}/api/admin/users
Authorization: Bearer {{token}}
Content-Type: application/json
{
  "username": "nc916_viewer",
  "password": "Nc916ViewerPassword1!",
  "email": "nc916_viewer@example.com",
  "role": "user"
}

HTTP 201
[Captures]
viewer_id: jsonpath "$.user.id"


POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "nc916_viewer", "password": "Nc916ViewerPassword1!" }

HTTP 200
[Captures]
viewer_token: jsonpath "$.access_token"


POST {{base_url}}/api/drives
Authorization: Bearer {{token}}
Content-Type: application/json
{
  "kind": "shared",
  "name": "nc-916-shared",
  "owner": { "type": "user", "id": "{{admin_id}}" }
}

HTTP 201
[Captures]
shared_drive_id: jsonpath "$.id"


POST {{base_url}}/api/grants
Authorization: Bearer {{token}}
Content-Type: application/json
{
  "subject":  { "type": "user",  "id": "{{viewer_id}}" },
  "resource": { "type": "drive", "id": "{{shared_drive_id}}" },
  "role": "viewer"
}

HTTP 201


PUT {{base_url}}/webdav/@drive/{{shared_drive_id}}/nc-916-viewer.txt
Authorization: Bearer {{viewer_token}}
file,fixtures/nc-assembly-refcount.txt;

HTTP 403


GET {{base_url}}/api/admin/dedup/check/{{survivor_hash}}
Authorization: Bearer {{token}}

HTTP 200
[Asserts]
jsonpath "$.ref_count" == 1


# Cleanup: a failed assembly keeps its chunks for a retry, so abort the
# session (storage_cleanup_check.sh fails on leftovers in .uploads).
DELETE {{base_url}}/remote.php/dav/uploads/{{nc_username}}/nc-916-session
[BasicAuth]
{{nc_username}}: {{nc_password}}

HTTP 204

# Cleanup: trash, then purge (only the purge releases the reference).
DELETE {{base_url}}/api/files/{{survivor_id}}
Authorization: Bearer {{token}}

HTTP 204


GET {{base_url}}/api/trash/resources
Authorization: Bearer {{token}}

HTTP 200
[Captures]
survivor_trash_id: jsonpath "$.items[?(@.resource.id == '{{survivor_id}}')].resource.id"


DELETE {{base_url}}/api/trash/{{survivor_trash_id}}
Authorization: Bearer {{token}}

HTTP 200


DELETE {{base_url}}/api/auth/app-passwords/{{ap_id}}
Authorization: Bearer {{token}}

HTTP *
