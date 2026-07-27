# =============================================================
# OxiCloud — GET /api/folders/{id}/ancestors
# =============================================================
# Pins the shared-breadcrumb endpoint. Coverage:
#   1. Own personal drive: leaf returns full chain [root, sub, leaf]
#      with access_source.kind = "drive" + drive info.
#   2. Drive-root leaf: chain has a single element (the root itself).
#   3. Anti-enum: unknown UUID / no-Read → 404 (not 403).
#   4. Cross-user: ancestors_stranger can't read admin's folder → 404.
# =============================================================

# ─────────────────────────────────────────────────────────────
# Setup — admin login + ancestors_stranger provisioning
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


# Anti-enum registration: 200 whether ancestors_stranger existed or not.
POST {{base_url}}/api/auth/register
Content-Type: application/json
{
  "username": "ancestors_stranger",
  "email":    "ancestors_stranger@example.com",
  "password": "BobPassword1!"
}

HTTP 200


POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "ancestors_stranger", "password": "BobPassword1!" }

HTTP 200
[Captures]
ancestors_stranger_token: jsonpath "$.access_token"


# ─────────────────────────────────────────────────────────────
# Step 1 — Create a small tree under admin's home:
#           home > ancestors-test > child > grandchild
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/folders
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "name": "ancestors-test", "parent_id": "{{admin_home_id}}" }

HTTP 201
[Captures]
mid_folder_id: jsonpath "$.id"


POST {{base_url}}/api/folders
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "name": "child", "parent_id": "{{mid_folder_id}}" }

HTTP 201
[Captures]
child_folder_id: jsonpath "$.id"


POST {{base_url}}/api/folders
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "name": "grandchild", "parent_id": "{{child_folder_id}}" }

HTTP 201
[Captures]
leaf_folder_id: jsonpath "$.id"


# ─────────────────────────────────────────────────────────────
# Step 2 — Ancestors on the deepest leaf.
#          Chain must be root → mid → child → grandchild.
#          access_source.kind = "drive" (admin owns the personal drive).
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/folders/{{leaf_folder_id}}/ancestors
Authorization: Bearer {{admin_token}}

HTTP 200
[Asserts]
jsonpath "$.ancestors" count == 4
jsonpath "$.ancestors[0].id"        == "{{admin_home_id}}"
jsonpath "$.ancestors[0].parent_id" == null
# Every ancestor carries drive_id (post-2026-07-26 addition) so
# /files can derive the current drive without an extra `getFolder`.
# Folders in one chain share a drive; checking `isString` on the
# leaf is enough — Hurl can't cleanly assert field equality across
# path indices.
jsonpath "$.ancestors[0].drive_id"  isString
jsonpath "$.ancestors[3].drive_id"  isString
jsonpath "$.ancestors[1].id"        == "{{mid_folder_id}}"
jsonpath "$.ancestors[1].name"      == "ancestors-test"
jsonpath "$.ancestors[1].parent_id" == "{{admin_home_id}}"
jsonpath "$.ancestors[2].id"        == "{{child_folder_id}}"
jsonpath "$.ancestors[2].name"      == "child"
jsonpath "$.ancestors[3].id"        == "{{leaf_folder_id}}"
jsonpath "$.ancestors[3].name"      == "grandchild"
jsonpath "$.access_source.kind"        == "drive"
jsonpath "$.access_source.drive.id"    isString
jsonpath "$.access_source.drive.name"  isString
jsonpath "$.access_source.drive.kind"  == "personal"
# Subject enrichment (2026-07-27): field carries the SHARER
# (`role_grants.granted_by`), not the grantee. On admin's own personal
# drive the drive grant is self-seeded with `granted_by = admin`, so
# the assertion still resolves to `{{username}}` — but the semantic is
# "who shared this?" and would surface a different name on a folder
# shared with admin by someone else.
jsonpath "$.access_source.subject.kind" == "user"
jsonpath "$.access_source.subject.id"   isString
jsonpath "$.access_source.subject.name" == "{{username}}"
# Caller's role via the boundary grant (2026-07-27) — piggybacked on
# the same `role_grants` row that carries `granted_by`. Personal
# drive owner grant is `owner`.
jsonpath "$.access_source.caller_role"  == "owner"


# ─────────────────────────────────────────────────────────────
# Step 3 — Drive-root leaf. Chain is one element (the root).
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/folders/{{admin_home_id}}/ancestors
Authorization: Bearer {{admin_token}}

HTTP 200
[Asserts]
jsonpath "$.ancestors" count == 1
jsonpath "$.ancestors[0].id"        == "{{admin_home_id}}"
jsonpath "$.ancestors[0].parent_id" == null
jsonpath "$.access_source.kind"     == "drive"


# ─────────────────────────────────────────────────────────────
# Step 4 — Anti-enum: unknown-UUID and cross-user access both
#          return 404 (never 403). A well-formed UUID that
#          doesn't exist and a real folder the caller can't
#          Read produce the same shape — attackers can't
#          distinguish "no such folder" from "not yours."
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/folders/00000000-0000-0000-0000-000000000000/ancestors
Authorization: Bearer {{admin_token}}

HTTP 404


GET {{base_url}}/api/folders/{{leaf_folder_id}}/ancestors
Authorization: Bearer {{ancestors_stranger_token}}

HTTP 404


# Middleware-level 401 (no auth) is deliberately NOT tested here.
# Prior login steps in this file leave Hurl's cookie jar populated,
# so an omitted `Authorization:` header still authenticates via cookie
# and lands on the handler — which returns the endpoint's anti-enum
# 404 rather than the middleware 401. The middleware 401 case is
# pinned separately at the TOP of `search_basic.hurl`, before any
# login has run.


# ─────────────────────────────────────────────────────────────
# Step 5 — Teardown: recursive delete of the top folder takes
#          the whole subtree. `DELETE /api/folders/{id}` is a
#          soft-delete-to-trash — every downstream test that
#          expects an empty trash (`trash.hurl`, `trash_resources.hurl`,
#          …) would find our orphan. Follow up with `empty` so
#          the trash returns to its clean baseline.
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/api/folders/{{mid_folder_id}}
Authorization: Bearer {{admin_token}}

HTTP 204


DELETE {{base_url}}/api/trash/empty
Authorization: Bearer {{admin_token}}

HTTP 200
