# =============================================================
# OxiCloud – Derived blobs survive a copy, and are SHARED not duplicated
# =============================================================
# Guards two properties of `docs/plan/derived-blobs.md` that are easy to
# break and silent when broken.
#
# 1. **Derived content is content-keyed, so a copy gets it for free.**
#    `storage.content_derived_blobs` is keyed on `source_hash`, and a copy
#    carries the SAME `blob_hash` as its original. So the copy resolves to
#    the very same thumbnail row — nothing is duplicated, and nothing is
#    re-rendered. A regression that made copy duplicate those rows would
#    still return 200 here; the byte-identity assertions are what catch it,
#    because a re-render produces different bytes than a cache hit only if
#    the pipeline is non-deterministic — so we also assert the ref_count,
#    which a duplicated row would inflate.
#
# 2. **A copy takes a real blob reference, via BOTH copy paths.**
#    `storage.copy_file_satellites` (migration `20261019000000`) is now the
#    single home for that, called by the single-file path and by
#    `storage.copy_folder_tree`. The tree path previously bumped
#    `storage.blobs` only — which matched nothing for a manifest-backed
#    file, so a folder copy took NO reference and deleting the original
#    reaped bytes the copy still needed. Steps 6 and 9 are what would fail.
#
# The strongest assertion is step 11: after the ORIGINAL is permanently
# deleted and GC has run, both copies must still serve their thumbnail.
# That only holds if the references were real.
#
# Coverage note: `dedup-test.jpg` is single-chunk, so `file_hash` equals its
# lone chunk's hash — the aliasing case whose `NOT EXISTS` guard stops one
# reference being counted at both levels. The multi-chunk fan-out (where
# file_hash names a manifest that is NOT a chunk) differs only in that the
# hashes differ; it has no thumbnail-capable fixture at this size, so it is
# covered at the SQL level rather than here.
#
# Prerequisites: setup.hurl must have run (admin user exists).
#
# Run:
#   hurl --variables-file tests/api/test.env --file-root tests \
#        --test tests/api/derived_blob_copy.hurl
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
  "name": "hurl-derived-src"
}

HTTP 201
[Captures]
src_folder_id: jsonpath "$.id"


POST {{base_url}}/api/folders
Authorization: Bearer {{token}}
Content-Type: application/json
{
  "name": "hurl-derived-dst"
}

HTTP 201
[Captures]
dst_folder_id: jsonpath "$.id"


# ─────────────────────────────────────────────────────────────
# Step 3 – Upload the source image
#
# `content_hash` is captured rather than hardcoded so the test does not
# break if the fixture is ever regenerated.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/files/upload
Authorization: Bearer {{token}}
[MultipartFormData]
folder_id: {{src_folder_id}}
file: file,fixtures/dedup-test.jpg; image/jpeg

HTTP 201
[Captures]
orig_file_id: jsonpath "$.id"
orig_file_name: jsonpath "$.name"
blob_hash: jsonpath "$.content_hash"
[Asserts]
jsonpath "$.content_hash" isString


# One file holds the blob.
GET {{base_url}}/api/dedup/check/{{blob_hash}}
Authorization: Bearer {{token}}

HTTP 200
[Asserts]
jsonpath "$.exists" == true
jsonpath "$.ref_count" == 1


# ─────────────────────────────────────────────────────────────
# Step 4 – Render the thumbnail. THIS is what creates the derived blob:
#          `content_derived_blobs(source_hash = blob_hash, 'thumbnail', …)`.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/files/{{orig_file_id}}/thumbnail/preview
Authorization: Bearer {{token}}

HTTP 200
[Captures]
thumb_bytes: bytes


# ─────────────────────────────────────────────────────────────
# Step 5 – Single-file copy into the destination folder
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


# ─────────────────────────────────────────────────────────────
# Step 6 – The copy took a reference.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/dedup/check/{{blob_hash}}
Authorization: Bearer {{token}}

HTTP 200
[Asserts]
jsonpath "$.exists" == true
jsonpath "$.ref_count" == 2


# ─────────────────────────────────────────────────────────────
# Step 7 – The copy serves the SAME thumbnail bytes.
#
# It shares the original's `blob_hash`, so it resolves the same
# `content_derived_blobs` row. Nothing was copied to make this work —
# that is the content-keying payoff.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/files/{{file_copy_id}}/thumbnail/preview
Authorization: Bearer {{token}}

HTTP 200
[Asserts]
bytes == {{thumb_bytes}}


# ─────────────────────────────────────────────────────────────
# Step 8 – Folder copy — the OTHER copy path, through
#          `storage.copy_folder_tree` → `copy_file_satellites`.
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
jsonpath "$[0].content_hash" == "{{blob_hash}}"


# ─────────────────────────────────────────────────────────────
# Step 9 – Three references now. Before `copy_file_satellites` the tree
#          path contributed nothing here and this stayed at 2.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/dedup/check/{{blob_hash}}
Authorization: Bearer {{token}}

HTTP 200
[Asserts]
jsonpath "$.ref_count" == 3


GET {{base_url}}/api/files/{{tree_copy_id}}/thumbnail/preview
Authorization: Bearer {{token}}

HTTP 200
[Asserts]
bytes == {{thumb_bytes}}


# ─────────────────────────────────────────────────────────────
# Step 10 – Permanently delete the ORIGINAL.
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


# Two copies remain, so the content must too.
GET {{base_url}}/api/dedup/check/{{blob_hash}}
Authorization: Bearer {{token}}

HTTP 200
[Asserts]
jsonpath "$.exists" == true
jsonpath "$.ref_count" == 2


# ─────────────────────────────────────────────────────────────
# Step 11 – Run GC, then prove both copies still work.
#
# This is the assertion the whole file exists for. If either copy had
# failed to take a reference, the original's deletion would have walked
# the count to 0 and GC would have reaped the content AND its derived
# thumbnail — leaving these 5xx. That was a real, shipped bug on the
# folder-copy path.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/admin/jobs/dedup_gc/trigger
Authorization: Bearer {{token}}
[Options]
delay: 500ms

HTTP 200


GET {{base_url}}/api/files/{{file_copy_id}}/thumbnail/preview
Authorization: Bearer {{token}}

HTTP 200
[Asserts]
bytes == {{thumb_bytes}}


GET {{base_url}}/api/files/{{tree_copy_id}}/thumbnail/preview
Authorization: Bearer {{token}}

HTTP 200
[Asserts]
bytes == {{thumb_bytes}}


# ─────────────────────────────────────────────────────────────
# Step 12 – Teardown. Hurl files share one database within run.sh, so
#           everything created here must go, including from trash.
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
