# =============================================================
# OxiCloud — login dispatcher (PR 17)
# =============================================================
# After PR 17 the `username` field on /api/auth/login accepts BOTH
# a username (no `@`) and an email address. The server dispatches
# on `@`-in-input: with `@` → email lookup, without → username
# lookup. The two namespaces are provably disjoint (PR 16 forbids
# `@` in usernames), so this is unambiguous.
# =============================================================


# ─────────────────────────────────────────────────────────────
# Case 1 — Login by username (the classic path).
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "{{username}}", "password": "{{password}}" }

HTTP 200
[Asserts]
jsonpath "$.access_token" exists
jsonpath "$.user.full.user.email" == "{{email}}"


# ─────────────────────────────────────────────────────────────
# Case 2 — Login by email (new path).
#           The same DTO field, different lookup branch because
#           the input contains `@`.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "{{email}}", "password": "{{password}}" }

HTTP 200
[Asserts]
jsonpath "$.access_token" exists
jsonpath "$.user.full.user.email" == "{{email}}"


# ─────────────────────────────────────────────────────────────
# Case 3 — Wrong password on the username path → uniform 403.
#           Anti-enumeration: same error shape as unknown-user.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "{{username}}", "password": "definitely-wrong" }

HTTP 403


# ─────────────────────────────────────────────────────────────
# Case 4 — Wrong password on the email path → uniform 403.
#           Same as Case 3 but with the email path. The error
#           shape is identical regardless of which branch fired.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "{{email}}", "password": "definitely-wrong" }

HTTP 403


# ─────────────────────────────────────────────────────────────
# Case 5 — Unknown username → uniform 403, audit reason
#           `unknown_user`. The visible response is identical
#           to wrong-password (Case 3) so a probing attacker
#           can't distinguish "user exists" from "user doesn't".
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "ghost-user-that-doesnt-exist", "password": "{{password}}" }

HTTP 403


# ─────────────────────────────────────────────────────────────
# Case 6 — Unknown email → uniform 403, same anti-enumeration
#           guarantee as Case 5 but exercising the email branch.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "ghost@nowhere.invalid", "password": "{{password}}" }

HTTP 403


# ─────────────────────────────────────────────────────────────
# Case 7 — /api/config.auth.* advertises the auth-method policy
#           the SPA needs to render the correct forms.
#
# Was `GET /api/auth/oidc/providers` (retired on feat/registration);
# the four auth-method flags moved onto `/api/config.auth.*` as
# sub-objects per concern. The shape pins live in
# `auth_config_discovery.hurl`; this case re-pins the exact VALUES
# a server booted from `tests/common/server.env` is expected to
# publish:
#   OXICLOUD_OIDC_ENABLED=false       → auth.oidc.enabled: false
#   OXICLOUD_SMTP_MOCK=true           → auth.magic_link.enabled: true
#   default OXICLOUD_AUTH_METHODS     → auth.password.enabled: true
#   default (not set)                 → auth.require_verified_email: false
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/config

HTTP 200
[Asserts]
jsonpath "$.auth.oidc.enabled" == false
jsonpath "$.auth.password.enabled" == true
jsonpath "$.auth.magic_link.enabled" == true
jsonpath "$.auth.require_verified_email" == false

