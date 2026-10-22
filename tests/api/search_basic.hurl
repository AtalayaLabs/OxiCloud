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


# `type=folder` must return NO files even when the query matches file
# CONTENT. This is the assertion that would have caught the bug Ed hit
# in the UI, and the reason it belongs *here* rather than with the other
# type-filter cases below: those fixtures match by NAME only, so
# `lookup_content_hits` never fires for them and the leak stayed
# invisible. Ed's UI query matched content, which is why he saw files
# under the Folders filter while the name-only assertions passed.
#
# The leak was the same empty-set trap as the file branch:
# `merge_content_hits` hydrates its leftovers through
# `fetch_files_by_ids_filtered`, whose category predicate comes from
# `category_orders_for_filter` — and `folder` maps to an EMPTY set,
# which contributes no predicate, so every content-matched file passed
# and was appended after the folder branch had been suppressed. Fixed by
# returning early from `lookup_content_hits` when the filter excludes
# files, which also saves the Tantivy query and its authz re-check.
#
# No `delay` needed: the request above already drained the worker.
GET {{base_url}}/api/search?query=ContentIndexCanaryXyzzy2026Drive&type=folder
Authorization: Bearer {{admin_token}}

HTTP 200
[Asserts]
jsonpath "$.items[*].resource_type" not includes "file"
body not contains "{{canary_file_id}}"


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


# ═════════════════════════════════════════════════════════════
# 6b — `?type=` filters by CATEGORY, derived from mime server-side
# ═════════════════════════════════════════════════════════════
# Replaces a comma-separated extension list the SPA assembled
# (`type=mp3,wav,flac,…`) and matched against the filename with
# `LOWER(SUBSTRING(name FROM '\.([^.]+)$'))`. The fixture below is chosen
# to be unmatchable by that predicate: the filename has **no dot at all**,
# so the old SQL yielded NULL for it and no category filter could ever
# return it — even though its mime is known.
#
# Uploaded with an explicit `audio/mpeg` content type, which the handler
# stores verbatim as `mime_type`, so `category_order_for` classifies it
# Audio (300) from the mime alone.
POST {{base_url}}/api/files/upload
Authorization: Bearer {{admin_token}}
[MultipartFormData]
folder_id: {{search_folder_id}}
file: file,fixtures/searchcat-no-extension; audio/mpeg

HTTP 201


# A second audio file, this one covering the OTHER defect the old shape
# had: the SPA's per-category extension list could be incomplete. Its
# audio list was mp3/wav/flac/aac/ogg/m4a/opus — `wma` was missing, even
# though /music's own list carried it and the server has always
# classified it. So a `.wma` was findable from /music and invisible under
# search's "Audio".
#
# Uploaded as `application/octet-stream`, which is what a legacy or
# dumb client sends. Note what that does NOT test, because it is easy to
# assume otherwise: it does not reach `category_for`'s extension
# fallback. `refine_content_type` (`src/common/mime_detect.rs`) resolves
# generic claims as magic bytes → `mime_guess::from_path` → claimed, and
# this fixture's bytes are plain text, so `infer` stays silent and
# `mime_guess` maps `.wma` to `audio/x-ms-wma`. The row therefore lands
# with a real audio mime and is classified by the MIME arm.
#
# That makes the pin a different and still useful one: the whole chain
# from a generic-mime upload through sniffing to the category filter.
# `wma` is the right extension for it because the SPA's old audio list
# omitted it while /music's carried it — so this file used to be
# findable from /music and invisible under search's "Audio".
#
# Corollary worth knowing: since content sniffing landed on the upload
# path, `category_for`'s extension fallback is reachable only for rows
# whose mime is STILL generic — i.e. unrecognisable bytes AND an
# extension `mime_guess` does not know. Those are legacy rows, and
# `content_rescan` (docs/plan/blob-resniff.md §B) is what will retire
# them. No fixture here can exercise that arm through the upload API.
POST {{base_url}}/api/files/upload
Authorization: Bearer {{admin_token}}
[MultipartFormData]
folder_id: {{search_folder_id}}
file: file,fixtures/searchcat-legacy.wma; application/octet-stream

HTTP 201


# A folder matching the same query, so the folder assertions below prove
# inclusion/exclusion rather than passing vacuously on an empty page.
POST {{base_url}}/api/folders
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "name": "searchcat-subfolder",
  "parent_id": "{{search_folder_id}}"
}

HTTP 201


# Every assertion below counts rows and indexes into them rather than
# using `contains`. Deliberate: Hurl collapses a single-element JSONPath
# result to a SCALAR, and `contains` on a string is SUBSTRING matching —
# so `$.items[*].resource.name not contains "x"` silently changes meaning
# with the number of results. Counting is unambiguous either way.
#
# `query=searchcat` matches exactly three things: the two audio files and
# the folder created above.
GET {{base_url}}/api/search?query=searchcat&type=audio
Authorization: Bearer {{admin_token}}

HTTP 200
[Asserts]
# THE assertions of this block, one per classifier arm:
#
#   * `searchcat-no-extension` — specific mime, NO extension. Under the
#     old predicate this was unmatchable: `SUBSTRING(name FROM
#     '\.([^.]+)$')` is NULL without a dot, so the file could not appear
#     under any category filter at all.
#   * `searchcat-legacy.wma` — generic mime, classified by EXTENSION.
#     `wma` was absent from the SPA's audio list, so it was invisible
#     under search's "Audio" while /music (whose list had it) found it.
#
# Both are audio; the folder is not. Counting rather than using
# `contains` — Hurl collapses a single-element result to a scalar, where
# `contains` silently becomes substring matching.
jsonpath "$.items" count == 2
jsonpath "$.items[*].resource.name" includes "searchcat-no-extension"
jsonpath "$.items[*].resource.name" includes "searchcat-legacy.wma"
# And the folder is excluded — the defect Ed reported. `search_folders`
# cannot narrow by type, so the fix is to skip the folder query entirely
# for any filter other than `all` / `folders`.
jsonpath "$.items[*].resource_type" not includes "folder"


# The same file must NOT come back under a different category. Proves the
# filter narrows rather than being ignored — a filter that silently did
# nothing would satisfy the block above on its own, which is exactly what
# happened before `search_files_paginated` gained the shared filters: a
# search with no folder scope delegates there, and it applied neither the
# category NOR the size NOR the date criteria.
GET {{base_url}}/api/search?query=searchcat&type=image
Authorization: Bearer {{admin_token}}

HTTP 200
[Asserts]
jsonpath "$.items" count == 0


# `type=folder` returns the folder and no files — including no audio
# file, even though it matches the query. The file half is load-bearing:
# `folders` maps to an EMPTY category set, and an empty set contributes
# no SQL predicate, so without the `includes_files()` gate this would
# return every file rather than none.
GET {{base_url}}/api/search?query=searchcat&type=folder
Authorization: Bearer {{admin_token}}

HTTP 200
[Asserts]
jsonpath "$.items" count == 1
jsonpath "$.items[0].resource_type" == "folder"
jsonpath "$.items[0].resource.name" == "searchcat-subfolder"


# `all` (the default) still returns both kinds — so the gates above narrow
# rather than having broken the unfiltered path. Two files + one folder.
GET {{base_url}}/api/search?query=searchcat
Authorization: Bearer {{admin_token}}

HTTP 200
[Asserts]
jsonpath "$.items" count == 3


# Unknown category → 400 naming the accepted values, because `type` is a
# typed `SearchTypeFilter` enum rather than free text. The old shape
# accepted any string and silently matched nothing.
GET {{base_url}}/api/search?query=searchcat&type=definitely-not-a-category
Authorization: Bearer {{admin_token}}

HTTP 400
[Asserts]
body contains "folder"


# The retired extension form is refused too, rather than quietly matching
# nothing. A client still sending `type=mp3` learns that the contract
# changed instead of seeing an empty page.
GET {{base_url}}/api/search?query=searchcat&type=mp3
Authorization: Bearer {{admin_token}}

HTTP 400


# ─────────────────────────────────────────────────────────────
# 7 — Teardown: removing the folder recursively takes the files
#     with it, so a single DELETE is enough.
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/api/folders/{{search_folder_id}}
Authorization: Bearer {{admin_token}}

HTTP 204
