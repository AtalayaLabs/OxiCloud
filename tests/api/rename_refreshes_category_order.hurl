# =============================================================
# OxiCloud — renaming a file refreshes its category rank
# =============================================================
# `storage.files.category_order` is derived from BOTH inputs:
# `category_order_for(name, mime)`. The classifier consults the
# extension for the cases MIME provably cannot answer — source code
# has no magic bytes, and `text/plain` cannot separate a C file from
# a shopping list — so a rename that changes the extension changes
# the category while the MIME stays exactly as it was.
#
# `rename_file` wrote `name`, `updated_at` and `updated_by` and left
# the rank alone. That column is what the search type filter MATCHES
# ON — not `mime_type` — so a renamed file kept sorting and filtering
# under its previous type, silently, until something else rewrote it.
#
# ── Why the assertion is an ORDERING ─────────────────────────────
# `category_order` is not on the wire; `?order_by=type` sorting is its
# only observable effect, which the file arm ranks by
# `(category_order, LOWER(name), id)`. Same approach as
# `copy_preserves_category_order.hurl`.
#
# The fixture names and the target extension are both chosen so the
# NAME tie-break cannot mask the rank. Renaming `a-canary.txt` to
# `a-canary.deb` moves it from Text (1100) to Package (1200) — LATER
# in rank while its name still sorts FIRST:
#
#   with the fix     hello.txt (1100), a-canary.deb (1200)
#   without it       both 1100, so the tie-break decides and
#                    "a-canary.deb" sorts FIRST
#
# Two traps this avoids, both of which produce a test that passes
# either way:
#
#   * a rename that moves the rank the SAME direction the name
#     sorts (e.g. `.txt` -> `.mp3`, Audio 300) — the renamed file is
#     first before and after;
#   * `.bin`, which looks like it should be Unknown but is NOT:
#     an unclassified extension on `text/plain` content stays Text,
#     deliberately, so the rank would not move at all. (That is the
#     first thing this test caught.)
#
# Self-contained: own folder, removed in teardown.
# =============================================================


# ─────────────────────────────────────────────────────────────
# Setup
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "{{username}}", "password": "{{password}}" }

HTTP 200
[Captures]
token: jsonpath "$.access_token"


POST {{base_url}}/api/folders
Authorization: Bearer {{token}}
Content-Type: application/json
{ "name": "hurl-rename-category" }

HTTP 201
[Captures]
folder_id: jsonpath "$.id"


# Two text files, so both start at the same rank (Text = 1100) and
# only the later rename separates them.
POST {{base_url}}/api/files/upload
Authorization: Bearer {{token}}
[MultipartFormData]
folder_id: {{folder_id}}
file: file,fixtures/hello.txt; text/plain

HTTP 201
[Captures]
keep_id: jsonpath "$.id"


POST {{base_url}}/api/files/upload
Authorization: Bearer {{token}}
[MultipartFormData]
folder_id: {{folder_id}}
file: file,fixtures/content-canary.txt; text/plain

HTTP 201
[Captures]
rename_id: jsonpath "$.id"


# Give it the name the ordering argument above depends on.
PUT {{base_url}}/api/files/{{rename_id}}/rename
Authorization: Bearer {{token}}
Content-Type: application/json
{ "name": "a-canary.txt" }

HTTP 200


# ─────────────────────────────────────────────────────────────
# Baseline — both are Text, so the NAME tie-break orders them and
# `a-canary.txt` comes first. This establishes that the ranking is
# live, so the assertion after the rename compares against a
# known-good answer rather than an assumption.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/folders/{{folder_id}}/resources?limit=50&order_by=type&resource_types=file
Authorization: Bearer {{token}}

HTTP 200
[Asserts]
jsonpath "$.items" count == 2
jsonpath "$.items[0].resource.name" == "a-canary.txt"
jsonpath "$.items[1].resource.name" == "hello.txt"


# ─────────────────────────────────────────────────────────────
# THE rename. `.txt` → `.deb` keeps the stored MIME (`text/plain`,
# set at upload and untouched by a rename) and moves the category
# from Text to Package.
# ─────────────────────────────────────────────────────────────
PUT {{base_url}}/api/files/{{rename_id}}/rename
Authorization: Bearer {{token}}
Content-Type: application/json
{ "name": "a-canary.deb" }

HTTP 200
[Asserts]
jsonpath "$.name" == "a-canary.deb"
# The MIME is deliberately unchanged — this is the point. The rank
# has to move on the NAME alone.
jsonpath "$.mime_type" == "text/plain"
# And the RETURNED category moved with it. This half is the
# classifier rather than the stored column: `FileDto::category` is
# recomputed from (name, mime) on every read, so it is right even
# when the column is stale — which is exactly why the column needs
# its own assertion below. Asserting both is what separates "the
# classifier works" from "the denormalised rank was refreshed".
jsonpath "$.category" == "Package"


# ─────────────────────────────────────────────────────────────
# THE assertion. The order must now be the other way round.
#
# Without the fix the renamed file keeps rank 1100, ties with
# `hello.txt`, and the name tie-break puts `a-canary.deb` first —
# i.e. the listing looks exactly like the baseline above and the
# rename is invisible to the type sort.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/folders/{{folder_id}}/resources?limit=50&order_by=type&resource_types=file
Authorization: Bearer {{token}}

HTTP 200
[Asserts]
jsonpath "$.items" count == 2
jsonpath "$.items[0].resource.name" == "hello.txt"
jsonpath "$.items[1].resource.name" == "a-canary.deb"


# ─────────────────────────────────────────────────────────────
# And back again, so the refresh is not one-directional — a rename
# that RESTORES a classified extension must restore the rank too.
# ─────────────────────────────────────────────────────────────
PUT {{base_url}}/api/files/{{rename_id}}/rename
Authorization: Bearer {{token}}
Content-Type: application/json
{ "name": "a-canary.txt" }

HTTP 200


GET {{base_url}}/api/folders/{{folder_id}}/resources?limit=50&order_by=type&resource_types=file
Authorization: Bearer {{token}}

HTTP 200
[Asserts]
jsonpath "$.items[0].resource.name" == "a-canary.txt"
jsonpath "$.items[0].resource.mime_type" == "text/plain"
jsonpath "$.items[0].resource.category" == "Text"
jsonpath "$.items[1].resource.name" == "hello.txt"


# ─────────────────────────────────────────────────────────────
# The same property with the clearest pair of extensions: `.txt`
# and `.ini` are BOTH `text/plain`, so nothing about the content or
# its MIME differs — only the name — and the category still has to
# move (Text → Config). This is the case the classifier exists for,
# and the one no amount of content sniffing can decide.
# ─────────────────────────────────────────────────────────────
PUT {{base_url}}/api/files/{{rename_id}}/rename
Authorization: Bearer {{token}}
Content-Type: application/json
{ "name": "a-canary.ini" }

HTTP 200
[Asserts]
jsonpath "$.mime_type" == "text/plain"
jsonpath "$.category" == "Config"


# Config (950) still sorts before Text (1100), so `a-canary.ini`
# leads — same as the baseline, but now for a different reason. The
# assertion that distinguishes them is the one above on `category`.
GET {{base_url}}/api/folders/{{folder_id}}/resources?limit=50&order_by=type&resource_types=file
Authorization: Bearer {{token}}

HTTP 200
[Asserts]
jsonpath "$.items[0].resource.name" == "a-canary.ini"
jsonpath "$.items[1].resource.name" == "hello.txt"


# ═════════════════════════════════════════════════════════════
# Teardown
# ═════════════════════════════════════════════════════════════
DELETE {{base_url}}/api/files/{{rename_id}}
Authorization: Bearer {{token}}

HTTP 204


DELETE {{base_url}}/api/files/{{keep_id}}
Authorization: Bearer {{token}}

HTTP 204


DELETE {{base_url}}/api/folders/{{folder_id}}
Authorization: Bearer {{token}}

HTTP 204
