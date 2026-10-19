# =============================================================
# OxiCloud — a recursive copy preserves each file's type rank
# =============================================================
# Guards the defect fixed by
# `migrations/20261103000009_copy_folder_tree_category_order.sql`.
#
# `copy_folder_tree`'s file INSERT named its columns explicitly and
# omitted `category_order`, so every copied file took the column default
# — 9999, the catch-all "Other" rank — instead of 100/image, 400/pdf and
# so on. Copied files sorted to the end of every type-grouped listing
# regardless of what they were. Six such rows existed on the dev
# instance before the fix.
#
# Only the CASCADING path was affected; a single-file copy goes through
# `save_file_with_blob`, which computes the rank explicitly. So the two
# copy paths silently disagreed, which is what an explicit column list
# invites.
#
# ── Why this test asserts ORDER, not a field ──────────────────────────
# `category_order` is not on the wire. Its only observable effect is the
# ordering under `?order_by=type`, which the file arm sorts by
# `(category_order, LOWER(name), id)` — see folder_db_repository's
# "type" branch.
#
# That makes the naming load-bearing. The two files are renamed after
# upload so that ALPHABETICAL order is the OPPOSITE of TYPE order:
#
#   aa-doc.pdf   rank 400 (pdf)    — first by name, second by type
#   zz-pic.png   rank 100 (image)  — second by name, first by type
#
#   correct  → zz-pic.png first (rank 100 wins)
#   BROKEN   → both ranks are 9999, the tie falls through to LOWER(name),
#              so aa-doc.pdf comes first
#
# With the fixtures' own names (`blue-image.png`, `broken.pdf`) both
# implementations happen to return the image first — `bl` sorts before
# `br` — so the test would pass against the bug. Hence the renames.
#
# Renaming is safe for this test: `rename_file` sets only
# `name`/`updated_at`/`updated_by` and does NOT recompute
# `category_order`, so the rank stays whatever the upload computed.
#
# Self-contained: creates its own folders and deletes them in teardown.
# =============================================================


# ─────────────────────────────────────────────────────────────
# Setup — login, a source folder and a copy destination.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "{{username}}", "password": "{{password}}" }

HTTP 200
[Captures]
token: jsonpath "$.access_token"


POST {{base_url}}/api/folders
Authorization: Bearer {{token}}
Content-Type: application/json
{
  "name": "hurl-catorder-src"
}

HTTP 201
[Captures]
src_id: jsonpath "$.id"


POST {{base_url}}/api/folders
Authorization: Bearer {{token}}
Content-Type: application/json
{
  "name": "hurl-catorder-dst"
}

HTTP 201
[Captures]
dst_id: jsonpath "$.id"


# ─────────────────────────────────────────────────────────────
# Seed one image and one PDF, and pin the mime types.
#
# MIME is magic-bytes-first (`infer`) with an extension fallback
# (`mime_guess`) — see common/mime_detect.rs. Asserting the stored
# mime here pins the premise: if detection ever changes, this test
# fails on the ASSERTION rather than mysteriously on the ordering
# further down.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/files/upload
Authorization: Bearer {{token}}
[MultipartFormData]
folder_id: {{src_id}}
file: file,fixtures/blue-image.png; image/png

HTTP 201
[Captures]
img_id: jsonpath "$.id"
[Asserts]
jsonpath "$.mime_type" == "image/png"


POST {{base_url}}/api/files/upload
Authorization: Bearer {{token}}
[MultipartFormData]
folder_id: {{src_id}}
file: file,fixtures/broken.pdf; application/pdf

HTTP 201
[Captures]
pdf_id: jsonpath "$.id"
[Asserts]
jsonpath "$.mime_type" == "application/pdf"


# ─────────────────────────────────────────────────────────────
# Invert the alphabetical order against the type order, so the
# correct and broken implementations give DIFFERENT answers.
# ─────────────────────────────────────────────────────────────
PUT {{base_url}}/api/files/{{img_id}}/rename
Authorization: Bearer {{token}}
Content-Type: application/json
{
  "name": "zz-pic.png"
}

HTTP 200


PUT {{base_url}}/api/files/{{pdf_id}}/rename
Authorization: Bearer {{token}}
Content-Type: application/json
{
  "name": "aa-doc.pdf"
}

HTTP 200


# ─────────────────────────────────────────────────────────────
# Baseline — the SOURCE orders image-before-pdf despite the names.
# Establishes that `order_by=type` really is rank-driven, so the
# assertion on the copy below is comparing against a known-good
# answer rather than an assumption.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/folders/{{src_id}}/resources?limit=50&order_by=type&resource_types=file
Authorization: Bearer {{token}}

HTTP 200
[Asserts]
jsonpath "$.items" count == 2
jsonpath "$.items[0].resource.name" == "zz-pic.png"
jsonpath "$.items[1].resource.name" == "aa-doc.pdf"


# ═════════════════════════════════════════════════════════════
# The recursive copy
# ═════════════════════════════════════════════════════════════
POST {{base_url}}/api/batch/folders/copy
Authorization: Bearer {{token}}
Content-Type: application/json
{
  "folder_ids": ["{{src_id}}"],
  "target_folder_id": "{{dst_id}}"
}

HTTP 200
[Captures]
copy_root_id: jsonpath "$.successful[0].new_root_folder_id"
[Asserts]
jsonpath "$.stats.successful" == 1
jsonpath "$.stats.failed" == 0


# ─────────────────────────────────────────────────────────────
# THE assertion — the copies keep their ranks.
#
# Pre-fix both copied rows carried 9999, the tie fell through to
# LOWER(name), and this listing returned aa-doc.pdf first.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/folders/{{copy_root_id}}/resources?limit=50&order_by=type&resource_types=file
Authorization: Bearer {{token}}

HTTP 200
[Asserts]
jsonpath "$.items" count == 2
jsonpath "$.items[0].resource.name" == "zz-pic.png"
jsonpath "$.items[1].resource.name" == "aa-doc.pdf"
# The copies are distinct rows, not the originals re-listed — otherwise
# the assertion above would pass without the copy having worked at all.
jsonpath "$.items[0].resource.id" != "{{img_id}}"
jsonpath "$.items[1].resource.id" != "{{pdf_id}}"
# Mime survived the copy too (it IS in the copy's column list, unlike
# category_order was) — so a failure above is about the rank alone.
jsonpath "$.items[0].resource.mime_type" == "image/png"
jsonpath "$.items[1].resource.mime_type" == "application/pdf"


# ═════════════════════════════════════════════════════════════
# Teardown
# ═════════════════════════════════════════════════════════════
DELETE {{base_url}}/api/folders/{{copy_root_id}}
Authorization: Bearer {{token}}

HTTP 204


DELETE {{base_url}}/api/folders/{{dst_id}}
Authorization: Bearer {{token}}

HTTP 204


DELETE {{base_url}}/api/folders/{{src_id}}
Authorization: Bearer {{token}}

HTTP 204
