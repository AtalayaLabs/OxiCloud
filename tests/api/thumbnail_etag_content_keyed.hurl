# =============================================================
# OxiCloud – Thumbnail ETag is keyed on CONTENT, not on file id
# =============================================================
# Regression guard for a stale-cache bug.
#
# The thumbnail ETag used to be `"thumb-{file_id}-{size}-{format}"`, sent
# with `Cache-Control: public, max-age=31536000, immutable`. Replacing a
# file's content preserves its id — the upload service rebuilds the entity
# with `parts.id` and a new hash, then fires `on_file_updated`, which
# regenerates the thumbnails — so the server produced a NEW thumbnail while
# advertising the OLD ETag. And `immutable` tells a conforming browser not
# to revalidate at all inside the freshness window, so clients kept showing
# the previous image for up to a year with no way to invalidate it.
#
# Keying on the content hash fixes it: new bytes → new hash → new ETag.
#
# This file asserts the invalidation direction. The sharing direction (two
# distinct files with identical content answering with the SAME ETag, so a
# copy revalidates to 304) is covered in `derived_blob_copy.hurl`.
#
# Overwrite goes through WebDAV PUT because that is the path that replaces
# content in place; the REST upload endpoint creates a new file instead.
#
# Prerequisites: setup.hurl must have run (admin user exists).
# =============================================================


# ─────────────────────────────────────────────────────────────
# Step 1 – Login
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/login
Content-Type: application/json
{
  "username": "{{username}}",
  "password": "{{password}}"
}

HTTP 200
[Captures]
token: jsonpath "$.access_token"


# ─────────────────────────────────────────────────────────────
# Step 2 – Upload the first image into a folder of its own.
#
# `folder_id` is required — the upload path resolves the owner from the
# destination folder, so omitting it is a 500, not a root upload. The
# folder also gives the WebDAV overwrite in step 4 a deterministic path.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/folders
Authorization: Bearer {{token}}
Content-Type: application/json
{
  "name": "hurl-etag-src"
}

HTTP 201
[Captures]
folder_id: jsonpath "$.id"


POST {{base_url}}/api/files/upload
Authorization: Bearer {{token}}
[MultipartFormData]
folder_id: {{folder_id}}
file: file,fixtures/red-image.png; image/png

HTTP 201
[Captures]
file_id: jsonpath "$.id"
file_name: jsonpath "$.name"
hash_before: jsonpath "$.content_hash"


# ─────────────────────────────────────────────────────────────
# Step 3 – Its thumbnail, and the ETag that goes with it
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/files/{{file_id}}/thumbnail/preview
Authorization: Bearer {{token}}

HTTP 200
[Captures]
etag_before: header "ETag"
thumb_before: bytes
[Asserts]
header "Cache-Control" contains "immutable"


# Unchanged content revalidates to 304 — the caching path works.
GET {{base_url}}/api/files/{{file_id}}/thumbnail/preview
Authorization: Bearer {{token}}
If-None-Match: {{etag_before}}

HTTP 304


# ─────────────────────────────────────────────────────────────
# Step 4 – Replace the content in place, keeping the same file id.
# ─────────────────────────────────────────────────────────────
PUT {{base_url}}/webdav/hurl-etag-src/{{file_name}}
Authorization: Bearer {{token}}
Content-Type: image/png
file,fixtures/green-image.png;

HTTP *
[Asserts]
status >= 200
status < 300


# Same file row, different content.
GET {{base_url}}/api/files/{{file_id}}
Authorization: Bearer {{token}}

HTTP 200
[Captures]
hash_after: jsonpath "$.content_hash"
[Asserts]
jsonpath "$.id" == "{{file_id}}"
jsonpath "$.content_hash" != "{{hash_before}}"


# ─────────────────────────────────────────────────────────────
# Step 5 – The ETag must have changed with the content.
#
# This is the assertion the file exists for. With the id-keyed ETag it was
# byte-identical to `etag_before`, and the next request would have been
# answered 304 from cache — serving the OLD image indefinitely.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/files/{{file_id}}/thumbnail/preview
Authorization: Bearer {{token}}

HTTP 200
[Captures]
etag_after: header "ETag"
[Asserts]
header "ETag" != "{{etag_before}}"


# A client holding the stale ETag must be told to refetch, not given a 304.
GET {{base_url}}/api/files/{{file_id}}/thumbnail/preview
Authorization: Bearer {{token}}
If-None-Match: {{etag_before}}

HTTP 200
[Asserts]
header "ETag" == "{{etag_after}}"


# ...and the new ETag revalidates normally.
GET {{base_url}}/api/files/{{file_id}}/thumbnail/preview
Authorization: Bearer {{token}}
If-None-Match: {{etag_after}}

HTTP 304


# ─────────────────────────────────────────────────────────────
# Step 6 – Teardown. Hurl files share one database within run.sh, so the
#          folder goes too, and both leave trash empty behind them.
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/api/files/{{file_id}}
Authorization: Bearer {{token}}

HTTP 204


GET {{base_url}}/api/trash/resources
Authorization: Bearer {{token}}

HTTP 200
[Captures]
trash_id: jsonpath "$.items[?(@.resource.id == '{{file_id}}')].resource.id"


DELETE {{base_url}}/api/trash/{{trash_id}}
Authorization: Bearer {{token}}

HTTP 200


DELETE {{base_url}}/api/folders/{{folder_id}}
Authorization: Bearer {{token}}

HTTP 204


GET {{base_url}}/api/trash/resources
Authorization: Bearer {{token}}

HTTP 200
[Captures]
trash_folder_id: jsonpath "$.items[?(@.resource.id == '{{folder_id}}')].resource.id"


DELETE {{base_url}}/api/trash/{{trash_folder_id}}
Authorization: Bearer {{token}}

HTTP 200
