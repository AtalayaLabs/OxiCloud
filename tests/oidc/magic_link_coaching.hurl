# =============================================================
# OxiCloud — Magic-link coaching email, per-user eligibility matrix
# =============================================================
# Pins the two SECURITY-CRITICAL rows of
# `AuthApplicationService::magic_link_eligibility()` on a deployment
# where `magic_link` is advertised alongside OIDC
# (`server-with-oidc.env` carries
# `OXICLOUD_AUTH_METHODS=password,magic_link,oidc` and neither
# `permit_magic_link_for_{password,oidc}_users` policy — default
# strict):
#
#   Row 1 — password user:
#     `POST /api/auth/magic-link/send` returns 200 (uniform
#     anti-enumeration response); the SMTP mock captures a
#     COACHING email with subject
#     "Use your password to sign in to OxiCloud". No login token
#     is minted.
#
#   Row 2 — OIDC-federated user:
#     Same uniform 200; captured email's subject is
#     "Sign in to OxiCloud via SSO". No login token is minted.
#
# Why this matters: pre-feat/registration the deployment-wide
# OIDC-master rule hard-disabled `magic_link_login_allowed` whenever
# OIDC was on, which also blocked external users on the same
# deployment. feat/registration retires that deployment-wide rule in
# favour of the per-user eligibility matrix below. Without these
# pins a regression that drops the Reject arm (or swaps the subjects
# per reason) could silently mint login tokens for OIDC-federated
# accounts — mailbox possession bypassing the IdP's MFA.
#
# The admin-only `GET /api/admin/smtp/test/captured?to=<email>`
# endpoint reads the mock SMTP capture (`OXICLOUD_SMTP_MOCK=true`
# in server-with-oidc.env) — same probe `jobs_notify.hurl` uses.
#
# Depends on: `oidc.hurl` must run first so the OIDC-federated
# `oidc_user` exists with email `oidc@example.com`.
# =============================================================


# ─────────────────────────────────────────────────────────────
# Prelude — admin login, used by every captured-SMTP probe.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "{{username}}", "password": "{{password}}" }

HTTP 200
[Captures]
admin_token: jsonpath "$.access_token"


# ─────────────────────────────────────────────────────────────
# Row 1 — Password user receives "use your password" coaching.
#
# Register a password user, then submit a magic-link request for
# their email. The response is 200 (uniform — anti-enumeration
# applies even to a shape nobody is enumerating from, so the real
# user and an attacker see the same thing). The captured email is
# the only side channel that reveals the right method.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/register
Content-Type: application/json
{
  "email": "pwuser-coaching@oxicloud.local",
  "username": "pwuser-coaching",
  "password": "TestPassword1!"
}

# Anti-enumeration mode (SMTP wired) — register returns the uniform
# 200 "Registration request received." shape rather than 201 CREATED,
# regardless of whether the account was created or the email already
# existed. See `auth_handler::register::uniform_ok`.
HTTP 200


POST {{base_url}}/api/auth/magic-link/send
Content-Type: application/json
{ "email": "pwuser-coaching@oxicloud.local" }

HTTP 200


GET {{base_url}}/api/admin/smtp/test/captured?to=pwuser-coaching@oxicloud.local
Authorization: Bearer {{admin_token}}

HTTP 200
[Asserts]
jsonpath "$.to"      == "pwuser-coaching@oxicloud.local"
jsonpath "$.subject" == "Use your password to sign in to OxiCloud"
# Body names the method the user should use. The exact phrase is
# load-bearing: the FE translators work from the English copy, and
# a regression that quietly changed the method-name token (e.g. to
# "use your passphrase") would still contain "password" but mislead
# every downstream translation.
jsonpath "$.text_body" contains "sign in with your password"
# No magic-link token path — this is coaching, not a login flow.
jsonpath "$.text_body" not contains "/magic/v1/"


# ─────────────────────────────────────────────────────────────
# Row 2 — OIDC-federated user receives "sign in via SSO" coaching.
#
# `oidc.hurl` already JIT-provisioned `oidc_user` with email
# `oidc@example.com` (verified; federation_kind=oidc). Submit the
# magic-link path for that email — the per-user eligibility ladder's
# OIDC check fires, coaching email dispatches, no login token.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/magic-link/send
Content-Type: application/json
{ "email": "oidc@example.com" }

HTTP 200


GET {{base_url}}/api/admin/smtp/test/captured?to=oidc@example.com
Authorization: Bearer {{admin_token}}

HTTP 200
[Asserts]
jsonpath "$.to"      == "oidc@example.com"
jsonpath "$.subject" == "Sign in to OxiCloud via SSO"
# Body names SSO + the user action. Same stronger-phrase pattern
# as Row 1 — "SSO" alone would still match if the body collapsed
# into a boilerplate error mentioning SSO; the exact phrase pins
# the coaching intent.
jsonpath "$.text_body" contains "Sign in with SSO"
jsonpath "$.text_body" not contains "/magic/v1/"


# ─────────────────────────────────────────────────────────────
# Row 3 — External user ALWAYS receives a login link, regardless
# of OIDC + password policies on the deployment.
#
# This is the matrix row that unblocks external users on OIDC-
# enabled deployments: a user with no password and no federation
# (typical of folder-share invitations to outside addresses) is
# the ONE case for whom magic-link is the only path. Any
# regression that made `magic_link_eligibility()` more defensive
# would silently lock them out — a real-world regression this
# PR itself is fixing (external users on OIDC deployments were
# previously stuck under the deployment-wide master rule).
#
# Register email-only (no password). The server mints a welcome
# magic link + sends it on registration already; we're pinning the
# EXPLICIT `/magic-link/send` path here (the user-driven resend),
# so submit again and verify the captured email is a login link —
# NOT a coaching email.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/register
Content-Type: application/json
{ "email": "external-coaching@oxicloud.local" }

# Anti-enumeration mode (see Row 1 note on register's uniform 200).
HTTP 200


POST {{base_url}}/api/auth/magic-link/send
Content-Type: application/json
{ "email": "external-coaching@oxicloud.local" }

HTTP 200


GET {{base_url}}/api/admin/smtp/test/captured?to=external-coaching@oxicloud.local
Authorization: Bearer {{admin_token}}

HTTP 200
[Asserts]
jsonpath "$.to"      == "external-coaching@oxicloud.local"
# Login-link email subject — distinct from the two coaching subjects
# ("Use your password...", "Sign in to OxiCloud via SSO"). Pin the
# full English subject so a regression swapping the templates for
# the external case trips here.
jsonpath "$.subject" == "Sign in to OxiCloud"
# The body carries a real magic-link token path — the opposite of
# the coaching rows, which deliberately omit any `/magic/v1/` URL.
jsonpath "$.text_body" contains "/magic/v1/"
