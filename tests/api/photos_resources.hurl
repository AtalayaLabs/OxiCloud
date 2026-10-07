# =============================================================
# OxiCloud – GET /api/photos/resources (§1 of the photos → /resources migration)
# =============================================================
# Pins the normalized envelope shape, the item-level photo signals
# (sort_date, captured_at, orientation, has_gps, width, height), the
# opaque-cursor validation surface (400 on malformed / mismatched
# axis / unsupported order_by), and reuses the §0 multi-grant
# fixture to prove the new endpoint also honors the one-row-per-
# accessible-drive invariant.
#
# Scenarios 1-9 of docs/plan/photos-resources-migration.md's test
# matrix map to steps below. Scenarios 6 (order_by cursor mismatch),
# 8-9 (ETag revalidation) are deferred — scenario 6 lands with §3
# (configurable sort axis), 8-9 land with §2 (shared ETag engine).
# =============================================================


# ─────────────────────────────────────────────────────────────
# Step 1 — Admin login (alice).
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "{{username}}", "password": "{{password}}" }

HTTP 200
[Captures]
alice_token: jsonpath "$.access_token"


# ─────────────────────────────────────────────────────────────
# Step 2 — Home folder id (upload target). Alice's personal
#          drive already has include_in_photo_index=true at
#          creation (per-user-default; backfill migration
#          20260901000000), so uploads here surface in /photos.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/folders
Authorization: Bearer {{alice_token}}

HTTP 200
[Captures]
home_folder_id: jsonpath "$[0].id"


# ─────────────────────────────────────────────────────────────
# Step 3 — Baseline empty-gallery semantics (scenario 1).
#          Clean fixture: no photo for this test user yet;
#          endpoint returns 200 with items=[] and no next_cursor.
#          Match the suite's "empty state is not 404" rule.
#
#          NOTE: the suite's shared DB may carry photos uploaded
#          by prior hurl runs on alice's personal drive, so we
#          only assert the ENVELOPE shape here, not count==0.
#          Fresh content assertions land in step 4.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/photos/resources?limit=50
Authorization: Bearer {{alice_token}}

HTTP 200
[Asserts]
jsonpath "$.items" isCollection
# `next_cursor` is omitted (not null) when there is no next page.


# ─────────────────────────────────────────────────────────────
# Step 4 — Upload a JPEG with real EXIF and confirm the item-
#          level fields (scenario 2). Reuses `oxicloud-logo.jpg`
#          already used by photos_etag.hurl / photos_multigrant
#          — content-addressed blob stays shared.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/files/upload
Authorization: Bearer {{alice_token}}
[MultipartFormData]
folder_id: {{home_folder_id}}
file: file,fixtures/oxicloud-logo.jpg; image/jpeg

HTTP 201
[Captures]
photo_file_id: jsonpath "$.id"


GET {{base_url}}/api/photos/resources?limit=50
Authorization: Bearer {{alice_token}}

HTTP 200
[Asserts]
jsonpath "$.items" isCollection
# The uploaded file must be the newest entry — ORDER BY
# media_sort_date DESC pins it at position 0.
jsonpath "$.items[0].resource.id"      == "{{photo_file_id}}"
jsonpath "$.items[0].resource_type"    == "file"
# Always-present item-level fields. `width`, `height`, `captured_at`,
# `orientation` are Option<T> with `skip_serializing_if = none` —
# they only appear once the async post-upload metadata job has
# written to `storage.file_metadata`, and this scenario intentionally
# does not block on it. `sort_date` and `has_gps` are populated at
# the SELECT level (not from fm columns alone) and are always on
# the wire.
jsonpath "$.items[0].sort_date"        exists
jsonpath "$.items[0].has_gps"          exists
# Deliberately NOT disclosed on the listing (scenario 5): raw
# coords on a per-item basis. These paths are already absent today
# because FileDto doesn't carry latitude/longitude at all, but
# pinning them here fails loud if that ever changes.
jsonpath "$.items[0].resource.latitude"  not exists
jsonpath "$.items[0].resource.longitude" not exists


# ─────────────────────────────────────────────────────────────
# Step 4b — §2 ETag revalidation (scenarios 8-9 of the plan).
#          The §2 shared engine stamps `ETag` + `Cache-Control:
#          private, no-cache` on every /resources response and
#          honors `If-None-Match` with an empty 304. First GET
#          captures the ETag; second GET with that same token
#          returns 304 + empty body; a stale/mismatched token
#          still gets the full 200.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/photos/resources?limit=50
Authorization: Bearer {{alice_token}}

HTTP 200
[Captures]
photos_resources_etag: header "ETag"
[Asserts]
header "ETag"          exists
header "Cache-Control" contains "no-cache"


GET {{base_url}}/api/photos/resources?limit=50
Authorization: Bearer {{alice_token}}
If-None-Match: {{photos_resources_etag}}

HTTP 304
[Asserts]
header "ETag" == "{{photos_resources_etag}}"
bytes count == 0


GET {{base_url}}/api/photos/resources?limit=50
Authorization: Bearer {{alice_token}}
If-None-Match: "stale-etag-does-not-match"

HTTP 200
[Asserts]
jsonpath "$.items" isCollection


# ─────────────────────────────────────────────────────────────
# Step 4c — ETag varies across filter axes (regression for §6
#          and §6b). The `weak_etag` hash takes `(cursor, limit,
#          fresh_signal, row_count, next_cursor)`; filter axes
#          are not named directly in the hash, so correctness
#          relies on `fresh_signal`/`row_count`/`next_cursor`
#          differing across filter combinations. Pin that so a
#          future refactor can't quietly make an If-None-Match
#          from one URL false-304 against a differently-filtered
#          URL (which would show the viewer the wrong page's
#          cached body).
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/photos/resources?limit=50&kind=photo
Authorization: Bearer {{alice_token}}

HTTP 200
[Captures]
etag_kind_photo: header "ETag"
[Asserts]
header "ETag" exists


GET {{base_url}}/api/photos/resources?limit=50&kind=video
Authorization: Bearer {{alice_token}}

HTTP 200
[Captures]
etag_kind_video: header "ETag"
[Asserts]
header "ETag" exists
# The two filtered URLs cover disjoint content (fixtures have 0
# videos, ≥1 photo) and must therefore surface distinct ETags —
# their equality would mean the server collapsed two different
# page bodies to one revalidation key, which is the false-304
# bug this test exists to prevent.
header "ETag" != "{{etag_kind_photo}}"


# Revalidation stays correct per URL: the photo ETag matches
# itself (304) but is treated as stale against the video URL
# (200 with the video body — zero items).
GET {{base_url}}/api/photos/resources?limit=50&kind=photo
Authorization: Bearer {{alice_token}}
If-None-Match: {{etag_kind_photo}}

HTTP 304


GET {{base_url}}/api/photos/resources?limit=50&kind=video
Authorization: Bearer {{alice_token}}
If-None-Match: {{etag_kind_photo}}

HTTP 200
[Asserts]
jsonpath "$.items" count == 0


# ─────────────────────────────────────────────────────────────
# Step 5 — Pagination (scenario 4 abbreviated). Needs ≥ 2 photos
#          for `limit=1` to produce an over-fetch — the suite's
#          DB may be fresh with only the step-4 upload on alice's
#          personal drive, so upload a second row explicitly.
#          Use a distinct fixture (fixtures/many/img-00000.png
#          vs step 4's oxicloud-logo.jpg) so there is no filename
#          collision in the home folder; each row is a bona-fide
#          separate file.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/files/upload
Authorization: Bearer {{alice_token}}
[MultipartFormData]
folder_id: {{home_folder_id}}
file: file,fixtures/many/img-00000.png; image/png

HTTP 201
[Captures]
photo_file_id_2: jsonpath "$.id"


GET {{base_url}}/api/photos/resources?limit=1
Authorization: Bearer {{alice_token}}

HTTP 200
[Captures]
page1_cursor: jsonpath "$.next_cursor"
page1_item_id: jsonpath "$.items[0].resource.id"
[Asserts]
jsonpath "$.items"       count == 1
jsonpath "$.next_cursor" exists


# Cursor round-trip: feed the next_cursor back and the row that
# comes out MUST be a different file. The two uploads share the
# same media_sort_date granularity (both bucketed at the current
# clock second), so the cursor's (sort_value, file_id) tie-break
# is what keeps the two pages from skipping or re-emitting — and
# the id difference is what proves the tie-break fires.
GET {{base_url}}/api/photos/resources?limit=1&cursor={{page1_cursor}}
Authorization: Bearer {{alice_token}}

HTTP 200
[Asserts]
jsonpath "$.items"                count == 1
jsonpath "$.items[0].resource.id" != "{{page1_item_id}}"


# ─────────────────────────────────────────────────────────────
# Step 6 — Opaque-cursor validation surface (new in §1): a
#          malformed cursor must 400, not silently fall back
#          to page 1. Pagination correctness over "forgiving"
#          parsing.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/photos/resources?limit=50&cursor=not-a-valid-cursor
Authorization: Bearer {{alice_token}}

HTTP 400
[Asserts]
jsonpath "$.error_type" == "bad_request"


# ─────────────────────────────────────────────────────────────
# Step 7 — Unsupported sort axis. §3 reserves `created_at` but
#          doesn't accept it today — the handler returns 400
#          with error_type=bad_request. Mislabeling the default
#          axis would silently paginate against the wrong column
#          once §3 lands, so we fail loud now.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/photos/resources?order_by=created_at
Authorization: Bearer {{alice_token}}

HTTP 400
[Asserts]
jsonpath "$.error_type" == "bad_request"


# ─────────────────────────────────────────────────────────────
# Step 8 — Multi-grant dedup regression on the NEW endpoint
#          (mirrors photos_multigrant_dedup.hurl, which pins
#          the same property on /api/photos). Builds the exact
#          overlap: fresh user mia with BOTH a direct user
#          grant AND a group grant on the same shared drive.
#          Resource-shaped response: one item, not two.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/admin/users
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{ "username": "mia_resources", "password": "MiaPassword1!", "email": "mia_resources@example.com", "role": "user" }

HTTP 201
[Captures]
mia_user_id: jsonpath "$.user.id"

POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "{{username}}", "password": "{{password}}" }

HTTP 200
[Captures]
alice_user_id: jsonpath "$.user.full.user.id"

POST {{base_url}}/api/drives
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{
  "kind": "shared",
  "name": "multigrant-resources-hurl",
  "owner": { "type": "user", "id": "{{alice_user_id}}" }
}

HTTP 201
[Captures]
drive_id: jsonpath "$.id"
root_folder_id: jsonpath "$.root_folder_id"


PATCH {{base_url}}/api/drives/{{drive_id}}/policies
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{ "include_in_photo_index": true }

HTTP 200


POST {{base_url}}/api/files/upload
Authorization: Bearer {{alice_token}}
[MultipartFormData]
folder_id: {{root_folder_id}}
file: file,fixtures/oxicloud-logo.jpg; image/jpeg

HTTP 201
[Captures]
shared_photo_id: jsonpath "$.id"


POST {{base_url}}/api/groups
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{ "name": "grp-multigrant-resources-hurl", "description": "overlap for /resources" }

HTTP 201
[Captures]
grp_id: jsonpath "$.id"


POST {{base_url}}/api/groups/{{grp_id}}/members
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{ "user_id": "{{mia_user_id}}" }

HTTP 201


POST {{base_url}}/api/drives/{{drive_id}}/members
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{ "subject": { "type": "user", "id": "{{mia_user_id}}" }, "role": "viewer" }

HTTP 201


POST {{base_url}}/api/drives/{{drive_id}}/members
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{ "subject": { "type": "group", "id": "{{grp_id}}" }, "role": "viewer" }

HTTP 201


POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "mia_resources", "password": "MiaPassword1!" }

HTTP 200
[Captures]
mia_token: jsonpath "$.access_token"


# Mia sees the shared photo exactly once in the envelope, even
# though she reaches the drive via two overlapping grants. Her
# own personal drive has no uploads so items.length == 1.
GET {{base_url}}/api/photos/resources?limit=50
Authorization: Bearer {{mia_token}}

HTTP 200
[Asserts]
jsonpath "$.items"              count == 1
jsonpath "$.items[0].resource.id" == "{{shared_photo_id}}"


# ─────────────────────────────────────────────────────────────
# Step 8b — §6 `?kind=` filter + §6b `?drive_id=` filter.
#          Reuse mia's single-visible-photo fixture — predictable
#          counts.
#            * ?kind=photo        → her 1 image row stays
#            * ?kind=video        → zero (no videos in the fixture)
#            * ?drive_id=<drive>  → her 1 row scoped to that drive
#            * ?drive_id=<nil>    → empty (invisible drive = anti-enum)
#            * compose            → ?drive_id=<drive>&kind=video → zero
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/photos/resources?limit=50&kind=photo
Authorization: Bearer {{mia_token}}

HTTP 200
[Asserts]
jsonpath "$.items" count == 1
jsonpath "$.items[0].resource.id" == "{{shared_photo_id}}"


GET {{base_url}}/api/photos/resources?limit=50&kind=video
Authorization: Bearer {{mia_token}}

HTTP 200
[Asserts]
jsonpath "$.items" count == 0


GET {{base_url}}/api/photos/resources?limit=50&drive_id={{drive_id}}
Authorization: Bearer {{mia_token}}

HTTP 200
[Asserts]
jsonpath "$.items" count == 1
jsonpath "$.items[0].resource.id" == "{{shared_photo_id}}"


# Anti-enum: a drive_id mia cannot see (fully random uuid) returns
# the same empty-page shape as a drive she CAN see that has no
# photos. No 403/404 disclosure difference.
GET {{base_url}}/api/photos/resources?limit=50&drive_id=00000000-0000-0000-0000-000000000000
Authorization: Bearer {{mia_token}}

HTTP 200
[Asserts]
jsonpath "$.items" count == 0


# Compose both axes: drive_id scopes to the shared drive AND
# kind=video excludes all image rows → 0 items. Validates the
# predicates stack cleanly (not an either/or).
GET {{base_url}}/api/photos/resources?limit=50&drive_id={{drive_id}}&kind=video
Authorization: Bearer {{mia_token}}

HTTP 200
[Asserts]
jsonpath "$.items" count == 0


# Cursor axis mismatch — grab a cursor against kind=photo (limit=1
# forces an over-fetch on alice's personal drive which has ≥2
# photo rows now), then try to reuse it under kind=video.
POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "{{username}}", "password": "{{password}}" }

HTTP 200
[Captures]
alice_token_again: jsonpath "$.access_token"


GET {{base_url}}/api/photos/resources?limit=1&kind=photo
Authorization: Bearer {{alice_token_again}}

HTTP 200
[Captures]
kind_scoped_cursor: jsonpath "$.next_cursor"


GET {{base_url}}/api/photos/resources?limit=1&kind=video&cursor={{kind_scoped_cursor}}
Authorization: Bearer {{alice_token_again}}

HTTP 400
[Asserts]
jsonpath "$.error_type" == "bad_request"
jsonpath "$.message"    contains "kind"


# ─────────────────────────────────────────────────────────────
# Step 8c — ?favorite_only=true filter. Alice's drive has at
#          least two photos (step 4 + step 5); favourite BOTH so
#          `limit=1` overfetches and produces a cursor (`limit=1`
#          with only one favourited row would land on the last
#          page immediately — `next_cursor` is omitted on the
#          last page, so there would be nothing to feed into the
#          axis-mismatch check below).
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/favorites/file/{{photo_file_id}}
Authorization: Bearer {{alice_token_again}}

HTTP *


POST {{base_url}}/api/favorites/file/{{photo_file_id_2}}
Authorization: Bearer {{alice_token_again}}

HTTP *


GET {{base_url}}/api/photos/resources?limit=50&favorite_only=true
Authorization: Bearer {{alice_token_again}}

HTTP 200
[Asserts]
jsonpath "$.items" count == 2
jsonpath "$.items[*].resource.id" contains "{{photo_file_id}}"
jsonpath "$.items[*].resource.id" contains "{{photo_file_id_2}}"


# Cursor axis mismatch — a cursor minted with favorite_only=true
# must 400 against a request that drops the flag. Grab a cursor
# from a limit=1 page (two favourites → page 1 has 1 item + a
# next_cursor) and reuse it against the default feed → 400 with
# "favorite_only" in the message.
GET {{base_url}}/api/photos/resources?limit=1&favorite_only=true
Authorization: Bearer {{alice_token_again}}

HTTP 200
[Captures]
favorite_only_cursor: jsonpath "$.next_cursor"
[Asserts]
jsonpath "$.next_cursor" exists


GET {{base_url}}/api/photos/resources?limit=1&cursor={{favorite_only_cursor}}
Authorization: Bearer {{alice_token_again}}

HTTP 400
[Asserts]
jsonpath "$.error_type" == "bad_request"
jsonpath "$.message"    contains "favorite_only"


# Cleanup — un-favourite both so later scenarios/suites don't see
# the starred rows and poison their favourites assertions.
DELETE {{base_url}}/api/favorites/file/{{photo_file_id}}
Authorization: Bearer {{alice_token_again}}

HTTP *


DELETE {{base_url}}/api/favorites/file/{{photo_file_id_2}}
Authorization: Bearer {{alice_token_again}}

HTTP *


# ─────────────────────────────────────────────────────────────
# Step 9 — Teardown. Hurl shares one DB; leaving the overlap
#          fixture around would poison later photo-count
#          assertions (same rule as the §0 regression test).
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/api/drives/{{drive_id}}/members/group/{{grp_id}}
Authorization: Bearer {{alice_token}}

HTTP *

DELETE {{base_url}}/api/drives/{{drive_id}}/members/user/{{mia_user_id}}
Authorization: Bearer {{alice_token}}

HTTP *

DELETE {{base_url}}/api/groups/{{grp_id}}
Authorization: Bearer {{alice_token}}

HTTP *

DELETE {{base_url}}/api/admin/users/{{mia_user_id}}
Authorization: Bearer {{alice_token}}

HTTP *

DELETE {{base_url}}/api/files/{{shared_photo_id}}
Authorization: Bearer {{alice_token}}

HTTP *

DELETE {{base_url}}/api/admin/drives/{{drive_id}}
Authorization: Bearer {{alice_token}}

HTTP *

# Alice's step-4 upload stays — photos_etag.hurl relies on an
# existing photo on alice's personal drive for its own baseline.
# A later test run cleans up through its own teardown.
