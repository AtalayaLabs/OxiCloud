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
# ENTRY STATE (post-oidc.hurl):
#   * `admin` — local, password auth, email admin@example.com,
#     NOT federation-linked.
#   * `oidc_user` — JIT-provisioned, federation_kind='oidc',
#     issuer=<fake_idp>, subject=oidc-test-user, email
#     oidc@example.com, NO password / NO OPAQUE.
#   * Fake IdP — subOverride=null, emailOverride=null,
#     verified=true, accountId=`oidc-test-user`.
#
# Scenarios covered here (indexed against
# docs/plan/oidc-account-linking.md § Hurl test coverage):
#
#   [Refusals on the default sub]
#     — Self-service link routing: POST /link/start returns the
#       expected authorize URL.
#     — Unlink idempotency: POST /unlink on an unlinked user is
#       a 200 no-op.
#     6. Self-service link email_mismatch (default email vs admin).
#     8. Self-service link already_linked_elsewhere (email swapped
#        to admin's, but the sub is still linked to oidc_user).
#
#   [Fresh subs — needs /control/set-sub on the fake IdP]
#     5. Self-service link happy round-trip → /profile?linked=1
#        + /me shows the federation identity. Then unlink and
#        assert federation cleared (scenario 9).
#     7. +alias normalization link — IdP returns
#        `admin+oidc@example.com` which normalizes to admin's
#        `admin@example.com`. Link succeeds.
#     1. Auto-link happy path — OIDC login callback sees a
#        (iss, sub) miss but email matches admin, so it auto-
#        links + logs admin in.
#     2. Auto-link refused — email_verified=false. Callback
#        redirects the browser to
#        /login?login_error=auto_link_email_not_verified so the
#        SPA login page can render a localized notice. Sibling
#        of the /profile?link_error=<reason> redirect used by
#        the self-service link flow. See auth_handler.rs
#        AutoLinkRefused arm.
#
#   [OIDC-only user]
#    10. `oidc_user` unlink refused (would lock them out) with
#        error_type NoAlternativeAuth.
#
# NOT covered (backend gap; not a Hurl gap):
#   3. Auto-link refused — email_ambiguous. The current
#      auto-link path uses `get_user_by_email` (exact match),
#      not a normalized-email lookup. It CAN'T see two rows
#      normalizing to the same value, so the ambiguity branch
#      in the plan doc is unreachable from wire input. Needs a
#      `list_users_by_normalized_email` repo method before
#      Hurl can exercise it — separate PR.
#
# NOT covered (config gap; would need a second server boot):
#   4. Auto-link disabled by OXICLOUD_OIDC_AUTO_LINK_EMAIL_MATCH=false.
#      Server boots with the flag ON in server-with-oidc.env;
#      testing the OFF branch means a separate hurl invocation
#      with a re-launched server, which the current run.sh does
#      not do.
# =============================================================


# ─────────────────────────────────────────────────────────────
# Step 1 — Log in as local admin (password auth path).
#
# Captures the double-submit CSRF cookie the SPA reads and
# mirrors into the X-CSRF-Token header on every mutating
# request. Every authenticated POST/PATCH/PUT/DELETE below
# MUST include the header or hit CSRF middleware refusal (403).
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
# Step 3 — Routing check for /link/start. Returns the authorize
#          URL the SPA would use to full-page-navigate to the
#          IdP. We don't follow it here; scenarios below own
#          the round-trip.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/oidc/link/start
Content-Type: application/json
X-CSRF-Token: {{admin_csrf_token}}
{}

HTTP 200
[Asserts]
jsonpath "$.authorize_url" matches "^{{oidc_issuer}}/auth\\?response_type=code&"


# ─────────────────────────────────────────────────────────────
# Step 4 — Idempotent unlink on an unlinked user is a 200 no-op.
#          Admin has a local password, so the no_alternative_auth
#          guard wouldn't fire even if there were something to
#          unlink.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/oidc/unlink
Content-Type: application/json
X-CSRF-Token: {{admin_csrf_token}}
{}

HTTP 200


# ═════════════════════════════════════════════════════════════
# Step 5 (Scenario 6) — Self-service link, email_mismatch refusal.
# ═════════════════════════════════════════════════════════════
# Default fake-IdP state (emailOverride=null) returns
# oidc@example.com — which does NOT match admin's
# admin@example.com. The callback recognises the Link intent,
# runs the email normalization comparison, and refuses with
# `email_mismatch`.
#
# Wire contract: 307 to `/profile?link_error=email_mismatch`.
# With `location: true` Hurl follows the full chain (authorize
# → IdP login+consent auto-approve → callback → redirect) and
# lands on the SPA-served /profile page (status 200 — the
# `index.html` fallback serves any client-router path).
# Assertion pins the exact URL shape the SPA reads on mount.
# ═════════════════════════════════════════════════════════════


# Belt-and-braces: reset any subOverride / emailOverride that
# might have leaked from an earlier suite (defensive — oidc.hurl
# doesn't use them, but a re-used fake-idp process could).
POST {{oidc_issuer}}/control/set-sub
Content-Type: application/json
{}

HTTP 200


POST {{oidc_issuer}}/control/set-email
Content-Type: application/json
{}

HTTP 200


POST {{base_url}}/api/auth/oidc/link/start
Content-Type: application/json
X-CSRF-Token: {{admin_csrf_token}}
{}

HTTP 200
[Captures]
mismatch_authorize_url: jsonpath "$.authorize_url"


GET {{mismatch_authorize_url}}
[Options]
location: true
location-trusted: true

# SPA fallback serves index.html for any client-router path
# (SvelteKit adapter-static + ServeDir fallback in
# src/interfaces/web/mod.rs). Assert on URL, not on body — the
# body is the SPA shell in every case.
HTTP 200
[Asserts]
url matches "^http://localhost:8087/profile\\?link_error=email_mismatch$"


# Post-refusal invariant: admin row unchanged, no federation
# fields populated. This is the load-bearing safety proof —
# a regression that mistakenly UPDATE-d the row before the
# email check would trip here.
GET {{base_url}}/api/auth/me

HTTP 200
[Asserts]
jsonpath "$.federation_kind" not exists
jsonpath "$.federation_issuer" not exists


# ═════════════════════════════════════════════════════════════
# Step 6 (Scenario 8) — already_linked_elsewhere refusal.
# ═════════════════════════════════════════════════════════════
# Flip the fake IdP to return admin's email so the email check
# passes; the sub is STILL `oidc-test-user`, which oidc.hurl
# already linked to `oidc_user`. The pre-UPDATE check in
# complete_oidc_link (`get_user_by_federation_subject` → row
# belongs to another user) fires and refuses with
# `already_linked_elsewhere`.
# ═════════════════════════════════════════════════════════════


POST {{oidc_issuer}}/control/set-email
Content-Type: application/json
{ "email": "{{email}}" }

HTTP 200
[Asserts]
jsonpath "$.overridden" == true
jsonpath "$.email" == "{{email}}"


POST {{base_url}}/api/auth/oidc/link/start
Content-Type: application/json
X-CSRF-Token: {{admin_csrf_token}}
{}

HTTP 200
[Captures]
taken_authorize_url: jsonpath "$.authorize_url"


GET {{taken_authorize_url}}
[Options]
location: true
location-trusted: true

HTTP 200
[Asserts]
url matches "^http://localhost:8087/profile\\?link_error=already_linked_elsewhere$"


GET {{base_url}}/api/auth/me

HTTP 200
[Asserts]
jsonpath "$.federation_kind" not exists
jsonpath "$.federation_issuer" not exists


# ═════════════════════════════════════════════════════════════
# Step 7 (Scenario 5) — Self-service link happy round-trip.
# ═════════════════════════════════════════════════════════════
# Swap the IdP to a FRESH sub (not-yet-linked to anyone) and
# keep the admin email match from Step 6. The set-sub endpoint
# also clears the OP's session cookies in the response so the
# next authorize dance re-prompts login and binds to the new
# sub. All five safety checks pass → link_federation_identity
# UPDATE runs → callback returns LinkCompleted → 307 to
# /profile?linked=1. /me now shows the federation identity.
#
# Then unlink admin (scenario 9 — success, has password) and
# assert the federation fields clear.
# ═════════════════════════════════════════════════════════════


POST {{oidc_issuer}}/control/set-sub
Content-Type: application/json
{ "sub": "sub-link-happy" }

HTTP 200
[Asserts]
jsonpath "$.overridden" == true
jsonpath "$.sub" == "sub-link-happy"


POST {{base_url}}/api/auth/oidc/link/start
Content-Type: application/json
X-CSRF-Token: {{admin_csrf_token}}
{}

HTTP 200
[Captures]
happy_authorize_url: jsonpath "$.authorize_url"


GET {{happy_authorize_url}}
[Options]
location: true
location-trusted: true

HTTP 200
[Asserts]
url == "http://localhost:8087/profile?linked=1"


# Federation now set on admin.
GET {{base_url}}/api/auth/me

HTTP 200
[Asserts]
jsonpath "$.username" == "{{username}}"
jsonpath "$.federation_kind" == "oidc"
jsonpath "$.federation_issuer" == "{{oidc_issuer}}"


# Scenario 9 — unlink success (admin has a password, so the
# no_alternative_auth guard doesn't fire).
POST {{base_url}}/api/auth/oidc/unlink
Content-Type: application/json
X-CSRF-Token: {{admin_csrf_token}}
{}

HTTP 200


GET {{base_url}}/api/auth/me

HTTP 200
[Asserts]
jsonpath "$.federation_kind" not exists
jsonpath "$.federation_issuer" not exists


# ═════════════════════════════════════════════════════════════
# Step 8 (Scenario 7) — +alias normalization link.
# ═════════════════════════════════════════════════════════════
# `common::text::normalize_email_for_link` strips +alias
# sub-addressing (`admin+oidc@example.com` → `admin@example.com`)
# and case-folds. Set the fake IdP to a FRESH sub and to the
# +alias email; the link check normalizes both sides and finds
# equivalence, so the link succeeds despite the raw strings
# differing.
# ═════════════════════════════════════════════════════════════


POST {{oidc_issuer}}/control/set-sub
Content-Type: application/json
{ "sub": "sub-alias" }

HTTP 200


POST {{oidc_issuer}}/control/set-email
Content-Type: application/json
{ "email": "admin+oidc@example.com" }

HTTP 200


POST {{base_url}}/api/auth/oidc/link/start
Content-Type: application/json
X-CSRF-Token: {{admin_csrf_token}}
{}

HTTP 200
[Captures]
alias_authorize_url: jsonpath "$.authorize_url"


GET {{alias_authorize_url}}
[Options]
location: true
location-trusted: true

HTTP 200
[Asserts]
url == "http://localhost:8087/profile?linked=1"


GET {{base_url}}/api/auth/me

HTTP 200
[Asserts]
jsonpath "$.federation_kind" == "oidc"


# Unlink to reset state before the auto-link scenarios.
POST {{base_url}}/api/auth/oidc/unlink
Content-Type: application/json
X-CSRF-Token: {{admin_csrf_token}}
{}

HTTP 200


# ═════════════════════════════════════════════════════════════
# Step 9 (Scenario 1) — Auto-link happy path.
# ═════════════════════════════════════════════════════════════
# This is the OIDC LOGIN callback path (not the self-service
# link flow). The callback's (iss, sub) lookup MISSES on the
# fresh sub, falls into the "match by email" branch, finds
# admin (email match + email_verified=true + admin not already
# linked), and auto-links. The flow proceeds like a regular
# OIDC login: the callback yields WebLogin { exchange_code }
# and 307s to /login?oidc_code=<code>. POST /exchange then
# mints tokens for admin (not a JIT-provisioned new user).
#
# The admin session cookies from Step 1 are still in the jar;
# /exchange returns fresh cookies that OVERWRITE the old ones
# so subsequent requests use the auto-link-issued session.
# ═════════════════════════════════════════════════════════════


POST {{oidc_issuer}}/control/set-sub
Content-Type: application/json
{ "sub": "sub-auto-happy" }

HTTP 200


# Reset email to admin's (Step 8 left it at admin+oidc@example.com,
# which would ALSO auto-link — but we assert on the strict-match
# behavior here).
POST {{oidc_issuer}}/control/set-email
Content-Type: application/json
{ "email": "{{email}}" }

HTTP 200


GET {{base_url}}/api/auth/oidc/authorize
[Options]
location: true
location-trusted: true

HTTP 200
[Captures]
autolink_oidc_code: url regex "oidc_code=([a-f0-9]+)"
[Asserts]
url matches "^http://localhost:8087/login\\?oidc_code=[a-f0-9]+$"


POST {{base_url}}/api/auth/oidc/exchange
Content-Type: application/json
{ "code": "{{autolink_oidc_code}}" }

HTTP 200
[Asserts]
# Auto-link resolved to the pre-existing admin, NOT a fresh
# JIT-provisioned user. The load-bearing assertion.
jsonpath "$.user.username" == "{{username}}"
jsonpath "$.user.federation_kind" == "oidc"
jsonpath "$.user.federation_issuer" == "{{oidc_issuer}}"
[Captures]
# Fresh cookies replace the password session's; capture the
# new CSRF for the unlink below.
autolink_csrf_token: cookie "oxicloud_csrf"


# /me confirms admin session AND that the federation columns
# are populated. Auto-link should have set kind=oidc, issuer=<fake>,
# subject=sub-auto-happy on admin's row.
GET {{base_url}}/api/auth/me

HTTP 200
[Asserts]
jsonpath "$.username" == "{{username}}"
jsonpath "$.federation_kind" == "oidc"
jsonpath "$.federation_issuer" == "{{oidc_issuer}}"


# Reset admin state before the next scenario (auto-link would
# refuse if admin is already linked, so we'd never reach the
# email_verified=false branch we want to test).
POST {{base_url}}/api/auth/oidc/unlink
Content-Type: application/json
X-CSRF-Token: {{autolink_csrf_token}}
{}

HTTP 200


# ═════════════════════════════════════════════════════════════
# Step 10 (Scenario 2) — Auto-link refused, email_verified=false.
# ═════════════════════════════════════════════════════════════
# Same shape as scenario 1 but with the IdP asserting
# email_verified=false. Auto-link gates on
# `email_verified == Some(true)` regardless of the operator-level
# OXICLOUD_REQUIRE_VERIFIED_EMAIL flag (auto-link uses the IdP
# email as a takeover-mitigation signal — an unverified email
# does not satisfy that), so the refusal path fires and the
# callback returns `DomainError::AlreadyExists`.
#
# Wire behavior: HTTP 409 with error_type "Already Exists"
# (ErrorKind::AlreadyExists → CONFLICT). The message is the
# "contact admin to link your OIDC identity" text that surfaces
# in the SPA login form's error toast.
#
# The handler maps each auto-link refusal reason to a distinct
# CamelCase error_type — AutoLinkDisabled /
# AutoLinkEmailNotVerified / AutoLinkAlreadyLinkedElsewhere —
# so the SPA can render targeted copy per refusal reason.
# ═════════════════════════════════════════════════════════════


POST {{oidc_issuer}}/control/set-sub
Content-Type: application/json
{ "sub": "sub-verified-false" }

HTTP 200


POST {{oidc_issuer}}/control/email-verified/false

HTTP 200


# Follow the whole OIDC dance. Hurl's location: true follows 3xx
# up to the callback; the callback redirects to /login with a
# machine-readable reason on the query string, and the SPA login
# page lands at 200 (index.html fallback). Assert on URL, since
# that's the load-bearing wire contract the SPA reads on mount.
GET {{base_url}}/api/auth/oidc/authorize
[Options]
location: true
location-trusted: true

HTTP 200
[Asserts]
url matches "^http://localhost:8087/login\\?login_error=auto_link_email_not_verified$"


# Belt-and-braces invariant: admin's row is still un-linked
# (the refusal ran BEFORE link_federation_identity would fire).
GET {{base_url}}/api/auth/me

HTTP 200
[Asserts]
jsonpath "$.federation_kind" not exists


# Reset fake IdP state (email_verified back to true, sub back
# to TEST_USER_SUB) so the oidc_user re-login below resolves
# correctly. The set-sub call ALSO clears the OP's session
# cookies from the jar, ensuring the login prompt fires with
# the reset sub bound instead of reusing the sub-verified-false
# session.
POST {{oidc_issuer}}/control/email-verified/true

HTTP 200


POST {{oidc_issuer}}/control/set-sub
Content-Type: application/json
{}

HTTP 200
[Asserts]
jsonpath "$.overridden" == false


POST {{oidc_issuer}}/control/set-email
Content-Type: application/json
{}

HTTP 200


# ═════════════════════════════════════════════════════════════
# Step 11 (Scenario 10) — Unlink refused for the OIDC-only user.
# ═════════════════════════════════════════════════════════════
# Fresh OIDC login as `oidc_user` (the JIT-provisioned
# federated principal from oidc.hurl). Uses the reset default
# sub, so the (iss, sub) lookup HITS the existing oidc_user
# row (linked in oidc.hurl Step 5) and the existing-user branch
# runs — NOT auto-link.
# ═════════════════════════════════════════════════════════════


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
# Fresh CSRF from the OIDC session cookies — the admin CSRFs
# won't validate against these new cookies.
oidc_user_csrf_token: cookie "oxicloud_csrf"


# oidc_user has no password AND no OPAQUE envelope — unlinking
# would lock them out entirely. The app service returns
# DomainError::AccessDenied with reason=no_alternative_auth in
# the audit log; the handler translates the generic AccessDenied
# into a stable machine-readable `error_type` the SPA switches
# on to render the "set a password first" affordance.
POST {{base_url}}/api/auth/oidc/unlink
Content-Type: application/json
X-CSRF-Token: {{oidc_user_csrf_token}}
{}

HTTP 403
[Asserts]
jsonpath "$.error_type" == "NoAlternativeAuth"


# Post-refusal invariant: the OIDC identity is still linked.
GET {{base_url}}/api/auth/me

HTTP 200
[Asserts]
jsonpath "$.federation_kind" == "oidc"
jsonpath "$.federation_issuer" == "{{oidc_issuer}}"
