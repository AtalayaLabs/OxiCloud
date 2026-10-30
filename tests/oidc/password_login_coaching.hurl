# =============================================================
# OxiCloud — Password-login side-channel recovery
# =============================================================
# Pins the two SECURITY-CRITICAL rows of `AuthApplicationService::
# login()`'s no-password short-circuit on a deployment where
# `OXICLOUD_AUTH_METHODS=password,magic_link,oidc` is set
# (`server-with-oidc.env`):
#
#   Row 1 — OIDC-federated user attempts a password login:
#     `POST /api/auth/login` with the OIDC-federated principal's
#     email + any password returns 401 Invalid credentials
#     (anti-enumeration uniform response); the SMTP mock captures a
#     LOGIN COACHING email whose subject is
#     "Sign in to OxiCloud via SSO". No login token is minted —
#     federated identities are IdP-owned, so even on a user-driven
#     recovery flow we must NOT hand out a session primitive that
#     bypasses the IdP's MFA / step-up / revocation contract.
#
#   Row 2 — Passwordless (external, no federation) user attempts a
#     password login:
#     Same uniform 401; captured email is a REAL login email with a
#     clickable one-click magic-link URL (standard
#     "Sign in to OxiCloud" subject + a `/magic/v1/<token>` link in
#     the body). The user tried to sign in, we know their account
#     signs in via mailbox link, so we hand them the link they'd
#     otherwise have to type into the login page a second time —
#     one-click recovery instead of coaching copy.
#
# Why this matters: this path is the symmetric partner of
# `magic_link_coaching.hurl`. The two SECURITY-critical regressions it
# guards against are:
#   * Row 1: a regression that routed the OIDC case through the Row 2
#     one-click branch would mint a magic-link token for an
#     IdP-federated identity — mailbox possession bypassing the IdP's
#     MFA. The subject + "no `/magic/v1/`" assertions catch this.
#   * Row 2: a regression that dropped the mint (e.g. swapped the
#     Arc OnceLock for an always-None field, or forgot to wire it in
#     `common/di.rs`) would leave passwordless users with no feedback
#     — the audit log would still show `auth.login_rejected reason=
#     no_password` but the mailbox would stay silent and the user
#     would silently conclude their password was wrong.
#
# Captures via `GET /api/admin/smtp/test/captured?to=<email>` — same
# admin probe `magic_link_coaching.hurl` uses, backed by
# `OXICLOUD_SMTP_MOCK=true` in server-with-oidc.env.
#
# Depends on: `oidc.hurl` must run first so the OIDC-federated
# `oidc_user` exists with email `oidc@example.com` and no password
# hash on file.
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
# Row 1 — OIDC-federated user receives "sign in via SSO" coaching
# when they attempt a password login.
#
# `oidc.hurl` already JIT-provisioned `oidc_user` with email
# `oidc@example.com` (verified; federation_kind=oidc) and no local
# password. Submit a password login — the no_password short-circuit
# fires, coaching dispatches with reason `oidc_user`, uniform 401 is
# returned. No login token is minted.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "oidc@example.com", "password": "AnyWrongPassword1!" }

HTTP 401


GET {{base_url}}/api/admin/smtp/test/captured?to=oidc@example.com
Authorization: Bearer {{admin_token}}

HTTP 200
[Asserts]
jsonpath "$.to"      == "oidc@example.com"
# Login-side OIDC subject — same copy as the magic-link-side oidc
# coaching ("Sign in to OxiCloud via SSO") because the destination
# method is the same. A regression that routed the login-side path
# to the magic-link-side template (`coaching.oidc` instead of
# `login_coaching.oidc`) would still pass this subject assertion but
# fail the body assertion below.
jsonpath "$.subject" == "Sign in to OxiCloud via SSO"
# Body MUST frame the attempted method: "tried to sign in with a
# password" is the login-side wording. The magic-link-side oidc
# template says "requested a sign-in link" instead — using that
# template here would mislead the user into thinking they'd
# requested a magic-link they never asked for.
jsonpath "$.text_body" contains "tried to sign in to your OxiCloud account with a password"
jsonpath "$.text_body" contains "Sign in with SSO"
# No magic-link token path — federated identities are IdP-owned and
# must NEVER receive a session-primitive via mailbox possession,
# even on a user-driven recovery flow. A regression that routed OIDC
# users through `send_unbound_login_link` would trip here.
jsonpath "$.text_body" not contains "/magic/v1/"


# ─────────────────────────────────────────────────────────────
# Row 2 — Passwordless external user receives a REAL one-click
# magic-link email when they attempt a password login.
#
# Register email-only (no password, no federation) so the account
# exists but has NULL password_hash. The registration email itself
# is a magic link — we exercise it (register → clear capture → log
# in with wrong password → verify a FRESH link lands) by registering
# a sibling probe address first and then switching to the one under
# test so the capture reflects the login path, not registration.
# ─────────────────────────────────────────────────────────────
POST {{base_url}}/api/auth/register
Content-Type: application/json
{ "email": "pwless-login-recovery@oxicloud.local" }

# Anti-enumeration mode (see magic_link_coaching.hurl Row 1 note on
# register's uniform 200). The user is created without a password.
HTTP 200


# Password login attempt — hits the no_password short-circuit. The
# dispatcher mints an unbound magic-link token and emails it as a
# standard login-link (not a coaching template). The LATEST captured
# email for this address is now the login-recovery one, overwriting
# the registration welcome link.
POST {{base_url}}/api/auth/login
Content-Type: application/json
{ "username": "pwless-login-recovery@oxicloud.local", "password": "AnyWrongPassword1!" }

HTTP 401


GET {{base_url}}/api/admin/smtp/test/captured?to=pwless-login-recovery@oxicloud.local
Authorization: Bearer {{admin_token}}

HTTP 200
[Asserts]
jsonpath "$.to"      == "pwless-login-recovery@oxicloud.local"
# Standard login-link subject — same as `server.magic_link.email.login`.
# A regression that routed Row 2 through the Row 1 SSO-coaching
# template would land "Sign in to OxiCloud via SSO" here and the
# assertion trips. The matching "/magic/v1/" body assertion below
# then catches the inverse mistake (right subject, no token minted).
jsonpath "$.subject" == "Sign in to OxiCloud"
# Body MUST carry a clickable magic-link URL. The one-click recovery
# is the whole point of Row 2 — a regression that dropped the mint
# but kept dispatch would still fire an email but it would be a
# coaching message with no clickable token.
jsonpath "$.text_body" contains "/magic/v1/"
