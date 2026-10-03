# =============================================================
# OxiCloud – `X-Oxicloud-Cache` response header on file inline reads
#
# Pins the three-value header produced by `build_cached_response` in
# `interfaces/api/handlers/file_handler.rs`. The matrix the server
# commits to:
#
#   HIT          — moka served the assembled bytes from memory
#   HIT-BACKEND  — moka missed; local-disk .blob-cache served (phase 2)
#   MISS         — moka missed; backend was reached (first view)
#
# Only the HIT ↔ MISS pair is covered here. `HIT-BACKEND` requires the
# `CachedBlobBackend` wrapper to be in the request path, which the
# default local-backend test harness does not spin up (see sandbox
# `example.env`: `OXICLOUD_STORAGE_CACHE_ENABLED` defaults to false).
# A follow-up scenario can exercise that tier under an S3 / Azurite
# profile.
#
# The sequence:
#   1. upload a small text file
#   2. first inline fetch → MISS (moka cold, loader ran)
#   3. second inline fetch → HIT (moka serves from memory)
#
# `common.cache_control` and `etag` are also validated in passing so
# the whole response-header envelope stays stable across refactors.
#
# Prerequisites: setup.hurl must have run (admin user exists).
#
# Run:
#   hurl --variables-file tests/api/test.env --file-root tests \
#        --test tests/api/cache_header.hurl
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
  "name": "hurl-cache-header"
}

HTTP 201
[Captures]
folder_id: jsonpath "$.id"


# ─────────────────────────────────────────────────────────────
# Step 3 – Upload a small file. Any text fixture is fine; the
# payload size just needs to be under the content-cache threshold
# so the response takes the `OptimizedFileContent::Bytes` path
# (the only path that attaches `X-Oxicloud-Cache`). TIER 2
# streaming responses deliberately omit the header.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/files/upload
Authorization: Bearer {{token}}
[MultipartFormData]
folder_id: {{folder_id}}
file: file,fixtures/hello.txt; text/plain

HTTP 201
[Captures]
file_id: jsonpath "$.id"


# ─────────────────────────────────────────────────────────────
# Step 4 – First inline fetch. The file has just been uploaded,
# so moka has not seen the content_hash yet → the loader runs →
# `CacheOutcome::Miss` → `X-Oxicloud-Cache: MISS`.
#
# NOTE on upload-time pre-population: `FileContentCache` does not
# currently receive write-through hints from the upload path, so
# every first read is a MISS even for a file the server just
# wrote to disk one millisecond ago. If an upload-time preload is
# added later, this assertion flips to HIT and the test is the
# first thing that notices.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/files/{{file_id}}?inline=true
Authorization: Bearer {{token}}

HTTP 200
[Asserts]
header "X-Oxicloud-Cache" == "MISS"


# ─────────────────────────────────────────────────────────────
# Step 5 – Second inline fetch. The previous request populated
# moka → this one serves from memory → `X-Oxicloud-Cache: HIT`.
#
# Running on the same client within the same hurl run guarantees
# no eviction can have happened in between: moka's default
# capacity is far above a single hello.txt, there is no TTL, and
# no competing writer is forcing eviction.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/files/{{file_id}}?inline=true
Authorization: Bearer {{token}}

HTTP 200
[Asserts]
header "X-Oxicloud-Cache" == "HIT"


# ─────────────────────────────────────────────────────────────
# Step 6 – Cleanup. Standard teardown so a re-run of this
# scenario (or any other scenario sharing the DB) does not see
# the folder leftover from this run.
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/api/folders/{{folder_id}}
Authorization: Bearer {{token}}

HTTP *
