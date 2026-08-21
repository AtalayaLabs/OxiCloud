# =============================================================
# OxiCloud — Baseline: auth session lifecycle
# =============================================================
# Covers the refresh-token / logout / status surface that the
# existing `auth_login.hurl` deliberately leaves alone (login +
# lockout only). Browsers and the desktop NC client both rely
# on the rotate-and-revoke semantics being correct; this file
# pins them as a single end-to-end flow.
#
# Coverage:
#   1. GET  /api/auth/status     (no auth required, used by login page)
#   2. POST /api/auth/login      (capture initial access + refresh)
#   3. POST /api/auth/refresh    (rotate; capture new tokens)
#   4. New access token works on /api/auth/me
#   5. OLD refresh token rejected after rotation (session-family
#      single-use enforcement)
#   6. POST /api/auth/logout     (revokes the current refresh)
#   7. Refresh after logout → 401 (revocation actually took effect)
#   8. Access token still works briefly until it expires — we
#      don't assert that explicitly because TTL is configurable
#      and the access-token revocation semantics are documented
#      as "JWT remains valid until exp"; logout only kills the
#      refresh path.
# =============================================================


# ─────────────────────────────────────────────────────────────
# 1 — Unauthenticated /api/auth/status probe.
#     The login page hits this on every load; the response
#     determines whether the "Create first admin" flow shows.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/auth/status

HTTP 200
[Asserts]
jsonpath "$.initialized"          == true
jsonpath "$.admin_count"          >= 1
jsonpath "$.registration_allowed" == true


# ─────────────────────────────────────────────────────────────
# 2 — Login as admin. Capture BOTH tokens — we need the
#     refresh later to verify rotation semantics.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "{{username}}", "password": "{{password}}" }

HTTP 200
[Captures]
access_v1:  jsonpath "$.access_token"
refresh_v1: jsonpath "$.refresh_token"


# ─────────────────────────────────────────────────────────────
# 3 — Refresh: mint a new (access, refresh) pair. The refresh
#     token is rotated — the response carries a NEW refresh
#     that supersedes refresh_v1.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/refresh
Content-Type: application/json
{ "refresh_token": "{{refresh_v1}}" }

HTTP 200
[Captures]
access_v2:  jsonpath "$.access_token"
refresh_v2: jsonpath "$.refresh_token"
[Asserts]
jsonpath "$.access_token"  != "{{access_v1}}"
jsonpath "$.refresh_token" != "{{refresh_v1}}"


# ─────────────────────────────────────────────────────────────
# 4 — The new access token works on a protected endpoint.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/auth/me
Authorization: Bearer {{access_v2}}

HTTP 200
[Asserts]
jsonpath "$.full.user.username" == "{{username}}"


# ─────────────────────────────────────────────────────────────
# 5 — The OLD refresh token MUST be rejected after rotation.
#     This is the session-family single-use property: replay
#     of a used refresh token is treated as theft and rejected.
#
#     The handler's OpenAPI doc says 401, but the actual response
#     is 403: the refresh service raises `ErrorKind::AccessDenied`
#     which maps to HTTP 403 in this codebase. Pinning the
#     observed-and-correct behavior here.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/refresh
Content-Type: application/json
{ "refresh_token": "{{refresh_v1}}" }

HTTP 403


# ─────────────────────────────────────────────────────────────
# 6 — Logout using the v2 refresh + v2 access. Server-side
#     this revokes the session and clears auth cookies in the
#     response.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/logout
Authorization: Bearer {{access_v2}}
Content-Type: application/json
{ "refresh_token": "{{refresh_v2}}" }

HTTP 200


# ─────────────────────────────────────────────────────────────
# 7 — Post-logout: the v2 refresh token is now revoked. A
#     refresh attempt is rejected with 403 (same AccessDenied
#     mapping as step 5).
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/refresh
Content-Type: application/json
{ "refresh_token": "{{refresh_v2}}" }

HTTP 403


# ─────────────────────────────────────────────────────────────
# 8 — Sanity re-check: status still reports the system as
#     initialized after logout (no state regression).
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/auth/status

HTTP 200
[Asserts]
jsonpath "$.initialized" == true
