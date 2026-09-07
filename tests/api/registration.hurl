# =============================================================
# OxiCloud — email-only registration (PR 18)
# =============================================================
# PR 18 makes `password` (and `username`) optional in
# `POST /api/auth/register`. Email-only signup:
#   - returns a uniform 200 message (no JWT, no UserDto)
#   - mints a welcome magic-link mailed to `email`
#   - redemption lands the new internal user on `/files`
#     (not `/shared-with-me`, which is for externals)
#
# Requires `OXICLOUD_SMTP_MOCK=true` (set in tests/common/server.env).
# =============================================================


# ─────────────────────────────────────────────────────────────
# Step 1 — admin login (cleanup ops at the end need her token).
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "{{username}}", "password": "{{password}}" }

HTTP 200
[Captures]
alice_token: jsonpath "$.access_token"


# ─────────────────────────────────────────────────────────────
# Step 2 — Classic registration (with password). PR 20 anti-
#          enumeration mode (SMTP wired) returns a uniform 200
#          regardless of success or collision. No UserDto in
#          the response — the frontend logs the user in
#          separately to get a session.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/register
Content-Type: application/json
{
  "username": "charlie",
  "email": "charlie@example.com",
  "password": "TestPassword1!"
}

HTTP 200
[Asserts]
jsonpath "$.message" contains "request received"


# ─────────────────────────────────────────────────────────────
# Step 2b — Log in as charlie to confirm registration succeeded
#           AND to capture her user_id for cleanup.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "charlie", "password": "TestPassword1!" }

HTTP 200
[Captures]
charlie_token: jsonpath "$.access_token"
charlie_user_id: jsonpath "$.user.full.user.id"


# ─────────────────────────────────────────────────────────────
# Step 3 — Email-only registration. No username, no password.
#          Returns 200 + uniform message; welcome magic-link
#          is captured by the MockEmailSender.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/register
Content-Type: application/json
{
  "email": "pr18-emailonly@example.com"
}

HTTP 200
[Captures]
# PR 22 — capture the browser-binding cookie so the redemption can
# replay it. Hurl's automatic cookie jar doesn't reliably attach
# Path-scoped cookies in this test setup, so we wire it through
# explicitly via the Set-Cookie header.
pr18_magic_cookie: header "set-cookie" regex "oxicloud_magic_request=([^;]+)"
[Asserts]
jsonpath "$.message" contains "request received"


# ─────────────────────────────────────────────────────────────
# Step 4 — Capture the welcome mail + extract the magic-link.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/admin/smtp/test/captured?to=pr18-emailonly@example.com
Authorization: Bearer {{alice_token}}

HTTP 200
[Asserts]
jsonpath "$.to" == "pr18-emailonly@example.com"
jsonpath "$.text_body" matches "/magic/v1/[A-Za-z0-9_-]+"
[Captures]
pr18_magic_url: jsonpath "$.text_body" regex "(https?://[^\\s]+/magic/v1/[A-Za-z0-9_-]+)"


# ─────────────────────────────────────────────────────────────
# Step 5a — Redeem the welcome link WITHOUT the browser-binding
#           cookie. PR 22 shows the cross-browser confirmation
#           page (HTTP 200, HTML) rather than redeeming.
# ─────────────────────────────────────────────────────────────
GET {{pr18_magic_url}}

HTTP 200
[Asserts]
header "content-type" startsWith "text/html"
body contains "different browser"


# ─────────────────────────────────────────────────────────────
# Step 5b — Same link, this time with the matching cookie.
#           PR 22 binds the magic-link to the requesting browser;
#           a matching cookie redeems instantly. Internal user
#           with no resource target → lands on `/files`.
# ─────────────────────────────────────────────────────────────
GET {{pr18_magic_url}}
Cookie: oxicloud_magic_request={{pr18_magic_cookie}}

HTTP 302
[Asserts]
# SPA route (SvelteKit path-based). Historical value pre-migration was
# `/#/files` (legacy vanilla frontend hash-routing). Changed alongside
# the migration off the legacy shell — landing on the hash route now
# serves the legacy `static/index.html` with its meta-CSP inline
# scripts, which the SPA CSP blocks.
header "Location" == "/files"
[Captures]
pr18_access_token: cookie "oxicloud_access"


# ─────────────────────────────────────────────────────────────
# Step 6 — The new user can read their own profile. After PR 18
#          the username field is omitted (no handle claimed yet),
#          and `is_external` is false (they're an internal user
#          who signed up directly, not via invitation).
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/auth/me
Authorization: Bearer {{pr18_access_token}}

HTTP 200
[Asserts]
jsonpath "$.full.user.email" == "pr18-emailonly@example.com"
jsonpath "$.full.user.is_external" == false
jsonpath "$.full.user.username" not exists
# PR 23 — the user redeemed the welcome magic-link in Step 5b, so
# email_verified_at is stamped (the click IS the proof of inbox
# control, regardless of whether the redemption went through the
# direct or cross-browser-confirm path).
jsonpath "$.full.email_verified_at" exists
[Captures]
pr18_user_id: jsonpath "$.full.user.id"


# ─────────────────────────────────────────────────────────────
# Step 6a — PR 24: empty PATCH body is a no-op, returns the
#           current UserDto unchanged.
# ─────────────────────────────────────────────────────────────
PATCH {{base_url}}/api/auth/me/profile
Authorization: Bearer {{pr18_access_token}}
Content-Type: application/json
{}

HTTP 200
[Asserts]
jsonpath "$.full.user.id" == "{{pr18_user_id}}"
jsonpath "$.full.user.username" not exists
jsonpath "$.full.user.given_name" not exists


# ─────────────────────────────────────────────────────────────
# Step 6b — PR 24: set given_name and family_name. Username
#           stays unclaimed.
# ─────────────────────────────────────────────────────────────
PATCH {{base_url}}/api/auth/me/profile
Authorization: Bearer {{pr18_access_token}}
Content-Type: application/json
{ "given_name": "Pee Are", "family_name": "Eighteen" }

HTTP 200
[Asserts]
jsonpath "$.full.user.given_name" == "Pee Are"
jsonpath "$.full.user.family_name" == "Eighteen"
jsonpath "$.full.user.username" not exists


# ─────────────────────────────────────────────────────────────
# Step 6c — PR 24: empty string given_name is rejected (use the
#           field's ABSENCE for "no change"; null-clearing is
#           out of scope for v1).
# ─────────────────────────────────────────────────────────────
PATCH {{base_url}}/api/auth/me/profile
Authorization: Bearer {{pr18_access_token}}
Content-Type: application/json
{ "given_name": "" }

HTTP 400


# ─────────────────────────────────────────────────────────────
# Step 6d — PR 24: attempting to claim a username taken by
#           another user (admin) → 409 with `username_taken`
#           audit reason.
# ─────────────────────────────────────────────────────────────
PATCH {{base_url}}/api/auth/me/profile
Authorization: Bearer {{pr18_access_token}}
Content-Type: application/json
{ "username": "{{username}}" }

HTTP 409


# ─────────────────────────────────────────────────────────────
# Step 6e — PR 24: claim a fresh handle. Username is None →
#           Some, allowed.
# ─────────────────────────────────────────────────────────────
PATCH {{base_url}}/api/auth/me/profile
Authorization: Bearer {{pr18_access_token}}
Content-Type: application/json
{ "username": "pr18handle" }

HTTP 200
[Asserts]
jsonpath "$.full.user.username" == "pr18handle"


# ─────────────────────────────────────────────────────────────
# Step 6f — PR 24: claim-once enforcement. Username is already
#           set; second PATCH with a different handle → 409
#           UsernameImmutable. The NC client surface depends on
#           usernames being stable; admin override is the only
#           escape hatch (out of scope for this endpoint).
# ─────────────────────────────────────────────────────────────
PATCH {{base_url}}/api/auth/me/profile
Authorization: Bearer {{pr18_access_token}}
Content-Type: application/json
{ "username": "different-handle" }

HTTP 409


# ─────────────────────────────────────────────────────────────
# Step 6g — PR 24: PATCH with the SAME existing username — also
#           409 immutable, since "no-op username" semantically
#           differs from "no field" (the latter is the actual
#           no-op).
# ─────────────────────────────────────────────────────────────
PATCH {{base_url}}/api/auth/me/profile
Authorization: Bearer {{pr18_access_token}}
Content-Type: application/json
{ "username": "pr18handle" }

HTTP 409


# ─────────────────────────────────────────────────────────────
# Step 6h — PR 24: invalid format (contains '@' — reserved for
#           the email namespace) → 400.
# ─────────────────────────────────────────────────────────────
PATCH {{base_url}}/api/auth/me/profile
Authorization: Bearer {{pr18_access_token}}
Content-Type: application/json
{ "given_name": "Pr18@Handle" }

HTTP 200
[Asserts]
jsonpath "$.full.user.given_name" == "Pr18@Handle"


# ─────────────────────────────────────────────────────────────
# Step 6i — PR 24: final state check. Username is pr18handle,
#           given/family are set. PR 23 email_verified_at still
#           present.
# ─────────────────────────────────────────────────────────────
GET {{base_url}}/api/auth/me
Authorization: Bearer {{pr18_access_token}}

HTTP 200
[Asserts]
jsonpath "$.full.user.username" == "pr18handle"
jsonpath "$.full.user.given_name" == "Pr18@Handle"
jsonpath "$.full.user.family_name" == "Eighteen"
jsonpath "$.full.email_verified_at" exists


# ─────────────────────────────────────────────────────────────
# Step 7 — The new user can request another magic-link (no
#          password configured → eligible). Anti-enumeration
#          200 either way.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/magic-link/send
Content-Type: application/json
{ "email": "pr18-emailonly@example.com" }

HTTP 200
[Asserts]
jsonpath "$.message" contains "sign-in link"


# ─────────────────────────────────────────────────────────────
# Step 8 — PR 20 anti-enumeration: register with charlie's
#          email AGAIN (different password). Response is the
#          same uniform 200 — attacker can't tell from the
#          HTTP shape whether the email was already taken.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/register
Content-Type: application/json
{
  "username": "charlie-imposter",
  "email": "charlie@example.com",
  "password": "AttackerPassword99!"
}

HTTP 200
[Asserts]
jsonpath "$.message" contains "request received"


# ─────────────────────────────────────────────────────────────
# Step 9 — Verify the collision was silently suppressed: the
#          attacker's password does NOT work (the original
#          row is intact, no rewrite happened).
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "charlie@example.com", "password": "AttackerPassword99!" }

HTTP 403


# ─────────────────────────────────────────────────────────────
# Step 10 — Charlie's original password still works — the
#           collision didn't touch her account.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "charlie", "password": "TestPassword1!" }

HTTP 200


# ─────────────────────────────────────────────────────────────
# Step 11 — Username collision (different email): same uniform
#           200, no new user, audit `username_taken`.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/register
Content-Type: application/json
{
  "username": "charlie",
  "email": "charlie-other@example.com",
  "password": "AttackerPassword99!"
}

HTTP 200
[Asserts]
jsonpath "$.message" contains "request received"


# ─────────────────────────────────────────────────────────────
# Step 12 — OXICLOUD_REGISTRATION_ALLOWED_EMAIL_DOMAINS gate.
#
# `tests/common/server.env` pins the allowlist to
# `example.com,example.test`. Every legitimate signup above stayed
# inside that set. Now attempt an off-domain address and assert:
#
#   * HTTP 403 (NOT the anti-enumeration 200 — instance-wide policy
#     is not a per-user oracle; a rejected domain hasn't
#     established whether a specific address exists).
#   * `RegistrationDomainNotAllowed` error code so operators and
#     frontends can distinguish this from other 403 shapes
#     (`RegistrationDisabled`, `PasswordRegistrationDisabled`).
#
# The gate is CASE-INSENSITIVE on the post-`@` part — extra
# request with mixed case pins that behaviour so a future refactor
# can't silently regress a lowercase-only match.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/register
Content-Type: application/json
{
  "username": "off-domain",
  "email": "someone@nowhere.invalid",
  "password": "TestPassword1!"
}

HTTP 403
[Asserts]
# `$.error` carries the human-readable message; the stable
# machine-readable code lives at `$.error_type` (see
# `interfaces/errors.rs::ErrorResponse`). Pin `error_type` so a
# future copy-edit of the message doesn't break the test.
jsonpath "$.error_type" == "RegistrationDomainNotAllowed"


# Case-insensitive matching regression pin: `EXAMPLE.COM` in the
# post-`@` part is normalised to `example.com` and accepted. Reuse
# charlie's already-taken email so the request lands on the
# anti-enum-200 collision path — this way we exercise the domain
# gate (must pass) without creating a new user that would need
# cleanup, and pin the "case-insensitive normalization" invariant
# in one step.
POST {{base_url}}/api/auth/register
Content-Type: application/json
{
  "username": "case-check",
  "email": "charlie@EXAMPLE.COM",
  "password": "TestPassword1!"
}

HTTP 200
[Asserts]
jsonpath "$.message" contains "request received"


# ─────────────────────────────────────────────────────────────
# Cleanup — admin deletes both test users.
# ─────────────────────────────────────────────────────────────
DELETE {{base_url}}/api/admin/users/{{charlie_user_id}}
Authorization: Bearer {{alice_token}}

HTTP *

DELETE {{base_url}}/api/admin/users/{{pr18_user_id}}
Authorization: Bearer {{alice_token}}

HTTP *
