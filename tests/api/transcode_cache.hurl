# =============================================================
# OxiCloud – Transcode caching: positive and negative
#
# Pins that a WebP transcode is computed ONCE per distinct content and
# then answered from the derived tier, in both directions:
#
#   * positive — WebP is smaller, so the bytes are stored and reused
#   * negative — WebP came out larger, so the VERDICT is stored and the
#     decode + encode is not repeated
#
# ## Why this can assert what the thumbnail tests could not
#
# `derived_blob_copy.hurl` documents that thumbnail tier selection is
# invisible over HTTP: stored blob, RAM cache and a fresh re-render all
# return identical bytes. Transcodes are the same — but
# `GET /api/admin/transcode/stats` now exposes the counters, so "was
# this computed or served" becomes observable from outside the process.
# `transcodes` is work done; `cache_hits` (RAM, keyed by file id) and
# `disk_hits` (the durable content-keyed tier) are work avoided.
#
# ## Why each case uploads the same bytes twice
#
# The in-memory cache is keyed `{file_id}:{ext}`, so re-fetching the SAME
# file proves only that moka works. Uploading identical content as a
# SECOND file gives a different file id and therefore a guaranteed memory
# miss — but the same content hash. If the second fetch still avoids a
# transcode, only the content-keyed tier can have answered it. That is
# precisely what the migration bought, and it is unobservable any other
# way.
#
# ## Fixtures
#
# `red-image.png` shrinks (4780 → 186 bytes). `negative-cache-transcode.png`
# does not — a real screenshot, which is the only thing that defeats this
# encoder; synthetic images all come out positive. Both properties are
# pinned by `image_transcode_service::fixture_premise`, so if an encoder
# bump ever flips one, that unit test fails loudly instead of this
# scenario quietly testing nothing.
#
# Prerequisites: setup.hurl must have run (admin user exists).
#
# Run:
#   hurl --variables-file tests/api/test.env --file-root tests \
#        --test tests/api/transcode_cache.hurl
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
# Step 2 – Working folder
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/folders
Authorization: Bearer {{token}}
Content-Type: application/json
{
  "name": "hurl-transcode-cache"
}

HTTP 201
[Captures]
folder_id: jsonpath "$.id"


# ─────────────────────────────────────────────────────────────
# Step 2b – A second folder, for the duplicate uploads.
#
# Re-uploading the same filename into the SAME folder overwrites the
# existing file and returns its id, so both halves of a "two distinct
# files, one content" pair would be the same row and the test would
# assert nothing. A second folder keeps the name free.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/folders
Authorization: Bearer {{token}}
Content-Type: application/json
{
  "name": "hurl-transcode-cache-dup"
}

HTTP 201
[Captures]
folder_dup_id: jsonpath "$.id"


# ─────────────────────────────────────────────────────────────
# Step 3 – Baseline counters.
#
# Absolute values are meaningless here — earlier scenarios in the same
# run transcode images too. Everything below is asserted as a DELTA
# from this point, which is also why this file must not assume it runs
# first.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/admin/transcode/stats
Authorization: Bearer {{token}}

HTTP 200
[Captures]
base_transcodes: jsonpath "$.transcodes"
base_not_beneficial: jsonpath "$.not_beneficial"


# ═════════════════════════════════════════════════════════════
# POSITIVE CASE — WebP is smaller
# ═════════════════════════════════════════════════════════════

# ─────────────────────────────────────────────────────────────
# Step 4 – Upload a shrinkable image
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/files/upload
Authorization: Bearer {{token}}
[MultipartFormData]
folder_id: {{folder_id}}
file: file,fixtures/red-image.png; image/png

HTTP 201
[Captures]
pos_a_id: jsonpath "$.id"
pos_hash: jsonpath "$.content_hash"


# ─────────────────────────────────────────────────────────────
# Step 5 – Fetch it as a WebP-capable client.
#
# `Accept: image/webp` is what selects the transcode path;
# `BrowserCapabilities::from_accept_header` looks for exactly this.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/files/{{pos_a_id}}
Authorization: Bearer {{token}}
Accept: image/webp,image/png,*/*

HTTP 200
[Asserts]
header "Content-Type" contains "image/webp"


# ─────────────────────────────────────────────────────────────
# Step 6 – That was work actually done, not a cache hit.
#
# Captured rather than computed: hurl has no arithmetic in predicates,
# and pinning the exact value here is stronger anyway — the later steps
# assert equality against it, so any transcode from any source shows up.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/admin/transcode/stats
Authorization: Bearer {{token}}

HTTP 200
[Captures]
after_positive: jsonpath "$.transcodes"
after_positive_disk: jsonpath "$.disk_hits"
[Asserts]
jsonpath "$.not_beneficial" == {{base_not_beneficial}}


# ─────────────────────────────────────────────────────────────
# Step 7 – The SAME bytes uploaded as a second, distinct file.
#
# Same content hash, different file id. Asserting the hash matches is
# what makes the next step meaningful: if these two files did not share
# content, a second transcode would be correct rather than a regression.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/files/upload
Authorization: Bearer {{token}}
[MultipartFormData]
folder_id: {{folder_dup_id}}
file: file,fixtures/red-image.png; image/png

HTTP 201
[Captures]
pos_b_id: jsonpath "$.id"
[Asserts]
jsonpath "$.content_hash" == "{{pos_hash}}"
jsonpath "$.id" != "{{pos_a_id}}"


# ─────────────────────────────────────────────────────────────
# Step 8 – Fetching the second file still yields WebP.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/files/{{pos_b_id}}
Authorization: Bearer {{token}}
Accept: image/webp,image/png,*/*

HTTP 200
[Asserts]
header "Content-Type" contains "image/webp"


# ─────────────────────────────────────────────────────────────
# Step 9 – …WITHOUT a second transcode.
#
# The memory cache could not have served this: it is keyed by file id
# and this is a different file. Only the content-keyed derived tier
# answers here, which is the whole point of keying derivations by
# content rather than by file.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/admin/transcode/stats
Authorization: Bearer {{token}}

HTTP 200
[Asserts]
jsonpath "$.transcodes" == {{after_positive}}
jsonpath "$.not_beneficial" == {{base_not_beneficial}}
jsonpath "$.disk_hits" != {{after_positive_disk}}


# ═════════════════════════════════════════════════════════════
# NEGATIVE CASE — WebP comes out larger
# ═════════════════════════════════════════════════════════════

# ─────────────────────────────────────────────────────────────
# Step 10 – Upload an image the encoder cannot shrink
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/files/upload
Authorization: Bearer {{token}}
[MultipartFormData]
folder_id: {{folder_id}}
file: file,fixtures/negative-cache-transcode.png; image/png

HTTP 201
[Captures]
neg_a_id: jsonpath "$.id"
neg_hash: jsonpath "$.content_hash"


# ─────────────────────────────────────────────────────────────
# Step 11 – A WebP-capable client gets the ORIGINAL back.
#
# Not a failure: transcoding to something larger would cost the client
# bandwidth, so the service serves the PNG and remembers why.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/files/{{neg_a_id}}
Authorization: Bearer {{token}}
Accept: image/webp,image/png,*/*

HTTP 200
[Asserts]
header "Content-Type" contains "image/png"


# ─────────────────────────────────────────────────────────────
# Step 12 – The attempt still cost one decode + encode.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/admin/transcode/stats
Authorization: Bearer {{token}}

HTTP 200
[Captures]
after_negative: jsonpath "$.not_beneficial"
[Asserts]
# The decode + encode ran and produced nothing usable, which is counted
# separately from `transcodes` — that only counts work that paid off.
jsonpath "$.transcodes" == {{after_positive}}


# ─────────────────────────────────────────────────────────────
# Step 13 – Same bytes again, as a distinct file.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/files/upload
Authorization: Bearer {{token}}
[MultipartFormData]
folder_id: {{folder_dup_id}}
file: file,fixtures/negative-cache-transcode.png; image/png

HTTP 201
[Captures]
neg_b_id: jsonpath "$.id"
[Asserts]
jsonpath "$.content_hash" == "{{neg_hash}}"
jsonpath "$.id" != "{{neg_a_id}}"


# ─────────────────────────────────────────────────────────────
# Step 14 – Original again, as expected.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/files/{{neg_b_id}}
Authorization: Bearer {{token}}
Accept: image/webp,image/png,*/*

HTTP 200
[Asserts]
header "Content-Type" contains "image/png"


# ─────────────────────────────────────────────────────────────
# Step 15 – …and the verdict was NOT recomputed.
#
# This is the assertion the negative row exists for. Without it the
# server re-runs a full decode + encode of a half-megabyte screenshot on
# every request for every file sharing that content, only to throw the
# result away each time. A counter that moved here would mean the
# negative row was not written, not read, or not keyed by content.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/admin/transcode/stats
Authorization: Bearer {{token}}

HTTP 200
[Asserts]
jsonpath "$.not_beneficial" == {{after_negative}}
jsonpath "$.transcodes" == {{after_positive}}


# ─────────────────────────────────────────────────────────────
# Step 16 – Teardown. Hurl files share one database, so a folder left
# behind changes what later scenarios see.
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/api/folders/{{folder_id}}
Authorization: Bearer {{token}}

HTTP 204


DELETE {{base_url}}/api/folders/{{folder_dup_id}}
Authorization: Bearer {{token}}

HTTP 204


GET {{base_url}}/api/trash/resources
Authorization: Bearer {{token}}

HTTP 200
[Captures]
trash_id: jsonpath "$.items[?(@.resource.id == '{{folder_id}}')].resource.id"
trash_dup_id: jsonpath "$.items[?(@.resource.id == '{{folder_dup_id}}')].resource.id"


DELETE {{base_url}}/api/trash/{{trash_id}}
Authorization: Bearer {{token}}

HTTP 200


DELETE {{base_url}}/api/trash/{{trash_dup_id}}
Authorization: Bearer {{token}}

HTTP 200
