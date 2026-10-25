# =============================================================
# OxiCloud — External file mounts over the REST API (the SPA)
# =============================================================
# Regression pins for #793 (reported 2026-10-06): REST prechecks
# parsed `ext:<mount>:<node>` ids as UUIDs and answered 404 before
# the handlers' mount branches could run. Only the mount root (a
# real UUID) was usable from the web UI; every subfolder and every
# file inside a mount 404'd — browse, download, preview, metadata,
# upload into a subfolder.
#
# Shape: writable local_fs mount on /tmp, mkdir a subfolder and
# upload a file through the mount root (always worked), then pin
# every SPA call on the `ext:` ids. Thumbnails of mount files are
# the documented 204 ("no thumbnail") instead of 404; metadata is
# the "no EXIF" body. A stranger is denied as 404 (anti-enum).
# Cleanup removes the created entries (mounts have no trash).
#
# Requires OXICLOUD_ENABLE_EXTERNAL_MOUNTS=true (server.env).
# =============================================================

POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "{{username}}", "password": "{{password}}" }

HTTP 200
[Captures]
token: jsonpath "$.access_token"


GET {{base_url}}/api/drives
Authorization: Bearer {{token}}

HTTP 200
[Captures]
drive_id: jsonpath "$[0].id"


POST {{base_url}}/api/admin/external-mounts
Authorization: Bearer {{token}}
Content-Type: application/json
{ "name": "hurl-rest-mount", "host_path": "/tmp", "read_only": false, "drive_id": "{{drive_id}}" }

HTTP 201
[Captures]
mount_id: jsonpath "$.mount_folder_id"


POST {{base_url}}/api/folders
Authorization: Bearer {{token}}
Content-Type: application/json
{ "name": "hurl-rest-sub", "parent_id": "{{mount_id}}" }

HTTP 201
[Captures]
sub_id: jsonpath "$.id"
[Asserts]
jsonpath "$.id" startsWith "ext:"


# ─────────────────────────────────────────────────────────────
# #793 — upload INTO the `ext:` subfolder (precheck on the folder).
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/files/upload
Authorization: Bearer {{token}}
[MultipartFormData]
folder_id: {{sub_id}}
file: file,fixtures/hello.txt; text/plain

HTTP 201
[Captures]
file_id: jsonpath "$.id"
[Asserts]
jsonpath "$.id" startsWith "ext:"
jsonpath "$.folder_id" == "{{sub_id}}"


# Folder metadata + listing of the `ext:` subfolder.
GET {{base_url}}/api/folders/{{sub_id}}
Authorization: Bearer {{token}}

HTTP 200
[Asserts]
jsonpath "$.id" == "{{sub_id}}"
jsonpath "$.name" == "hurl-rest-sub"
jsonpath "$.parent_id" == "{{mount_id}}"


GET {{base_url}}/api/folders/{{sub_id}}/resources
Authorization: Bearer {{token}}

HTTP 200
[Asserts]
jsonpath "$.items[*].resource.name" includes "hello.txt"


# Download, preview, metadata of the `ext:` file.
GET {{base_url}}/api/files/{{file_id}}
Authorization: Bearer {{token}}

HTTP 200
[Asserts]
header "Content-Length" == "32"
body == "Hello from OxiCloud Hurl tests.\n"


GET {{base_url}}/api/files/{{file_id}}?metadata=true
Authorization: Bearer {{token}}

HTTP 200
[Asserts]
jsonpath "$.name" == "hello.txt"
jsonpath "$.size" == 32


GET {{base_url}}/api/files/{{file_id}}/thumbnail/preview
Authorization: Bearer {{token}}

HTTP 204


GET {{base_url}}/api/files/{{file_id}}/metadata
Authorization: Bearer {{token}}

HTTP 200
[Asserts]
jsonpath "$.message" == "No EXIF metadata available"


# ─────────────────────────────────────────────────────────────
# Stranger: every one of the calls above is a 404 (anti-enum).
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/register
Content-Type: application/json
{ "username": "mount_rest_eve", "email": "mount_rest_eve@example.com", "password": "MountRestEve1!" }

# 200 (anti-enum) or 201 depending on the registration policy; the login
# below is what this scenario relies on.
HTTP *


POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "mount_rest_eve", "password": "MountRestEve1!" }

HTTP 200
[Captures]
eve_token: jsonpath "$.access_token"


GET {{base_url}}/api/folders/{{sub_id}}/resources
Authorization: Bearer {{eve_token}}

HTTP 404


GET {{base_url}}/api/files/{{file_id}}
Authorization: Bearer {{eve_token}}

HTTP 404


GET {{base_url}}/api/files/{{file_id}}/thumbnail/preview
Authorization: Bearer {{eve_token}}

HTTP 404


# ─────────────────────────────────────────────────────────────
# Cleanup — permanent inside a mount.
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/api/files/{{file_id}}
Authorization: Bearer {{token}}

HTTP *


DELETE {{base_url}}/api/folders/{{sub_id}}
Authorization: Bearer {{token}}

HTTP *


DELETE {{base_url}}/api/admin/external-mounts/{{mount_id}}
Authorization: Bearer {{token}}

HTTP 204
