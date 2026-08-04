# =============================================================
# OxiCloud — Baseline: admin user-mutation surface
# =============================================================
# Exercises the cluster of admin-only `PUT /api/admin/users/{id}/*`
# endpoints that operators rely on for incident response:
#   - disable a compromised account
#   - rotate its password
#   - change its role
#   - set / clear its quota
#
# Each is a one-shot mutation, but the failure mode of any one
# is severe (operator can't lock out an attacker, can't reset a
# password). Pinning them together keeps the cluster intact
# under refactors.
#
# Coverage:
#   1. Admin creates a fresh fixture user via POST /api/admin/users
#   2. Fixture user logs in successfully (baseline)
#   3. PUT /quota → fixture user's /me reports updated quota
#   4. PUT /role  → fixture user becomes admin
#   5. PUT /password (admin reset) → old password no longer works,
#      new password works
#   6. PUT /active=false → fixture user login → 403
#   7. PUT /active=true  → fixture user login works again
#   8. Cleanup via DELETE /api/admin/users/{id}
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
# 1 — Admin creates the fixture user "charlie-ops"
#     Uses a name that doesn't collide with charlie in
#     registration.hurl (which uses just "charlie"), so this
#     file is order-independent.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/admin/users
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "username": "charlie-ops",
  "password": "OriginalPassword1!",
  "email":    "charlie-ops@example.com",
  "role":     "user"
}

HTTP 201
[Captures]
charlie_id: jsonpath "$.id"


# ─────────────────────────────────────────────────────────────
# 2 — Baseline: fixture user can log in with the password
#     admin assigned.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "charlie-ops", "password": "OriginalPassword1!" }

HTTP 200
[Captures]
charlie_token_v1: jsonpath "$.access_token"


# ─────────────────────────────────────────────────────────────
# 3 — Set a non-default quota; charlie's own /me must reflect it.
#     200 MiB = 209715200 bytes — keeps the assertion exact while
#     still being a believable per-user cap.
# ─────────────────────────────────────────────────────────────
PUT {{base_url}}/api/admin/users/{{charlie_id}}/quota
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "quota_bytes": 209715200 }

HTTP 200

GET {{base_url}}/api/auth/me
Authorization: Bearer {{charlie_token_v1}}

HTTP 200
[Asserts]
jsonpath "$.storage_quota_bytes" == 209715200


# ─────────────────────────────────────────────────────────────
# 4 — Promote charlie to admin. After this the /me payload's
#     role field must reflect the change.
# ─────────────────────────────────────────────────────────────
PUT {{base_url}}/api/admin/users/{{charlie_id}}/role
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "role": "admin" }

HTTP 200

GET {{base_url}}/api/auth/me
Authorization: Bearer {{charlie_token_v1}}

HTTP 200
[Asserts]
jsonpath "$.role" == "admin"


# ─────────────────────────────────────────────────────────────
# 5 — Admin resets charlie's password.
# ─────────────────────────────────────────────────────────────
PUT {{base_url}}/api/admin/users/{{charlie_id}}/password
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "new_password": "AdminResetPassword2!" }

HTTP 200

# Old password no longer works. Login failures map to 403
# (AccessDenied) in this codebase — both "invalid credentials"
# and "account deactivated" go through the same error kind.
POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "charlie-ops", "password": "OriginalPassword1!" }

HTTP 403

# New password works AND the login response carries
# `force_password_change: true` — the admin-picked password is
# temporary; the SPA reads this to enter mandatory-mode and route
# the user to `/profile?forcePasswordChange=1`. See the backend
# `admin_reset_password` → `OpaquePgRepository::clear_registration`
# (or `UserPgRepository::set_force_password_change` when OPAQUE is
# off) for the atomic flag write.
POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "charlie-ops", "password": "AdminResetPassword2!" }

HTTP 200
[Captures]
charlie_token_v2: jsonpath "$.access_token"
[Asserts]
jsonpath "$.force_password_change" == true


# `require_no_password_change_pending_layer` middleware assertion:
# a random authenticated endpoint that ISN'T on the allowlist
# (`/me`, `/change-password`, `/logout`) must refuse with
# `403 PasswordChangeRequired` while the flag is set. Without
# this gate the admin-picked password would let holders reach
# files / DAV / admin via any non-SPA client.
GET {{base_url}}/api/folders
Authorization: Bearer {{charlie_token_v2}}

HTTP 403
[Asserts]
jsonpath "$.error_type" == "PasswordChangeRequired"


# Same session, but the allowlisted `/api/auth/me` DOES pass —
# the SPA needs this to detect the flag and render the mandatory
# banner. Response also mirrors the flag so a page reload sees
# the same state a fresh login would.
GET {{base_url}}/api/auth/me
Authorization: Bearer {{charlie_token_v2}}

HTTP 200
[Asserts]
jsonpath "$.force_password_change" == true


# Trying to change back to the SAME password must fail with a
# distinct `PasswordUnchanged` error_type — silently accepting
# the no-op would clear the force flag without actually rotating
# the credential, defeating the temporary-password pattern.
PUT {{base_url}}/api/auth/change-password
Authorization: Bearer {{charlie_token_v2}}
Content-Type: application/json
{ "current_password": "AdminResetPassword2!", "new_password": "AdminResetPassword2!" }

HTTP 400
[Asserts]
jsonpath "$.error_type" == "PasswordUnchanged"


# Change to a genuinely different password: succeeds AND
# `change_password` revokes all sessions (per its own contract);
# the CURRENT token stops working right after. That side effect
# is what forces the user through a fresh login where the flag
# is now cleared.
PUT {{base_url}}/api/auth/change-password
Authorization: Bearer {{charlie_token_v2}}
Content-Type: application/json
{ "current_password": "AdminResetPassword2!", "new_password": "CharliePicked3!" }

HTTP 200


# Fresh login with the user-picked password: succeeds AND the
# flag has flipped back to false, so mandatory-mode is off.
POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "charlie-ops", "password": "CharliePicked3!" }

HTTP 200
[Captures]
charlie_token_v3: jsonpath "$.access_token"
[Asserts]
jsonpath "$.force_password_change" == false


# Same random endpoint that 403'd above now succeeds — the gate
# has lifted.
GET {{base_url}}/api/folders
Authorization: Bearer {{charlie_token_v3}}

HTTP 200


# ─────────────────────────────────────────────────────────────
# 6 — Disable the account. The next login attempt must report
#     403 (account disabled) — distinct from 401 (bad creds)
#     so operators can tell "I locked you out" from "you typed
#     the wrong password".
# ─────────────────────────────────────────────────────────────
PUT {{base_url}}/api/admin/users/{{charlie_id}}/active
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "active": false }

HTTP 200

POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "charlie-ops", "password": "CharliePicked3!" }

HTTP 403


# ─────────────────────────────────────────────────────────────
# 7 — Re-enable; login works again.
# ─────────────────────────────────────────────────────────────
PUT {{base_url}}/api/admin/users/{{charlie_id}}/active
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{ "active": true }

HTTP 200

POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "charlie-ops", "password": "CharliePicked3!" }

HTTP 200


# ─────────────────────────────────────────────────────────────
# 8 — Teardown
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/api/admin/users/{{charlie_id}}
Authorization: Bearer {{admin_token}}

HTTP 200
