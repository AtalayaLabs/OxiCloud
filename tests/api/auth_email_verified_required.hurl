# =============================================================
# OxiCloud — OXICLOUD_REQUIRE_VERIFIED_EMAIL=true
# =============================================================
# Runs in its OWN server pass. `tests/api/run.sh` stops the main
# server and restarts it with this one policy flipped, because it
# cannot be on for the rest of the suite: 14 scenarios register
# users through `POST /api/auth/register`, which does NOT stamp
# `email_verified_at`, and those users then log in. With the flag
# on they would all be refused.
#
# The override is a per-command env prefix at the launch site in
# `run.sh`, not an ambient variable and not a second config file:
# the binary's precedence is defaults < file < environment, and
# `--config` keeps suppressing the repo-root `.env` probe so a
# developer's sandbox settings cannot leak into a test server.
#
# Coverage:
#   1. An unverified account is REFUSED
#   2. The Owner is exempt — the instance cannot lock itself out
#   3. An admin-CREATED account is verified by fiat, so it passes
#   4. Recovery: verify by magic link, then the login works
#   5. Teardown
#
# Why 2 and 3 matter as much as 1: a policy that refuses everyone
# would satisfy 1 on its own. These pin that the exemption is
# Owner-only (NOT `at_least(Admin)`) and that admin provisioning
# still produces usable accounts.
# =============================================================


# ─────────────────────────────────────────────────────────────
# 0 — The Owner logs in, WITH the policy on and regardless of
#     their own verification state.
#
#     This is assertion 2, first, because it is also the
#     precondition for everything below: if the Owner could not
#     log in, an operator who enabled this flag would have
#     bricked their instance, and no later step could run.
#     `/api/setup` creates the first account as `UserRole::Owner`.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "{{username}}", "password": "{{password}}" }

HTTP 200
[Captures]
owner_token: jsonpath "$.access_token"


# ─────────────────────────────────────────────────────────────
# 1 — An admin-CREATED account logs in.
#
#     `POST /api/admin/users` stamps `email_verified_at = NOW()`
#     — admin fiat counts as verification — so enabling the
#     policy must not break provisioning. Were this to fail, the
#     flag would be unusable on any instance whose users are
#     admin-created, which is most of them.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/admin/users
Authorization: Bearer {{owner_token}}
Content-Type: application/json
{
  "username": "verified-by-fiat",
  "password": "VerifiedByFiat1!",
  "email": "verified-by-fiat@example.com",
  "role": "user"
}

HTTP 201
[Captures]
fiat_id: jsonpath "$.user.id"

POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "verified-by-fiat", "password": "VerifiedByFiat1!" }

HTTP 200


# ─────────────────────────────────────────────────────────────
# 2 — A SELF-REGISTERED account is unverified, and is REFUSED.
#
#     This is the policy actually doing something. Registration
#     succeeds — the account exists — but the login does not.
#     403 with `error_type: AccessDenied`, which is the shape
#     `check_email_verified` returns; a drift to a generic 401
#     would mean the SPA shows "bad credentials" to someone whose
#     password was perfectly correct.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/register
Content-Type: application/json
{
  "username": "needs-verification",
  "email": "needs-verification@example.com",
  "password": "NeedsVerify1!"
}

HTTP 200

POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "needs-verification", "password": "NeedsVerify1!" }

HTTP 403
[Asserts]
# `EmailNotVerified`, not the service's underlying `AccessDenied`: the
# handler remaps it to a distinguished `error_type` so the SPA can show
# "verify your address" instead of "bad credentials". Pinning the generic
# kind here would let that remap be dropped without anything failing, and
# the user would be told their password was wrong.
jsonpath "$.error_type" == "EmailNotVerified"


# ─────────────────────────────────────────────────────────────
# 3 — RECOVERY. The user proves control of the address via a
#     magic link, which stamps `email_verified_at`.
#
#     This is the half that makes the policy operable rather than
#     a trap: without a verification path, enabling the flag
#     would permanently exclude every already-registered
#     unverified user. The mock SMTP (`OXICLOUD_SMTP_MOCK=true`)
#     captures the mail so the link can be followed here.
#
#     NOTE on what this does NOT exercise. The refused login in
#     step 2 ALSO auto-sends a verification link — the handler
#     reasons that a caller who proved the password can safely be
#     mailed one, bypassing the anti-enum-fronted
#     `magic-link/send`. But it generates the browser-binding
#     challenge, mails it, and returns a plain error with NO
#     `oxicloud_magic_request` cookie on the response. So the
#     auto-sent link can only ever be redeemed as a
#     CROSS-BROWSER redemption, which takes the confirmation
#     prompt rather than a clean 302 — even for the very browser
#     that just attempted the login.
#
#     That is why this step calls `magic-link/send` explicitly
#     (permitted here by
#     `OXICLOUD_AUTH_POLICIES=permit_magic_link_for_password_users`):
#     it sets the cookie, so the redemption below is the
#     same-browser path. Whether the auto-send should set the
#     cookie too is a product question, not a test one — but a
#     reader comparing this file to the handler deserves to know
#     the two links differ.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/magic-link/send
Content-Type: application/json
{ "email": "needs-verification@example.com" }

HTTP 200
[Captures]
verify_cookie: header "set-cookie" regex "oxicloud_magic_request=([^;]+)"

GET {{base_url}}/api/admin/smtp/test/captured?to=needs-verification@example.com
Authorization: Bearer {{owner_token}}

HTTP 200
[Captures]
verify_url: jsonpath "$.text_body" regex "(https?://[^\\s]+/magic/v1/[A-Za-z0-9_-]+)"

# Redeeming the link both logs them in AND marks the address
# verified — the redemption IS the proof of control.
GET {{verify_url}}
Cookie: oxicloud_magic_request={{verify_cookie}}

HTTP 302


# ─────────────────────────────────────────────────────────────
# 4 — And now the PASSWORD login works, which is the assertion
#     that closes the loop.
#
#     Same account, same password, same policy — the only thing
#     that changed is the verification stamp. Without this row,
#     step 2's refusal could equally be explained by a broken
#     account rather than by the policy.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "needs-verification", "password": "NeedsVerify1!" }

HTTP 200
[Captures]
# `$.user.full.user.id`, not `$.user.id`: the login response is
# `AuthResponseDto { user: SelfUserDto { full: FullUserDto { user:
# PublicUserDto } } }` and nothing is `#[serde(flatten)]`ed, so the id
# really is three levels down. Checked against the generated spec rather
# than guessed — the shorter path reads correctly and silently captures
# nothing.
recovered_id: jsonpath "$.user.full.user.id"


# ─────────────────────────────────────────────────────────────
# 5 — Teardown. Both fixtures, so a hand-run repeat starts clean
#     rather than colliding on the username.
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/api/admin/users/{{fiat_id}}
Authorization: Bearer {{owner_token}}

HTTP 200

DELETE {{base_url}}/api/admin/users/{{recovered_id}}
Authorization: Bearer {{owner_token}}

HTTP 200
