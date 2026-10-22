# =============================================================
# OxiCloud — metadata mutations must NOT move the content clock
# =============================================================
# §1's verification half, which shipped without it: "rename, move,
# trash, grant and favourite leave the column **untouched** while still
# bumping `updated_at`".
#
# This is the invariant the whole dates plan exists to establish. Before
# §2, every mtime read resolved to `updated_at`, which all five of these
# bump — so renaming or sharing a file announced a content change to
# every sync client and triggered a re-download of bytes that had not
# moved.
#
# `nc_webdav_client_mtime.hurl` already covers RENAME. This covers the
# other four, which had no coverage at all.
#
# ── How the assertion is made timing-proof ────────────────────────────
# The file is uploaded through the NextCloud surface with
# `X-OC-Mtime: 1700086400` — 2023-11-15T22:13:20Z. Every mutation below
# happens now. So "unchanged" means "still reports 2023", three years
# from anything these requests could stamp, and no delay or clock care
# is needed anywhere.
#
# That matters because the naive version of this test — mutate, then
# compare to a captured value — can pass vacuously when both readings
# land in the same second. An absolute instant from the past cannot.
#
# ── What is NOT asserted, and why ─────────────────────────────────────
# The second half of §1's sentence — "while still bumping `updated_at`"
# — is **not observable over HTTP**. `updated_at` is deliberately absent
# from every DTO and wire surface (docs/architecture/dates.md § Roles:
# it is Internal). So this file can prove the content clock held; it
# cannot prove ctime moved. That is a consequence of the design, not a
# gap in the test — the column being unexposed is exactly why it can be
# trusted.
#
# Self-contained: own folders, own app password, all removed in teardown.
# =============================================================


# ─────────────────────────────────────────────────────────────
# Setup — JWT, an NC app password (X-OC-Mtime is NC-only), a
#         source folder and a move destination.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "{{username}}", "password": "{{password}}" }

HTTP 200
[Captures]
jwt: jsonpath "$.access_token"
admin_id: jsonpath "$.user.full.user.id"


POST {{base_url}}/api/auth/app-passwords
Authorization: Bearer {{jwt}}
Content-Type: application/json
{ "label": "content_clock_metadata_mutations" }

HTTP 200
[Captures]
nc_username: jsonpath "$.username"
nc_password: jsonpath "$.password"
ap_id:       jsonpath "$.id"


POST {{base_url}}/api/folders
Authorization: Bearer {{jwt}}
Content-Type: application/json
{
  "name": "ccmm-src"
}

HTTP 201
[Captures]
src_id: jsonpath "$.id"


POST {{base_url}}/api/folders
Authorization: Bearer {{jwt}}
Content-Type: application/json
{
  "name": "ccmm-dst"
}

HTTP 201
[Captures]
dst_id: jsonpath "$.id"


# ─────────────────────────────────────────────────────────────
# Seed one file with a content clock three years in the past.
# ─────────────────────────────────────────────────────────────
PUT {{base_url}}/remote.php/dav/files/{{username}}/ccmm-src/probe.txt
Content-Type: text/plain
X-OC-Mtime: 1700086400
[BasicAuth]
{{nc_username}}: {{nc_password}}
```
content whose clock must survive every metadata mutation below
```

HTTP 201


# Baseline. The folder holds exactly one file, so `items[0]` is it —
# positional access, because Hurl collapses a single-match `[?(...)]`
# filter to a scalar and `nth` then errors.
GET {{base_url}}/api/folders/{{src_id}}/resources?limit=50&resource_types=file
Authorization: Bearer {{jwt}}

HTTP 200
[Captures]
file_id: jsonpath "$.items[0].resource.id"
[Asserts]
jsonpath "$.items" count == 1
jsonpath "$.items[0].resource.modified_at" == "2023-11-15T22:13:20Z"


# ═════════════════════════════════════════════════════════════
# 1. FAVOURITE — a per-user flag, nothing to do with content
# ═════════════════════════════════════════════════════════════
POST {{base_url}}/api/favorites/file/{{file_id}}
Authorization: Bearer {{jwt}}

HTTP 201


GET {{base_url}}/api/folders/{{src_id}}/resources?limit=50&resource_types=file
Authorization: Bearer {{jwt}}

HTTP 200
[Asserts]
jsonpath "$.items[0].resource.modified_at" == "2023-11-15T22:13:20Z"
# Prove the mutation actually happened — otherwise an endpoint that
# silently no-ops would make this whole section pass for free.
jsonpath "$.items[0].resource.is_favorite" == true


# ═════════════════════════════════════════════════════════════
# 2. GRANT — sharing is not editing
# ═════════════════════════════════════════════════════════════
# Self-grant: the point is that writing a `role_grants` row does not
# touch the file, so the subject being the owner is irrelevant and
# saves provisioning a second user.
POST {{base_url}}/api/grants
Authorization: Bearer {{jwt}}
Content-Type: application/json
{
  "subject":  { "type": "user", "id": "{{admin_id}}" },
  "resource": { "type": "file", "id": "{{file_id}}" },
  "role": "viewer"
}

HTTP 201


GET {{base_url}}/api/folders/{{src_id}}/resources?limit=50&resource_types=file
Authorization: Bearer {{jwt}}

HTTP 200
[Asserts]
jsonpath "$.items[0].resource.modified_at" == "2023-11-15T22:13:20Z"


# ═════════════════════════════════════════════════════════════
# 3. MOVE — a different parent, the same bytes
# ═════════════════════════════════════════════════════════════
PUT {{base_url}}/api/files/{{file_id}}/move
Authorization: Bearer {{jwt}}
Content-Type: application/json
{
  "folder_id": "{{dst_id}}"
}

HTTP 200


# Read from the DESTINATION now — which also proves the move landed.
GET {{base_url}}/api/folders/{{dst_id}}/resources?limit=50&resource_types=file
Authorization: Bearer {{jwt}}

HTTP 200
[Asserts]
jsonpath "$.items" count == 1
jsonpath "$.items[0].resource.id" == "{{file_id}}"
jsonpath "$.items[0].resource.modified_at" == "2023-11-15T22:13:20Z"


# ═════════════════════════════════════════════════════════════
# 4. TRASH then RESTORE — a soft-delete flag round trip
# ═════════════════════════════════════════════════════════════
# Worth covering as a pair: restore is the one of the five that needs no
# code to preserve the clock (it flips `is_trashed`, so the value
# survives by construction) — which is exactly why a regression here
# would be silent.
DELETE {{base_url}}/api/trash/files/{{file_id}}
Authorization: Bearer {{jwt}}

HTTP 200


POST {{base_url}}/api/trash/{{file_id}}/restore
Authorization: Bearer {{jwt}}

HTTP 200


GET {{base_url}}/api/folders/{{dst_id}}/resources?limit=50&resource_types=file
Authorization: Bearer {{jwt}}

HTTP 200
[Asserts]
jsonpath "$.items" count == 1
jsonpath "$.items[0].resource.modified_at" == "2023-11-15T22:13:20Z"


# ═════════════════════════════════════════════════════════════
# Control — a real content change DOES move it
# ═════════════════════════════════════════════════════════════
# Without this the file proves only that the clock never moves, which a
# frozen column would satisfy too. Overwriting with no `X-OC-Mtime`
# must leave it reporting something other than the 2023 instant.
PUT {{base_url}}/remote.php/dav/files/{{username}}/ccmm-dst/probe.txt
Content-Type: text/plain
[BasicAuth]
{{nc_username}}: {{nc_password}}
```
genuinely different bytes, no asserted mtime
```

HTTP 204


GET {{base_url}}/api/folders/{{dst_id}}/resources?limit=50&resource_types=file
Authorization: Bearer {{jwt}}

HTTP 200
[Asserts]
jsonpath "$.items[0].resource.modified_at" != "2023-11-15T22:13:20Z"


# ═════════════════════════════════════════════════════════════
# Teardown
# ═════════════════════════════════════════════════════════════
DELETE {{base_url}}/api/folders/{{dst_id}}
Authorization: Bearer {{jwt}}

HTTP 204


DELETE {{base_url}}/api/folders/{{src_id}}
Authorization: Bearer {{jwt}}

HTTP 204


DELETE {{base_url}}/api/auth/app-passwords/{{ap_id}}
Authorization: Bearer {{jwt}}

HTTP 200
