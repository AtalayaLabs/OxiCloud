# =============================================================
# Self-service session management — GET/DELETE /api/auth/sessions
#
# The user-scoped mirror of /api/admin/sessions. Before it existed a
# user could not see their own sessions at all: the only session
# surfaces were `POST /api/auth/logout` (the current one), the
# device-flow list (`origin = 'device'` only — TVs and CLIs, never a
# browser), and the admin endpoints. Someone who left a laptop logged
# in at the office had to ask an administrator.
#
# Fixture: Bob with TWO sessions, Alice with one. What is pinned is
# SCOPE, because that is the whole risk of giving users a version of
# an admin endpoint:
#
#   1. Bob can list his sessions
#   2. and sees ONLY his — Alice's row must not appear
#   3. Bob revokes his SECOND session: it is kicked, while his own
#      session AND Alice's both survive
#   4. Bob revokes SOMEONE ELSE's session: 404, and it SURVIVES
#   5. Bob revokes his CURRENT session: allowed, and he is kicked
#   6. a malformed id is 400, not 404 or 500
#
# Assertion 4 is the one that would be a vulnerability rather than a
# bug, and it has two halves on purpose: the status code alone would
# pass against an implementation that revoked the session and THEN
# returned 404. The survival check is what proves nothing happened.
#
# 404 rather than 403 for a session that exists but belongs to someone
# else: session ids are unguessable, but distinguishing "not yours"
# from "no such session" would make this endpoint an oracle for "is
# this id a live session". The audit line carries the real reason
# (`session.revocation_rejected`, whose `reason` separates the two).
#
# Hurl note, learned the hard way elsewhere in this suite (see
# `calendar_rest.hurl`): Hurl >= 8.1 coerces a ONE-element filter
# result to a scalar, so `count` and `nth 0` fail on it with "invalid
# filter input type". Hence no `count` on a filter here — and the same
# coercion is what lets a single-match filter be captured directly as
# the id string.
# =============================================================


# ─────────────────────────────────────────────────────────────
# 0 — Admin, for fixture setup and teardown. Teardown must not
#     depend on Bob's token, since step 5 deliberately kills it.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "{{username}}", "password": "{{password}}" }

HTTP 200
[Captures]
admin_token: jsonpath "$.access_token"


# ─────────────────────────────────────────────────────────────
# 1 — Two users. Admin-created, so `email_verified_at` is stamped
#     and they can log in whatever the verified-email policy is.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/admin/users
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "username": "own-sessions-bob",
  "email": "own-sessions-bob@example.com",
  "password": "OwnSessionsBob1!",
  "role": "user"
}

HTTP 201
[Captures]
bob_id: jsonpath "$.user.id"


POST {{base_url}}/api/admin/users
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "username": "own-sessions-alice",
  "email": "own-sessions-alice@example.com",
  "password": "OwnSessionsAlice1!",
  "role": "user"
}

HTTP 201
[Captures]
alice_id: jsonpath "$.user.id"


# ─────────────────────────────────────────────────────────────
# 2 — Alice logs in. Hers is the session that must survive every
#     one of Bob's actions below — both the refused one and the
#     successful one.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "own-sessions-alice", "password": "OwnSessionsAlice1!" }

HTTP 200
[Captures]
alice_access: jsonpath "$.access_token"


# ─────────────────────────────────────────────────────────────
# 3 — Alice reads her own list. Two jobs: assertion 1 for an
#     ordinary user with no admin role at all, and capturing a
#     session id that is hers and NOT Bob's — the id Bob gets
#     refused on in step 8.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/auth/sessions
Authorization: Bearer {{alice_access}}

HTTP 200
[Captures]
alice_session_id: jsonpath "$.sessions[0].id"
[Asserts]
jsonpath "$.sessions[0].user_id" == "{{alice_id}}"
# The row she is reading this through is flagged, so a UI can warn
# before she revokes the session the click came from.
jsonpath "$.sessions[0].is_current" == true


# ─────────────────────────────────────────────────────────────
# 4 — Bob logs in TWICE. The SECOND login owns the cookie jar and
#     is therefore his current session; the first is the one he
#     will revoke in step 6.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "own-sessions-bob", "password": "OwnSessionsBob1!" }

HTTP 200
[Captures]
bob_second: jsonpath "$.access_token"


POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "own-sessions-bob", "password": "OwnSessionsBob1!" }

HTTP 200
[Captures]
bob_current: jsonpath "$.access_token"


# ─────────────────────────────────────────────────────────────
# 5 — Assertions 1 and 2. Bob's list shows Bob's sessions and no
#     one else's. Alice has a live session right now, so a missing
#     or widened filter would put her here.
#
#     `body not contains` rather than a filter+count: see the Hurl
#     note in the header. Alice's id cannot legitimately appear
#     anywhere in a correct response, so containment is the same
#     claim a filter count would have made.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/auth/sessions
Authorization: Bearer {{bob_current}}

HTTP 200
[Captures]
# Index, not an `is_current` filter. Rows come back
# `ORDER BY created_at DESC`, so [0] is the second login (the session
# this request is riding) and [1] is the first — the one to revoke.
#
# A filter would be the clearer expression of intent, but with both
# rows non-current it would match TWO elements and the capture would
# take an array instead of an id. If the ordering ever flips, step 7's
# two assertions fail immediately rather than this passing for the
# wrong reason.
bob_doomed_session: jsonpath "$.sessions[1].id"
[Asserts]
jsonpath "$.sessions" count == 2
jsonpath "$.sessions[*].user_id" contains "{{bob_id}}"
body not contains "{{alice_id}}"
body not contains "{{alice_session_id}}"
# The row this request is riding is flagged, so a UI can warn before
# the user revokes the session the click came from. True here for a
# plain PASSWORD login, which is the point: `is_current` used to
# require both sides DPoP-bound and was therefore always false for
# clients without WebCrypto — see
# `is_current_matches_on_session_id_without_dpop`.
jsonpath "$.sessions[0].is_current" == true
jsonpath "$.sessions[1].is_current" == false


# ─────────────────────────────────────────────────────────────
# 6 — Assertion 3: Bob revokes his SECOND (non-current) session.
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/api/auth/sessions/{{bob_doomed_session}}
Authorization: Bearer {{bob_current}}

HTTP 200


# ─────────────────────────────────────────────────────────────
# 7 — The three halves of assertion 3.
#
#     The revoked session is refused IMMEDIATELY, not within the
#     flags-cache TTL: `revoke_own_session` drops the cached flags
#     for exactly this reason. Without that drop a button
#     reporting success would do nothing for up to 30 s.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/auth/me
Authorization: Bearer {{bob_second}}

HTTP 401


# Bob's own session is untouched — a revoke-other must not take the
# caller down with it.
GET {{base_url}}/api/auth/me
Authorization: Bearer {{bob_current}}

HTTP 200


# And Alice is untouched. Revocation is scoped to one user; this is
# the cross-user half, after a SUCCESSFUL revoke rather than a
# refused one.
GET {{base_url}}/api/auth/me
Authorization: Bearer {{alice_access}}

HTTP 200


# ─────────────────────────────────────────────────────────────
# 8 — Assertion 4, the security one: Bob tries to revoke ALICE's
#     session. 404 — the same answer as for an id that does not
#     exist at all.
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/api/auth/sessions/{{alice_session_id}}
Authorization: Bearer {{bob_current}}

HTTP 404


# …and Alice is STILL logged in. Without this, the step above would
# pass against an implementation that revoked the session and then
# returned 404: the status code proves the refusal was REPORTED, not
# that it happened.
GET {{base_url}}/api/auth/me
Authorization: Bearer {{alice_access}}

HTTP 200


# ─────────────────────────────────────────────────────────────
# 9 — A session id that exists nowhere: also 404, the same shape
#     as "not yours". That sameness IS the anti-enumeration
#     property — the two cases must be indistinguishable on the
#     wire.
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/api/auth/sessions/00000000-0000-0000-0000-00000000dead
Authorization: Bearer {{bob_current}}

HTTP 404


# ─────────────────────────────────────────────────────────────
# 10 — Assertion 6: a malformed id is 400 from the handler's parse,
#      before any lookup. Distinct from 404 deliberately — a client
#      sending garbage has a bug, not a permission problem, and
#      collapsing the two would hide it.
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/api/auth/sessions/not-a-uuid
Authorization: Bearer {{bob_current}}

HTTP 400


# ─────────────────────────────────────────────────────────────
# 11 — Assertion 5: Bob revokes his CURRENT session.
#
#      Allowed on purpose. It is a logout that happens to be
#      spelled as a revoke, and refusing it would mean a user
#      looking at their session list cannot act on the row they
#      most want to act on — the laptop they are holding. The
#      `is_current` flag is what makes the choice informed.
#
#      One session is left by now (step 6 revoked the other, and
#      revoked rows are hidden without `include_revoked`), so the
#      filter matches exactly one element and coerces to the id
#      string.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/auth/sessions
Authorization: Bearer {{bob_current}}

HTTP 200
[Captures]
bob_current_session: jsonpath "$.sessions[0].id"
[Asserts]
# Exactly one row left, so [0] IS the current session — no filter
# needed, and the count assertion is what makes the index safe.
jsonpath "$.sessions" count == 1
jsonpath "$.sessions[0].is_current" == true


DELETE {{base_url}}/api/auth/sessions/{{bob_current_session}}
Authorization: Bearer {{bob_current}}

HTTP 200


# ─────────────────────────────────────────────────────────────
# 12 — Bob is kicked. His own token no longer authenticates, which
#      is the point: the revoke applied to the session the request
#      itself was riding.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/auth/me
Authorization: Bearer {{bob_current}}

HTTP 401


# Alice, who has done nothing this whole time, is still logged in.
GET {{base_url}}/api/auth/me
Authorization: Bearer {{alice_access}}

HTTP 200


# ─────────────────────────────────────────────────────────────
# 13 — Teardown. Required: this suite shares one database, so a
#      leaked fixture makes a later run fail for an unrelated
#      reason. Uses the ADMIN token, which is why step 0 captured
#      one — Bob's is deliberately dead by now.
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/api/admin/users/{{bob_id}}
Authorization: Bearer {{admin_token}}

HTTP 200


DELETE {{base_url}}/api/admin/users/{{alice_id}}
Authorization: Bearer {{admin_token}}

HTTP 200
