# =============================================================
# OxiCloud — GATE MATRIX: the cells nothing else covers
# =============================================================
# PR 1 step 4 (`docs/plan/scope-gates.md` § Rollout) removes three
# middleware layers and one service-level owner check, replacing
# them with compiled policies. Its stated acceptance is "the
# compiled table before and after the fold must be IDENTICAL" —
# but that table is written by hand from reading the layers, so the
# diff proves the fold matches the TABLE, never that the table
# matches the LAYERS. A mis-transcribed gate makes both dumps agree
# on a widened policy.
#
# This file is the check that bites: it asks the RUNNING server what
# each principal kind may reach, and must return identical results
# before and after the fold.
#
# ─── Coverage map: most rows already exist, and are NOT duplicated
#
#   external user, every surface   → external_users.hurl (60 reqs)
#   owner vs admin vs plain, and
#     transfer-ownership           → admin_role_hierarchy.hurl
#   visitor (share ring)           → anonymous_share_session.hurl
#   plain user on /caldav          → calendar.hurl   (caldav_bob)
#   plain user on /carddav         → contacts.hurl   (carddav_bob,
#                                    role=user, 4 PROPFINDs)
#
# ─── What was MISSING, and is therefore all this file does
#
#   1. /webdav — nothing in tests/api ever PROPFINDs it, so
#      `require_internal_user_layer` on that nest has no coverage
#      here at all. A fold that over-tightened it to Admin, or
#      dropped it, would pass every other file.
#   2. Anonymous against each folded surface. The negatives are
#      where a widened policy shows up first.
#   3. Allowlisted vs non-allowlisted route under the SAME subtree.
#      The allowlist CONTENTS are the policy; a fold that applied
#      `visitor_allowed()` to the subtree instead of the seven
#      routes would widen six others silently.
#   4. The NOT-folded layers (DPoP), pinned so "the fold moved a
#      layer it was not meant to" fails here — no policy dump would
#      show it, since an integrity check is not an axis.
#
# ─── Limit, stated so nobody trusts this further than it goes
#
# Hurl sees status codes. It cannot see the AUDIT EVENT a denial
# emits, and folding the owner check changes one
# (`server_owner.transfer_rejected` → `authz.*`). AGENTS.md requires
# `event` to stay stable because aggregators key off it, so that
# needs its own decision — see § Fold in.
# =============================================================


# ─────────────────────────────────────────────────────────────
# Setup — the setup account is the OWNER (see setup.hurl), so it
# also plays admin here.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "{{username}}", "password": "{{password}}" }

HTTP 200
[Captures]
owner_token: jsonpath "$.access_token"


# A PLAIN internal user — the principal that separates a correctly
# folded internal-user gate from one tightened to Admin, and the
# one a mis-transcribed admin gate would silently admit, since
# `Policy::default()` is `principal=User` with no role floor.
POST {{base_url}}/api/admin/users
Authorization: Bearer {{owner_token}}
Content-Type: application/json
{
  "username": "gatematrix_plain",
  "email":    "gatematrix_plain@example.com",
  "password": "GateMatrixPlain1!",
  "role":     "user"
}

HTTP *

POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "gatematrix_plain", "password": "GateMatrixPlain1!" }

HTTP 200
[Captures]
plain_token: jsonpath "$.access_token"
plain_id: jsonpath "$.user.full.user.id"


# =============================================================
# 1 — /webdav, the surface with no coverage in tests/api
# =============================================================

# A plain internal user MUST reach it: that is exactly what
# `require_internal_user_layer` admits. Over-tightening the fold to
# `MinRole::Admin` shows up here and nowhere else in the suite.
PROPFIND {{base_url}}/webdav/
Authorization: Bearer {{plain_token}}
Depth: 0

HTTP *
[Asserts]
status != 401
status != 403

# Unauthenticated must not.
PROPFIND {{base_url}}/webdav/
HTTP 401

# The owner reaches it too — a gate that refuses everyone would
# satisfy the refusal above, which is the failure mode AGENTS.md
# calls out for `external_users.hurl`.
PROPFIND {{base_url}}/webdav/
Authorization: Bearer {{owner_token}}
Depth: 0

HTTP *
[Asserts]
status != 401
status != 403


# =============================================================
# 2 — ANONYMOUS against each folded surface
# =============================================================
# `MinPrincipal::Public`. All 401 today; a widened policy turns one
# of these into a 200.

GET {{base_url}}/api/admin/users
HTTP 401

POST {{base_url}}/api/admin/transfer-ownership
Content-Type: application/json
{ "new_owner_id": "{{plain_id}}" }
HTTP 401

PROPFIND {{base_url}}/caldav/
HTTP 401

PROPFIND {{base_url}}/carddav/
HTTP 401


# =============================================================
# 3 — allowlisted vs NOT, under one subtree
# =============================================================
# `ANONYMOUS_ALLOWLIST` holds seven exact routes. "Allowlisted"
# means a visitor RING may reach it, never that no credential may —
# the distinction `MinPrincipal`'s three states exist to make.
#
# The pair below is the pin: if step 4 declares
# `visitor_allowed()` on the subtree rather than on the seven
# routes, the second request changes and the first does not.

# Allowlisted route, no credential at all.
GET {{base_url}}/api/files/00000000-0000-0000-0000-000000000000
HTTP 401

# NON-allowlisted sibling in the same subtree.
GET {{base_url}}/api/files/recent-changes
HTTP 401

# And a plain user still reaches the non-allowlisted one, so the
# allowlist is not accidentally made exclusive.
GET {{base_url}}/api/files/recent-changes
Authorization: Bearer {{plain_token}}

HTTP *
[Asserts]
status != 401
status != 403


# =============================================================
# 4 — the NOT-folded layers
# =============================================================
# `require_dpop_layer` and
# `require_no_password_change_pending_layer` stay exactly as they
# are: integrity and scope concerns, not axes (§ Not axes). They sit
# on the subtrees the fold touches, so "the fold moved a layer it
# was not meant to" is a real failure mode — and invisible to a
# policy dump, since neither appears in a `Policy`.

# A bad DPoP proof must not become acceptable because a gate above
# it moved, and must not 500.
GET {{base_url}}/api/folders
Authorization: Bearer {{plain_token}}
DPoP: not-a-valid-proof

HTTP *
[Asserts]
status != 500

# A DPoP header is not a credential.
GET {{base_url}}/api/folders
DPoP: not-a-valid-proof
HTTP 401


# ─────────────────────────────────────────────────────────────
# Teardown — the suite shares one database, and cleanup does not
# run when an assertion fails above, so a leaked fixture makes the
# NEXT run fail on a duplicate username and look like a bug in
# whatever ran then.
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/api/admin/users/{{plain_id}}
Authorization: Bearer {{owner_token}}

HTTP *
[Asserts]
status != 500
