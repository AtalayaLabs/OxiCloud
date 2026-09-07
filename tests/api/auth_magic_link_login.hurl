# =============================================================
# OxiCloud — magic-link login for password users
# =============================================================
# Regression pin for the `OXICLOUD_AUTH_POLICIES=permit_magic_link_for_password_users`
# switch. Default eligibility ladder refuses `has_password` accounts
# (the strict argument: mailbox-strength shouldn't shadow the stronger
# credential). Operators who prefer modern-SaaS UX opt-in via this
# policy; when set, `POST /api/auth/magic-link/send` mints a login token
# for accounts that also have a password.
#
# Cross-file coupling: `tests/common/server.env` sets
# `OXICLOUD_AUTH_POLICIES=permit_magic_link_for_password_users`. Without
# it, Step 2 below would land on `reason="has_password"` and mail nothing
# — Step 3's SMTP capture would fail with an empty inbox.
#
# What is NOT exercised here:
#   * OIDC-master rule: covered separately in tests/oidc/oidc.hurl
#     step 2b (magic-link SEND refused when OIDC is enabled).
#   * `has_password` rejection under the strict default: can't be
#     exercised in the same run — the env is global. Rust unit test
#     on `magic_link_eligibility()` covers it directly.
# =============================================================


# ─────────────────────────────────────────────────────────────
# Step 1 — Admin login. Needed to reach the mock-SMTP capture
#          endpoint (admin-scoped: /api/admin/smtp/test/captured).
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "{{username}}", "password": "{{password}}" }

HTTP 200
[Captures]
alice_token: jsonpath "$.access_token"


# ─────────────────────────────────────────────────────────────
# Step 2 — Baseline: admin logs in normally with a password.
#          Confirms nothing about the policy has broken the
#          classic path. Same call as Step 1, kept as a
#          named baseline for readers of the test log.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "{{username}}", "password": "{{password}}" }

HTTP 200
[Asserts]
jsonpath "$.access_token" exists


# ─────────────────────────────────────────────────────────────
# Step 3 — Request a magic-link for the SAME user via email.
#          Anti-enum uniform 200 regardless of eligibility, so
#          the real proof of "policy fired, mail actually sent"
#          is the SMTP capture in Step 5. Without the policy
#          in server.env, this same request would be refused
#          under `reason="has_password"` and no mail would be
#          captured.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/magic-link/send
Content-Type: application/json
{ "email": "{{email}}" }

HTTP 200
[Asserts]
jsonpath "$.message" contains "sign-in link"


# ─────────────────────────────────────────────────────────────
# Step 4 — Same request, but with the LOGIN-IDENTIFIER passed
#          as a username (no `@`). Server dispatches on `@` and
#          resolves the username to the registered email BEFORE
#          rate-limiting, so `admin` and `admin@example.com`
#          bucket on one budget. Uniform 200 either way.
#
#          The browser-binding challenge cookie is captured HERE
#          (not on Step 3): each `/send` request mints a fresh
#          challenge, and Step 5 will fetch the MOST RECENT mail —
#          which was minted by this very request. Capturing from
#          Step 3 instead would pair a stale cookie with Step 4's
#          token, and Step 6's redemption would land on PR 22's
#          cross-browser confirmation page (200 HTML) instead of
#          the direct 302.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/magic-link/send
Content-Type: application/json
{ "email": "{{username}}" }

HTTP 200
[Asserts]
jsonpath "$.message" contains "sign-in link"
[Captures]
alice_magic_cookie: header "set-cookie" regex "oxicloud_magic_request=([^;]+)"


# ─────────────────────────────────────────────────────────────
# Step 5 — Capture the mail. The mock SMTP records every
#          outbound message keyed on the recipient. Two magic-
#          link mails should have landed (steps 3 and 4), both
#          addressed to the admin's registered email. The
#          captured endpoint returns the MOST RECENT one — we
#          extract its link.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/admin/smtp/test/captured?to={{email}}
Authorization: Bearer {{alice_token}}

HTTP 200
[Asserts]
jsonpath "$.to" == "{{email}}"
jsonpath "$.text_body" matches "/magic/v1/[A-Za-z0-9_-]+"
[Captures]
alice_magic_url: jsonpath "$.text_body" regex "(https?://[^\\s]+/magic/v1/[A-Za-z0-9_-]+)"


# ─────────────────────────────────────────────────────────────
# Step 6 — Redeem the link with the matching browser-binding
#          cookie. Internal user, no resource target → lands
#          on `/files` (SPA route). Access-token cookie is set
#          on the redirect response.
# ─────────────────────────────────────────────────────────────
GET {{alice_magic_url}}
Cookie: oxicloud_magic_request={{alice_magic_cookie}}

HTTP 302
[Asserts]
header "Location" == "/files"
[Captures]
alice_magic_access_token: cookie "oxicloud_access"


# ─────────────────────────────────────────────────────────────
# Step 7 — The cookie session works: /api/auth/me returns the
#          admin's own profile. Proves the magic-link redemption
#          created a real session for the password-holding user
#          — the point of the whole `permit_magic_link_for_password_users`
#          policy.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/auth/me
Authorization: Bearer {{alice_magic_access_token}}

HTTP 200
[Asserts]
jsonpath "$.full.user.email" == "{{email}}"
jsonpath "$.full.user.username" == "{{username}}"
[Captures]
admin_user_id: jsonpath "$.full.user.id"


# ─────────────────────────────────────────────────────────────
# Step 7b — SessionOrigin stamping regression. Every login handler
#           records HOW the session was minted; the admin panel
#           surfaces that. This step proves TWO origins land
#           correctly on the same account:
#             * `password`   — from Steps 1-2 legacy /api/auth/login
#             * `magic_link` — from Step 6 magic-link redemption
#           A missing/drifted stamp (e.g. a handler forgetting to
#           pass the SessionOrigin arg after a refactor) would
#           surface here as `unknown` instead of the expected value.
#
#           `include_revoked=true` because Step 1 and Step 2 both
#           create sessions and the second may have rotated the
#           first out — we want ALL of admin's sessions in-frame.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/admin/sessions?user_id={{admin_user_id}}&include_revoked=true
Authorization: Bearer {{alice_token}}

HTTP 200
[Asserts]
# `body contains` rather than a jsonpath collection predicate because
# Hurl unwraps single-element `[*]` results to scalars — see the same
# pattern in tests/oidc/oidc.hurl Step 8b for the full reasoning.
body contains "\"origin\":\"password\""
body contains "\"origin\":\"magic_link\""
# `access_token_expiry_secs` also served for the SPA's revoke-lag
# notice. Belt-and-braces with tests/oidc/oidc.hurl Step 8b (same
# handler, both suites verify the field ships so a shape change
# would fail at least one of them).
jsonpath "$.access_token_expiry_secs" isInteger
jsonpath "$.access_token_expiry_secs" > 0


# ─────────────────────────────────────────────────────────────
# Step 8 — Anti-enum sanity: magic-link for a non-existent
#          identifier. Same uniform 200 shape, no mail sent.
#          The audit log records reason="no_account" — not
#          observable from the client, but the response shape
#          is IDENTICAL to Step 3, which is the whole point.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/magic-link/send
Content-Type: application/json
{ "email": "ghost-user-that-doesnt-exist" }

HTTP 200
[Asserts]
jsonpath "$.message" contains "sign-in link"
