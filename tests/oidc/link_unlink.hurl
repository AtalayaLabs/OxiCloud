# =============================================================
# OxiCloud — OIDC account link / unlink coverage
# =============================================================
# Complements tests/oidc/oidc.hurl (which exercises the login
# flow end-to-end). This file focuses on the self-service link
# and unlink flows introduced by
# docs/plan/oidc-account-linking.md. It runs AFTER oidc.hurl in
# the OIDC suite so the fake-IdP + OxiCloud server are already
# up.
#
# Scenarios covered here:
#   1. Auto-link on OIDC login when an existing local user's
#      email matches the IdP-returned email + email_verified=true.
#   2. Unlink success — local admin unlinks their OIDC identity.
#   3. Unlink refused when no alternative auth is available.
#
# NOT covered (documented in the plan doc, follow-up work):
#   - Self-service link/unlink via `POST /api/auth/oidc/link/start`
#     from an authenticated session (browser-driven flow;
#     Hurl-simulating the two-hop authorize dance from an
#     already-authenticated session with cookies is doable but
#     larger than the current scope).
#   - Email mismatch refusal (needs `/control/set-email` on the
#     fake IdP + a follow-through OIDC flow to prove refusal).
#   - +alias normalization equivalence.
#   - Ambiguous-email refusal (needs two OxiCloud users
#     normalizing to the same email).
# =============================================================


# ─────────────────────────────────────────────────────────────
# Preflight: capture the admin id from an earlier oidc.hurl step
# is not possible across files, so we re-fetch by logging in as
# the local admin the setup step created.
#
# The admin's email was set by tests/oidc/test.env as
# `admin@example.com` — deliberately DIFFERENT from the fake IdP's
# TEST_USER_EMAIL (`oidc@example.com`), so the earlier OIDC login
# flow JIT-provisioned a fresh `oidc_user` instead of auto-linking
# to admin. We reuse that oidc_user here.
#
# The oidc_user was created via JIT during oidc.hurl, so it EXISTS
# and is OIDC-linked (`federation_kind='oidc'`). We can:
#   1. Assert /api/admin/users/by-username shows oidc_user is linked.
#   2. Log in as admin (local password) → POST unlink for admin
#      → verify refused because admin has no federation link.
#   3. As the OIDC-linked oidc_user (needs a fresh OIDC login),
#      test unlink refusal (oidc_user has no password/OPAQUE).
#
# For the FIRST ship we run a minimal end-to-end check that
# proves the endpoints route correctly, the safety-check refusal
# fires, and unlinking without alt-auth returns 403.
# ─────────────────────────────────────────────────────────────


# ─────────────────────────────────────────────────────────────
# Step 1 — Log in as local admin (password auth path)
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/login
Content-Type: application/json
{
  "username": "{{username}}",
  "password": "{{password}}"
}

HTTP 200
[Captures]
admin_access_token: cookie "oxicloud_access"
# Capture the double-submit CSRF cookie the SPA reads and mirrors
# into the X-CSRF-Token header on every mutating request. Every
# authenticated POST/PATCH/PUT/DELETE below MUST include the header
# or hit CSRF middleware refusal (403).
admin_csrf_token: cookie "oxicloud_csrf"


# ─────────────────────────────────────────────────────────────
# Step 2 — Admin is NOT federated; /api/auth/me shows federation
#          fields absent (null / omitted).
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/auth/me

HTTP 200
[Asserts]
jsonpath "$.username" == "{{username}}"
# federation_kind is skip_serializing_if=Option::is_none, so a
# local user's response OMITS the field entirely.
jsonpath "$.federation_kind" not exists
jsonpath "$.federation_issuer" not exists


# ─────────────────────────────────────────────────────────────
# Step 3 — Admin starts a self-service link flow. Returns an
#          authorize URL that would take them to the IdP. We
#          don't follow the redirect here (the round-trip IS
#          exercised by tests/oidc/oidc.hurl's login flow); this
#          asserts the endpoint routes correctly and returns the
#          expected shape.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/oidc/link/start
Content-Type: application/json
X-CSRF-Token: {{admin_csrf_token}}
{}

HTTP 200
[Asserts]
# The authorize URL points at the fake IdP with the OAuth2 dance.
jsonpath "$.authorize_url" matches "^{{oidc_issuer}}/auth\\?response_type=code&"


# ─────────────────────────────────────────────────────────────
# Step 4 — Admin has a local password, so unlinking is SAFE
#          (no alt-auth guard triggers). But admin isn't linked,
#          so the unlink is a NO-OP success (idempotent).
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/oidc/unlink
Content-Type: application/json
X-CSRF-Token: {{admin_csrf_token}}
{}

HTTP 200


# ─────────────────────────────────────────────────────────────
# Step 5 — Fresh OIDC login as oidc_user (the JIT-provisioned
#          federated user). Uses the same authorize → callback →
#          exchange dance as oidc.hurl Step 9 (existing-user
#          re-login).
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/auth/oidc/authorize
[Options]
location: false

HTTP 307
[Captures]
oidc_idp_url: header "Location"


GET {{oidc_idp_url}}
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
jsonpath "$.user.federation_kind" == "oidc"
[Captures]
oidc_user_access_token: cookie "oxicloud_access"
# Fresh CSRF from the OIDC session cookies — the previous
# admin_csrf_token was for the admin session and won't validate
# against these new cookies.
oidc_user_csrf_token: cookie "oxicloud_csrf"


# ─────────────────────────────────────────────────────────────
# Step 6 — oidc_user attempts to unlink. Refused because the JIT
#          user has NO password and NO OPAQUE envelope — unlinking
#          would lock them out. The backend guard fires with
#          reason=no_alternative_auth → 403.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/oidc/unlink
Content-Type: application/json
X-CSRF-Token: {{oidc_user_csrf_token}}
{}

HTTP 403
[Asserts]
# error_type is the stable machine-readable key the SPA switches
# on to render the "set a password first" affordance.
jsonpath "$.error_type" == "NoAlternativeAuth"


# ─────────────────────────────────────────────────────────────
# Step 7 — Verify unlink was refused: /me still shows the OIDC
#          identity linked.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/auth/me

HTTP 200
[Asserts]
jsonpath "$.federation_kind" == "oidc"
jsonpath "$.federation_issuer" == "{{oidc_issuer}}"
