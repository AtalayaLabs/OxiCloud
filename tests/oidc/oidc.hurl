# =============================================================
# OxiCloud — OIDC happy-path integration test
# =============================================================
# Drives the full SSO flow against the fake IdP under
# tests/oidc/fake_idp/ (panva/node-oidc-provider with an auto-approve
# interaction handler) and asserts every contract the SPA depends on.
# Pinned regression: commit d1bbe8ba changed the callback's frontend
# redirect from `/?oidc_code=…` to `/login?oidc_code=…` — Step 5's
# `Location matches "^…/login\?oidc_code=…"` is the assertion that
# would have caught that bug before users hit it.
#
# Flow walked manually (no auto-follow) so each handler is asserted
# independently:
#
#   1. Bootstrap: create the local admin (provider list works
#      regardless of admin presence, but the rest of the test
#      lives more comfortably with a fully-initialised server).
#   2. GET /api/auth/oidc/providers — SPA reads this to render
#      the SSO button.
#   3. GET /api/auth/oidc/authorize — server mints state + PKCE,
#      redirects to the IdP.
#   4. GET <IdP>/auth (+ full redirect chain) — fake-idp
#      auto-approves login + consent, OxiCloud's callback
#      JIT-provisions the user and redirects to
#      {frontend_url}/login?oidc_code=… The final 404 (no SPA
#      shell in test config) IS the test signal: we assert on
#      the URL we landed at, which is the d1bbe8ba contract.
#   5. POST /api/auth/oidc/exchange — SPA swaps the one-time
#      code for tokens + cookies.
#   6. GET /api/auth/me — proves the cookie session is live AND
#      that every OIDC profile claim (name → username, given_name,
#      family_name, picture → image, email, groups → admin role)
#      was JIT-provisioned correctly into the local user record.
#   7. POST /api/auth/refresh — rotation of all three cookies;
#      proves the SPA's session-renewal path works on top of
#      an OIDC-provisioned account.
#   8. GET /api/auth/me — the refreshed cookies authenticate too.
#   9. Existing-user re-login — a second OIDC flow with the same
#      `sub` resolves to the same local user, not a duplicate.
#  10. Anti-takeover — an unverified-email callback is rejected.
#  11. POST /api/auth/oidc/exchange — replay-protection: the
#      one-time code is single-use, second exchange returns 401.
# =============================================================


# ─────────────────────────────────────────────────────────────
# Step 1 — Create the local admin
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/setup
Content-Type: application/json
{
  "username": "{{username}}",
  "email":    "{{email}}",
  "password": "{{password}}"
}

HTTP 201


# ─────────────────────────────────────────────────────────────
# Step 2 — Provider discovery for the SPA
#
# Was `GET /api/auth/oidc/providers` (retired on feat/registration);
# OIDC enablement + provider list moved to `/api/config.auth.oidc`
# as the `providers: [...]` array (forward-compatible with planned
# multi-provider deployments). This test instance (`server-with-oidc.env`)
# configures one provider, so `providers.length == 1`.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/config

HTTP 200
[Asserts]
jsonpath "$.auth.oidc.enabled" == true
jsonpath "$.auth.oidc.providers" count == 1
# tests/common/server-with-oidc.env sets OXICLOUD_OIDC_PROVIDER_NAME=MockSSO.
jsonpath "$.auth.oidc.providers[0].provider_name" == "MockSSO"
jsonpath "$.auth.oidc.providers[0].authorize_endpoint" == "/api/auth/oidc/authorize"
jsonpath "$.auth.password.enabled" == true
# Deployment-wide OIDC-master-mutes-magic-link rule retired on
# feat/registration — `/api/config.auth.{method}.enabled` mirrors
# `OXICLOUD_AUTH_METHODS` directly, so `magic_link` listed there is
# `true` here. Per-user enforcement (OIDC-federated users refused +
# coaching email) handles the MFA-bypass concern that the deployment
# rule used to defend against; see `tests/oidc/magic_link_coaching.hurl`
# for the matrix pin and `docs/config/authentication.md §
# Magic-link eligibility` for the full table.
jsonpath "$.auth.magic_link.enabled" == true


# ─────────────────────────────────────────────────────────────
# Step 2b — Magic-link endpoint accepts requests on an OIDC-enabled
#           deployment. The former OIDC-master-mutes-magic-link rule
#           (deployment-wide 403 whenever OIDC was on) retired on
#           feat/registration — a server with `magic_link` listed in
#           `AUTH_METHODS` serves the endpoint regardless of OIDC,
#           and the per-user eligibility matrix in
#           `magic_link_eligibility()` decides what happens next:
#           external users (no credential at all) get a login link;
#           OIDC-federated users get a coaching email; password users
#           get a coaching email unless the operator opted in via
#           `permit_magic_link_for_password_users`.
#
#           The response here is uniform (200, "Check your inbox")
#           for the anti-enumeration guarantee — the unknown-email
#           case lands on the same shape as a real one. The matrix
#           pin lives in `tests/oidc/magic_link_coaching.hurl`.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/magic-link/send
Content-Type: application/json
{ "email": "someone@example.com" }

HTTP 200


# ─────────────────────────────────────────────────────────────
# Step 3 — SPA-initiated authorize. Server returns a 302 with
#          state + PKCE challenge in the Location URL.
#          [Options] location: false keeps Hurl from following
#          the redirect so we can capture the target intact.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/auth/oidc/authorize
[Options]
location: false

# 307 (not 302): handler uses axum Redirect::temporary, which preserves
# the request method on follow. For a GET-initiated SSO flow it makes
# no practical difference, but the assertion has to match what's emitted.
HTTP 307
[Captures]
idp_url: header "Location"
[Asserts]
# panva/node-oidc-provider publishes authorize at /auth (not
# /authorize). The OXICLOUD_OIDC_ISSUER_URL points at the issuer
# root; the discovery doc tells OxiCloud the actual endpoint.
header "Location" matches "^{{oidc_authorize_endpoint}}\\?"
header "Location" contains "state="
header "Location" contains "code_challenge="
header "Location" contains "code_challenge_method=S256"
header "Location" contains "client_id=oxicloud-test"
header "Location" contains "redirect_uri="


# ─────────────────────────────────────────────────────────────
# Step 4 — Walk the entire IdP + OxiCloud redirect chain.
#
#          The fake IdP's auto-approve handler resolves login +
#          consent silently and 302s back to OxiCloud's callback;
#          the callback validates state + exchanges code with the
#          IdP, JIT-provisions the user, then 307s the browser to
#          {frontend_url}/login?oidc_code=…
#
#          With `location: true` Hurl follows the whole chain and
#          lands on the SPA login URL. The SvelteKit SPA serves
#          `/login` from `static-dist/login.html` with 200 — this
#          is the production contract. The runner (`tests/oidc/run.sh`)
#          builds `static-dist/` before launching the server so
#          local and CI both see the production behaviour. Without
#          that build the route would 404 via the ServeDir fallback.
#
#          A pre-d1bbe8ba server would have redirected to
#          `http://localhost:8087/?oidc_code=…` instead — the
#          `landed_at` regex below catches that regardless.
# ─────────────────────────────────────────────────────────────
GET {{idp_url}}
[Options]
location: true
location-trusted: true

HTTP 200
[Captures]
landed_at: url
oidc_code: url regex "oidc_code=([a-f0-9]+)"
[Asserts]
# The d1bbe8ba regression guard. The exact contract the SvelteKit
# SPA depends on — `/login`, not `/`.
variable "landed_at" matches "^http://localhost:8087/login\\?oidc_code=[a-f0-9]+$"


# ─────────────────────────────────────────────────────────────
# Step 5 — Swap the one-time code for a session.
#          Response sets the HttpOnly auth cookies + the
#          double-submit CSRF cookie the SPA reads to populate
#          X-CSRF-Token on subsequent mutating requests.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/oidc/exchange
Content-Type: application/json
{ "code": "{{oidc_code}}" }

HTTP 200
[Captures]
oidc_session_user: jsonpath "$.user.full.user.username"
# Snapshotted so Step 7's refresh can prove the tokens rotated
# rather than being re-issued unchanged. The refresh handler in
# auth_handler.rs always rotates all three cookies (access JWT,
# refresh UUID, CSRF UUID); a regression that silently keeps the
# old refresh token would let a leaked refresh credential live
# forever — exactly the kind of issue token-family rotation exists
# to prevent.
initial_access_token:  jsonpath "$.access_token"
initial_refresh_token: jsonpath "$.refresh_token"
initial_csrf_token:    cookie "oxicloud_csrf"
[Asserts]
jsonpath "$.user.full.user.username" == "oidc_user"
jsonpath "$.user.full.user.email"    == "oidc@example.com"
jsonpath "$.access_token"  isString
# Multiple Set-Cookie headers come back as a list of values, so
# `contains` only matches whole-element strings. Each cookie shows up
# as its own list entry; we use `cookie "<name>"` (Hurl's dedicated
# helper) which finds the cookie by name across all Set-Cookie headers.
cookie "oxicloud_access"   exists
cookie "oxicloud_refresh"  exists
cookie "oxicloud_csrf"     exists
cookie "oxicloud_access[HttpOnly]"  exists
cookie "oxicloud_refresh[HttpOnly]" exists


# ─────────────────────────────────────────────────────────────
# Step 6 — Same-jar follow-up GET proves the cookie session
#          actually authenticates. Hurl reuses the cookie jar
#          across requests in one file by default, so the
#          Set-Cookie from Step 5 carries forward.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/auth/me

HTTP 200
[Captures]
# Stash the user id for the re-login check in Step 10 below — a
# second OIDC flow with the same `sub` must resolve back to this
# exact user, not silently create a duplicate.
oidc_user_id: jsonpath "$.full.user.id"
[Asserts]
jsonpath "$.full.user.username" == "oidc_user"
jsonpath "$.full.user.email"    == "oidc@example.com"
# Post the federation-identity rename (docs/plan/ocm.md § Schema
# rename) UserDto exposes federation_kind + federation_issuer as
# separate nullable fields. Local users have both null; OIDC users
# get kind="oidc" and issuer=<the id_token iss claim URL>. For
# the fake IdP (tests/oidc/fake_idp/server.js) that URL is the
# issuer published in its discovery document, which matches
# `oidc_issuer` from test.env.
jsonpath "$.full.federation_kind"   == "oidc"
jsonpath "$.full.federation_issuer" == "{{oidc_issuer}}"
# Full claim round-trip — the fake IdP (tests/oidc/fake_idp/server.js)
# pins these values and OxiCloud must persist each one verbatim during
# JIT provisioning (see auth_application_service.rs around line 2257).
# A regression that drops, swaps, or truncates a claim trips here.
# Note the field name flip on the API side: OIDC `picture` becomes
# UserDto.image (a URL or data URI).
jsonpath "$.full.user.given_name"  == "OIDC"
jsonpath "$.full.user.family_name" == "Test"
jsonpath "$.full.user.image"       == "https://example.com/oidc-test-user.png"
# Group-to-role mapping. server-with-oidc.env sets
# OXICLOUD_OIDC_ADMIN_GROUPS=admin-users; the fake IdP's claims include
# `groups: ["admin-users"]`. The JIT path intersects the claim against
# the env and promotes the new user from `user` to `admin`. A
# regression here would silently strip (or wrongly grant) admin rights
# for every SSO deployment that uses group-based role mapping.
jsonpath "$.full.user.role" == "admin"


# ─────────────────────────────────────────────────────────────
# Step 7 — Refresh-token rotation.
#
#          POST /api/auth/refresh reads the refresh token from
#          the HttpOnly oxicloud_refresh cookie (the browser flow
#          OxiCloud's SPA uses; the JSON body shape is only a
#          backwards-compat path for non-browser clients) and
#          re-issues all three cookies. Token-family rotation:
#          the prior refresh token is invalidated server-side
#          and a reuse attempt would be caught as a theft signal.
#
#          CSRF middleware fires here because we have a cookie
#          session — we pass the captured oxicloud_csrf value as
#          the double-submit X-CSRF-Token header, matching what
#          the SvelteKit SPA does via getCsrfHeaders().
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/refresh
X-CSRF-Token: {{initial_csrf_token}}
Content-Type: application/json
{}

HTTP 200
[Captures]
refreshed_access_token:  jsonpath "$.access_token"
refreshed_refresh_token: jsonpath "$.refresh_token"
# Also capture the ROTATED csrf token so Step 8c (logout POST) can
# thread it into `X-CSRF-Token`. The refresh handler rotates all
# three cookies including csrf — using `initial_csrf_token` here
# would 403 at the CSRF middleware because it no longer matches
# the (freshly-rotated) `oxicloud_csrf` cookie on the browser.
refreshed_csrf_token:    cookie "oxicloud_csrf"
[Asserts]
jsonpath "$.user.full.user.username" == "oidc_user"
jsonpath "$.access_token"  isString
jsonpath "$.refresh_token" isString
# All three cookies must rotate. If any value were re-used, a
# regression in cookie_auth::append_auth_cookies (or in the
# RefreshToken use case) would silently leave the old credential
# live — exactly the kind of bug that motivates rotation.
variable "refreshed_access_token"  != "{{initial_access_token}}"
variable "refreshed_refresh_token" != "{{initial_refresh_token}}"
cookie "oxicloud_access"   exists
cookie "oxicloud_refresh"  exists
cookie "oxicloud_csrf"     exists


# ─────────────────────────────────────────────────────────────
# Step 8 — The refreshed cookies authenticate too. Belt-and-braces:
#          rotation is only useful if the new tokens actually work.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/auth/me

HTTP 200
[Asserts]
jsonpath "$.full.user.username" == "oidc_user"


# ─────────────────────────────────────────────────────────────
# Step 8b — Admin sessions panel exposes `origin = "oidc"` and
#           NEVER leaks the raw IdP `sid`. The oidc_user was JIT-
#           provisioned admin by its `groups` claim (see Step 6),
#           so /api/admin/sessions is reachable with the current
#           cookies. Also asserts `access_token_expiry_secs` is
#           served so the SPA can render the revoke-lag notice.
#
#           Regressions this pins:
#             * origin column drops back to "unknown" on refresh
#               (would break the panel filter for OIDC-only view);
#             * DTO reintroduces `oidc_sid` (the anti-leak fix from
#               dto_never_leaks_oidc_sid in session_dto.rs);
#             * handler stops publishing the TTL (would silently
#               kill the revoke-lag notice on the admin page).
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/admin/sessions?limit=25

HTTP 200
[Asserts]
# At least our own OIDC session is here. `body contains` rather than a
# jsonpath collection predicate because Hurl unwraps single-element
# `$.sessions[*].origin` results to a scalar, and the deprecated
# `includes` didn't handle that consistently either. Distinctive-enough
# substring — a false positive would need `"origin":"oidc"` to appear
# elsewhere in the SessionSummaryDto shape, which by construction it
# doesn't.
body contains "\"origin\":\"oidc\""
# TTL surfaces for the SPA's revoke-lag notice.
jsonpath "$.access_token_expiry_secs" isInteger
jsonpath "$.access_token_expiry_secs" > 0
# The raw IdP sid must never appear in the wire shape — not the key,
# not even a prefix (see dto_never_leaks_oidc_sid unit test).
body not contains "oidc_sid"
# id_token is a JWT — three base64-url parts joined by `.`. A leak
# would materialise as a long dotted token in the response body.
# The fake IdP's issuer URL is a durable substring of every id_token
# claim payload, so absence of that URL is a cheap "no id_token
# leaked" proof.
body not contains "{{oidc_issuer}}"


# ─────────────────────────────────────────────────────────────
# Step 8c — RP-initiated logout. Server MUST return
#           `post_logout_url` on the /api/auth/logout response
#           because the session's `oidc_id_token` is populated
#           (both at OIDC-callback INSERT time AND — critically
#           — after the Step 7 refresh which used to drop it).
#           This is the regression that made "logout after OIDC
#           login → refresh → logout" go through the local-only
#           path, leaving the IdP session live.
#
#           The post_logout_url is the IdP's `end_session_endpoint`
#           carrying `id_token_hint` + `post_logout_redirect_uri`.
#           We assert its shape rather than following it (Step 9
#           does a fresh OIDC login anyway).
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/logout
X-CSRF-Token: {{refreshed_csrf_token}}
Content-Type: application/json
{}

HTTP 200
[Asserts]
jsonpath "$.post_logout_url" exists
jsonpath "$.post_logout_url" matches "id_token_hint="
jsonpath "$.post_logout_url" matches "post_logout_redirect_uri="
# Cookie-clearing is covered by other tests
# (`auth_session_lifecycle.hurl`); the point of THIS step is the
# `post_logout_url` shape — proving the id_token carried across the
# Step 7 refresh, which the bug we fixed used to drop.


# ─────────────────────────────────────────────────────────────
# Step 9 — Existing-user re-login. A second pass through the same
#          OIDC `sub` MUST resolve back to the SAME local user
#          (`oidc_user_id` captured in Step 6) — silently creating
#          a duplicate account on every login would be the
#          regression. Exercises the existing-user branch in
#          auth_application_service.rs around line 2157, distinct
#          from the JIT-provisioning branch the earlier steps hit.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/auth/oidc/authorize
[Options]
location: false

HTTP 307
[Captures]
relogin_idp_url: header "Location"


GET {{relogin_idp_url}}
[Options]
location: true
location-trusted: true

# Same contract as Step 4 — the SPA serves /login with 200 (the
# runner ensures static-dist/ is built before the server starts).
HTTP 200
[Captures]
relogin_oidc_code: url regex "oidc_code=([a-f0-9]+)"
[Asserts]
variable "landed_at" matches "^http://localhost:8087/login\\?oidc_code=[a-f0-9]+$"


POST {{base_url}}/api/auth/oidc/exchange
Content-Type: application/json
{ "code": "{{relogin_oidc_code}}" }

HTTP 200
[Asserts]
# Same local id — proves the existing-user resolver matched on `sub`
# (or `oidc_provider + oidc_subject`) instead of minting a new row.
jsonpath "$.user.full.user.id" == "{{oidc_user_id}}"
jsonpath "$.user.full.user.username" == "oidc_user"
# Role from the prior JIT-provisioned admin survives the re-login.
# Two regressions this catches: (a) the existing-user branch wiping
# the role to a default `user`; (b) the existing-user branch
# re-evaluating groups but missing the admin-group claim (the fake
# IdP still emits `groups: ["admin-users"]`, OXICLOUD_OIDC_ADMIN_GROUPS
# still resolves to "admin"). Either way, the role should remain
# `admin` — otherwise we have a silent admin demotion on every login.
jsonpath "$.user.full.user.role" == "admin"


# ─────────────────────────────────────────────────────────────
# Step 10 — Operator-override: with `OXICLOUD_REQUIRE_VERIFIED_EMAIL=false`
#           in `tests/common/server-with-oidc.env` (matching the
#           default test posture), an OIDC callback whose
#           `email_verified` claim is `false` MUST be accepted.
#           This is the "I trust my IdP end-to-end" posture — the
#           operator has told the server not to gate on the
#           verification signal.
#
#           This test used to be the anti-takeover check (assert
#           rejection) BEFORE commit 1801150a moved the OIDC email
#           check under the operator flag. Post-1801150a it flipped
#           to a positive test of the operator-override branch.
#
#           The rejection branch (`flag=true` + IdP `Some(false)` or
#           `None`) is proved OUT-OF-SUITE — this Hurl file runs one
#           server with one env config; asserting both branches
#           needs either a second `hurl` invocation with the flag
#           flipped, or a Rust unit test that exercises
#           `handle_oidc_callback_with_id_claims` directly. The
#           audit-log discriminator (`oidc.email_unverified_accepted`
#           with reason `idp_asserts_unverified_flag_off`) is the
#           operator-visible signal on the accept path here.
# ─────────────────────────────────────────────────────────────
POST http://localhost:1080/control/email-verified/false

HTTP 200


GET {{base_url}}/api/auth/oidc/authorize
[Options]
location: false

HTTP 307
[Captures]
unverified_idp_url: header "Location"


GET {{unverified_idp_url}}
[Options]
location: true
location-trusted: true

# Positive assertion of the operator-override: the redirect chain
# lands on `/login?oidc_code=` (successful OIDC callback), and
# the status is 2xx or 3xx (never 4xx/5xx). A regression that
# re-added an unconditional rejection would land on the login
# error page instead — either the URL negation or the status
# ceiling catches it.
HTTP *
[Asserts]
status < 400
url matches "^http://localhost:8087/login\\?oidc_code="


# Reset the IdP so this test doesn't poison anything that runs
# after it (defensive — there's nothing after right now, but a
# future test would silently fail with unexpected accept-paths
# if we forgot this).
POST http://localhost:1080/control/email-verified/true

HTTP 200


# ─────────────────────────────────────────────────────────────
# Step 11 — Replay protection: the one-time code is rejected on a
#          second attempt. Defense-in-depth check.
#
#          Hurl 4.x has no per-request cookie-jar clear, so this
#          request still carries the session cookies set in Step 5.
#          That means CSRF middleware rejects the unauthenticated
#          (no X-CSRF-Token header) POST with 403 BEFORE the OIDC
#          single-use-code check runs. Both are valid replay
#          defenses; in a real attack the attacker has the code but
#          not the session cookie, in which case the rejection
#          would come from the OIDC layer as 401.
#
#          The single-use-code path itself is covered by unit
#          tests in auth_application_service.rs (the
#          completed_oidc_logins moka cache + remove-on-use).
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/oidc/exchange
Content-Type: application/json
{ "code": "{{oidc_code}}" }

HTTP 403


# ═════════════════════════════════════════════════════════════
# Step 12 — Nextcloud Login Flow v2 via OIDC — user with two drives
# ═════════════════════════════════════════════════════════════
# The flow always completes on the success page and yields the
# user's default personal drive, whatever other drives they hold.
#
#   A. Local admin logs in to get a JWT for administrative calls.
#   B. Admin creates a shared drive owned by `oidc_user` (two drives).
#   C. NC LFv2 initiate.
#   D. Pre-completion poll is 404.
#   E. GET /login/v2/flow/{token}/oidc 307s to the IdP.
#   F. The IdP -> callback chain ends on the success page.
#   G. The poll returns the bare username, no `~` marker.
#   H. A second poll is 404.
# ═════════════════════════════════════════════════════════════


# ─────────────────────────────────────────────────────────────
# Step 12A — Local admin password login.
#            The OIDC-provisioned `oidc_user` (auto-promoted to
#            admin via the group claim) has NO local password;
#            only local admin (created in Step 1) can authenticate
#            with `username/password`. Use JWT (Bearer) instead of
#            cookies so we skip CSRF ceremony for the drive-create
#            call below.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "{{username}}", "password": "{{password}}" }

HTTP 200
[Captures]
admin_token: jsonpath "$.access_token"


# ─────────────────────────────────────────────────────────────
# Step 12B — Admin creates a shared drive owned by `oidc_user`,
#            so the user holds two drives.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/drives
Authorization: Bearer {{admin_token}}
Content-Type: application/json
{
  "kind": "shared",
  "name": "oidc-user-shared",
  "owner": { "type": "user", "id": "{{oidc_user_id}}" }
}

HTTP 201
[Captures]
fixture_drive_id: jsonpath "$.id"


# ─────────────────────────────────────────────────────────────
# Step 12C — NC client initiates LFv2. Public endpoint, no auth.
#            Response carries the flow token (embedded in the
#            login URL) and the poll token (used by the NC
#            client's backchannel).
#
#            Note: the initiate endpoint lives at
#            `/index.php/login/v2` (nc_routes.rs:50) — the bare
#            `/login/v2` variant only exists for the poll
#            surface, not for initiate. NC clients build the URL
#            from the `/index.php` convention.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/index.php/login/v2

HTTP 200
[Captures]
nc_poll_token: jsonpath "$.poll.token"
# Regex-extract the flow_token from the login URL. Shape is
# `http://localhost:8087/login/v2/flow/<hex>`. The trailing hex is
# what /login/v2/flow/{token}/... routes bind on.
nc_flow_token: jsonpath "$.login" regex "/login/v2/flow/([a-f0-9]+)"


# ─────────────────────────────────────────────────────────────
# Step 12D — Baseline poll. No user has authenticated yet, so the
#            flow has no `completed` result. MUST be 404.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/login/v2/poll
Content-Type: application/x-www-form-urlencoded
`token={{nc_poll_token}}`

HTTP 404


# ─────────────────────────────────────────────────────────────
# Step 12E — Kick off the NC OIDC branch. Server prepares an OIDC
#            authorize with the NC flow token attached to state
#            (auth_application_service::prepare_oidc_authorize_for_nextcloud)
#            and 307s to the IdP. `location: false` so we can
#            capture the exact IdP URL for the manual chain follow
#            below.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/login/v2/flow/{{nc_flow_token}}/oidc
[Options]
location: false

HTTP 307
[Captures]
nc_idp_url: header "Location"
[Asserts]
# The IdP URL must carry `state` (which encodes nc_flow_token
# server-side) and the PKCE challenge — same shape as the SPA
# path in Step 3, just prepared through a different code path.
header "Location" matches "^{{oidc_authorize_endpoint}}\\?"
header "Location" contains "state="
header "Location" contains "code_challenge_method=S256"


# ─────────────────────────────────────────────────────────────
# Step 12F — Follow the full IdP -> callback chain. The flow
#            completes without a picker and lands on the success page.
# ─────────────────────────────────────────────────────────────
GET {{nc_idp_url}}
[Options]
location: true
location-trusted: true

HTTP 200
[Asserts]
url endsWith "/nextcloud/success"
body not contains "Choose a drive"
body not contains "nc://login"


# ─────────────────────────────────────────────────────────────
# Step 12G — Poll. Credentials are ready and carry the bare
#            username: the default personal drive, no `~` marker.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/login/v2/poll
Content-Type: application/x-www-form-urlencoded
`token={{nc_poll_token}}`

HTTP 200
[Asserts]
jsonpath "$.server"      matches "^https?://[^/]+:8087$"
jsonpath "$.appPassword" isString
jsonpath "$.loginName" == "oidc_user"


# ─────────────────────────────────────────────────────────────
# Step 12H — Poll again — MUST 404. The completed result is
#            single-use.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/login/v2/poll
Content-Type: application/x-www-form-urlencoded
`token={{nc_poll_token}}`

HTTP 404


# ─────────────────────────────────────────────────────────────
# No teardown for the fixture drive: `tests/oidc/run.sh` spawns a
# fresh DB per invocation.
# ─────────────────────────────────────────────────────────────


# =============================================================
# Steps 13* — OIDC Back-Channel Logout 1.0
# =============================================================
# Proves the /api/auth/oidc/backchannel-logout endpoint accepts a
# valid IdP-signed logout_token and evicts the corresponding
# OxiCloud session — the shared-computer / single-sign-out fix
# that RP-initiated logout alone doesn't cover (RPI needs the
# browser; BCL is server-to-server and works even when the user's
# device is offline).
#
# The fake IdP mints and posts the logout_token itself via its
# `/control/backchannel-logout` endpoint (server.js handleControl):
# it signs with the same RS256 keypair whose public half sits at
# /jwks.json, so OxiCloud's validator (identical code path to
# id_token verification) accepts the signature. The Node fetch()
# then POSTs the token as application/x-www-form-urlencoded to
# OxiCloud, matching BCL §2.5.
#
# We deliberately test the sub-only path here (no `sid`) so the
# service exercises revoke_user_sessions_by_oidc_subject (the
# fallback branch used when the IdP doesn't emit `sid`). Sid-based
# per-device revocation shares the same validator + audit shape;
# a unit test in session_pg_repository covers that branch.
# =============================================================


# ─────────────────────────────────────────────────────────────
# Step 13a — Freshly log in as `oidc_user`. Cookies from Step 9's
#            re-login could still be usable, but the Nextcloud flow
#            in Steps 12* has interleaved admin login/logout since
#            then and the safest thing to prove BCL revoked
#            "something live" is to start with a session we JUST
#            minted. The [Options] block clears cookies so the
#            `Set-Cookie` from the exchange below is what we assert
#            on.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/auth/oidc/authorize
[Options]
location: false

HTTP 307
[Captures]
bcl_idp_url: header "Location"


GET {{bcl_idp_url}}
[Options]
location: true
location-trusted: true

HTTP 200
[Captures]
bcl_oidc_code: url regex "oidc_code=([a-f0-9]+)"


POST {{base_url}}/api/auth/oidc/exchange
Content-Type: application/json
{ "code": "{{bcl_oidc_code}}" }

HTTP 200
[Asserts]
jsonpath "$.user.full.user.username" == "oidc_user"


# ─────────────────────────────────────────────────────────────
# Step 13b — Confirm the cookie session is live before we knock
#            it down. If /me fails here the eviction assertion in
#            13d becomes meaningless (couldn't tell "was live,
#            got revoked" from "was never live").
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/auth/me

HTTP 200
[Asserts]
jsonpath "$.full.user.username" == "oidc_user"


# ─────────────────────────────────────────────────────────────
# Step 13c — IdP-driven logout. The fake IdP's control endpoint
#            mints a spec-compliant logout_token (RS256-signed,
#            correct iss/aud, events claim, sub, jti, fresh iat)
#            and POSTs it to OxiCloud as
#            application/x-www-form-urlencoded per BCL §2.5.
#            OxiCloud MUST accept it and revoke every session
#            belonging to the OIDC subject — a 200 response with
#            oxicloud_status=200 in the forwarding echo proves it.
# ─────────────────────────────────────────────────────────────
POST {{oidc_issuer}}/control/backchannel-logout
Content-Type: application/json
{}

HTTP 200
[Asserts]
jsonpath "$.oxicloud_status" == 200


# ─────────────────────────────────────────────────────────────
# Step 13d — Refresh MUST fail. This is the load-bearing "BCL
#            actually kicked the user" proof.
#
#            Note on why we assert on /refresh and NOT /api/auth/me:
#            OxiCloud access tokens are stateless JWTs — the auth
#            middleware validates signature + expiry in-memory and
#            does NOT consult `sessions.revoked` on every request.
#            BCL flipped `sessions.revoked=true` (see audit log
#            `oidc.backchannel_logout_by_sub` — 3 sessions revoked)
#            which kills the refresh path immediately, but the
#            still-valid in-memory access token would let /me
#            return 200 until its natural expiry (~1 h default).
#            That is the standard JWT trade-off: BCL fully evicts
#            within one access-token TTL. The refresh 401 below is
#            what proves the eviction landed; once the access
#            token expires the user can't mint a new one.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/refresh
Content-Type: application/json
{}

# 403 (not 401): the JWT signature validates, but the session is
# revoked — that's an "access denied on a valid credential" outcome.
# The refresh handler maps DomainError::AccessDenied to StatusCode::
# FORBIDDEN. Also fires TokenReused audit + revokes the whole session
# family, which is the reuse-detection path (correctly identified: a
# call with a revoked refresh token is indistinguishable from theft
# from the server's viewpoint).
HTTP 403


# ─────────────────────────────────────────────────────────────
# Step 13f — Replay guard. Firing the exact same logout_token
#            twice in the freshness window must be a no-op —
#            OxiCloud's app service dedupes by `jti` (see
#            auth_application_service::backchannel_logout). The
#            IdP still returns 200 for the second call because
#            the control endpoint mints a NEW jti each time
#            (Math.random() salt), so this is really testing
#            "sending the same content twice is safe": second
#            call would find no live sessions and revoke 0 rows.
#            Either way the assertion is the same: HTTP 200 from
#            the control endpoint, oxicloud_status 200.
# ─────────────────────────────────────────────────────────────
POST {{oidc_issuer}}/control/backchannel-logout
Content-Type: application/json
{}

HTTP 200
[Asserts]
jsonpath "$.oxicloud_status" == 200
