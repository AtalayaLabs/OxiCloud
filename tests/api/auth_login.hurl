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
# Case 7 — /api/auth/oidc/providers advertises the auth-method
#           policy the SPA needs to render the correct forms.
#
# tests/common/server.env has OXICLOUD_OIDC_ENABLED=false,
# OXICLOUD_SMTP_MOCK=true (so SMTP is "wired"), and the default
# OXICLOUD_AUTH_METHODS (both methods allowed). Expected shape:
#   enabled: false                  — no OIDC IdP configured
#   password_login_enabled: true    — default allowlist includes it
#   magic_link_login_enabled: true  — SMTP wired + allowlist + no OIDC
#   require_verified_email: false   — default
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/auth/oidc/providers

HTTP 200
[Asserts]
jsonpath "$.enabled" == false
jsonpath "$.password_login_enabled" == true
jsonpath "$.magic_link_login_enabled" == true
jsonpath "$.require_verified_email" == false

