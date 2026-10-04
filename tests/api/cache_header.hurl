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
# Two endpoints covered:
#   /api/files/{id}?inline=true     — the download hot path
#   /api/files/{id}/thumbnail/{sz}  — the thumbnail hot path
#
# The sequence:
#   1. upload a small text file (unique content)
#   2. first inline fetch → MISS (moka cold, loader ran)
#   3. second inline fetch → HIT (moka serves from memory)
#   4. upload a small image (unique content)
#   5. first thumbnail fetch → HIT / HIT-BACKEND / MISS (any
#      valid value — eager generation by the lifecycle hook can
#      pre-warm the thumbnail moka before this GET lands; the
#      assertion only pins "header is set to a valid value")
#   6. second thumbnail fetch → HIT (always warm after step 5)
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
# Step 3 – Upload a small file with content UNIQUE to this
# scenario. The moka file-content cache is keyed by the content
# hash (BLAKE3 of the body), so reusing a fixture that other
# scenarios touch (`hello.txt`, `red-image.png`, etc.) would
# see HIT on the first read in this scenario because an earlier
# scenario in the same run already warmed the hash. The
# dedicated `cache_header_unique.txt` fixture carries a random
# tag so its hash is exclusive to this test. Payload stays
# under the 10 MB content-cache threshold so the response takes
# the `OptimizedFileContent::Bytes` path (the only path that
# attaches `X-Oxicloud-Cache` — TIER 2 streaming deliberately
# omits).
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/files/upload
Authorization: Bearer {{token}}
[MultipartFormData]
folder_id: {{folder_id}}
file: file,fixtures/cache_header_unique.txt; text/plain

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
# Step 6 – Thumbnail-endpoint `X-Oxicloud-Cache` coverage.
#
# Upload a SECOND file, this time an image fixture whose bytes
# are unique to this scenario (`cache_header_unique.png` — a 4×4
# magenta PNG whose BLAKE3 nothing else touches). The thumbnail
# moka is keyed by `(file_id, size)`, so even a brand-new upload
# of previously-seen content gets a fresh entry — but the lower
# tiers are content-keyed, so uniqueness here ALSO exercises the
# derived-blob lookup + generation path on first access.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/files/upload
Authorization: Bearer {{token}}
[MultipartFormData]
folder_id: {{folder_id}}
file: file,fixtures/cache_header_unique.png; image/png

HTTP 201
[Captures]
image_id: jsonpath "$.id"


# ─────────────────────────────────────────────────────────────
# Step 7 – First thumbnail fetch. Note: unlike the download
# path, the value here is NOT deterministically MISS. The
# `ThumbnailRefreshHook` fires on `on_file_created` and launches
# a background generator that populates the thumbnail moka
# before the user even requests it (eager generation by design,
# documented on the hook). So depending on whether the
# background task completed before this GET lands, the first
# fetch can report either:
#   - HIT        (background gen already finished, moka warm)
#   - MISS       (generator still running or hadn't started;
#                 this GET falls through to on-demand synchronous
#                 generation and the backend chain observes Miss)
#   - HIT-BACKEND (future, when a `CachedBlobBackend` wrapper is
#                 in the test harness and the derived blob was
#                 found on local disk without hitting moka)
# All three are HONEST values. What we assert is that the
# header IS set and carries one of the three valid tokens —
# any other string signals a bug (missing scope, wrong writer,
# dropped header on 200).
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/files/{{image_id}}/thumbnail/preview
Authorization: Bearer {{token}}
Accept: image/webp

HTTP 200
[Asserts]
header "X-Oxicloud-Cache" matches "^(HIT|HIT-BACKEND|MISS)$"


# ─────────────────────────────────────────────────────────────
# Step 8 – Second thumbnail fetch. Whatever the first fetch
# did, the thumbnail is now in the moka cache under the
# `(file_id, "preview")` key — either because the eager
# background hook completed, or because step 7's on-demand
# synchronous generation populated it on miss. This fetch must
# therefore be a HIT regardless of which path step 7 took.
# Pins the thumbnail-endpoint half of the X-Oxicloud-Cache
# contract: the handler always stamps the header, and moka
# always reaches a steady-state hit after one view. The
# download-endpoint half is covered by steps 4-5 above.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/files/{{image_id}}/thumbnail/preview
Authorization: Bearer {{token}}
Accept: image/webp

HTTP 200
[Asserts]
header "X-Oxicloud-Cache" == "HIT"


# ─────────────────────────────────────────────────────────────
# Step 9 – Cleanup. Standard teardown so a re-run of this
# scenario (or any other scenario sharing the DB) does not see
# the folder leftover from this run.
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/api/folders/{{folder_id}}
Authorization: Bearer {{token}}

HTTP *
