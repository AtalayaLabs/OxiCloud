# =============================================================
# OxiCloud — revocation stops a session NOW, not at renewal
# =============================================================
# `auth.sessions.revoked` lives on the row holding the REFRESH
# token, and the auth middleware reads `claims.sid` only to feed
# the last-seen tracker. So every form of revocation used to mean
# "cannot renew" while the access token already issued kept
# working for the rest of its hour:
#
#   * `POST /api/auth/logout` logged nobody out until their token
#     expired.
#   * `DELETE /api/admin/sessions/{id}` was "stop renewing".
#   * `change_password` left the other devices usable.
#
# Closed by two mechanisms, pinned separately here because they
# are independent and fail differently:
#
#   * `auth.users.sessions_valid_from` — a user-wide `iat`
#     watermark, for revoke-ALL.
#   * `auth.sessions.revoked_at` + a bounded revoked-sid set on
#     the cached flags — per-session, which a single watermark
#     cannot express ("all except this one").
#
# Coverage:
#   1. Fixture user with two concurrent sessions
#   2. logout kills ITS OWN access token immediately
#   3. change-password kills the OTHER device, keeps the caller's,
#      and leaves ANOTHER USER's session alone
#   4. admin reset kills everything for that user via the user-wide
#      cutoff — and still nobody else's
#   5. Teardown
#
# Not covered here: an app password or a WebSocket session, which
# carry no `sid` and so cannot be reached by per-session
# revocation at all. An app password is revoked by DELETING it
# (`app_passwords.hurl`); the deactivation path is in
# `auth_doors_deactivated_account.hurl`.
# =============================================================


# ─────────────────────────────────────────────────────────────
# 0 — Admin, for fixture setup and the step-4 reset.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "{{username}}", "password": "{{password}}" }

HTTP 200
[Captures]
admin_token: jsonpath "$.access_token"


# ─────────────────────────────────────────────────────────────
# 1 — Fixture user. Its own account, so revoking its sessions
#     cannot disturb a later scenario that logs in as admin —
#     and step 3 changes a password, which would otherwise
#     poison every subsequent file.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/admin/users
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "username": "revoke-probe",
  "password": "RevokeProbePass1!",
  "email":    "revoke-probe@example.com",
  "role":     "user"
}

HTTP 201
[Captures]
fixture_id: jsonpath "$.user.id"


# ─────────────────────────────────────────────────────────────
# 2 — Session A, and proof its access token works.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "revoke-probe", "password": "RevokeProbePass1!" }

HTTP 200
[Captures]
access_a: jsonpath "$.access_token"
refresh_a: jsonpath "$.refresh_token"

GET {{base_url}}/api/auth/me
Authorization: Bearer {{access_a}}

HTTP 200


# ─────────────────────────────────────────────────────────────
# 3 — LOGOUT kills its own access token.
#
# This is the assertion `auth_session_lifecycle.hurl` step 8
# used to say was impossible: "logout only kills the refresh
# path". It did, and that was the defect — logging out left the
# bearer able to read and write for the rest of the token's
# lifetime. Per-session revocation puts this `sid` in the
# revoked set, and the revoke path drops the cached flags, so
# the refusal lands on the very next request rather than within
# the 30 s TTL.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/logout
Authorization: Bearer {{access_a}}
Content-Type: application/json
{ "refresh_token": "{{refresh_a}}" }

HTTP 200

GET {{base_url}}/api/auth/me
Authorization: Bearer {{access_a}}

HTTP 401


# ─────────────────────────────────────────────────────────────
# 4 — Two concurrent sessions, B then C.
#
# ORDER MATTERS, and not for a cosmetic reason. The
# change-password handler derives `keep_session_id` from the
# refresh COOKIE, not from the Bearer token — and Hurl keeps a
# cookie jar across a file, so the second login overwrites the
# first's cookie. C therefore owns the cookie and is the session
# the handler keeps. Swapping these two logins would invert
# step 5's expectations while still "passing" against a broken
# implementation that revoked everything.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "revoke-probe", "password": "RevokeProbePass1!" }

HTTP 200
[Captures]
access_b: jsonpath "$.access_token"

POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "revoke-probe", "password": "RevokeProbePass1!" }

HTTP 200
[Captures]
access_c: jsonpath "$.access_token"

# Both live before the change.
GET {{base_url}}/api/auth/me
Authorization: Bearer {{access_b}}

HTTP 200

GET {{base_url}}/api/auth/me
Authorization: Bearer {{access_c}}

HTTP 200


# ─────────────────────────────────────────────────────────────
# 5 — CHANGE PASSWORD: the other device dies, the caller lives.
#
# The pair of assertions below is the whole point of
# per-session revocation. A user-wide `iat` cutoff cannot
# express "all except this one", so if the implementation ever
# falls back to one, the second assertion fails — and that
# matters beyond tidiness: the caller's session is kept
# deliberately so the SPA can finish re-registering its OPAQUE
# envelope on the cookie it already holds. Killing it would
# leave the envelope bound to the OLD password.
# ─────────────────────────────────────────────────────────────
PUT {{base_url}}/api/auth/change-password
Authorization: Bearer {{access_c}}
Content-Type: application/json
{ "current_password": "RevokeProbePass1!", "new_password": "RevokeProbePass2!" }

HTTP 200

# The OTHER device: revoked, and refused immediately.
GET {{base_url}}/api/auth/me
Authorization: Bearer {{access_b}}

HTTP 401

# The CALLER: still working.
GET {{base_url}}/api/auth/me
Authorization: Bearer {{access_c}}

HTTP 200

# ANOTHER USER: untouched. `revoke_other_user_sessions(user_id, keep)`
# and the cutoff are both scoped to one user_id, so a password change
# must not reach anyone else's sessions.
#
# This was previously only implicit — step 6 drives the admin reset with
# this same token, so a cross-user leak would already have failed the
# file. But it would have failed as a 401 on an admin endpoint, which
# reads like a permissions regression and sends the next person to the
# wrong layer. Asserted here so the failure names itself.
GET {{base_url}}/api/auth/me
Authorization: Bearer {{admin_token}}

HTTP 200


# ─────────────────────────────────────────────────────────────
# 6 — ADMIN PASSWORD RESET: the user-wide cutoff.
#
# The other mechanism, and the one that matters most in an
# incident: an admin resetting a password is often responding to
# a compromise, and the attacker's live access token used to
# keep working for the rest of its hour. `sessions_valid_from`
# rejects every token minted before the reset, so the caller's
# surviving session from step 5 dies here too — correctly, since
# this is revoke-ALL rather than revoke-others.
# ─────────────────────────────────────────────────────────────
PUT {{base_url}}/api/admin/users/{{fixture_id}}/password
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "new_password": "AdminResetProbe3!" }

HTTP 200

GET {{base_url}}/api/auth/me
Authorization: Bearer {{access_c}}

HTTP 401

# And again, ANOTHER USER is untouched. The cutoff is a per-user
# watermark (`auth.users.sessions_valid_from`), not a global one — if it
# were ever read from the wrong row, or applied globally, an admin
# resetting one password would sign out the whole instance. That is the
# kind of blast radius worth one explicit assertion.
GET {{base_url}}/api/auth/me
Authorization: Bearer {{admin_token}}

HTTP 200


# ─────────────────────────────────────────────────────────────
# 7 — And a token minted AFTER the cutoff works, so the pins
#     above are a revocation rather than a permanent lockout.
#     Were the comparison inverted, every assertion above would
#     still pass while the account was bricked.
#
#     The admin reset sets `force_password_change`, so this
#     login succeeds and the session is scoped to the
#     change-password flow — 200 here is the token being
#     ISSUED, which is all this step claims.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "revoke-probe", "password": "AdminResetProbe3!" }

HTTP 200


# ─────────────────────────────────────────────────────────────
# 8 — Teardown.
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/api/admin/users/{{fixture_id}}
Authorization: Bearer {{admin_token}}

HTTP 200
