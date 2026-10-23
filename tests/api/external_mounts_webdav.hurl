# =============================================================
# OxiCloud — External file mounts over BOTH WebDAV surfaces
# =============================================================
# Regression pins for #792 and #794 (reported 2026-10-06):
#
#   #792  PROPFIND Depth:1 on a mount directory listed the files but
#         no subdirectories — the keyset folder listing
#         (`FolderService::list_folders_batch_with_perms`) had lost
#         its external-mount branch in the keyset refactor while the
#         file half kept it.
#   #794  Every path BELOW a mount root 404'd on `/webdav/…` and on
#         `/remote.php/dav/files/{user}/…`: the handlers resolved
#         paths through the DB-only PathResolver without the
#         mount-aware fallback, and then ran `Uuid::parse_str` on the
#         resolved `ext:` id before the permission check.
#
# Shape: create a writable local_fs mount on /tmp (present on every
# runner), mkdir a subdirectory THROUGH the mount (REST, mount root is
# a real UUID so this path always worked), upload a file into the
# mount root, then pin — per surface — Depth:1 on the mount root lists
# the subdirectory, PROPFIND on the subdirectory is 207, GET/HEAD of
# the file return the bytes, and a Range GET is honoured (206). Cleanup
# deletes the created entries (mount deletes are permanent — no trash)
# and the mount itself.
#
# DAV surfaces refuse the account password; an app password is minted
# first. Requires OXICLOUD_ENABLE_EXTERNAL_MOUNTS=true (server.env).
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
[Asserts]
jsonpath "$[0].kind" == "personal"


POST {{base_url}}/api/auth/app-passwords
Authorization: Bearer {{token}}
Content-Type: application/json
{ "label": "external_mounts_webdav hurl" }

HTTP 200
[Captures]
ap_id: jsonpath "$.id"
ap_pw: jsonpath "$.password"


# ─────────────────────────────────────────────────────────────
# Setup — writable mount on /tmp, a subdirectory and a file in it.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/admin/external-mounts
Authorization: Bearer {{token}}
Content-Type: application/json
{ "name": "hurl-dav-mount", "host_path": "/tmp", "read_only": false, "drive_id": "{{drive_id}}" }

HTTP 201
[Captures]
mount_id: jsonpath "$.mount_folder_id"


# Leftovers from an aborted earlier run would make the mkdir below fail;
# /tmp outlives the test DB, so sweep first (404 when clean).
DELETE {{base_url}}/webdav/hurl-dav-mount/hello.txt
[BasicAuth]
{{username}}: {{ap_pw}}

HTTP *


DELETE {{base_url}}/webdav/hurl-dav-mount/hurl-dav-sub/
[BasicAuth]
{{username}}: {{ap_pw}}

HTTP *


POST {{base_url}}/api/folders
Authorization: Bearer {{token}}
Content-Type: application/json
{ "name": "hurl-dav-sub", "parent_id": "{{mount_id}}" }

HTTP 201
[Captures]
sub_id: jsonpath "$.id"
[Asserts]
jsonpath "$.id" startsWith "ext:"


POST {{base_url}}/api/files/upload
Authorization: Bearer {{token}}
[MultipartFormData]
folder_id: {{mount_id}}
file: file,fixtures/hello.txt; text/plain

HTTP 201
[Captures]
file_id: jsonpath "$.id"
[Asserts]
jsonpath "$.id" startsWith "ext:"
jsonpath "$.size" == 32


# ─────────────────────────────────────────────────────────────
# #792 — native WebDAV: Depth:1 on the mount root lists the subdir.
# ─────────────────────────────────────────────────────────────
PROPFIND {{base_url}}/webdav/hurl-dav-mount/
Depth: 1
[BasicAuth]
{{username}}: {{ap_pw}}

HTTP 207
[Asserts]
body contains "hurl-dav-sub"
body contains "hello.txt"
body contains "<D:collection/>"


# ─────────────────────────────────────────────────────────────
# #794 — native WebDAV: the subdirectory and the file resolve.
# ─────────────────────────────────────────────────────────────
PROPFIND {{base_url}}/webdav/hurl-dav-mount/hurl-dav-sub/
Depth: 0
[BasicAuth]
{{username}}: {{ap_pw}}

HTTP 207
[Asserts]
body contains "hurl-dav-sub"


PROPFIND {{base_url}}/webdav/hurl-dav-mount/hello.txt
Depth: 0
[BasicAuth]
{{username}}: {{ap_pw}}

HTTP 207
[Asserts]
body contains "hello.txt"
body contains "<D:getcontentlength>32</D:getcontentlength>"


HEAD {{base_url}}/webdav/hurl-dav-mount/hello.txt
[BasicAuth]
{{username}}: {{ap_pw}}

HTTP 200
[Asserts]
header "Content-Length" == "32"
header "ETag" exists


GET {{base_url}}/webdav/hurl-dav-mount/hello.txt
[BasicAuth]
{{username}}: {{ap_pw}}

HTTP 200
[Asserts]
header "Content-Length" == "32"
header "Accept-Ranges" == "bytes"
body == "Hello from OxiCloud Hurl tests.\n"


# Range reads go through the provider (206), the same way REST does.
GET {{base_url}}/webdav/hurl-dav-mount/hello.txt
Range: bytes=0-4
[BasicAuth]
{{username}}: {{ap_pw}}

HTTP 206
[Asserts]
header "Content-Length" == "5"
header "Content-Range" == "bytes 0-4/32"
body == "Hello"


# ─────────────────────────────────────────────────────────────
# Same pins on the NextCloud-compatible surface (the sync clients).
# ─────────────────────────────────────────────────────────────
PROPFIND {{base_url}}/remote.php/dav/files/{{username}}/hurl-dav-mount/
Depth: 1
[BasicAuth]
{{username}}: {{ap_pw}}

HTTP 207
[Asserts]
body contains "hurl-dav-sub"
body contains "hello.txt"


PROPFIND {{base_url}}/remote.php/dav/files/{{username}}/hurl-dav-mount/hurl-dav-sub/
Depth: 1
[BasicAuth]
{{username}}: {{ap_pw}}

HTTP 207


GET {{base_url}}/remote.php/dav/files/{{username}}/hurl-dav-mount/hello.txt
[BasicAuth]
{{username}}: {{ap_pw}}

HTTP 200
[Asserts]
header "Content-Length" == "32"
body contains "Hello"


HEAD {{base_url}}/remote.php/dav/files/{{username}}/hurl-dav-mount/hello.txt
[BasicAuth]
{{username}}: {{ap_pw}}

HTTP 200


# A path that does not exist below the mount is still a clean 404.
PROPFIND {{base_url}}/webdav/hurl-dav-mount/does-not-exist/
Depth: 0
[BasicAuth]
{{username}}: {{ap_pw}}

HTTP 404


# ─────────────────────────────────────────────────────────────
# Cleanup — mount content first, through native WebDAV DELETE
# (`resolve_or_legacy` + `delete_*_with_perms`, mount-aware; mounts
# have no trash, so this is permanent), then the mount and the app
# password.
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/webdav/hurl-dav-mount/hello.txt
[BasicAuth]
{{username}}: {{ap_pw}}

HTTP 204


DELETE {{base_url}}/webdav/hurl-dav-mount/hurl-dav-sub/
[BasicAuth]
{{username}}: {{ap_pw}}

HTTP 204


DELETE {{base_url}}/api/admin/external-mounts/{{mount_id}}
Authorization: Bearer {{token}}

HTTP 204


DELETE {{base_url}}/api/auth/app-passwords/{{ap_id}}
Authorization: Bearer {{token}}

HTTP *
