# =============================================================
# OxiCloud – Favorites API end-to-end scenario
# =============================================================
# Depends on files-folders.hurl having run first:
#   - home folder exists with sub-folders test1 and test2-renamed
#   - test2-renamed contains hello-renamed.txt
#
# Run:
#   hurl --variables-file tests/api/test.env --test tests/api/favorites.hurl
# =============================================================


# ─────────────────────────────────────────────────────────────
# Step 1 – Login and capture the JWT token
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
[Asserts]
jsonpath "$.access_token" isString


# ─────────────────────────────────────────────────────────────
# Step 2 – No favorites yet
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/favorites/resources
Authorization: Bearer {{token}}

HTTP 200
[Asserts]
jsonpath "$.items" isCollection
jsonpath "$.items" count == 0


# ─────────────────────────────────────────────────────────────
# Step 3 – Discover item IDs from the folder structure
#           Folders are ORDER BY name: test1 ($[0]), test2-renamed ($[1])
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/folders
Authorization: Bearer {{token}}

HTTP 200
[Captures]
home_folder_id: jsonpath "$[0].id"


GET {{base_url}}/api/folders/{{home_folder_id}}/resources?resource_types=folder
Authorization: Bearer {{token}}

HTTP 200
[Captures]
test1_id:  jsonpath "$.items[0].resource.id"
test2_id:  jsonpath "$.items[1].resource.id"
[Asserts]
jsonpath "$.items[0].resource.name" == "test1"
jsonpath "$.items[1].resource.name" == "test2-renamed"


GET {{base_url}}/api/files?folder_id={{test2_id}}
Authorization: Bearer {{token}}

HTTP 200
[Captures]
file_id: jsonpath "$[0].id"
[Asserts]
jsonpath "$[0].name" == "hello-renamed.txt"


# ─────────────────────────────────────────────────────────────
# Step 4 – Add hello-renamed.txt to favorites
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/favorites/file/{{file_id}}
Authorization: Bearer {{token}}

HTTP 201


# ─────────────────────────────────────────────────────────────
# Step 5 – Favorites contains only hello-renamed.txt
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/favorites/resources
Authorization: Bearer {{token}}

HTTP 200
[Asserts]
jsonpath "$.items" count == 1
jsonpath "$.items[0].resource.id" == {{file_id}}
jsonpath "$.items[0].resource_type" == "file"
jsonpath "$.items[0].resource.name" == "hello-renamed.txt"
# Caller-flag contract — every row on /api/favorites/resources IS
# favorited by construction (the listing SQL hardcodes
# `TRUE AS is_favorite`). `is_shared` is a real per-row EXISTS.
jsonpath "$.items[0].resource.is_favorite" == true
jsonpath "$.items[0].resource.is_shared" == false


# ─────────────────────────────────────────────────────────────
# Step 6 – Add test1 folder to favorites
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/favorites/folder/{{test1_id}}
Authorization: Bearer {{token}}

HTTP 201


# ─────────────────────────────────────────────────────────────
# Step 7 – Favorites contains both items (order-independent)
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/favorites/resources
Authorization: Bearer {{token}}

HTTP 200
[Asserts]
jsonpath "$.items" count == 2
jsonpath "$.items[*].resource.id" contains {{file_id}}
jsonpath "$.items[*].resource.id" contains {{test1_id}}


# ─────────────────────────────────────────────────────────────
# Step 8 – Remove hello-renamed.txt from favorites
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/api/favorites/file/{{file_id}}
Authorization: Bearer {{token}}

HTTP 200


# ─────────────────────────────────────────────────────────────
# Step 9 – Favorites contains only test1 folder
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/favorites/resources
Authorization: Bearer {{token}}

HTTP 200
[Asserts]
jsonpath "$.items" count == 1
jsonpath "$.items[0].resource.id" == {{test1_id}}
jsonpath "$.items[0].resource_type" == "folder"
jsonpath "$.items[0].resource.name" == "test1"


# ─────────────────────────────────────────────────────────────
# Step 10 – Cleanup: remove test1 from favorites
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/api/favorites/folder/{{test1_id}}
Authorization: Bearer {{token}}

HTTP 200


# ─────────────────────────────────────────────────────────────
# Step 11 – Favorites is empty again
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/favorites/resources
Authorization: Bearer {{token}}

HTTP 200
[Asserts]
jsonpath "$.items" count == 0


# ─────────────────────────────────────────────────────────────
# Step 12 — Cross-tenant regression (post-Drive AuthZ audit,
#           Round 1 HIGH). Before this fix, `POST /api/favorites/…`
#           accepted any UUID and enrolled it; the listing endpoint
#           then JOINed back to storage.files/folders and returned
#           name/mime/size/drive_id for anything the caller had
#           managed to add — an information oracle over the whole
#           tenant. Now the write path calls `authz.require(Read, …)`
#           per item; a caller with no grant gets 404 (anti-enum)
#           + `authz.denied` audit line. See
#           `docs/plan/authz_audit/rest_storage.md`.
# ─────────────────────────────────────────────────────────────

# Create a second, unprivileged user. Idempotent: `HTTP *` accepts
# either 201 (first run) or 409 (subsequent runs). The login below
# is the actual precondition — if it succeeds we know the user
# exists with the expected password.
POST {{base_url}}/api/admin/users
Authorization: Bearer {{token}}
Content-Type: application/json
{ "username": "fav_mallory", "password": "FavMalloryPassword1!", "email": "fav_mallory@example.com", "role": "user" }

HTTP *


POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "fav_mallory", "password": "FavMalloryPassword1!" }

HTTP 200
[Captures]
mallory_token: jsonpath "$.access_token"


# Step 12a — Single-add on admin's file: 404 (anti-enum shape).
POST {{base_url}}/api/favorites/file/{{file_id}}
Authorization: Bearer {{mallory_token}}

HTTP 404


# Step 12b — Single-add on admin's folder: 404.
POST {{base_url}}/api/favorites/folder/{{test1_id}}
Authorization: Bearer {{mallory_token}}

HTTP 404


# Step 12c — Batch: must fail wholesale on the first denial. A partial
#           success would still leak "which items are valid" — the same
#           oracle we're closing.
POST {{base_url}}/api/favorites/batch
Authorization: Bearer {{mallory_token}}
Content-Type: application/json
{
  "items": [
    { "item_id": "{{file_id}}",  "item_type": "file"   },
    { "item_id": "{{test1_id}}", "item_type": "folder" }
  ]
}

HTTP 404


# Step 12d — Mallory's favorites list is EMPTY — no partial success
#           slipped through.
GET {{base_url}}/api/favorites/resources
Authorization: Bearer {{mallory_token}}

HTTP 200
[Asserts]
jsonpath "$.items" count == 0


# ─────────────────────────────────────────────────────────────
# Step 13 — §7: an unknown order_by is a 400, not a silent default.
# ─────────────────────────────────────────────────────────────
# `order_by` is a typed `FavoritesOrderBy` enum, so serde rejects
# anything outside the six variants at wire-decode time. Before this it
# was a bare String re-validated at three layers, each with its own
# silent fallback to `name`:
#
#   * `build_favorites_cursor`'s `_` arm doubled as the `name` case AND
#     the unknown case, so a typo minted a `name` cursor
#   * the repository's `(_, false)` / `(_, true)` arms ordered by name
#
# The two then disagreed with each other for any axis the caller named
# but the fallback did not honour — a keyset predicate on one column
# with an ORDER BY on another skips or repeats rows, which is the
# hazard §2 had to fix on the folder listing. Both arms are gone and
# both matches are exhaustive, so a new axis is a compile error.
GET {{base_url}}/api/favorites/resources?order_by=definitely-not-an-axis
Authorization: Bearer {{mallory_token}}

HTTP 400
[Asserts]
# serde's derived error names the accepted variants, so the caller
# learns what to send — no custom extractor needed.
body contains "favorited_at"


# A valid non-default axis still works, so the 400 above is the enum
# rejecting an unknown value rather than the parameter being broken.
GET {{base_url}}/api/favorites/resources?order_by=favorited_at
Authorization: Bearer {{mallory_token}}

HTTP 200
