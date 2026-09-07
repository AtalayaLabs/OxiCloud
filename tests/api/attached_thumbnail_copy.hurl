# =============================================================
# OxiCloud – An UPLOADED thumbnail survives both copy paths
# =============================================================
# A user-supplied preview is not derivable from the file's content, so
# nothing can regenerate it. If a copy loses it, it is gone — and the loss
# is silent, because the server quietly falls back to rendering one from
# the source (or to 204 for a PDF, which has no render path at all).
#
# That was the behaviour before `storage.file_attached_blobs`: the PUT
# wrote `ext-{file_id}.jpg`, keyed by file id, which no copy path
# duplicates and no other instance can see.
#
# The test distinguishes "preserved" from "re-rendered" by making the two
# visibly different: the FILE is red-image.png, the uploaded thumbnail is
# derived from green-image.png. A server-side render of the file could
# only ever produce the red one. So byte-equality with the post-upload
# bytes proves the copy served the ATTACHMENT, not a fresh render.
#
# Step 4 is what makes that airtight — it captures the rendered thumbnail
# BEFORE the upload and requires the upload to change it. Without that,
# byte-equality across copies could be satisfied by three identical
# renders.
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
# Step 2 – Source and destination folders
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/folders
Authorization: Bearer {{token}}
Content-Type: application/json
{
  "name": "hurl-attach-src"
}

HTTP 201
[Captures]
src_folder_id: jsonpath "$.id"


POST {{base_url}}/api/folders
Authorization: Bearer {{token}}
Content-Type: application/json
{
  "name": "hurl-attach-dst"
}

HTTP 201
[Captures]
dst_folder_id: jsonpath "$.id"


# ─────────────────────────────────────────────────────────────
# Step 3 – Upload the file (RED)
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/files/upload
Authorization: Bearer {{token}}
[MultipartFormData]
folder_id: {{src_folder_id}}
file: file,fixtures/red-image.png; image/png

HTTP 201
[Captures]
orig_file_id: jsonpath "$.id"
orig_file_name: jsonpath "$.name"


# ─────────────────────────────────────────────────────────────
# Step 4 – The server-rendered thumbnail, before any upload.
#          Captured so the upload can be shown to have replaced it.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/files/{{orig_file_id}}/thumbnail/preview
Authorization: Bearer {{token}}

HTTP 200
[Captures]
rendered_thumb: bytes
rendered_etag: header "ETag"


# ─────────────────────────────────────────────────────────────
# Step 5 – Upload a custom thumbnail (GREEN) for that file
# ─────────────────────────────────────────────────────────────
PUT {{base_url}}/api/files/{{orig_file_id}}/thumbnail/preview
Authorization: Bearer {{token}}
Content-Type: image/png
file,fixtures/green-image.png;

HTTP 201


# It must now serve the upload, not the render.
GET {{base_url}}/api/files/{{orig_file_id}}/thumbnail/preview
Authorization: Bearer {{token}}

HTTP 200
[Captures]
uploaded_thumb: bytes
uploaded_etag: header "ETag"
[Asserts]
bytes != {{rendered_thumb}}
# The ETag must move with the bytes. It is keyed on the ATTACHED blob's own
# hash, because uploading a preview leaves the file's content — and so a
# source-keyed ETag — unchanged. With `immutable` set, an unchanged
# validator means clients never revalidate and keep the old render for a
# year.
header "ETag" != "{{rendered_etag}}"


# A client holding the pre-upload validator must be told to refetch.
GET {{base_url}}/api/files/{{orig_file_id}}/thumbnail/preview
Authorization: Bearer {{token}}
If-None-Match: {{rendered_etag}}

HTTP 200
[Asserts]
header "ETag" == "{{uploaded_etag}}"


# ...and the new one revalidates.
GET {{base_url}}/api/files/{{orig_file_id}}/thumbnail/preview
Authorization: Bearer {{token}}
If-None-Match: {{uploaded_etag}}

HTTP 304


# ─────────────────────────────────────────────────────────────
# Step 6 – Single-file copy → the attachment comes with it.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/batch/files/copy
Authorization: Bearer {{token}}
Content-Type: application/json
{
  "file_ids":         ["{{orig_file_id}}"],
  "target_folder_id": "{{dst_folder_id}}"
}

HTTP 200
[Captures]
file_copy_id: jsonpath "$.successful[0].id"
[Asserts]
jsonpath "$.successful[0].id" != "{{orig_file_id}}"


GET {{base_url}}/api/files/{{file_copy_id}}/thumbnail/preview
Authorization: Bearer {{token}}

HTTP 200
[Asserts]
bytes == {{uploaded_thumb}}
bytes != {{rendered_thumb}}
# Same bytes, so the same validator — the copy's attachment row points at
# the same blob. This is also what stops the collision a source-keyed ETag
# would allow: the copy inherits the source hash, so if either side later
# gets a DIFFERENT preview the two would serve different bytes under one
# ETag, and a shared cache could hand either to either.
header "ETag" == "{{uploaded_etag}}"


# ─────────────────────────────────────────────────────────────
# Step 7 – Folder copy → same, through storage.copy_folder_tree.
#
# The other copy path. It reaches the attachment through the same
# `copy_file_satellites` call, and this is the leg that would break if
# the tree path ever grew its own fan-out again.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/batch/folders/copy
Authorization: Bearer {{token}}
Content-Type: application/json
{
  "folder_ids":       ["{{src_folder_id}}"],
  "target_folder_id": "{{dst_folder_id}}"
}

HTTP 200
[Captures]
tree_root_id: jsonpath "$.successful[0].new_root_folder_id"
[Asserts]
jsonpath "$.stats.failed" == 0


GET {{base_url}}/api/files?folder_id={{tree_root_id}}
Authorization: Bearer {{token}}

HTTP 200
[Captures]
tree_copy_id: jsonpath "$[0].id"
[Asserts]
jsonpath "$" count == 1
jsonpath "$[0].name" == "{{orig_file_name}}"
jsonpath "$[0].id" != "{{orig_file_id}}"


GET {{base_url}}/api/files/{{tree_copy_id}}/thumbnail/preview
Authorization: Bearer {{token}}

HTTP 200
[Asserts]
bytes == {{uploaded_thumb}}
bytes != {{rendered_thumb}}
header "ETag" == "{{uploaded_etag}}"


# ─────────────────────────────────────────────────────────────
# Step 8 – Delete the ORIGINAL, run GC, and require both copies to keep
#          serving the upload.
#
# Each copy holds its own reference on the attached blob — the rows are
# duplicated, not shared, because the table is file-keyed. If the copy
# had failed to take one, deleting the original would walk the count to
# zero and GC would reap bytes that cannot be regenerated.
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/api/files/{{orig_file_id}}
Authorization: Bearer {{token}}

HTTP 204


GET {{base_url}}/api/trash/resources
Authorization: Bearer {{token}}

HTTP 200
[Captures]
trash_orig_id: jsonpath "$.items[?(@.resource.id == '{{orig_file_id}}')].resource.id"


DELETE {{base_url}}/api/trash/{{trash_orig_id}}
Authorization: Bearer {{token}}

HTTP 200


POST {{base_url}}/api/admin/jobs/dedup_gc/trigger
Authorization: Bearer {{token}}
[Options]
delay: 500ms

HTTP 200


GET {{base_url}}/api/files/{{file_copy_id}}/thumbnail/preview
Authorization: Bearer {{token}}

HTTP 200
[Asserts]
bytes == {{uploaded_thumb}}


GET {{base_url}}/api/files/{{tree_copy_id}}/thumbnail/preview
Authorization: Bearer {{token}}

HTTP 200
[Asserts]
bytes == {{uploaded_thumb}}


# ─────────────────────────────────────────────────────────────
# Step 9 – Teardown. Hurl files share one database within run.sh.
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/api/folders/{{src_folder_id}}
Authorization: Bearer {{token}}

HTTP 204


DELETE {{base_url}}/api/folders/{{dst_folder_id}}
Authorization: Bearer {{token}}

HTTP 204


GET {{base_url}}/api/trash/resources
Authorization: Bearer {{token}}

HTTP 200
[Captures]
trash_src_id: jsonpath "$.items[?(@.resource.id == '{{src_folder_id}}')].resource.id"
trash_dst_id: jsonpath "$.items[?(@.resource.id == '{{dst_folder_id}}')].resource.id"


DELETE {{base_url}}/api/trash/{{trash_src_id}}
Authorization: Bearer {{token}}

HTTP 200


DELETE {{base_url}}/api/trash/{{trash_dst_id}}
Authorization: Bearer {{token}}

HTTP 200
