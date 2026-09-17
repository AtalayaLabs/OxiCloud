# =============================================================
# Role hierarchy — Owner > Admin > User (issue #690)
#
# Before this, any admin could demote, deactivate, delete, or
# reset the password of any other admin — including the person
# who set the instance up. One rogue or compromised admin account
# could lock everyone else out.
#
# The rule is one strict rank comparison, and these requests pin
# the three consequences that matter. The sharpest is the
# password reset: it is a full account takeover, so "an admin may
# not reset a peer admin's password" is the assertion this file
# exists for.
#
# The setup account is the OWNER (see setup.hurl), so it plays
# the top of the hierarchy here.
#
# Coverage:
#   1. Owner creates two users; promotes one to admin
#   2. Admin CAN act on a plain user (the hierarchy still works
#      downward — a denial-only test would pass if everything
#      were broken)
#   3. Admin CANNOT reset a peer admin's password → 403
#   4. Admin CANNOT demote / deactivate / delete a peer → 403
#   5. NOBODY can touch the owner → 403
#   6. Admin cannot create another admin (no minting peers)
#   7. Self-directed admin actions → 400
#   8. Only the owner may transfer ownership; transfer moves it
#      and demotes the outgoing owner
#   9. Cleanup
# =============================================================


# ─────────────────────────────────────────────────────────────
# Setup — owner login (the /api/setup account)
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "{{username}}", "password": "{{password}}" }

HTTP 200
[Captures]
owner_token: jsonpath "$.access_token"
# Login returns AuthResponseDto, whose `user` is a SelfUserDto — which
# composes over FullUserDto, which composes over PublicUserDto. None of
# the three layers is serde-flattened, so the identity lives at
# `$.user.full.user`. (A `POST /api/admin/users` response is a
# FullUserDto, one layer shallower: `$.user.id` there.)
owner_id: jsonpath "$.user.full.user.id"

[Asserts]
jsonpath "$.user.full.user.role" == "owner"


# ─────────────────────────────────────────────────────────────
# 1 — Owner creates two users, then promotes one to admin.
#     Only the owner can do this: step 6 shows an admin cannot.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/admin/users
Authorization: Bearer {{owner_token}}
Content-Type: application/json
{
  "username": "hier-admin",
  "password": "HierAdminPass1!",
  "email":    "hier-admin@example.com",
  "role":     "user"
}

HTTP 201
[Captures]
peer_admin_id: jsonpath "$.user.id"


POST {{base_url}}/api/admin/users
Authorization: Bearer {{owner_token}}
Content-Type: application/json
{
  "username": "hier-victim",
  "password": "HierVictimPass1!",
  "email":    "hier-victim@example.com",
  "role":     "user"
}

HTTP 201
[Captures]
victim_id: jsonpath "$.user.id"


# Owner promotes hier-admin. Owner outranks Admin, so this is allowed.
PUT {{base_url}}/api/admin/users/{{peer_admin_id}}/role
Authorization: Bearer {{owner_token}}
Content-Type: application/json
{ "role": "admin" }

HTTP 200


# A second admin, so step 3 has a peer to attack.
PUT {{base_url}}/api/admin/users/{{victim_id}}/role
Authorization: Bearer {{owner_token}}
Content-Type: application/json
{ "role": "admin" }

HTTP 200


POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "hier-admin", "password": "HierAdminPass1!" }

HTTP 200
[Captures]
peer_admin_token: jsonpath "$.access_token"


# ─────────────────────────────────────────────────────────────
# 2 — Downward still works.
#
# Without this, every assertion below would pass on a server
# that refused everything. Create a plain user as the admin and
# act on them.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/admin/users
Authorization: Bearer {{peer_admin_token}}
Content-Type: application/json
{
  "username": "hier-plain",
  "password": "HierPlainPass1!",
  "email":    "hier-plain@example.com",
  "role":     "user"
}

HTTP 201
[Captures]
plain_id: jsonpath "$.user.id"


PUT {{base_url}}/api/admin/users/{{plain_id}}/quota
Authorization: Bearer {{peer_admin_token}}
Content-Type: application/json
{ "quota_bytes": 104857600 }

HTTP 200


PUT {{base_url}}/api/admin/users/{{plain_id}}/password
Authorization: Bearer {{peer_admin_token}}
Content-Type: application/json
{ "new_password": "ResetByAdmin1!" }

HTTP 200


# ─────────────────────────────────────────────────────────────
# 3 — An admin may NOT reset a peer admin's password.
#
# The takeover path: set a password you know, then log in as
# them. Refused on rank, so the credential is never changed —
# the next request proves the peer's original password still
# works.
# ─────────────────────────────────────────────────────────────
PUT {{base_url}}/api/admin/users/{{victim_id}}/password
Authorization: Bearer {{peer_admin_token}}
Content-Type: application/json
{ "new_password": "TakenOver1!" }

HTTP 403


POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "hier-victim", "password": "TakenOver1!" }

HTTP 403


POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "hier-victim", "password": "HierVictimPass1!" }

HTTP 200


# ─────────────────────────────────────────────────────────────
# 4 — Nor demote, deactivate, or delete a peer admin.
# ─────────────────────────────────────────────────────────────
PUT {{base_url}}/api/admin/users/{{victim_id}}/role
Authorization: Bearer {{peer_admin_token}}
Content-Type: application/json
{ "role": "user" }

HTTP 403


PUT {{base_url}}/api/admin/users/{{victim_id}}/active
Authorization: Bearer {{peer_admin_token}}
Content-Type: application/json
{ "active": false }

HTTP 403


DELETE {{base_url}}/api/admin/users/{{victim_id}}
Authorization: Bearer {{peer_admin_token}}

HTTP 403


PUT {{base_url}}/api/admin/users/{{victim_id}}/quota
Authorization: Bearer {{peer_admin_token}}
Content-Type: application/json
{ "quota_bytes": 1 }

HTTP 403


# ─────────────────────────────────────────────────────────────
# 5 — The owner is untouchable from below. Nothing outranks
#     them, so every administrative verb is refused.
# ─────────────────────────────────────────────────────────────
PUT {{base_url}}/api/admin/users/{{owner_id}}/password
Authorization: Bearer {{peer_admin_token}}
Content-Type: application/json
{ "new_password": "OwnerTakenOver1!" }

HTTP 403


PUT {{base_url}}/api/admin/users/{{owner_id}}/role
Authorization: Bearer {{peer_admin_token}}
Content-Type: application/json
{ "role": "user" }

HTTP 403


PUT {{base_url}}/api/admin/users/{{owner_id}}/active
Authorization: Bearer {{peer_admin_token}}
Content-Type: application/json
{ "active": false }

HTTP 403


DELETE {{base_url}}/api/admin/users/{{owner_id}}
Authorization: Bearer {{peer_admin_token}}

HTTP 403


# The owner's credentials are untouched by all of that.
POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "{{username}}", "password": "{{password}}" }

HTTP 200


# ─────────────────────────────────────────────────────────────
# 6 — An admin cannot mint a peer admin.
#
# Otherwise the hierarchy is trivially escapable: create an
# admin, and you have an account you are forbidden to manage.
# 'owner' is refused for everyone — ownership only moves.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/admin/users
Authorization: Bearer {{peer_admin_token}}
Content-Type: application/json
{
  "username": "hier-minted",
  "password": "HierMintedPass1!",
  "email":    "hier-minted@example.com",
  "role":     "admin"
}

HTTP 403


POST {{base_url}}/api/admin/users
Authorization: Bearer {{owner_token}}
Content-Type: application/json
{
  "username": "hier-minted-owner",
  "password": "HierMintedPass1!",
  "email":    "hier-minted-owner@example.com",
  "role":     "owner"
}

HTTP 403


# ─────────────────────────────────────────────────────────────
# 7 — Self-directed actions: THREE are refused, the rest are not.
#
#     Exactly these three, because each is its own hazard —
#     escalation confusion, an irreversible footgun, and locking
#     yourself out. 400 rather than 403: the request is malformed
#     in intent, not unauthorized.
#
#     The hierarchy itself says nothing about your own account,
#     and must not: folding self into the rank comparison forbids
#     every self-directed admin action at once, because nobody
#     outranks themselves. That regression shipped briefly and
#     broke an admin tightening their own quota — which is how
#     tests/webdav/test_nextcloud_chunked_upload_cap.sh reaches
#     the 507 path. The positive case at the end of this section
#     is what pins it.
# ─────────────────────────────────────────────────────────────
PUT {{base_url}}/api/admin/users/{{peer_admin_id}}/role
Authorization: Bearer {{peer_admin_token}}
Content-Type: application/json
{ "role": "user" }

HTTP 400


DELETE {{base_url}}/api/admin/users/{{peer_admin_id}}
Authorization: Bearer {{peer_admin_token}}

HTTP 400


PUT {{base_url}}/api/admin/users/{{peer_admin_id}}/active
Authorization: Bearer {{peer_admin_token}}
Content-Type: application/json
{ "active": false }

HTTP 400


# Setting your OWN quota is allowed — there was never a guard
# against it, and it is how the chunked-upload test drives a 507.
PUT {{base_url}}/api/admin/users/{{peer_admin_id}}/quota
Authorization: Bearer {{peer_admin_token}}
Content-Type: application/json
{ "quota_bytes": 52428800 }

HTTP 200


# So is re-activating yourself: a no-op on the account you are
# already authenticated on, and the asymmetry the previous guard
# had. Only the deactivate direction is a lockout.
PUT {{base_url}}/api/admin/users/{{peer_admin_id}}/active
Authorization: Bearer {{peer_admin_token}}
Content-Type: application/json
{ "active": true }

HTTP 200


# ─────────────────────────────────────────────────────────────
# 8 — Ownership transfer.
#
# Only the owner may transfer. The swap demotes the outgoing
# owner in the same transaction, so afterwards there is exactly
# one owner and the old one is a plain admin.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/admin/transfer-ownership
Authorization: Bearer {{peer_admin_token}}
Content-Type: application/json
{ "new_owner_id": "{{peer_admin_id}}" }

HTTP 403


# The owner cannot transfer to themselves.
POST {{base_url}}/api/admin/transfer-ownership
Authorization: Bearer {{owner_token}}
Content-Type: application/json
{ "new_owner_id": "{{owner_id}}" }

HTTP 400


POST {{base_url}}/api/admin/transfer-ownership
Authorization: Bearer {{owner_token}}
Content-Type: application/json
{ "new_owner_id": "{{peer_admin_id}}" }

HTTP 200


# hier-admin is the owner now…
POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "hier-admin", "password": "HierAdminPass1!" }

HTTP 200
[Captures]
new_owner_token: jsonpath "$.access_token"

[Asserts]
jsonpath "$.user.full.user.role" == "owner"


# …and the former owner was demoted to admin in the same swap,
# so they can no longer act on the account that now outranks
# them. This is the assertion that proves the transfer was a
# move rather than a copy.
POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "{{username}}", "password": "{{password}}" }

HTTP 200
[Captures]
demoted_token: jsonpath "$.access_token"

[Asserts]
jsonpath "$.user.full.user.role" == "admin"


PUT {{base_url}}/api/admin/users/{{peer_admin_id}}/role
Authorization: Bearer {{demoted_token}}
Content-Type: application/json
{ "role": "user" }

HTTP 403


# ─────────────────────────────────────────────────────────────
# 9 — Transfer back, then clean up.
#
# The suite shares one database, so the setup account must end
# this file as the owner — later files assume it (and so does a
# re-run of this one).
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/admin/transfer-ownership
Authorization: Bearer {{new_owner_token}}
Content-Type: application/json
{ "new_owner_id": "{{owner_id}}" }

HTTP 200


POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "{{username}}", "password": "{{password}}" }

HTTP 200
[Captures]
restored_owner_token: jsonpath "$.access_token"

[Asserts]
jsonpath "$.user.full.user.role" == "owner"


DELETE {{base_url}}/api/admin/users/{{peer_admin_id}}
Authorization: Bearer {{restored_owner_token}}

HTTP 200


DELETE {{base_url}}/api/admin/users/{{victim_id}}
Authorization: Bearer {{restored_owner_token}}

HTTP 200


DELETE {{base_url}}/api/admin/users/{{plain_id}}
Authorization: Bearer {{restored_owner_token}}

HTTP 200
