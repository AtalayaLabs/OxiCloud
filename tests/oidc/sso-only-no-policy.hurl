# =============================================================
# OxiCloud — SSO-only WITHOUT auto-redirect policy
# =============================================================
# Sibling to tests/oidc/sso-only.hurl. Both run against
# server-with-oidc-only-no-policy.env (OXICLOUD_AUTH_METHODS=oidc but
# OXICLOUD_AUTH_POLICIES unset) and prove the SPECIFIC posture difference
# the auto_redirect_if_standalone_oidc policy makes:
#
#   * with policy (sso-only.hurl):    GET /login → 307 to /api/auth/oidc/authorize
#   * without policy (this file):     GET /login → 200 (SPA shell)
#
# In this posture the SPA renders the login page with the SSO button;
# the user clicks it to start the OIDC flow. Everything else about the
# OIDC surface is identical, so we DON'T re-run the full OIDC dance —
# that's covered by sso-only.hurl + oidc.hurl. This file only asserts
# what actually differs.
# =============================================================


# ─────────────────────────────────────────────────────────────
# Step 1 — Providers reports SSO-only but WITHOUT auto-redirect.
#          The `auto_redirect_to_oidc` field on the DTO comes from
#          AuthApplicationService::auto_redirect_to_oidc(), which
#          requires the policy in the vector. Without it → false.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/auth/oidc/providers

HTTP 200
[Asserts]
jsonpath "$.enabled" == true
jsonpath "$.password_login_enabled" == false
jsonpath "$.magic_link_login_enabled" == false
# The load-bearing difference from sso-only.hurl:
jsonpath "$.auto_redirect_to_oidc" == false


# ─────────────────────────────────────────────────────────────
# Step 2 — GET /login must NOT redirect. The middleware's
#          `should_redirect` predicate evaluates false because
#          `auto_redirect_to_oidc()` returns false (policy absent),
#          so the request falls through to the SPA fallback service.
#          Result: 200 with the SPA shell HTML.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/login
[Options]
location: false

HTTP 200
# Deliberately no Location-header assertion — we're proving its ABSENCE
# by way of the 200 status. If the middleware had incorrectly fired the
# redirect this would be a 307.


# ─────────────────────────────────────────────────────────────
# Step 3 — The authorize endpoint still works (user clicks the SSO
#          button → SPA fetches this URL → server redirects to IdP).
#          Same shape as sso-only.hurl Step 4; here we only assert
#          the FIRST hop returns a valid IdP URL, which is enough to
#          prove the OIDC surface is functional under this posture.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/auth/oidc/authorize
[Options]
location: false

HTTP 307
[Asserts]
# Location points at the fake IdP's /auth endpoint with the OAuth2
# response_type / client_id / redirect_uri / PKCE dance.
header "Location" matches "^{{oidc_issuer}}/auth\\?response_type=code&client_id={{oidc_client_id}}&"
