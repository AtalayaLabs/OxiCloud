# =============================================================
# OxiCloud — SSO-only posture: server-side /login 302 + RP-initiated logout
# =============================================================
# Complements tests/oidc/oidc.hurl (which runs with OXICLOUD_AUTH_METHODS
# accepting password + oidc and never fires the auto-redirect middleware).
# This suite runs against tests/common/server-with-oidc-only.env which
# sets:
#   * OXICLOUD_AUTH_METHODS=oidc
#   * OXICLOUD_AUTH_POLICIES=auto_redirect_if_standalone_oidc
#
# What it proves the plain OIDC suite can't:
#   1. GET /api/auth/oidc/providers reports the standalone-OIDC posture
#      correctly (auto_redirect_to_oidc=true, password + magic-link off).
#   2. GET /login returns a server-side 302 to /api/auth/oidc/authorize
#      BEFORE the SPA loads (interception lives in web/mod.rs, wired via
#      an axum middleware layer).
#   3. GET /login?error=… falls through to the SPA shell (loop-guard so
#      an IdP failure doesn't put the browser in an infinite redirect).
#   4. POST /api/auth/logout on an OIDC-backed session returns
#      `post_logout_url` shaped exactly like the RP-initiated logout URL
#      Keycloak / other IdPs expect: end_session_endpoint +
#      id_token_hint + post_logout_redirect_uri + client_id.
# =============================================================


# ─────────────────────────────────────────────────────────────
# Step 1 — Providers discovery reports the standalone-OIDC posture.
#          The SPA no longer reads auto_redirect_to_oidc (server-side
#          redirect handles it), but the field is still exposed for
#          diagnostics / future clients.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/auth/oidc/providers

HTTP 200
[Asserts]
jsonpath "$.enabled" == true
jsonpath "$.password_login_enabled" == false
# Magic-link is hard-off whenever OIDC is enabled (OIDC master rule)
# regardless of what AUTH_METHODS says. Belt-and-braces with the
# allowlist which also excludes it.
jsonpath "$.magic_link_login_enabled" == false
# The policy is on, no other method is live, so the flag resolves true.
jsonpath "$.auto_redirect_to_oidc" == true


# ─────────────────────────────────────────────────────────────
# Step 2 — /login returns 302 to /api/auth/oidc/authorize.
#          location: false so we assert on the header rather than
#          following. The middleware intercepts BEFORE ServeDir would
#          hand out the SPA shell, so no HTML body is produced.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/login
[Options]
location: false

HTTP 307
[Asserts]
# axum::response::Redirect::temporary → 307 with the target as Location.
header "Location" == "/api/auth/oidc/authorize"


# ─────────────────────────────────────────────────────────────
# Step 3 — Loop-guard: /login?error=… must NOT redirect. The IdP
#          bounces here on failure (Keycloak returns to
#          post_logout_redirect_uri with ?error= on some flows); a
#          middleware that redirected regardless would ping-pong the
#          browser between OxiCloud and the failing IdP forever.
#          Falling through to the SPA lets the login page render the
#          error banner.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/login?error=access_denied
[Options]
location: false

HTTP 200
# No Location header — the ServeDir fallback served the SPA shell.
# We don't assert on the body (the shell is minimal HTML) because the
# 200 status alone proves the middleware fell through instead of
# returning a redirect.


# ─────────────────────────────────────────────────────────────
# Step 3b — Loop-guard: /login?oidc_code=… must also NOT redirect.
#           This is the callback landing URL — the SPA reads the code
#           from the query string and swaps it for a session. If the
#           middleware redirected on this we'd never complete the login.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/login?oidc_code=deadbeef
[Options]
location: false

HTTP 200


# ─────────────────────────────────────────────────────────────
# Step 4 — Full OIDC dance. No local admin exists yet — SSO-only means
#          the first admin bootstraps by logging in via OIDC and getting
#          the admin role via the group mapping (OXICLOUD_OIDC_ADMIN_GROUPS
#          matches the fake IdP's `admin-users` group claim).
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/auth/oidc/authorize
[Options]
location: false

HTTP 307
[Captures]
idp_url: header "Location"


GET {{idp_url}}
[Options]
location: true
location-trusted: true

HTTP 200
[Captures]
oidc_code: url regex "oidc_code=([a-f0-9]+)"


POST {{base_url}}/api/auth/oidc/exchange
Content-Type: application/json
{ "code": "{{oidc_code}}" }

HTTP 200
[Asserts]
jsonpath "$.user.username" == "oidc_user"
# Group-to-role mapping worked — this is now the admin (and the only
# user).
jsonpath "$.user.role" == "admin"


# ─────────────────────────────────────────────────────────────
# Step 5 — Confirm the session is live before we log out. Load-bearing
#          for Step 6: without proving /me works first, a 401 in Step 6
#          could mean "logout worked" OR "session was never live".
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/auth/me

HTTP 200
[Asserts]
jsonpath "$.username" == "oidc_user"


# ─────────────────────────────────────────────────────────────
# Step 6 — RP-initiated logout returns the end_session URL. The backend
#          reads the OIDC id_token from the session row, calls the OIDC
#          service to build the URL from discovery's end_session_endpoint
#          + id_token_hint + post_logout_redirect_uri + client_id.
#          The SPA reads `post_logout_url` and window.location.replace's
#          to it — see AppShell.svelte::onLogout.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/logout
Content-Type: application/json
{}

HTTP 200
[Asserts]
# Field is present.
jsonpath "$.post_logout_url" isString
# Points at the IdP's end_session_endpoint (oidc-provider mounts it at
# /session/end by default).
jsonpath "$.post_logout_url" matches "^{{oidc_issuer}}/session/end\\?"
# id_token_hint is present and non-empty (JWT-shaped: three dot-separated
# base64url segments).
jsonpath "$.post_logout_url" matches "id_token_hint=[A-Za-z0-9_-]+\\.[A-Za-z0-9_-]+\\.[A-Za-z0-9_-]+"
# post_logout_redirect_uri points back at /login on this deployment.
# The value is URL-encoded so we look for the encoded form.
jsonpath "$.post_logout_url" contains "post_logout_redirect_uri=http%3A%2F%2Flocalhost%3A8090%2Flogin"
# client_id echoes the configured OIDC client. Real IdPs (Keycloak
# post-19) use this to fall back to the registered post-logout redirect
# when the id_token_hint has expired.
jsonpath "$.post_logout_url" contains "client_id={{oidc_client_id}}"


# ─────────────────────────────────────────────────────────────
# Step 7 — Local session gone. The backend cleared the auth cookies
#          alongside returning post_logout_url; the browser normally
#          proceeds to navigate to the IdP, but we skip that hop here
#          and verify locally that the cookies + session row are dead.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/auth/me

HTTP 401


# ─────────────────────────────────────────────────────────────
# Step 8 — Non-OIDC-session logout returns {} (no post_logout_url).
#          We can't easily manufacture a password/magic-link session
#          under SSO-only posture (both are refused at the endpoint
#          layer). Left as a note; unit test in
#          auth_application_service covers the `Ok(None)` return branch
#          when session.oidc_id_token IS NULL.
# ─────────────────────────────────────────────────────────────
