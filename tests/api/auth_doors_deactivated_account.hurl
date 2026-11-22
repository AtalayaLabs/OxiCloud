# =============================================================
# OxiCloud — every login door refuses a deactivated account
# =============================================================
# A cross-cutting gate pin, in the role AGENTS.md gives
# `external_users.hurl`: one file asserting ONE property across
# every surface that can grant a credential, so a new door is
# visibly missing from it rather than silently uncovered.
#
# The property: a deactivated account cannot obtain or keep a
# credential through ANY door. Before the admission checks were
# unified, each door enforced a different subset — OPAQUE and
# the device flow enforced nothing, and `refresh` and the grace
# replay each hand-rolled their own `is_active`.
#
# Every scenario asserts BOTH legs, per AGENTS.md: the fixture
# succeeding while ACTIVE, then the same call refused once
# deactivated. A refusal-only pin passes against a gate that
# refuses everyone, and these doors' failure mode was being too
# permissive.
#
# Coverage:
#   1. Fixture user + app password, created while active
#   2. App password over Basic auth — works, then refused
#   3. Refresh — rotates, then refused
#   4. NC Login Flow v2 — grants, then refused; AND the
#      anti-enumeration half (wrong password reveals nothing)
#   5. Device-flow approve — refused (see 13b on which layer)
#   6. Teardown
#
# Covered elsewhere, deliberately not duplicated here:
#   * Classic `POST /api/auth/login` — `admin_user_ops.hurl`
#     steps 6/7 pin both halves plus the Gap-3 anti-enum split.
#   * OPAQUE `login/ke3` — `src/bin/opaque-hurl-helper.rs`; the
#     handshake needs real OPRF blinding, which cannot sit in a
#     .hurl body.
#
# Covered, but read what it proves (13b):
#   * Device-flow `approve()`. The outcome is asserted — a
#     disabled account cannot approve a device — but middleware
#     refuses the caller before `approve()` runs, so the row
#     would still pass with that door's own check deleted. The
#     status is pinned precisely so a change in WHICH layer
#     refuses shows up here.
#
# NOT coverable as a black-box HTTP test:
#   * The unverified-email half of all of the above. It needs a
#     server started with `OXICLOUD_REQUIRE_VERIFIED_EMAIL=true`
#     and a pre-seeded unverified user that already holds a
#     credential — with the flag ON such a user can never acquire
#     one. See `docs/plan/authn-pipeline.md` § Test plan.
# =============================================================


# ─────────────────────────────────────────────────────────────
# 0 — Admin login (the fixture owner).
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "{{username}}", "password": "{{password}}" }

HTTP 200
[Captures]
admin_token: jsonpath "$.access_token"


# ─────────────────────────────────────────────────────────────
# 1 — Fixture user, created ACTIVE.
#
# A dedicated name so a leak is obvious and so this file never
# deactivates an account another scenario logs in with. Note the
# ordering constraint that shapes this whole file: every
# credential below must be minted while the account still works,
# because acquiring one is itself gated on being able to log in.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/admin/users
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "username": "doors-deactivated",
  "password": "DoorsFixturePass1!",
  "email":    "doors-deactivated@example.com",
  "role":     "user"
}

HTTP 201
[Captures]
fixture_id: jsonpath "$.user.id"


# ─────────────────────────────────────────────────────────────
# 2 — Fixture logs in: captures the refresh token for scenario 3
#     and proves the account is healthy before anything is
#     asserted about refusals.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "doors-deactivated", "password": "DoorsFixturePass1!" }

HTTP 200
[Captures]
fixture_token: jsonpath "$.access_token"
fixture_refresh: jsonpath "$.refresh_token"


# ─────────────────────────────────────────────────────────────
# 3 — Fixture mints an app password (while active — it cannot
#     once deactivated, which is the point).
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/app-passwords
Authorization: Bearer {{fixture_token}}
Content-Type: application/json
{ "label": "doors-deactivated probe" }

HTTP 200
[Captures]
fixture_app_pw: jsonpath "$.password"
fixture_app_id: jsonpath "$.id"


# ─────────────────────────────────────────────────────────────
# 4 — ALLOWED leg: the app password works over Basic auth while
#     the account is active. Without this, scenario 7's refusal
#     would also pass against a credential that never worked.
# ─────────────────────────────────────────────────────────────
PROPFIND {{base_url}}/webdav/
Depth: 0
[BasicAuth]
doors-deactivated: {{fixture_app_pw}}

HTTP 207


# ─────────────────────────────────────────────────────────────
# 5 — ALLOWED leg: refresh rotates while active. Captures the
#     rotated token, because the original is single-use and
#     scenario 8 needs a live one to be refused.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/refresh
Content-Type: application/json
{ "refresh_token": "{{fixture_refresh}}" }

HTTP 200
[Captures]
fixture_refresh_v2: jsonpath "$.refresh_token"


# ─────────────────────────────────────────────────────────────
# 6 — Deactivate the fixture. Everything below must now refuse.
# ─────────────────────────────────────────────────────────────
PUT {{base_url}}/api/admin/users/{{fixture_id}}/active
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "active": false }

HTTP 200


# ─────────────────────────────────────────────────────────────
# 7 — APP PASSWORD door: refused.
#
# This door is not a session mint — every Basic-auth request IS
# a login — which is exactly why it was the door most easily
# forgotten. The response is the GENERIC 401 challenge, not a
# distinct "deactivated" shape: the real reason goes to the
# audit log while the caller learns nothing, because a DAV client
# could not act on it anyway and an unauthenticated prober must
# not learn that an account exists and is disabled.
# ─────────────────────────────────────────────────────────────
PROPFIND {{base_url}}/webdav/
Depth: 0
[BasicAuth]
doors-deactivated: {{fixture_app_pw}}

HTTP 401


# ─────────────────────────────────────────────────────────────
# 8 — REFRESH door: refused.
#
# `refresh_token` used to hand-roll this check with no audit
# line and no email half. The session row is still unrevoked —
# deactivation does not revoke sessions, it is caught per call —
# so this proves the admission check runs on the refresh path
# rather than the token merely being dead.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/refresh
Content-Type: application/json
{ "refresh_token": "{{fixture_refresh_v2}}" }

HTTP 403


# ─────────────────────────────────────────────────────────────
# 9 — The already-issued ACCESS token is refused too, through
#     the per-call live-role check. Pinned here because it is
#     the half that bounds the damage from scenario 8: a
#     deactivated account's existing token does not outlive the
#     deactivation, even though its session row is untouched.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/auth/me
Authorization: Bearer {{fixture_token}}

HTTP 401


# ─────────────────────────────────────────────────────────────
# 10 — NC LOGIN FLOW v2: initiate (unauthenticated, always
#      allowed — it mints no credential and names no account).
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/index.php/login/v2

HTTP 200
[Captures]
flow_token: jsonpath "$.poll.token"


# ─────────────────────────────────────────────────────────────
# 11 — NC LOGIN FLOW v2 door: refused for the deactivated
#      account even with the RIGHT password.
#
#      This door ends by handing the client an app password — a
#      long-lived credential — so it owes a session mint's
#      checks.
#
#      The response is a 303 to the generic
#      `?type=invalid-credentials` page: `login_failed_response`
#      DISCARDS the error, so this door cannot leak the reason
#      even though the service now distinguishes it. The service
#      check still earns its place by running AFTER the password
#      verify (it used to run before) and by adding the email
#      half plus an audit line.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/login/v2/flow/{{flow_token}}
Content-Type: application/x-www-form-urlencoded
```
user=doors-deactivated&password=DoorsFixturePass1!
```

HTTP 303
[Captures]
refused_location: header "Location"


# ─────────────────────────────────────────────────────────────
# 12 — NC LOGIN FLOW v2, anti-enumeration half: a WRONG password
#      on the deactivated account must not reveal the
#      deactivation. Both rows together pin the ordering —
#      dropping this one would let the check drift back before
#      the password verify, where it leaks account state to
#      anyone guessing.
#
#      Same status AND the same Location as the right-password
#      row above, so a prober cannot distinguish "wrong password"
#      from "account disabled" at this door.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/index.php/login/v2

HTTP 200
[Captures]
flow_token_wrong: jsonpath "$.poll.token"

POST {{base_url}}/login/v2/flow/{{flow_token_wrong}}
Content-Type: application/x-www-form-urlencoded
```
user=doors-deactivated&password=NotThePassword9!
```

HTTP 303
[Asserts]
header "Location" == "{{refused_location}}"


# ─────────────────────────────────────────────────────────────
# 13 — The parity row: a wrong password on the ACTIVE admin
#      account answers identically. Without it, the two rows
#      above assert a status without establishing that the
#      DEACTIVATED state is what stays hidden — which is the
#      whole claim.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/index.php/login/v2

HTTP 200
[Captures]
flow_token_admin: jsonpath "$.poll.token"

POST {{base_url}}/login/v2/flow/{{flow_token_admin}}
Content-Type: application/x-www-form-urlencoded
```
user={{username}}&password=NotTheAdminPassword9!
```

HTTP 303
[Asserts]
header "Location" == "{{refused_location}}"


# ─────────────────────────────────────────────────────────────
# 13b — DEVICE-FLOW door: a deactivated user cannot approve a
#       pending device authorization.
#
#       Read the STATUS, not just the refusal — it names which
#       gate fired, and the two are at different layers:
#
#         401 → the per-call live-role check rejected the caller
#               in `auth_middleware`, before the handler ran.
#               This is what happens today.
#         403 → `approve()`'s own `check_account_admission` ran
#               and refused (AccountDeactivated → FORBIDDEN).
#
#       Either is correct for the user; the distinction matters
#       for what this pin proves. At 401 it asserts the OUTCOME
#       ("a disabled account cannot approve a device") but NOT
#       the service check — that would still pass with
#       `approve()`'s check deleted, because middleware beats it.
#       The service check is pinned at its own layer instead.
#
#       If this row ever flips to 403, middleware stopped
#       catching it first and `approve()`'s check became
#       load-bearing — worth knowing either way, which is why
#       the status is asserted rather than "any 4xx".
#
#       The pending code is created BEFORE deactivation because
#       `/device/authorize` is unauthenticated: it is the device
#       asking, not the user, so it stays available regardless.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/device/authorize
Content-Type: application/json
{ "client_name": "doors-deactivated probe device" }

HTTP 200
[Captures]
probe_user_code: jsonpath "$.user_code"

POST {{base_url}}/api/auth/device/verify
Authorization: Bearer {{fixture_token}}
Content-Type: application/json
{ "user_code": "{{probe_user_code}}", "action": "approve" }

HTTP 401


# ─────────────────────────────────────────────────────────────
# 14 — Reactivate, then confirm the account is usable again.
#      Pins that the refusals above are a STATE, not a
#      permanent lockout — if deactivation were one-way, every
#      assertion above would still pass.
# ─────────────────────────────────────────────────────────────
PUT {{base_url}}/api/admin/users/{{fixture_id}}/active
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "active": true }

HTTP 200

POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "doors-deactivated", "password": "DoorsFixturePass1!" }

HTTP 200


# ─────────────────────────────────────────────────────────────
# 15 — Teardown. The DB is dropped between `run.sh` invocations,
#      but a leaked fixture makes step 1 fail on a hand-run
#      repeat rather than silently reusing a stale account.
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/api/admin/users/{{fixture_id}}
Authorization: Bearer {{admin_token}}

HTTP 200
