# =============================================================
# OxiCloud — search's `order_by=updated_at` sorts on the content clock
# =============================================================
# Covers the §3 bugfix, which shipped without a test.
#
# Search orders its SQL page by a column chosen in
# `file_blob_read_repository`, and when content-index hits are blended
# into that page `search_service::sort_enriched_files` re-sorts the
# merged list IN MEMORY on `SearchFileResultDto.modified_at`. A comment
# in `search_service.rs` states the invariant that the two must agree.
#
# §2 repointed that DTO field at `content_modified_at` and left the SQL
# ordering on `fi.updated_at`, so from `8d0631be` until the §3 commit
# the invariant was false: a search sorted by `updated_at` was PAGED on
# one clock and RE-ORDERED on another. §3 repointed the SQL to match.
#
# ── What this test can and cannot reach ──────────────────────────────
# It pins the SQL ordering, which is the half that was wrong. It does
# NOT reproduce the blended-hit path, because that needs the Tantivy
# content index to have ingested the bodies — an async queue, so any
# assertion depending on it would be timing-dependent. The in-memory
# re-sort is covered by the unit tests around `sort_enriched_files`;
# what had no coverage at all was which column SQL ordered by.
#
# ── Why the ordering is unambiguous ──────────────────────────────────
# Two files are created in one order and given content clocks in the
# OPPOSITE order via `X-OC-Mtime` (the only surface that can set a
# content clock to a known past instant — the REST upload path has no
# mtime slot yet, plan §9):
#
#   created 1st → aaa, mtime 2023-11-15  (NEWER content clock)
#   created 2nd → bbb, mtime 2020-07-07  (OLDER content clock)
#
#   after §3  → aaa, bbb   (content clock DESC)
#   before §3 → bbb, aaa   (updated_at DESC, i.e. creation order)
#
# Exactly reversed, three years apart, so there is no timing
# sensitivity and no tie to break.
#
# Self-contained: unique needle in the filenames, both deleted in
# teardown along with the app password.
# =============================================================


# ─────────────────────────────────────────────────────────────
# Setup — JWT plus an NC app password (X-OC-Mtime is NC-only).
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "{{username}}", "password": "{{password}}" }

HTTP 200
[Captures]
jwt: jsonpath "$.access_token"


POST {{base_url}}/api/auth/app-passwords
Authorization: Bearer {{jwt}}
Content-Type: application/json
{ "label": "search_sort_content_clock" }

HTTP 200
[Captures]
nc_username: jsonpath "$.username"
nc_password: jsonpath "$.password"
ap_id:       jsonpath "$.id"


# ─────────────────────────────────────────────────────────────
# Created FIRST, with the NEWER content clock.
# ─────────────────────────────────────────────────────────────
PUT {{base_url}}/remote.php/dav/files/{{username}}/aaa-srchclock-needle.txt
Content-Type: text/plain
X-OC-Mtime: 1700086400
[BasicAuth]
{{nc_username}}: {{nc_password}}
```
first created, newest content clock
```

HTTP 201


# ─────────────────────────────────────────────────────────────
# Created SECOND, with the OLDER content clock.
# ─────────────────────────────────────────────────────────────
PUT {{base_url}}/remote.php/dav/files/{{username}}/bbb-srchclock-needle.txt
Content-Type: text/plain
X-OC-Mtime: 1594153342
[BasicAuth]
{{nc_username}}: {{nc_password}}
```
second created, oldest content clock
```

HTTP 201


# ─────────────────────────────────────────────────────────────
# Baseline — both files are findable, so an ordering failure below
# cannot be mistaken for a visibility failure.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/search?query=srchclock-needle
Authorization: Bearer {{jwt}}

HTTP 200
[Asserts]
jsonpath "$.items" count == 2


# ═════════════════════════════════════════════════════════════
# THE assertion
# ═════════════════════════════════════════════════════════════
# Newest content clock first, which is the REVERSE of the order the
# two files were created in. A build ordering on `updated_at` returns
# bbb first.
#
# Positional access rather than a `[?(...)]` filter: Hurl collapses a
# single-match filter to a bare scalar and `nth`/`count` then error.
GET {{base_url}}/api/search?query=srchclock-needle&order_by=updated_at
Authorization: Bearer {{jwt}}

HTTP 200
[Asserts]
jsonpath "$.items" count == 2
jsonpath "$.items[0].resource.name" == "aaa-srchclock-needle.txt"
jsonpath "$.items[1].resource.name" == "bbb-srchclock-needle.txt"
# And the reported dates are the asserted mtimes, not the upload
# moments — so the ordering above is the content clock rather than a
# coincidence of creation order.
jsonpath "$.items[0].resource.modified_at" startsWith "2023-11-15T22:13:20"
jsonpath "$.items[1].resource.modified_at" startsWith "2020-07-07T20:22:22"


# ─────────────────────────────────────────────────────────────
# Reversed — the same pair, oldest content clock first. `reverse`
# flips the SQL direction, and a build whose direction and column
# disagreed would show it here rather than above.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/search?query=srchclock-needle&order_by=updated_at&reverse=true
Authorization: Bearer {{jwt}}

HTTP 200
[Asserts]
jsonpath "$.items" count == 2
jsonpath "$.items[0].resource.name" == "bbb-srchclock-needle.txt"
jsonpath "$.items[1].resource.name" == "aaa-srchclock-needle.txt"


# ═════════════════════════════════════════════════════════════
# Teardown
# ═════════════════════════════════════════════════════════════
DELETE {{base_url}}/remote.php/dav/files/{{username}}/aaa-srchclock-needle.txt
[BasicAuth]
{{nc_username}}: {{nc_password}}

HTTP 204


DELETE {{base_url}}/remote.php/dav/files/{{username}}/bbb-srchclock-needle.txt
[BasicAuth]
{{nc_username}}: {{nc_password}}

HTTP 204


DELETE {{base_url}}/api/auth/app-passwords/{{ap_id}}
Authorization: Bearer {{jwt}}

HTTP 200
