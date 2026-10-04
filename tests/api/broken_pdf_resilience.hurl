# =============================================================
# OxiCloud – resilience to a malformed PDF upload
#
# Server-side defense already exists: `text_extractor::extract_pdf`
# wraps `pdf_extract::extract_text_from_mem` in `catch_unwind` and
# a corresponding unit test (`corrupt_pdf_fails_terminally_instead_of_panicking`)
# pins "panic → ExtractedText::Failed, not process crash". This
# scenario is the integration pin on top: a malformed PDF
# traversing the whole upload → store → retrieve → thumbnail-probe
# chain must not take the server down, poison the file row, or
# break downstream reads.
#
# What's exercised:
#   - Upload path: multipart POST with a byte sequence that
#     starts with `%PDF-1.4` but is truncated/malformed deeper
#     in. The CDC chunk store is content-blind — any bytes are
#     storable — so the upload must succeed.
#   - Inline fetch: the stored bytes come back verbatim.
#   - Thumbnail endpoint: a PDF isn't an image or a video, so
#     the handler returns 204 (`is_supported_image` + `is_supported_video`
#     both false — not an error, "no thumbnail available").
#     Must not try to decode the PDF content.
#   - Search-index text extraction (fires async off the lifecycle
#     hook): the extractor's `catch_unwind` guard swallows any
#     pdf-extract panic and records `ExtractedText::Failed`. No
#     visible failure here, but if the guard ever regresses the
#     server will crash during this scenario rather than much
#     later in production.
#   - Name-based search: content extraction failing MUST NOT
#     prevent FILENAME indexing. The file is renamed to a
#     unique needle (`broken-pdf-needle-xyzzy-aaa.pdf`), and a
#     subsequent search must surface it. If the extractor
#     panic ever started propagating up and aborting the
#     indexing path (instead of being caught per-document),
#     this search would return 0 results and the test would
#     flag that regression immediately.
#
# Fixture: `tests/fixtures/broken.pdf` — 430 bytes, starts with a
# valid PDF-1.4 header, structurally malformed thereafter.
# Preserved as a known-bad input so the resilience path has a
# stable regression target.
#
# Prerequisites: setup.hurl must have run (admin user exists).
#
# Run:
#   hurl --variables-file tests/api/test.env --file-root tests \
#        --test tests/api/broken_pdf_resilience.hurl
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
  "name": "hurl-broken-pdf"
}

HTTP 201
[Captures]
folder_id: jsonpath "$.id"


# ─────────────────────────────────────────────────────────────
# Step 3 – Upload the malformed PDF. The upload path is bytes-
# transparent (CDC + content-address, no format validation), so
# this must succeed with 201. If this starts failing with a 4xx,
# someone added upload-time content inspection that would also
# reject every other opaque binary; worth investigating.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/files/upload
Authorization: Bearer {{token}}
[MultipartFormData]
folder_id: {{folder_id}}
file: file,fixtures/broken.pdf; application/pdf

HTTP 201
[Captures]
file_id: jsonpath "$.id"
file_size: jsonpath "$.size"
[Asserts]
jsonpath "$.mime_type" == "application/pdf"


# ─────────────────────────────────────────────────────────────
# Step 3b – Rename to a filename containing a unique needle that
# nothing else in the suite's upload set uses. The search
# assertion below keys off this string; a shared fixture name
# would collide with other scenarios' files sharing the same
# content hash or name token.
# ─────────────────────────────────────────────────────────────
PUT {{base_url}}/api/files/{{file_id}}/rename
Authorization: Bearer {{token}}
Content-Type: application/json
{ "name": "broken-pdf-needle-xyzzy-aaa.pdf" }

HTTP 200


# ─────────────────────────────────────────────────────────────
# Step 4 – Inline fetch. Bytes come back verbatim via the
# content-addressable store — no transformation, no decode. Must
# return 200 with `application/pdf` content-type. The X-Oxicloud-
# Cache header is omitted on first fetch assertions (timing
# depends on cache tier state; `cache_header.hurl` is the pin
# for the header contract).
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/files/{{file_id}}?inline=true
Authorization: Bearer {{token}}

HTTP 200
[Asserts]
header "Content-Type" == "application/pdf"
header "Content-Length" == "{{file_size}}"


# ─────────────────────────────────────────────────────────────
# Step 5 – Thumbnail probe. A PDF isn't matched by
# `is_supported_image` or `is_supported_video`, so the handler
# returns 204 (No Content) rather than running any decoder on
# the malformed bytes. If this ever returns 200 for a PDF, the
# thumbnail service grew PDF rendering — great, but then this
# test needs to be refined to assert that the rendering handled
# the malformed input gracefully (likely 204 or 500 with no
# process crash).
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/files/{{file_id}}/thumbnail/preview
Authorization: Bearer {{token}}
Accept: image/webp

HTTP 204


# ─────────────────────────────────────────────────────────────
# Step 6 – Second inline fetch, after any async hooks (text
# extraction, lifecycle processing) have had a chance to touch
# the row. If an extractor panic poisoned something DB-side, a
# subsequent read would error out. Server must still return the
# bytes.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/files/{{file_id}}?inline=true
Authorization: Bearer {{token}}

HTTP 200
[Asserts]
header "Content-Type" == "application/pdf"
header "Content-Length" == "{{file_size}}"


# ─────────────────────────────────────────────────────────────
# Step 7 – (dropped) — `/api/files/{id}/metadata` returns EXIF
# image metadata, not the file DTO, so a PDF response there is
# just `{file_id, message: "No EXIF metadata available"}`. The
# row-survives-async-hooks property we wanted to pin is already
# covered by step 6: the second inline fetch returns the bytes
# with `Content-Type: application/pdf` after any lifecycle
# hooks have run. Nothing more to assert here without listing
# the folder.
#
# ─────────────────────────────────────────────────────────────
# Step 8 – Name-based search. The real resilience assertion:
# even though the PDF content extractor rejects the malformed
# body (via the `catch_unwind` guard around `pdf_extract`), the
# FILENAME path through the search index must still work.
# Content-extraction failure MUST NOT prevent name indexing;
# otherwise a broken-document upload would silently become
# invisible in search.
#
# `[Options] delay: 2500ms` matches the pattern in
# `search_basic.hurl`: the content-index worker is async with a
# default `OXICLOUD_CONTENT_INDEX_FLUSH_MS = 1500ms`; a 2.5-
# second wait drains it comfortably on CI. Raise if a slower
# machine flakes.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/search?query=broken-pdf-needle-xyzzy
Authorization: Bearer {{token}}
[Options]
delay: 2500ms

HTTP 200
[Asserts]
jsonpath "$.items" count >= 1
body contains "{{file_id}}"


# ─────────────────────────────────────────────────────────────
# Step 9 – Cleanup. Standard teardown so re-runs or sibling
# scenarios don't see this folder lingering.
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/api/folders/{{folder_id}}
Authorization: Bearer {{token}}

HTTP *
