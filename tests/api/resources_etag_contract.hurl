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
# Scenarios E and F are the exception, and they exist because of a bug this file's
# own "count-changing on purpose" rule walked straight past: favouriting a
# row changes NEITHER the row count NOR any `*_at` column, so the listing
# body changed while every ETag input stayed put. The next load got a 304
# and the browser re-served the pre-star body — the star vanished on
# refresh while the row sat in `auth.user_favorites`. So E and F mutate a
# FLAG only — exactly the shape the other scenarios avoid. E does it with
# a favourite, F with a grant (`is_shared`): two unrelated write paths
# that both have to reach the page identity.
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
# Scenario E — a flag-only mutation must still flip the ETag
# =============================================================
# THE regression. Starring a row rewrites `is_favorite` in the body and
# touches nothing else: no new row, no deleted row, no `modified_at`, no
# `accessed_at`. Every input to `cursor.rs::weak_etag` was therefore
# unchanged, the server answered the next load with `304 Not Modified`,
# and the browser (`Cache-Control: private, no-cache` → revalidate, keep
# the body) re-served the page as it was before the star. The POST had
# succeeded; the refresh showed it undone, which read as "the server
# never recorded it".
#
# Fixed by folding the per-row `is_favorite` / `is_shared` pair into the
# freshness signal — `etag::mix_row_state`. These steps pin BOTH halves:
# the identity must move (200, new ETag) AND the body must carry the new
# flag. Asserting only the 200 would pass on a server that flipped the
# ETag per request; asserting only the flag would pass on the stale-304
# server the moment a cache is not in play.

# E1 — A fresh file in the work folder. Scenario C trashed and purged the
#      first one, so this folder is empty again and the listing below has
#      exactly one row — positional asserts are safe.
POST {{base_url}}/api/files/upload
Authorization: Bearer {{token}}
[MultipartFormData]
folder_id: {{work_folder_id}}
file: file,fixtures/dedup-test.jpg; image/jpeg

HTTP 201
[Captures]
flag_file_id: jsonpath "$.id"


# E2 — Baseline folder listing: one row, not favourited.
GET {{base_url}}/api/folders/{{work_folder_id}}/resources
Authorization: Bearer {{token}}

HTTP 200
[Captures]
flag_etag_1: header "ETag"
[Asserts]
header "ETag" exists
jsonpath "$.items[0].resource.id" == "{{flag_file_id}}"
jsonpath "$.items[0].resource.is_favorite" == false


# E3 — Nothing changed yet, so the identity must hold. This is the step
#      that makes E4/E5 meaningful: it proves the 304 path is live on
#      this listing, so a 200 in E5 can only come from the mutation.
GET {{base_url}}/api/folders/{{work_folder_id}}/resources
Authorization: Bearer {{token}}
If-None-Match: {{flag_etag_1}}

HTTP 304


# E4 — Favourite it. No row added, no row removed, no timestamp moved.
POST {{base_url}}/api/favorites/file/{{flag_file_id}}
Authorization: Bearer {{token}}
Content-Type: application/json
{}

HTTP 201


# E5 — The pre-star ETag must MISS, and the fresh body must show the star.
#      Before the fix this returned 304 with an empty body and `/files`
#      kept painting an unstarred row.
GET {{base_url}}/api/folders/{{work_folder_id}}/resources
Authorization: Bearer {{token}}
If-None-Match: {{flag_etag_1}}

HTTP 200
[Asserts]
header "ETag" exists
header "ETag" != "{{flag_etag_1}}"
jsonpath "$.items[0].resource.id" == "{{flag_file_id}}"
jsonpath "$.items[0].resource.is_favorite" == true


# E6 — Same property on `/recent`, which is where this was reported. The
#      ledger holds other scenarios' rows too, so this one filters by id
#      instead of indexing. A one-element filter result collapses to a
#      bare scalar in Hurl's JSONPath, which is why these are equality
#      asserts and not `count` guards — and a filter that matches nothing
#      yields "no value", which fails every predicate, so a vanished row
#      goes red rather than passing vacuously.
POST {{base_url}}/api/recent/file/{{flag_file_id}}
Authorization: Bearer {{token}}

HTTP 200


GET {{base_url}}/api/recent/resources
Authorization: Bearer {{token}}

HTTP 200
[Captures]
flag_rec_etag_1: header "ETag"
[Asserts]
header "ETag" exists
jsonpath "$.items[?(@.resource.id == '{{flag_file_id}}')].resource.is_favorite" == true


GET {{base_url}}/api/recent/resources
Authorization: Bearer {{token}}
If-None-Match: {{flag_rec_etag_1}}

HTTP 304


# E7 — Un-favourite: the row stays in the ledger (un-starring is not a
#      removal here, unlike on `/favorites`), so again nothing but the
#      flag moves. The old ETag must miss and the body must show it.
DELETE {{base_url}}/api/favorites/file/{{flag_file_id}}
Authorization: Bearer {{token}}

HTTP 200


GET {{base_url}}/api/recent/resources
Authorization: Bearer {{token}}
If-None-Match: {{flag_rec_etag_1}}

HTTP 200
[Captures]
flag_rec_etag_2: header "ETag"
[Asserts]
header "ETag" exists
header "ETag" != "{{flag_rec_etag_1}}"
jsonpath "$.items[?(@.resource.id == '{{flag_file_id}}')].resource.is_favorite" == false


# E8 — …and back. This is the exact flow that was reported broken: star a
#      row on `/recent`, reload, find it unstarred. It needs its own leg
#      because the two directions are different code on both sides — a
#      different endpoint (`POST` vs `DELETE`), and a different outcome of
#      the SQL `EXISTS` that computes `is_favorite`. E7 proved the
#      true→false direction; without this step the false→true one — the
#      reported one — would be pinned on `/files` only.
#
#      Each of these 200s carries a FRESH body (the `If-None-Match` missed),
#      so the asserted flag is the whole chain end to end: the favourites
#      write → `auth.user_favorites` → the recent query's `EXISTS` →
#      `RecentResourceItemDto` → the JSON the SPA reads.
POST {{base_url}}/api/favorites/file/{{flag_file_id}}
Authorization: Bearer {{token}}
Content-Type: application/json
{}

HTTP 201


GET {{base_url}}/api/recent/resources
Authorization: Bearer {{token}}
If-None-Match: {{flag_rec_etag_2}}

HTTP 200
[Asserts]
header "ETag" exists
header "ETag" != "{{flag_rec_etag_2}}"
jsonpath "$.items[?(@.resource.id == '{{flag_file_id}}')].resource.is_favorite" == true


# =============================================================
# Scenario F — the other flag: is_shared
# =============================================================
# `is_shared` is `EXISTS (SELECT 1 FROM storage.role_grants WHERE
# resource_id = … )`, so granting someone a role flips it with no row
# added to the listing and no timestamp moved — the same hole as the
# star, reached through a completely different write path. It needs its
# own pin because nothing about the favourites fix proves the share half:
# one `mix_row_state` argument could be dropped and every other scenario
# in this file would stay green.
#
# A grant needs a subject, so this creates one user and removes it in
# teardown.

# F1 — The grantee.
POST {{base_url}}/api/admin/users
Authorization: Bearer {{token}}
Content-Type: application/json
{ "username": "etagmate", "password": "EtagMatePassword1!", "email": "etagmate@example.com", "role": "user" }

HTTP 201
[Captures]
mate_user_id: jsonpath "$.user.id"


# F2 — Baseline: the file is not shared with anyone.
GET {{base_url}}/api/folders/{{work_folder_id}}/resources
Authorization: Bearer {{token}}

HTTP 200
[Captures]
share_etag_1: header "ETag"
[Asserts]
jsonpath "$.items[0].resource.id" == "{{flag_file_id}}"
jsonpath "$.items[0].resource.is_shared" == false


# F3 — Identity holds while nothing changes.
GET {{base_url}}/api/folders/{{work_folder_id}}/resources
Authorization: Bearer {{token}}
If-None-Match: {{share_etag_1}}

HTTP 304


# F4 — Grant Viewer on the file itself (not the folder): one
#      `storage.role_grants` row, nothing else in the listing moves.
POST {{base_url}}/api/grants
Authorization: Bearer {{token}}
Content-Type: application/json
{
  "subject":  { "type": "user", "id": "{{mate_user_id}}" },
  "resource": { "type": "file", "id": "{{flag_file_id}}" },
  "role": "viewer"
}

HTTP 201
[Captures]
mate_grant_id: jsonpath "$.grants[0].id"


# F5 — Old ETag must miss, and the fresh body must show the share.
GET {{base_url}}/api/folders/{{work_folder_id}}/resources
Authorization: Bearer {{token}}
If-None-Match: {{share_etag_1}}

HTTP 200
[Captures]
share_etag_2: header "ETag"
[Asserts]
header "ETag" != "{{share_etag_1}}"
jsonpath "$.items[0].resource.is_shared" == true


# F6 — And back: revoking has to be visible on the same terms.
DELETE {{base_url}}/api/grants/{{mate_grant_id}}
Authorization: Bearer {{token}}

HTTP 204


GET {{base_url}}/api/folders/{{work_folder_id}}/resources
Authorization: Bearer {{token}}
If-None-Match: {{share_etag_2}}

HTTP 200
[Asserts]
header "ETag" != "{{share_etag_2}}"
jsonpath "$.items[0].resource.is_shared" == false


# =============================================================
# Teardown — drop the favourite E8 left behind, delete the two folders
# this test created, then purge both from trash. The suite shares its DB
# with every other hurl file; leftover rows break the home-listing
# name-order assertions in favorites.hurl / recent.hurl.
#
# The favourite goes first and explicitly: `auth.user_favorites.item_id`
# is TEXT with no foreign key, so purging the file does NOT remove its
# favourite row — it would survive as a dangling row and inflate every
# later `/favorites` count in the run.
# =============================================================
DELETE {{base_url}}/api/favorites/file/{{flag_file_id}}
Authorization: Bearer {{token}}

HTTP 200


DELETE {{base_url}}/api/admin/users/{{mate_user_id}}
Authorization: Bearer {{token}}

HTTP 200


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
