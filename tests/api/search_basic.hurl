# =============================================================
# OxiCloud — Baseline: search surface
# =============================================================
# Pins `/api/search` and `/api/search/suggest` plus the
# cross-user isolation property: a search MUST NEVER surface a
# file the caller doesn't own (and isn't shared with). Search
# is the kind of feature where a sloppy SQL join is exactly
# what introduces a cross-user leak — this test catches that.
#
# Requires OXICLOUD_ENABLE_SEARCH=true (set in tests/common/server.env).
#
# Coverage:
#   1. Admin uploads `unique-search-needle-aaa.txt` to her home
#   2. GET /api/search?query=unique-search-needle returns the file
#   3. GET /api/search?query=does-not-exist-xyz   returns 0 files
#   4. GET /api/search/suggest?query=unique-search-needle returns
#      something (suggestion-shape is allowed to be permissive)
#   5. Cross-user: bob searches "unique-search-needle" → MUST NOT
#      see admin's file (security baseline)
#   6. Teardown: delete the file
#
# Bob is (re-)created inline so this file is order-independent
# with respect to nc_second_user_setup.hurl (which runs later
# in run.sh).
# =============================================================


# ─────────────────────────────────────────────────────────────
# Pre-setup — anonymous request pin.
#
# `DELETE /api/admin/search/cache` with NO credentials must land as
# 401 Unauthorized (from `auth_middleware`, before the admin gate
# even runs). Kept at the very top of the file so no earlier
# request has populated any auth state that could accidentally
# authenticate this request. `[Options] cookie-storage-clear` was
# tried earlier but isn't supported in Hurl 8.0.1, so we rely on
# ordering instead — this DELETE runs FIRST, before any login.
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/api/admin/search/cache

HTTP 401


# ─────────────────────────────────────────────────────────────
# Setup — admin login + bob (re-)provisioning
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "{{username}}", "password": "{{password}}" }

HTTP 200
[Captures]
admin_token: jsonpath "$.access_token"


GET {{base_url}}/api/folders
Authorization: Bearer {{admin_token}}

HTTP 200
[Captures]
admin_home_id: jsonpath "$[0].id"


# Anti-enum registration: 200 whether bob existed or not.
POST {{base_url}}/api/auth/register
Content-Type: application/json
{
  "username": "bob",
  "email":    "bob@example.com",
  "password": "BobPassword1!"
}

HTTP 200


POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "bob", "password": "BobPassword1!" }

HTTP 200
[Captures]
bob_token: jsonpath "$.access_token"


# ─────────────────────────────────────────────────────────────
# 1 — Admin uploads `hello.txt` to a dedicated subfolder, then
#     renames it to a deliberately unique name so the search
#     assertion is unambiguous. The subfolder isolates this
#     test from any other test that already left a `hello.txt`
#     in admin's home (would otherwise 409).
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/folders
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "name": "search-basic-test", "parent_id": "{{admin_home_id}}" }

HTTP 201
[Captures]
search_folder_id: jsonpath "$.id"


POST {{base_url}}/api/files/upload
Authorization: Bearer {{admin_token}}
[MultipartFormData]
folder_id: {{search_folder_id}}
file: file,fixtures/hello.txt; text/plain

HTTP 201
[Captures]
needle_file_id: jsonpath "$.id"


PUT {{base_url}}/api/files/{{needle_file_id}}/rename
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "name": "unique-search-needle-aaa.txt" }

HTTP 200


# ─────────────────────────────────────────────────────────────
# 2 — Search hits the seeded file by substring of its name.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/search?query=unique-search-needle
Authorization: Bearer {{admin_token}}

HTTP 200
[Asserts]
# `/api/search` was normalised to the `/*/resources` envelope in
# PR search-normalize (2026-07): items[] carry `resource_type` +
# a `resource` (File | Folder | Drive) + inline search-meta. This
# assertion checks the same anti-regression property as before
# (needle file surfaces to its owner) against the new wire shape.
jsonpath "$.items" count >= 1
body contains "{{needle_file_id}}"


# ─────────────────────────────────────────────────────────────
# 3 — A search for a phrase that can't match anything must
#     return an empty result set, NOT an error. Empty-results
#     is a hot path; we don't want it to start 500ing.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/search?query=does-not-exist-xyz-zzz-9999
Authorization: Bearer {{admin_token}}

HTTP 200
[Asserts]
jsonpath "$.items" count == 0


# ─────────────────────────────────────────────────────────────
# 4 — Suggest returns a usable payload (shape is permissive —
#     just confirm the endpoint serves 200 and isn't truncating
#     to an error envelope).
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/search/suggest?query=unique-search-needle
Authorization: Bearer {{admin_token}}

HTTP 200


# ─────────────────────────────────────────────────────────────
# 5 — HEADLINE: bob MUST NOT see admin's file. If this assertion
#     ever flips, the search service has a cross-user leak.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/search?query=unique-search-needle
Authorization: Bearer {{bob_token}}

HTTP 200
[Asserts]
body not contains "unique-search-needle"
body not contains "{{needle_file_id}}"


# ─────────────────────────────────────────────────────────────
# 5b — REGRESSION: `/api/search/suggest` MUST also refuse to
#      surface admin's file to bob. Pre-fix (AuthZ audit #1,
#      2026-07-12) the suggest endpoint had NO `AuthUser`
#      extractor and its underlying `suggest_files_by_name` /
#      `suggest_folders_by_name` filtered only on
#      `NOT is_trashed AND name ILIKE $1` — any authenticated
#      user (including externals) could autocomplete names and
#      full `path` values across every tenant on the instance.
#      Fix: added `caller_id` to both repo queries via the
#      shared `CALLER_CAN_READ_DRIVE` predicate (`role_grants`
#      + `caller_group_ids`). This assertion is the anti-
#      regression pin.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/search/suggest?query=unique-search-needle
Authorization: Bearer {{bob_token}}

HTTP 200
[Asserts]
body not contains "unique-search-needle"
body not contains "{{needle_file_id}}"


# ─────────────────────────────────────────────────────────────
# 6 — CONTENT-search cross-drive isolation (docs/plan/drive.md §11).
#     The cross-user check above (step 5) verifies the NAME-search
#     path. The Tantivy content index is a separate code path with
#     its own filter: `Must drive_id ∈ accessible_drives`. This
#     block pins it.
#
#     Sequence:
#       6a. Admin uploads `content-canary.txt` whose body contains
#           the distinctive phrase `ContentIndexCanaryXyzzy2026Drive`.
#       6b. Wait ~2s for the async content-index worker
#           (`OXICLOUD_CONTENT_SEARCH_FLUSH_INTERVAL_MS` defaults
#           to 1500ms) to drain the dirty queue and apply the
#           Tantivy mutation.
#       6c. Admin searches for the phrase → MUST hit the file
#           (the index works).
#       6d. Bob searches for the same phrase → MUST be empty,
#           AND the response shape MUST carry no hidden-count
#           leak (no `total`/`hidden`/etc. field that could
#           reveal "you have N matches you can't see"). The
#           pivot from `Must user_id = caller` to `Must drive_id
#           ∈ accessible_drives` is the §11 security primitive;
#           a regression here would be a cross-drive leak.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/files/upload
Authorization: Bearer {{admin_token}}
[MultipartFormData]
folder_id: {{search_folder_id}}
file: file,fixtures/content-canary.txt; text/plain

HTTP 201
[Captures]
canary_file_id: jsonpath "$.id"


# Drain the content-index worker. 2s exceeds the 1500ms flush
# interval comfortably; raise if a slower CI machine flakes.
GET {{base_url}}/api/search?query=ContentIndexCanaryXyzzy2026Drive
Authorization: Bearer {{admin_token}}
[Options]
delay: 2500ms

HTTP 200
[Asserts]
# Admin sees the content match — proves indexing landed.
jsonpath "$.items" count >= 1
body contains "{{canary_file_id}}"


GET {{base_url}}/api/search?query=ContentIndexCanaryXyzzy2026Drive
Authorization: Bearer {{bob_token}}

HTTP 200
[Asserts]
# Bob has no access to admin's drive → Tantivy's Must-clause
# filters every doc that doesn't carry one of Bob's drive_ids,
# so the file vanishes entirely.
jsonpath "$.items" count == 0
body not contains "{{canary_file_id}}"
body not contains "ContentIndexCanaryXyzzy2026Drive"
# Anti-enum: every count the response surfaces must reflect the
# FILTERED set — i.e. zero when the caller has no accessible
# hits. The §11 rule is "no 'you have N hidden matches' field
# anywhere". `total` is the visible-to-caller count (permission-
# filtered SUM); it's OK when it equals the visible total (zero
# here). Old `total_count` / `has_more` names are retired with
# the `files/folders` split. Names below MUST stay absent — a
# future field like `hidden_count`/`filtered`/etc. that reveals
# matches Bob can't see would be the regression.
jsonpath "$.total" == 0
jsonpath "$.next_cursor" not exists
jsonpath "$.hidden_count" not exists
jsonpath "$.filtered" not exists


# ─────────────────────────────────────────────────────────────
# 6b — Regression pin for AuthZ audit #14 (2026-07-12).
#      `DELETE /api/admin/search/cache` calls moka `invalidate_all()`
#      on the shared results cache — one call cold-starts every
#      subsequent search for every tenant. Pre-fix, this lived at
#      `/api/search/cache` gated only by the top-level auth
#      middleware: any authenticated caller (including external /
#      magic-link accounts) could DELETE it in a loop and hold the
#      results cache empty indefinitely (sustained DoS). Fix: gate
#      on `require_admin` AND move the URL to `/api/admin/...` so
#      the taxonomy declares the intent up front. Moved 2026-07-17.
#
#      Bob (regular user) → 403; missing token → 401; admin → 200.
#      The 200 confirms the admin path still works (no regression
#      on the operator debug lever the endpoint remains for).
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/api/admin/search/cache
Authorization: Bearer {{bob_token}}

HTTP 403


# The unauthenticated 401 case is pinned at the top of the file
# (before any login has run) — see the pre-setup block. Placing it
# there instead of here avoids relying on Hurl's cookie / auth
# behaviour, which `cookie-storage-clear` (unsupported in 8.0.1)
# would otherwise be needed to reset.
DELETE {{base_url}}/api/admin/search/cache
Authorization: Bearer {{admin_token}}

HTTP 200


# ─────────────────────────────────────────────────────────────
# 7 — Teardown: removing the folder recursively takes the files
#     with it, so a single DELETE is enough.
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/api/folders/{{search_folder_id}}
Authorization: Bearer {{admin_token}}

HTTP 204
