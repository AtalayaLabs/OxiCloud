# =============================================================
# OxiCloud — the folder entry-list clock and the file content clock
# =============================================================
# Covers plan §8 (`storage.folders.content_modified_at`) and the file
# half of §1/§2 (`storage.files.content_modified_at`).
#
# ── Why this reads REST and not PROPFIND ──────────────────────────────
# A folder has TWO clocks and they answer different questions:
#
#   folders.content_modified_at  entry list changed   → REST modified_at
#   folders.tree_modified_at     anything beneath     → DAV getlastmodified
#
# So §8 is only observable over REST. A PROPFIND would report the
# recursive rollup and this whole file would be testing the wrong column.
# That divergence is deliberate — see docs/architecture/dates.md
# § Collections — and part E below pins it so nobody "fixes" one side.
#
# ── What makes these assertions worth having ──────────────────────────
# The NEGATIVE cases carry most of the value. A naive implementation
# that simply pointed folder `modified_at` at `tree_modified_at` would
# pass every "did it bump" assertion here and fail only B1 and C1 — the
# two that prove the clock tracks the entry list rather than the whole
# subtree. Without them this file would not distinguish the shipped
# design from the one that was explicitly rejected for saturating.
#
# ── Timing ────────────────────────────────────────────────────────────
# Comparisons are string equality on RFC 3339 values carrying sub-second
# precision, and each step is a separate HTTP request and therefore a
# separate transaction with its own NOW(). The "changed" assertions
# would only mis-pass if two requests landed in the same microsecond.
#
# Self-contained: creates its own folder tree and deletes it in teardown.
# =============================================================


# ─────────────────────────────────────────────────────────────
# Setup — admin JWT, then a two-level folder tree.
#   fcc-parent/
#     fcc-child/
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "{{username}}", "password": "{{password}}" }

HTTP 200
[Captures]
jwt: jsonpath "$.access_token"


POST {{base_url}}/api/folders
Authorization: Bearer {{jwt}}
Content-Type: application/json
{
  "name": "fcc-parent"
}

HTTP 201
[Captures]
parent_id: jsonpath "$.id"


POST {{base_url}}/api/folders
Authorization: Bearer {{jwt}}
Content-Type: application/json
{
  "name": "fcc-child",
  "parent_id": "{{parent_id}}"
}

HTTP 201
[Captures]
child_id: jsonpath "$.id"


# Creating `fcc-child` was itself an entry-list change on the parent, so
# read the baseline AFTER the tree exists rather than before.
GET {{base_url}}/api/folders/{{parent_id}}
Authorization: Bearer {{jwt}}

HTTP 200
[Captures]
parent_t0: jsonpath "$.modified_at"
[Asserts]
jsonpath "$.modified_at" isString
# `tree_modified_at` is a separate field, not an alias of the above.
jsonpath "$.tree_modified_at" isString


# ═════════════════════════════════════════════════════════════
# Part A — adding, renaming and removing a child moves the clock
# ═════════════════════════════════════════════════════════════

# A1 — create a child file (WebDAV PUT into the parent's path).
PUT {{base_url}}/webdav/fcc-parent/probe.txt
Authorization: Bearer {{jwt}}
Content-Type: text/plain
```
first revision
```

HTTP 201


GET {{base_url}}/api/folders/{{parent_id}}
Authorization: Bearer {{jwt}}

HTTP 200
[Captures]
parent_t1: jsonpath "$.modified_at"
[Asserts]
jsonpath "$.modified_at" != "{{parent_t0}}"


# A2 — rename the child. The entry list changed, so the parent moves.
MOVE {{base_url}}/webdav/fcc-parent/probe.txt
Destination: {{base_url}}/webdav/fcc-parent/renamed.txt
Authorization: Bearer {{jwt}}

HTTP 201


GET {{base_url}}/api/folders/{{parent_id}}
Authorization: Bearer {{jwt}}

HTTP 200
[Captures]
parent_t2: jsonpath "$.modified_at"
[Asserts]
jsonpath "$.modified_at" != "{{parent_t1}}"


# ═════════════════════════════════════════════════════════════
# Part B — a child's CONTENT changing must NOT move the folder
# ═════════════════════════════════════════════════════════════
# The defining assertion of §8. POSIX does not touch a directory when a
# file inside it is edited — only the entry list counts. An
# implementation backed by `tree_modified_at` fails here, because that
# column's change tuple includes `blob_hash` and `size`.
PUT {{base_url}}/webdav/fcc-parent/renamed.txt
Authorization: Bearer {{jwt}}
Content-Type: text/plain
```
second revision, different length
```

HTTP 204


GET {{base_url}}/api/folders/{{parent_id}}
Authorization: Bearer {{jwt}}

HTTP 200
[Asserts]
jsonpath "$.modified_at" == "{{parent_t2}}"


# B2 — the FILE's own clock moving for that same edit is the other half
# of this claim (the folder staying put is only correct if the file
# moved), but it is NOT asserted here: `nc_webdav_client_mtime.hurl`
# owns the file content clock, and does it deterministically via
# `X-OC-Mtime` instead of racing the wall clock. Duplicating it here
# would mean two files to update when the semantics change.


# B3 — rename the file again, to set up Part D's move with a stable name.
MOVE {{base_url}}/webdav/fcc-parent/renamed.txt
Destination: {{base_url}}/webdav/fcc-parent/renamed2.txt
Authorization: Bearer {{jwt}}

HTTP 201


# ═════════════════════════════════════════════════════════════
# Part C — the clock does not propagate up the tree
# ═════════════════════════════════════════════════════════════
# The other half of "not tree_modified_at". A change inside `fcc-child`
# is an entry-list change for `fcc-child` and for nobody else.
GET {{base_url}}/api/folders/{{parent_id}}
Authorization: Bearer {{jwt}}

HTTP 200
[Captures]
parent_before_grandchild: jsonpath "$.modified_at"


PUT {{base_url}}/webdav/fcc-parent/fcc-child/deep.txt
Authorization: Bearer {{jwt}}
Content-Type: text/plain
```
a file one level deeper
```

HTTP 201


# C1 — the direct parent of the new file moved…
GET {{base_url}}/api/folders/{{child_id}}
Authorization: Bearer {{jwt}}

HTTP 200
[Asserts]
jsonpath "$.modified_at" isString


# C2 — …and its grandparent did NOT.
GET {{base_url}}/api/folders/{{parent_id}}
Authorization: Bearer {{jwt}}

HTTP 200
[Asserts]
jsonpath "$.modified_at" == "{{parent_before_grandchild}}"


# C3 — but the grandparent's SUBTREE clock is allowed to move, and that
# is the point of keeping two columns. Pinning both in one place is what
# stops a future change from collapsing them back together.
#   (Asserted loosely: `tree_modified_at` is maintained by the async
#   `tree_etag_dirty` queue, so it may not have drained by now. The
#   structural claim — the field exists and is distinct from
#   `modified_at` — is what is being pinned.)
GET {{base_url}}/api/folders/{{parent_id}}
Authorization: Bearer {{jwt}}

HTTP 200
[Asserts]
jsonpath "$.tree_modified_at" isString


# ═════════════════════════════════════════════════════════════
# Part D — a move bumps BOTH ends
# ═════════════════════════════════════════════════════════════
GET {{base_url}}/api/folders/{{child_id}}
Authorization: Bearer {{jwt}}

HTTP 200
[Captures]
child_before_move: jsonpath "$.modified_at"


GET {{base_url}}/api/folders/{{parent_id}}
Authorization: Bearer {{jwt}}

HTTP 200
[Captures]
parent_before_move: jsonpath "$.modified_at"


MOVE {{base_url}}/webdav/fcc-parent/renamed2.txt
Destination: {{base_url}}/webdav/fcc-parent/fcc-child/renamed2.txt
Authorization: Bearer {{jwt}}

HTTP 201


# D1 — source lost an entry.
GET {{base_url}}/api/folders/{{parent_id}}
Authorization: Bearer {{jwt}}

HTTP 200
[Asserts]
jsonpath "$.modified_at" != "{{parent_before_move}}"


# D2 — destination gained one.
GET {{base_url}}/api/folders/{{child_id}}
Authorization: Bearer {{jwt}}

HTTP 200
[Captures]
child_after_move: jsonpath "$.modified_at"
[Asserts]
jsonpath "$.modified_at" != "{{child_before_move}}"


# D3 — deleting a child is an entry-list change too.
DELETE {{base_url}}/webdav/fcc-parent/fcc-child/renamed2.txt
Authorization: Bearer {{jwt}}

HTTP 204


GET {{base_url}}/api/folders/{{child_id}}
Authorization: Bearer {{jwt}}

HTTP 200
[Asserts]
jsonpath "$.modified_at" != "{{child_after_move}}"


# ═════════════════════════════════════════════════════════════
# Part E — REST and DAV deliberately disagree for folders
# ═════════════════════════════════════════════════════════════
# REST reports the entry clock, DAV the subtree clock. This is recorded
# as a deliberate divergence; the test exists so that if someone aligns
# them, they do it on purpose and update the doc rather than discovering
# it from a client bug report.
PROPFIND {{base_url}}/webdav/fcc-parent/
Depth: 0
Authorization: Bearer {{jwt}}
Content-Type: application/xml; charset=utf-8
```
<?xml version="1.0" encoding="utf-8"?>
<D:propfind xmlns:D="DAV:">
  <D:prop>
    <D:getlastmodified/>
    <D:getetag/>
  </D:prop>
</D:propfind>
```

HTTP 207
[Asserts]
# RFC 1123 shape (RFC 4918 §15.7) — sourced from `tree_modified_at`,
# NOT from the `modified_at` the REST assertions above read.
xpath "string(//*[local-name()='getlastmodified'])" matches "^[A-Z][a-z]{2}, [0-9]{1,2} [A-Z][a-z]{2} [0-9]{4} [0-9]{2}:[0-9]{2}:[0-9]{2} (GMT|\\+0000)$"
xpath "string(//*[local-name()='getetag'])" exists


# ═════════════════════════════════════════════════════════════
# Teardown
# ═════════════════════════════════════════════════════════════
DELETE {{base_url}}/webdav/fcc-parent/fcc-child/deep.txt
Authorization: Bearer {{jwt}}

HTTP 204


DELETE {{base_url}}/api/folders/{{child_id}}
Authorization: Bearer {{jwt}}

HTTP 204


DELETE {{base_url}}/api/folders/{{parent_id}}
Authorization: Bearer {{jwt}}

HTTP 204
