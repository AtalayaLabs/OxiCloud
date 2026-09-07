# =============================================================
# OxiCloud — Baseline: subject-group effective-members
# =============================================================
# Pins the transitive group-expansion contract that the ReBAC
# permissions story relies on. The interesting case is nesting:
# adding *user_X* into *Group_B*, and *Group_B* into *Group_A*,
# must make *user_X* visible from `GET /api/groups/{A}/effective-members`
# — the call site that authz lookups walk.
#
# Coverage:
#   1. Admin creates `Group_A` and `Group_B`
#   2. Admin creates fixture user `dora-eff`
#   3. PUT dora into Group_B          (direct membership)
#   4. PUT Group_B into Group_A       (nested membership)
#   5. GET /api/groups/{B}/members              (direct only) → dora
#   6. GET /api/groups/{A}/members              (direct only) → Group_B
#                                                NOT dora (she's transitive)
#   7. GET /api/groups/{A}/effective-members    → contains dora
#   8. Cleanup: remove user, remove group-member, delete groups + user
#
# This single nested scenario is the load-bearing one — if the
# transitive walk regresses, the ReBAC engine silently grants
# 0 permissions to nested members.
# =============================================================


# ─────────────────────────────────────────────────────────────
# Setup — admin login
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "{{username}}", "password": "{{password}}" }

HTTP 200
[Captures]
admin_token: jsonpath "$.access_token"


# ─────────────────────────────────────────────────────────────
# 1 — Create both groups. Names use kebab-case so the RFC-5321
#     local-part validator accepts them.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/groups
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "name": "eff-group-a", "description": "outer group" }

HTTP 201
[Captures]
group_a_id: jsonpath "$.id"


POST {{base_url}}/api/groups
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "name": "eff-group-b", "description": "inner group" }

HTTP 201
[Captures]
group_b_id: jsonpath "$.id"


# ─────────────────────────────────────────────────────────────
# 2 — Create the fixture user. "dora-eff" — distinct from any
#     user created by other test files, so this is order-safe.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/admin/users
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "username": "dora-eff",
  "password": "DoraPassword1!",
  "email":    "dora-eff@example.com",
  "role":     "user"
}

HTTP 201
[Captures]
dora_id: jsonpath "$.user.id"


# ─────────────────────────────────────────────────────────────
# 3 — Put dora into Group_B (direct user member)
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/groups/{{group_b_id}}/members
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "user_id": "{{dora_id}}" }

HTTP 201


# ─────────────────────────────────────────────────────────────
# 4 — Put Group_B into Group_A (nested group member)
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/groups/{{group_a_id}}/members
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "group_id": "{{group_b_id}}" }

HTTP 201


# ─────────────────────────────────────────────────────────────
# 5 — Group_B direct membership: ONLY dora.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/groups/{{group_b_id}}/members
Authorization: Bearer {{admin_token}}

HTTP 200
[Asserts]
body contains "{{dora_id}}"


# ─────────────────────────────────────────────────────────────
# 6 — Group_A direct membership: Group_B, NOT dora. The
#     direct-members endpoint is non-transitive by contract;
#     mixing in transitive members here would silently
#     conflate the two surfaces.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/groups/{{group_a_id}}/members
Authorization: Bearer {{admin_token}}

HTTP 200
[Asserts]
body contains "{{group_b_id}}"
body not contains "{{dora_id}}"


# ─────────────────────────────────────────────────────────────
# 7 — HEADLINE: Group_A effective-members reaches dora.
#     A regression here is the canary for any change that
#     breaks transitive expansion in the ReBAC layer.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/groups/{{group_a_id}}/effective-members
Authorization: Bearer {{admin_token}}

HTTP 200
[Asserts]
body contains "{{dora_id}}"


# ─────────────────────────────────────────────────────────────
# 8 — Teardown.
#     The D3a self-defense in `subject_group_service::remove_member`
#     refuses any individual membership removal that would empty a
#     seeded group's transitive user set — which BOTH of these
#     would (removing Group_B from A leaves A with no users;
#     removing dora from B leaves B with no users). So we skip the
#     manual member-by-member unwind and just DELETE the groups
#     directly. `delete_group` cascades through `subject_group_members`
#     via FK and isn't subject to the per-remove guard.
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/api/groups/{{group_a_id}}
Authorization: Bearer {{admin_token}}
HTTP 204

DELETE {{base_url}}/api/groups/{{group_b_id}}
Authorization: Bearer {{admin_token}}
HTTP 204

DELETE {{base_url}}/api/admin/users/{{dora_id}}
Authorization: Bearer {{admin_token}}
HTTP 200
