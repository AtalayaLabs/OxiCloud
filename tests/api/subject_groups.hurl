# =============================================================
# OxiCloud — ReBAC subject groups (POST/GET/PATCH/DELETE /api/groups)
# =============================================================
# Exercises:
#   • Admin-only gate on mutations (403 for non-admin)
#   • CRUD happy path (create / list / get / rename / delete)
#   • RFC 5321 name validation (400 on invalid names)
#   • Membership add/remove (users + nested groups)
#   • Cycle prevention (400 on circular reference)
#   • Authorization cascade: a user reaches a resource through group
#     membership (the headline feature)
#   • Authenticated /api/groups/search (non-admin can search)
#
# Runs after permissions.hurl + grants.hurl (alice = admin, bob = user).
# Self-contained group names ("grp-...-hurl") so it doesn't depend on
# external state and won't collide with other test files.
# =============================================================


# ─────────────────────────────────────────────────────────────
# Step 1 — Login as admin (alice) + create a second user (grace).
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "{{username}}", "password": "{{password}}" }

HTTP 200
[Captures]
alice_token: jsonpath "$.access_token"

POST {{base_url}}/api/admin/users
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{ "username": "grace", "password": "GracePassword1!", "email": "grace@example.com", "role": "user" }

HTTP 201
[Captures]
grace_user_id: jsonpath "$.user.id"

POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "grace", "password": "GracePassword1!" }

HTTP 200
[Captures]
grace_token: jsonpath "$.access_token"


# ─────────────────────────────────────────────────────────────
# Step 2 — Non-admin cannot create groups (403).
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/groups
Authorization: Bearer {{grace_token}}
Content-Type: application/json
{ "name": "grp-forbidden-hurl" }

HTTP 403


# ─────────────────────────────────────────────────────────────
# Step 3 — RFC 5321 name validation: space, leading dot, non-ASCII rejected.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/groups
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{ "name": "grp with space" }

HTTP 400

POST {{base_url}}/api/groups
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{ "name": ".leadingdot" }

HTTP 400

POST {{base_url}}/api/groups
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{ "name": "équipe" }

HTTP 400


# ─────────────────────────────────────────────────────────────
# Step 4 — CRUD happy path: create + list + get + rename.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/groups
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{ "name": "grp-engineers-hurl", "description": "engineering team" }

HTTP 201
[Captures]
engineers_id: jsonpath "$.id"
[Asserts]
jsonpath "$.name" == "grp-engineers-hurl"
jsonpath "$.is_virtual" == false
jsonpath "$.member_count" == 0
jsonpath "$.can_manage" == true

# Duplicate name rejected (case-insensitive via CITEXT).
POST {{base_url}}/api/groups
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{ "name": "Grp-Engineers-Hurl" }

HTTP 409

# GET by id.
GET {{base_url}}/api/groups/{{engineers_id}}
Authorization: Bearer {{alice_token}}

HTTP 200
[Asserts]
jsonpath "$.id" == "{{engineers_id}}"
jsonpath "$.name" == "grp-engineers-hurl"

# Rename.
PATCH {{base_url}}/api/groups/{{engineers_id}}
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{ "name": "grp-engineering-hurl" }

HTTP 200
[Asserts]
jsonpath "$.name" == "grp-engineering-hurl"


# ─────────────────────────────────────────────────────────────
# Step 5 — Add grace as a member of the group.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/groups/{{engineers_id}}/members
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{ "user_id": "{{grace_user_id}}" }

HTTP 201

# GET the group again — member_count now reflects the add.
GET {{base_url}}/api/groups/{{engineers_id}}
Authorization: Bearer {{alice_token}}

HTTP 200
[Asserts]
jsonpath "$.member_count" == 1

# List members shows grace.
GET {{base_url}}/api/groups/{{engineers_id}}/members
Authorization: Bearer {{alice_token}}

HTTP 200
[Asserts]
jsonpath "$[*].kind" contains "user"
jsonpath "$[*].id"   contains "{{grace_user_id}}"

# Idempotency: adding the same member twice is rejected with 409.
POST {{base_url}}/api/groups/{{engineers_id}}/members
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{ "user_id": "{{grace_user_id}}" }

HTTP 409


# ─────────────────────────────────────────────────────────────
# Step 6 — Cycle prevention: nest engineering inside qa, then try the loop.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/groups
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{ "name": "grp-qa-hurl" }

HTTP 201
[Captures]
qa_id: jsonpath "$.id"

# Add engineering as a member of qa (qa ∋ engineering).
POST {{base_url}}/api/groups/{{qa_id}}/members
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{ "group_id": "{{engineers_id}}" }

HTTP 201

# Now try the reverse — engineering ∋ qa — should be rejected as a cycle.
POST {{base_url}}/api/groups/{{engineers_id}}/members
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{ "group_id": "{{qa_id}}" }

HTTP 400


# ─────────────────────────────────────────────────────────────
# Step 7 — Authorization cascade through the group.
#
# Alice creates a folder, grants read to the engineering group;
# grace (a transitive member via engineering ⊆ qa) reaches the folder.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/folders
Authorization: Bearer {{alice_token}}

HTTP 200
[Captures]
alice_home_id: jsonpath "$[0].id"

POST {{base_url}}/api/folders
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{ "name": "grp-shared-hurl", "parent_id": "{{alice_home_id}}" }

HTTP 201
[Captures]
shared_folder_id: jsonpath "$.id"

# Grant read to the engineering group on this folder.
POST {{base_url}}/api/grants
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{
    "subject": { "type": "group", "id": "{{engineers_id}}" },
    "resource": { "type": "folder", "id": "{{shared_folder_id}}" },
    "role": "viewer"
}

HTTP 201

# Grace (a member of engineering) can now list the folder's contents.
GET {{base_url}}/api/folders/{{shared_folder_id}}
Authorization: Bearer {{grace_token}}

HTTP 200


# ─────────────────────────────────────────────────────────────
# Step 8 — Self-defense on group remove_member.
#          A group must not drop to 0 transitive users once seeded —
#          without this guard, an admin could empty a group that owns
#          a shared drive (D3a), leaving the drive with no effective
#          Owner. Conservative-by-default: the rule applies to every
#          group, not just drive-owning ones.
#
#          So the first attempt to remove grace (the sole member) is
#          refused with 400. We then seed the group with a second user,
#          re-attempt the removal, and assert it now succeeds — the
#          authz cascade tests below depend on grace being out of the
#          group.
#
#          Note: the authz cache has a 30s TTL — Hurl tests run within
#          seconds so grace may still see the folder during the cache
#          window. We assert the membership removal succeeded; the
#          post-TTL denial is exercised by the Rust integration tests,
#          not here (test runtime cost).
# ─────────────────────────────────────────────────────────────
# 8a — First removal refused: grace is the sole transitive user.
DELETE {{base_url}}/api/groups/{{engineers_id}}/members/user/{{grace_user_id}}
Authorization: Bearer {{alice_token}}

HTTP 400


# 8b — Seed engineering with a second user so the removal can succeed.
POST {{base_url}}/api/admin/users
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{ "username": "grp_helper", "password": "GrpHelperPwd1!", "email": "grp_helper@example.com", "role": "user" }

HTTP 201
[Captures]
helper_user_id: jsonpath "$.user.id"


POST {{base_url}}/api/groups/{{engineers_id}}/members
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{ "user_id": "{{helper_user_id}}" }

HTTP 201


# 8c — Grace removal now succeeds: engineering still has grp_helper.
DELETE {{base_url}}/api/groups/{{engineers_id}}/members/user/{{grace_user_id}}
Authorization: Bearer {{alice_token}}

HTTP 204


# 8d — Confirming the invariant still holds: removing the last user
#      (grp_helper) is again refused.
DELETE {{base_url}}/api/groups/{{engineers_id}}/members/user/{{helper_user_id}}
Authorization: Bearer {{alice_token}}

HTTP 400


# ─────────────────────────────────────────────────────────────
# Step 9 — Authenticated /api/groups/search (no admin role required).
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/groups/search?q=engineering
Authorization: Bearer {{grace_token}}

HTTP 200
[Asserts]
jsonpath "$[*].name" contains "grp-engineering-hurl"

# Virtual groups (Internal, future Everyone, …) are surfaced by the
# share-target search so they can be selected as grant subjects.
# Ordering puts virtuals first; see `subject_group_pg_repository::list`.
GET {{base_url}}/api/groups/search?q=Internal
Authorization: Bearer {{grace_token}}

HTTP 200
[Asserts]
jsonpath "$[*].name"       contains "Internal"
jsonpath "$[?(@.name=='Internal')].is_virtual" == true


# ─────────────────────────────────────────────────────────────
# Step 10 — Internal virtual group is immutable.
# ─────────────────────────────────────────────────────────────
# Internal group's well-known UUID is 00000000-0000-0000-0000-000000000001.
POST {{base_url}}/api/groups/00000000-0000-0000-0000-000000000001/members
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{ "user_id": "{{grace_user_id}}" }

HTTP 403

DELETE {{base_url}}/api/groups/00000000-0000-0000-0000-000000000001
Authorization: Bearer {{alice_token}}

HTTP 403


# ─────────────────────────────────────────────────────────────
# Step 11 — Sole-Owner group-delete guard (D3b).
#
# A group that is the only `Role::Owner` of a shared drive must NOT be
# deletable — wiping it would orphan the drive (no live Owner grant
# left). Symmetric to the last-owner-protection rule on `set_role` /
# `remove_member` from the membership API side; this guard catches
# the same invariant from the group-lifecycle side.
#
# Setup: admin creates a shared drive owned by `grp-engineering-hurl`,
# then tries to delete the group. Refused with 409. Promote a second
# Owner (a user), then the group delete succeeds.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/drives
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{
  "kind": "shared",
  "name": "grp-guarded-drive-hurl",
  "owner": { "type": "group", "id": "{{engineers_id}}" }
}

HTTP 201
[Captures]
guarded_drive_id: jsonpath "$.id"


# 11a — Group delete refused while it's the sole Owner of the drive.
DELETE {{base_url}}/api/groups/{{engineers_id}}
Authorization: Bearer {{alice_token}}

HTTP 409


# 11b — Add Grace as a co-Owner of the drive via the admin endpoint.
#       Alice (the OxiCloud admin) created the drive but doesn't
#       auto-grant herself a role on it, so she lacks `Manage` on the
#       user-facing `/api/drives/{id}/members` — the admin route
#       bypasses that check for exactly this case.
POST {{base_url}}/api/admin/drives/{{guarded_drive_id}}/members
Authorization: Bearer {{alice_token}}
Content-Type: application/json
{
  "subject": { "type": "user", "id": "{{grace_user_id}}" },
  "role": "owner"
}

HTTP 201


# 11c — Group delete now succeeds — the drive still has Grace as Owner.
DELETE {{base_url}}/api/groups/{{engineers_id}}
Authorization: Bearer {{alice_token}}

HTTP 204


# 11d — Cleanup: trash the drive (no content) so subsequent test files
#       don't see a dangling shared drive. After 11c, Grace is the
#       only remaining Owner via her direct grant, so she's the one
#       who can delete via the user-facing route.
DELETE {{base_url}}/api/drives/{{guarded_drive_id}}
Authorization: Bearer {{grace_token}}

HTTP 204


# ─────────────────────────────────────────────────────────────
# Step 12 — Cleanup: delete qa group.
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/api/groups/{{qa_id}}
Authorization: Bearer {{alice_token}}

HTTP 204

# Confirm gone.
GET {{base_url}}/api/groups/{{engineers_id}}
Authorization: Bearer {{alice_token}}

HTTP 404
