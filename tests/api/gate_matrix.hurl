# =============================================================
# OxiCloud — GATE MATRIX: the cells no other api test covers
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
# ─── The four axes, and where each is pinned
#
#   a visitor REACHES its allowed routes
#       → anonymous_share_session.hurl: GET /api/files/{id},
#         /metadata, /thumbnail, /api/folders/{id} all 200 from a ring
#   a visitor is REFUSED everything else — four directions, because
#   each is a different way a declaration can leak:
#       same path, other verb   → DELETE /api/files/{id}        403
#       upward, the collection  → GET /api/folders              403
#       downward, a subpath     → GET /api/files/{id}/manifest  403
#       sibling user-only reads → /api/auth/me, /api/groups/search,
#                                 /api/address-books            403
#       the role ladder         → GET /api/admin/users          403
#       the DAV surfaces        → /webdav, /caldav, /carddav     401
#   ADMIN required
#       → this file: plain user on GET /api/admin/users         403
#       → admin_role_hierarchy.hurl: the full ladder
#   OWNER required
#       → admin_role_hierarchy.hurl: four transfer-ownership
#         requests — admin refused 403, owner 200
#
#   No separate owner-vs-visitor row exists, and should not:
#   transfer-ownership is the only Owner-gated operation and sits
#   under `/api/admin`, whose nest declares `role(Admin)`. A visitor
#   is refused there before Owner is evaluated.
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
# ─── Structure: authenticated FIRST, then logout, then anonymous
#
# Hurl keeps ONE cookie jar per file and `POST /api/auth/login` sets
# `oxicloud_access` (`cookie_auth::append_auth_cookies`), so every
# entry after a login is cookie-authenticated whether or not it
# carries an `Authorization` header. The first draft of this file
# got a 207 on an "anonymous" PROPFIND for exactly that reason — it
# was the plain user's own session. So the anonymous section comes
# after a logout and opens with a CANARY: if the jar leaks, the
# canary fails loudly instead of the section silently becoming a
# test of the logged-in user's permissions.
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
# PART A — AUTHENTICATED positives
# =============================================================
# A gate that refuses EVERYONE satisfies every refusal assertion in
# part B, which is the failure mode AGENTS.md calls out for
# `external_users.hurl`. So the positives come first and are the
# half that catches over-tightening.
# =============================================================

# /webdav for a plain internal user — the surface with no coverage
# in tests/api, and exactly what `require_internal_user_layer`
# admits. A fold that over-tightened it to `MinRole::Admin` shows up
# here and nowhere else in the suite.
PROPFIND {{base_url}}/webdav/
Authorization: Bearer {{plain_token}}
Depth: 0

HTTP *
[Asserts]
status != 401
status != 403

# And for the owner.
PROPFIND {{base_url}}/webdav/
Authorization: Bearer {{owner_token}}
Depth: 0

HTTP *
[Asserts]
status != 401
status != 403

# A NON-allowlisted route stays reachable for a plain user, so the
# allowlist is not accidentally made exclusive by the fold.
GET {{base_url}}/api/files/recent-changes
Authorization: Bearer {{plain_token}}

HTTP *
[Asserts]
status != 401
status != 403

# The admin surface refuses a plain user: 403, not 401 — the caller
# IS authenticated, just not an admin. This is what separates a
# folded admin gate from a policy that merely demands a session.
GET {{base_url}}/api/admin/users
Authorization: Bearer {{plain_token}}
HTTP 403

# A bad DPoP proof must not become acceptable because a gate above
# it moved, and must not 500. `require_dpop_layer` is NOT folded
# (§ Not axes: integrity checks), and no policy dump would show it
# moving, since an integrity check is not an axis.
GET {{base_url}}/api/folders
Authorization: Bearer {{plain_token}}
DPoP: not-a-valid-proof

HTTP *
[Asserts]
status != 500


# ─────────────────────────────────────────────────────────────
# End of the authenticated session. Bearer is CSRF-exempt, and this
# clears `oxicloud_access` / `oxicloud_refresh` / `oxicloud_csrf`
# while leaving other cookies alone.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/logout
Authorization: Bearer {{plain_token}}

HTTP *
[Asserts]
status != 500


# =============================================================
# PART B — ANONYMOUS, no credential at all
# =============================================================

# CANARY. If the cookie jar still holds a session, this returns 200
# and every assertion below becomes a test of that user's
# permissions — passing for the wrong reason.
#
# Asserted as "not 200" rather than a hard code on purpose: a
# visitor holding a share ring gets 403 here (the handler refuses a
# profile read — `anonymous_share_session.hurl`), while a caller
# with NO credential is refused by `auth_middleware` before the
# handler. Both are correct answers to "no session"; only 200 means
# the jar leaked, and that is the thing this guards.
GET {{base_url}}/api/auth/me

HTTP *
[Asserts]
status != 200
status < 500

# /webdav unauthenticated. 401 WITH a challenge: DAV clients only
# send credentials after a 401 carrying `WWW-Authenticate`
# (regression #480), so the header is part of the contract and not
# incidental.
PROPFIND {{base_url}}/webdav/
Depth: 0

HTTP 401
[Asserts]
header "WWW-Authenticate" exists

PROPFIND {{base_url}}/caldav/
HTTP 401
[Asserts]
header "WWW-Authenticate" exists

PROPFIND {{base_url}}/carddav/
HTTP 401
[Asserts]
header "WWW-Authenticate" exists

GET {{base_url}}/api/admin/users
HTTP 401

POST {{base_url}}/api/admin/transfer-ownership
Content-Type: application/json
{ "new_owner_id": "{{plain_id}}" }
HTTP 401

# Allowlisted vs NOT, under one subtree. `ANONYMOUS_ALLOWLIST` holds
# seven exact routes, and "allowlisted" means a visitor RING may
# reach it — never that no credential may, which is the distinction
# `MinPrincipal`'s three states exist to make.
#
# This pair is the pin: if step 4 declares `visitor_allowed()` on
# the subtree instead of the seven routes, the second changes and
# the first does not.
GET {{base_url}}/api/files/00000000-0000-0000-0000-000000000000
HTTP 401

GET {{base_url}}/api/files/recent-changes
HTTP 401

# A DPoP header is not a credential.
GET {{base_url}}/api/folders
DPoP: not-a-valid-proof
HTTP 401


# ─────────────────────────────────────────────────────────────
# Teardown — the suite shares one database, and cleanup does not
# run when an assertion fails above, so a leaked fixture makes the
# NEXT run fail on a duplicate username and look like a bug in
# whatever ran then.
#
# The owner's Bearer is still valid: the logout above ended the
# PLAIN user's session, not this one.
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/api/admin/users/{{plain_id}}
Authorization: Bearer {{owner_token}}

HTTP *
[Asserts]
status != 500
