# =============================================================
# OxiCloud — keyset pagination over divergent clocks
# =============================================================
# The §2 cursor hazard, which is the only defect class in the dates work
# that corrupts results silently instead of erroring.
#
# `GET /api/folders/{id}/resources?order_by=modified_at` sorts on the
# `modified_at` ALIAS and pages with a keyset predicate naming a raw
# column. Those must be the same column. If the ORDER BY resolves to
# `content_modified_at` while the cursor still compares `updated_at`,
# the query sorts by one clock and paginates against another — rows are
# silently skipped and duplicated across page boundaries.
#
# **Page one always looks perfect.** There is no error, no 500, no log
# line; a client just never sees some of its files. That is why this
# needs a test rather than a code comment.
#
# ── Manufacturing the divergence ──────────────────────────────────────
# `X-OC-Mtime` sets `content_modified_at` while `updated_at` stays at
# upload time, so the four files below are created in one order and
# given content clocks in the OPPOSITE order:
#
#   file    created    content_modified_at   updated_at
#   a.txt   1st        2023-11-14 22:20:00   (newest)
#   b.txt   2nd        2023-11-14 22:18:20
#   c.txt   3rd        2023-11-14 22:16:40
#   d.txt   4th        2023-11-14 22:15:00   (oldest)
#
#   sorted by content_modified_at DESC → a, b, c, d
#   sorted by updated_at          DESC → d, c, b, a   ← exactly reversed
#
# A cursor reading the wrong column therefore does not merely reorder
# the results, it drops most of them: page 2's predicate would ask for
# rows with `updated_at` older than b.txt's, and c and d are NEWER by
# that clock, so they vanish entirely.
#
# ── Why a folder is in the mix ────────────────────────────────────────
# The listing is a UNION of a folder arm and a file arm, and the bug
# that shipped in `8d0631be` was in the folder arm specifically
# (`mk("f")` emitting a column that did not exist on `storage.folders`).
# `sub/` has a content clock of "now", far newer than the 2023 file
# clocks, so it sorts first deterministically — and the page boundary
# below falls INSIDE the run of files, so the paging crosses the arm
# boundary mid-result.
#
# Self-contained: creates its own tree and removes it in teardown.
# =============================================================


# ─────────────────────────────────────────────────────────────
# Setup — JWT, an NC app password (X-OC-Mtime is NC-only), and
#         the folder tree.
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
{ "label": "folder_listing_cursor_clocks" }

HTTP 200
[Captures]
nc_username: jsonpath "$.username"
nc_password: jsonpath "$.password"
ap_id:       jsonpath "$.id"


POST {{base_url}}/api/folders
Authorization: Bearer {{jwt}}
Content-Type: application/json
{
  "name": "cursorclk"
}

HTTP 201
[Captures]
root_id: jsonpath "$.id"


POST {{base_url}}/api/folders
Authorization: Bearer {{jwt}}
Content-Type: application/json
{
  "name": "sub",
  "parent_id": "{{root_id}}"
}

HTTP 201


# ─────────────────────────────────────────────────────────────
# Seed the four files, created ascending, content clocks descending.
# ─────────────────────────────────────────────────────────────
PUT {{base_url}}/remote.php/dav/files/{{username}}/cursorclk/a.txt
Content-Type: text/plain
X-OC-Mtime: 1700000400
[BasicAuth]
{{nc_username}}: {{nc_password}}
```
a
```

HTTP 201


PUT {{base_url}}/remote.php/dav/files/{{username}}/cursorclk/b.txt
Content-Type: text/plain
X-OC-Mtime: 1700000300
[BasicAuth]
{{nc_username}}: {{nc_password}}
```
b
```

HTTP 201


PUT {{base_url}}/remote.php/dav/files/{{username}}/cursorclk/c.txt
Content-Type: text/plain
X-OC-Mtime: 1700000200
[BasicAuth]
{{nc_username}}: {{nc_password}}
```
c
```

HTTP 201


PUT {{base_url}}/remote.php/dav/files/{{username}}/cursorclk/d.txt
Content-Type: text/plain
X-OC-Mtime: 1700000100
[BasicAuth]
{{nc_username}}: {{nc_password}}
```
d
```

HTTP 201


# ═════════════════════════════════════════════════════════════
# Part A — unpaged, to pin the expected order before paging it
# ═════════════════════════════════════════════════════════════
# If this order is wrong the paging assertions below would be testing
# the wrong expectation, so it is established first and separately.
# `sub` leads on its "now" clock; the files follow in content-clock
# order, which is the reverse of the order they were created in.
GET {{base_url}}/api/folders/{{root_id}}/resources?limit=50&order_by=modified_at
Authorization: Bearer {{jwt}}

HTTP 200
[Asserts]
jsonpath "$.items" count == 5
jsonpath "$.items[0].resource.name" == "sub"
jsonpath "$.items[1].resource.name" == "a.txt"
jsonpath "$.items[2].resource.name" == "b.txt"
jsonpath "$.items[3].resource.name" == "c.txt"
jsonpath "$.items[4].resource.name" == "d.txt"
# One page held everything, so there is nothing to continue.
jsonpath "$.next_cursor" not exists


# ═════════════════════════════════════════════════════════════
# Part B — the same result set, paged two at a time
# ═════════════════════════════════════════════════════════════
# Page 1 — the arm boundary falls inside this page: a folder then a
# file. Identical under a correct and a broken cursor, which is the
# whole point: a broken implementation gets this far looking healthy.
GET {{base_url}}/api/folders/{{root_id}}/resources?limit=2&order_by=modified_at
Authorization: Bearer {{jwt}}

HTTP 200
[Captures]
cur1: jsonpath "$.next_cursor"
[Asserts]
jsonpath "$.items" count == 2
jsonpath "$.items[0].resource.name" == "sub"
jsonpath "$.items[1].resource.name" == "a.txt"
jsonpath "$.next_cursor" exists


# Page 2 — THE assertion. The cursor was built from a.txt, whose
# `updated_at` is the NEWEST of the four files while its
# `content_modified_at` is the newest-but-one. A predicate on
# `updated_at` would return nothing here, or return d.txt; only a
# predicate on `content_modified_at` yields b and c.
GET {{base_url}}/api/folders/{{root_id}}/resources?limit=2&order_by=modified_at&cursor={{cur1}}
Authorization: Bearer {{jwt}}

HTTP 200
[Captures]
cur2: jsonpath "$.next_cursor"
[Asserts]
jsonpath "$.items" count == 2
jsonpath "$.items[0].resource.name" == "b.txt"
jsonpath "$.items[1].resource.name" == "c.txt"
jsonpath "$.next_cursor" exists


# Page 3 — the tail. d.txt is the row most at risk of being dropped: it
# is last by the content clock and FIRST by `updated_at`.
GET {{base_url}}/api/folders/{{root_id}}/resources?limit=2&order_by=modified_at&cursor={{cur2}}
Authorization: Bearer {{jwt}}

HTTP 200
[Asserts]
jsonpath "$.items" count == 1
jsonpath "$.items[0].resource.name" == "d.txt"
# Last page: the walk terminates instead of looping.
jsonpath "$.next_cursor" not exists


# ═════════════════════════════════════════════════════════════
# Part C — the same walk reversed
# ═════════════════════════════════════════════════════════════
# `reverse=true` flips both the ORDER BY direction and the comparison
# operator in the keyset predicate. They are built from the same pair,
# so a mismatch introduced on one side shows up only here.
GET {{base_url}}/api/folders/{{root_id}}/resources?limit=2&order_by=modified_at&reverse=true
Authorization: Bearer {{jwt}}

HTTP 200
[Captures]
rcur1: jsonpath "$.next_cursor"
[Asserts]
jsonpath "$.items" count == 2
jsonpath "$.items[0].resource.name" == "d.txt"
jsonpath "$.items[1].resource.name" == "c.txt"


GET {{base_url}}/api/folders/{{root_id}}/resources?limit=2&order_by=modified_at&reverse=true&cursor={{rcur1}}
Authorization: Bearer {{jwt}}

HTTP 200
[Asserts]
jsonpath "$.items" count == 2
jsonpath "$.items[0].resource.name" == "b.txt"
jsonpath "$.items[1].resource.name" == "a.txt"


# ═════════════════════════════════════════════════════════════
# Teardown
# ═════════════════════════════════════════════════════════════
DELETE {{base_url}}/api/folders/{{root_id}}
Authorization: Bearer {{jwt}}

HTTP 204


DELETE {{base_url}}/api/auth/app-passwords/{{ap_id}}
Authorization: Bearer {{jwt}}

HTTP 200
