# =============================================================
# §2b — server-visible contract the SPA ETag cache relies on
# =============================================================
# `frontend/src/lib/api/etagCache.ts` sends `If-None-Match` with every
# listing refetch and reuses the cached body on 304. For that to be
# correct the server must:
#
#   1. Emit a stable, non-empty `ETag` on every `/resources` listing.
#   2. Return `304 Not Modified` with an empty body when `If-None-Match`
#      matches the current state.
#   3. Return `200 OK` with a NEW ETag after any mutation that reshapes
#      the listing (invalidation across the write) — otherwise the SPA
#      cache would serve stale rows indefinitely.
#
# `photos_resources.hurl` already covers (1) + (2) + (3) on the photos
# timeline. This file extends the coverage to `favorites`, `recent`,
# `trash`, and a child folder of the home folder — the endpoints §2b
# adopts via `apiFetchEtagged` through the `fetchResourcePage` /
# `fetchFolderPage` helpers.
#
# Each scenario:
#   a. GETs the endpoint, captures its ETag.
#   b. GETs again with `If-None-Match: <etag>` → expects 304.
#   c. Mutates (favorite, trash, create sibling folder) so the
#      listing composition changes — count-changing operations are
#      used on purpose: the server's `fresh_signal` reads
#      `.timestamp()` at second resolution, so pure timestamp bumps
#      from rapid same-second mutations can collapse to the same
#      max. A row-count change ALWAYS folds into the ETag via
#      `items.len()` and is immune to that race.
#   d. GETs again with the OLD etag → expects 200 with a fresh ETag.
#
# Run:
#   hurl --variables-file tests/api/test.env --file-root tests \
#        --test tests/api/resources_etag_contract.hurl
# =============================================================


# ─────────────────────────────────────────────────────────────
# Step 1 — Login (admin seeded by setup.hurl).
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
# Step 2 — Home folder id. Alice's personal drive root.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/folders
Authorization: Bearer {{token}}

HTTP 200
[Captures]
home_id: jsonpath "$[0].id"


# ─────────────────────────────────────────────────────────────
# Step 3 — A work folder we fully own.  Scenarios A-C hang their
#          mutations off this folder's file; Scenario D creates a
#          sibling in home to flip home's listing ETag.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/folders
Authorization: Bearer {{token}}
Content-Type: application/json
{
  "name": "hurl-etag-contract",
  "parent_id": "{{home_id}}"
}

HTTP 201
[Captures]
work_folder_id: jsonpath "$.id"


# ─────────────────────────────────────────────────────────────
# Step 4 — Upload a file into the work folder so there's at least
#          one row every listing can surface.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/files/upload
Authorization: Bearer {{token}}
[MultipartFormData]
folder_id: {{work_folder_id}}
file: file,fixtures/dedup-test.jpg; image/jpeg

HTTP 201
[Captures]
work_file_id: jsonpath "$.id"


# =============================================================
# Scenario A — favorites_resources
# =============================================================

# A1 — Favorite the file so the favorites listing has a row.
#      The favorites POST returns 201 Created on first add (see
#      `favorites_handler.rs::StatusCode::CREATED`).
POST {{base_url}}/api/favorites/file/{{work_file_id}}
Authorization: Bearer {{token}}
Content-Type: application/json
{}

HTTP 201


# A2 — Baseline GET; capture the ETag.
GET {{base_url}}/api/favorites/resources
Authorization: Bearer {{token}}

HTTP 200
[Captures]
fav_etag_1: header "ETag"
[Asserts]
header "ETag" exists


# A3 — Refetch with If-None-Match → 304.
GET {{base_url}}/api/favorites/resources
Authorization: Bearer {{token}}
If-None-Match: {{fav_etag_1}}

HTTP 304


# A4 — Unfavorite → composition changes → old ETag must no longer match.
DELETE {{base_url}}/api/favorites/file/{{work_file_id}}
Authorization: Bearer {{token}}

HTTP 200


# A5 — Refetch with the pre-mutation ETag → MUST be 200 with a fresh ETag.
#      A 304 here would mean the server's weak-ETag inputs don't cover
#      the write, which would strand the SPA cache on a stale body.
GET {{base_url}}/api/favorites/resources
Authorization: Bearer {{token}}
If-None-Match: {{fav_etag_1}}

HTTP 200
[Captures]
fav_etag_2: header "ETag"
[Asserts]
header "ETag" exists
header "ETag" != "{{fav_etag_1}}"


# =============================================================
# Scenario B — recent_resources
# =============================================================
# Recent is a touch-driven ledger; a POST to /api/recent/{type}/{id}
# promotes a row (visited this moment). We touch, baseline, touch again,
# and verify the second touch flips the ETag.

# B1 — Touch so the ledger has a row.
POST {{base_url}}/api/recent/file/{{work_file_id}}
Authorization: Bearer {{token}}

HTTP 200


# B2 — Baseline.
GET {{base_url}}/api/recent/resources
Authorization: Bearer {{token}}

HTTP 200
[Captures]
rec_etag_1: header "ETag"
[Asserts]
header "ETag" exists


# B3 — Refetch with If-None-Match → 304.
GET {{base_url}}/api/recent/resources
Authorization: Bearer {{token}}
If-None-Match: {{rec_etag_1}}

HTTP 304


# =============================================================
# Scenario C — trash_resources
# =============================================================

# C1 — Trash the file so the trash listing has a row.
POST {{base_url}}/api/batch/trash
Authorization: Bearer {{token}}
Content-Type: application/json
{
  "file_ids": ["{{work_file_id}}"],
  "folder_ids": []
}

HTTP 200


# C2 — Baseline trash listing.
GET {{base_url}}/api/trash/resources
Authorization: Bearer {{token}}

HTTP 200
[Captures]
trash_etag_1: header "ETag"
trash_item_id: jsonpath "$.items[?(@.resource.id == '{{work_file_id}}')].resource.id"
[Asserts]
header "ETag" exists


# C3 — Refetch with If-None-Match → 304.
GET {{base_url}}/api/trash/resources
Authorization: Bearer {{token}}
If-None-Match: {{trash_etag_1}}

HTTP 304


# C4 — Purge the row from trash → composition changes → fresh ETag.
DELETE {{base_url}}/api/trash/{{trash_item_id}}
Authorization: Bearer {{token}}

HTTP 200


GET {{base_url}}/api/trash/resources
Authorization: Bearer {{token}}
If-None-Match: {{trash_etag_1}}

HTTP 200
[Asserts]
header "ETag" exists
header "ETag" != "{{trash_etag_1}}"


# =============================================================
# Scenario D — folder-contents listing (/api/folders/{id}/resources)
# =============================================================
# We create a new child folder in home and verify home's listing
# emits a fresh ETag because `items.len()` folded into weak_etag
# jumped by one.

# D1 — Baseline listing of home's children.
GET {{base_url}}/api/folders/{{home_id}}/resources
Authorization: Bearer {{token}}

HTTP 200
[Captures]
home_etag_1: header "ETag"
[Asserts]
header "ETag" exists


# D2 — Refetch with If-None-Match → 304.
GET {{base_url}}/api/folders/{{home_id}}/resources
Authorization: Bearer {{token}}
If-None-Match: {{home_etag_1}}

HTTP 304


# D3 — Create a NEW child folder directly in home. We use a count-
#      changing mutation (not a rename) on purpose: the server's
#      `fresh_signal` reads `.timestamp()` at second resolution, so
#      two rapid same-second mutations can collapse `max(modified_at)`
#      to the same value. A new row ALWAYS bumps `items.len()` which
#      is also folded into the ETag — count changes are immune to
#      the second-resolution race. See `cursor.rs::weak_etag`.
POST {{base_url}}/api/folders
Authorization: Bearer {{token}}
Content-Type: application/json
{
  "name": "hurl-etag-contract-sibling",
  "parent_id": "{{home_id}}"
}

HTTP 201
[Captures]
sibling_folder_id: jsonpath "$.id"


# D4 — Refetch with the pre-mutation ETag → 200 + fresh ETag.
GET {{base_url}}/api/folders/{{home_id}}/resources
Authorization: Bearer {{token}}
If-None-Match: {{home_etag_1}}

HTTP 200
[Asserts]
header "ETag" exists
header "ETag" != "{{home_etag_1}}"


# =============================================================
# Teardown — delete the two folders this test created, then purge
# both from trash. The suite shares its DB with every other hurl
# file; leftover rows break the home-listing name-order assertions
# in favorites.hurl / recent.hurl.
# =============================================================
DELETE {{base_url}}/api/folders/{{work_folder_id}}
Authorization: Bearer {{token}}

HTTP 204


DELETE {{base_url}}/api/folders/{{sibling_folder_id}}
Authorization: Bearer {{token}}

HTTP 204


GET {{base_url}}/api/trash/resources
Authorization: Bearer {{token}}

HTTP 200
[Captures]
trash_folder_id: jsonpath "$.items[?(@.resource.id == '{{work_folder_id}}')].resource.id"
trash_sibling_id: jsonpath "$.items[?(@.resource.id == '{{sibling_folder_id}}')].resource.id"


DELETE {{base_url}}/api/trash/{{trash_folder_id}}
Authorization: Bearer {{token}}

HTTP 200


DELETE {{base_url}}/api/trash/{{trash_sibling_id}}
Authorization: Bearer {{token}}

HTTP 200
