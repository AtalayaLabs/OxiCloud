# Authentication

OxiCloud ships with JWT-based authentication and Argon2id password hashing for local accounts. It also exposes status and OIDC-related auth endpoints under the same `/api/auth` namespace, plus a magic-link (email link) sign-in flow for accounts that don't use a password.

## Core Endpoints

| Method | Endpoint | Description |
| --- | --- | --- |
| `POST` | `/api/auth/register` | Create a local user account. `email` is required; `username` and `password` are both optional. |
| `POST` | `/api/auth/login` | Exchange an identifier (username **or** email — dispatches on `@`) and password for access and refresh tokens |
| `POST` | `/api/auth/magic-link/send` | Send a one-click sign-in link to the account's email. Accepts either a username or an email in the request body |
| `GET` | `/magic/v1/{token}` | Redeem a magic-link — creates a session and stamps `email_verified_at` on the account |
| `POST` | `/api/auth/refresh` | Refresh the session tokens |
| `GET` | `/api/auth/me` | Return the current authenticated user |
| `PUT` | `/api/auth/change-password` | Change the current user's password (requires the current password) |
| `POST` | `/api/auth/logout` | Invalidate the current session |
| `GET` | `/api/auth/status` | Return auth system state, including OIDC availability |

## OIDC Endpoints Under Auth

| Method | Endpoint | Description |
| --- | --- | --- |
| `GET` | `/api/auth/oidc/providers` | Report which self-service auth methods this deployment offers (see fields below) |
| `GET` | `/api/auth/oidc/authorize` | Build the authorization redirect URL |
| `GET` | `/api/auth/oidc/callback` | Handle provider redirect callback |
| `POST` | `/api/auth/oidc/exchange` | Exchange the auth code for OxiCloud session tokens |

`GET /api/auth/oidc/providers` fields:

| Field | Meaning |
| --- | --- |
| `enabled` | OIDC is configured on this deployment |
| `provider_name` | Display name for the IdP (shown on the SSO button) |
| `authorize_endpoint` | Where the SPA should start the OIDC round-trip |
| `password_login_enabled` | `POST /api/auth/login` will accept credentials |
| `magic_link_login_enabled` | `POST /api/auth/magic-link/send` will mint tokens (SMTP wired + allowlist + no OIDC — see rules below) |
| `require_verified_email` | `OXICLOUD_REQUIRE_VERIFIED_EMAIL` is set — the SPA uses this hint to explain `EmailNotVerified` responses |

## Configuring which methods are offered

Two environment variables control the auth surface. OIDC is a first-class allowlist token alongside `password` and `magic_link`.

### `OXICLOUD_AUTH_METHODS`

Comma-separated allowlist of `password`, `magic_link`, and/or `oidc`. Default (when unset): `password,magic_link`.

| Configuration | Effect |
| --- | --- |
| Unset | Password + magic-link (OIDC gated separately by `OXICLOUD_OIDC_ENABLED`) |
| `password,magic_link` | Same as unset — both self-service methods |
| `password` | Password login OK. Magic-link send / redeem → 403 `MagicLinkLoginDisabled` |
| `magic_link` | Password login → 403 `PasswordLoginDisabled`. Password-based `register` → 403 `PasswordRegistrationDisabled`. Email-only signup still works |
| `oidc` | **SSO-only** posture. Requires `OXICLOUD_OIDC_ENABLED=true` + a full OIDC config bucket; local password + magic-link both disabled |
| `password,oidc` | Hybrid: local password + SSO, no magic-link |
| `password,magic_link,oidc` | Everything on |

**Fail-fast.** Misconfiguration panics at boot instead of degrading silently:
- Unknown token (e.g. `password,sso2`) → boot panic with `expected: password, magic_link, oidc`
- Empty allowlist (e.g. `OXICLOUD_AUTH_METHODS=`) → boot panic (would lock everyone out otherwise)
- `oidc` listed but `OXICLOUD_OIDC_ENABLED != true` → boot panic (advertising a method the server can't serve)

**Loose semantic (documented).** The symmetric case is NOT fatal yet: when `OXICLOUD_AUTH_METHODS` is explicitly set WITHOUT `oidc` but `OXICLOUD_OIDC_ENABLED=true`, OIDC is served in addition to the listed methods — the enabled flag wins. A warning is logged at boot to make the mismatch visible. **Planned for the next major release**: this will escalate to a fail-fast panic so `AUTH_METHODS` becomes the authoritative allowlist for OIDC too. Align configs now (either add `oidc` to the list or set `OXICLOUD_OIDC_ENABLED=false`) to avoid the breaking change.

**Startup gate.** If `magic_link` is the only working method (no `password`, no `oidc`) AND no SMTP transport is configured (`OXICLOUD_SMTP_HOST` empty), the server refuses to start with a fatal message. A magic-link-only policy without a working mailer silently locks every user out.

**OIDC master rule.** When OIDC is enabled, magic-link login is **hard-disabled** regardless of this list. The IdP is the identity boundary; magic-link would bypass any 2FA / step-up policy the IdP enforces. The startup gate above does **not** trigger in this case — OIDC provides the login path.

Legacy alias: `OXICLOUD_OIDC_DISABLE_PASSWORD_LOGIN=true` still removes `password` from the effective allowlist.

### `OXICLOUD_REQUIRE_VERIFIED_EMAIL`

Default `false`. When `true`, `POST /api/auth/login` returns 403 `EmailNotVerified` for any account whose `email_verified_at IS NULL`.

**Order matters:** the verified-email check runs **after** password validation. An attacker without the password sees only the generic `Invalid credentials` shape — they can't probe whether an account's email is verified.

**Verification piggyback.** When the branch fires (password OK, email unverified), the server auto-sends a verification magic-link to the account's registered address using the same login request. The user sees `EmailNotVerified` in the response and a "check your inbox" hint on the login page; resubmitting the form re-sends the link. This is why there is no separate "resend verification" endpoint — offering an unauthenticated one would leak `has_password` state.

**Admin exemption.** Admin accounts (role `admin`) are exempt from this gate at login, regardless of `email_verified_at`. Rationale: an operator who flips the flag on an existing deployment must not lock the admin(s) out of their own instance. Fresh admin accounts created via `POST /api/setup` or `POST /api/admin/users` are stamped verified at creation; the exemption covers pre-existing accounts that predate the flag.

**Auto-verified on creation:** OIDC-JIT users, admin-created users (`POST /api/admin/users`), and the first-run setup admin (`POST /api/setup`). Verification is only ever missing on regular users who signed up before the flag was turned on.

## Login identifier dispatch

`POST /api/auth/login` accepts either a username (no `@`) or an email (contains `@`) in the `username` field. The two namespaces are provably disjoint — usernames forbid `@` — so the dispatch is unambiguous and both paths return the same session shape.

`POST /api/auth/magic-link/send` mirrors this convention. The `email` field can be either an email or a username; the server resolves username → registered email before rate-limiting so both shapes share one budget (no bypass).

## Registration flow

Since PR 18, both `username` and `password` are optional on `POST /api/auth/register`. The only required field is `email`.

| Combination | Result |
| --- | --- |
| `email + password` | Classic signup — account gets a password hash; user can log in immediately |
| `email + password + username` | Same, plus the username is claimed at creation |
| `email` only | Email-only signup — no password stored; server sends a welcome magic-link. Clicking it creates a session and stamps `email_verified_at`. The user can later claim a handle via `PATCH /api/auth/me/profile` and set a password via `PUT /api/auth/change-password` |

The response body is uniform across success, email collision, and username collision — the SPA does not learn whether an address is already taken. The real reason lands in the audit log.

### `OXICLOUD_DISABLE_REGISTRATION`

Turns the endpoint off entirely (returns 403 `RegistrationDisabled`).

### `OXICLOUD_REGISTRATION_ALLOWED_EMAIL_DOMAINS`

Comma-separated allowlist. Rejected registrations return 403 `RegistrationDomainNotAllowed`. Distinct from `OXICLOUD_EXTERNAL_EMAIL_DOMAINS`, which gates external-user **invitations**; self-registration and invitations have independent policies.

## Magic-link eligibility

`POST /api/auth/magic-link/send` looks up the resolved email → user, then applies the eligibility ladder:

1. **OIDC-linked user** → refused with `reason="oidc_user"`. Unconditional; the IdP is the security boundary and may enforce MFA that magic-link would sidestep.
2. **Has a password configured** → refused with `reason="has_password"` (default). Set `OXICLOUD_AUTH_POLICIES=permit_magic_link_for_password_users` to allow — this weakens the password to mailbox-strength for affected accounts; opt-in only.
3. **No credential** (typical external user or fresh email-only signup) → allow.

The verification-piggyback flow above deliberately **bypasses the `has_password` gate** — that path is only reachable after the user has already proven identity via password on the same login request, so mailbox-only trust is not being extended beyond what the password already established.

## Auth policy vector

`OXICLOUD_AUTH_POLICIES` is a comma-separated list of additive policy switches. Distinct from `OXICLOUD_AUTH_METHODS` (which enables/disables a method wholesale), each entry here grants a specific exception or restriction to default auth behaviour. Vector shape so future policies can be added by appending a token instead of introducing a new env var per behaviour. Variant names carry their own polarity (`Permit...`, future `Require...` / `Deny...`).

| Token | Effect |
| --- | --- |
| `permit_magic_link_for_password_users` | Allow magic-link login for accounts that also have a password. OIDC-linked users are still refused. |
| `auto_redirect_if_standalone_oidc` | When OIDC is the ONLY working login method (no password, no magic-link — via allowlist or the OIDC-master rule), `GET /login` returns a **server-side 302** to `/api/auth/oidc/authorize` before the SPA loads (no click-to-continue button, no flash). Off by default to avoid redirect loops on IdP failure; the interceptor falls through to the SPA when `?error=…` or `?oidc_code=…` are present. Silent no-op when other methods are also live. Pair with the RP-initiated logout setup below so users on shared computers can actually log out. |

Unknown tokens are logged-and-skipped at startup so a typo doesn't silently zero the vector.

## RP-initiated OIDC logout

When a session was minted through OIDC, `POST /api/auth/logout` returns a JSON body containing `post_logout_url`. The SPA reads this and navigates the browser there via `window.location.replace(url)` — the IdP kills its SSO cookie and redirects the browser back to `<oxicloud>/login`. Without this hop the IdP session stays alive: the very next `/login` visit would silently re-authenticate through the still-valid SSO cookie, which under `auto_redirect_if_standalone_oidc` looks like the logout button did nothing (shared-computer scenario).

Requirements:

- **IdP discovery must advertise `end_session_endpoint`** (OIDC Session Management 1.0). Keycloak does by default. If your IdP doesn't, `post_logout_url` is omitted and the SPA falls back to a local-only logout; the IdP session ends only when it naturally times out.
- **The OIDC client must register `<oxicloud-base-url>/login` as a valid post-logout redirect URI.** Keycloak calls this field "Valid post logout redirect URIs" on the client's Settings tab. If it's missing, the IdP shows its own error page after logging out instead of returning the user to OxiCloud.
- Backend uses `AppConfig::base_url()` (i.e. `OXICLOUD_BASE_URL` if set, else derived from `server_host` / `server_port`) to build the redirect URI. Set `OXICLOUD_BASE_URL` when the browser reaches OxiCloud through a URL different from what the server binds locally (reverse proxy, Docker, TLS-terminating LB).

The `id_token` used as `id_token_hint` is captured at login time from the OIDC token-exchange response and persisted on `auth.sessions.oidc_id_token`. Non-OIDC sessions leave the column NULL and `POST /api/auth/logout` returns `{}` (local-only logout).

## OIDC Back-Channel Logout

Complements RP-initiated logout by letting the **IdP** kick OxiCloud sessions server-to-server, without any browser involvement. Fires when:

- The user logged out of another RP (single sign-out across your fleet).
- An admin revoked the user's SSO session from the Keycloak admin console.
- The user's account was disabled at the IdP.

Endpoint: `POST /api/auth/oidc/backchannel-logout`. Public (no auth middleware, no CSRF, no cookies) — the signed `logout_token` JWT IS the authentication.

**IdP-side setup (Keycloak):**

1. On the client's Settings tab, set **Backchannel Logout URL** to `<oxicloud-base-url>/api/auth/oidc/backchannel-logout`.
2. Turn on **Backchannel Logout Session Required**. This makes Keycloak include the `sid` claim on both id_tokens (which OxiCloud persists on `auth.sessions.oidc_sid`) AND on the logout_tokens it sends. With `sid` present, OxiCloud revokes only the specific device that logged out; without it, we fall back to revoking every session belonging to the same OIDC subject (all of the user's OxiCloud devices).
3. Leave **Backchannel Logout Revoke Offline Sessions** off unless you have a reason — OxiCloud uses only online sessions today.

**What OxiCloud validates on the logout_token** (per OIDC Back-Channel Logout 1.0):

- Signature via the IdP's JWKS (same key material as id_token validation).
- `iss` matches the discovery document's issuer.
- `aud` contains our `client_id`.
- `events` claim contains the `http://schemas.openid.net/event/backchannel-logout` key.
- `sub` and/or `sid` present (else there's nothing to revoke — 400).
- `nonce` absent (spec §2.4 forbids it — a token with a nonce is either an IdP bug or a replay of an id_token; 400).
- `iat` within a 5-minute freshness window.
- `jti` (if present) deduped for 5 minutes so retransmissions don't cause double-audit.

Response codes are constrained by the spec:

- **200** — token validated; 0 or more sessions revoked (both are "handled" from the IdP's view).
- **400** — validation failed. Real reason is logged locally (`event=oidc.backchannel_logout_rejected`) and NOT returned in the body; the IdP just sees `invalid_request`.
- **503** — OIDC is not enabled on this deployment. The IdP shouldn't be calling us in that case.

**Compared to RP-initiated logout** (the flow triggered by `POST /api/auth/logout`): RP-initiated is browser-driven and evicts the local session + kills the IdP session. Back-channel is IdP-driven and evicts the local session; the IdP's own state is not affected. The two are complementary — enable both.

## Example Flows

### Register — classic

```json
{ "username": "testuser", "email": "test@example.com", "password": "SecurePassword123" }
```

### Register — email-only

```json
{ "email": "test@example.com" }
```

### Login

```json
{ "username": "testuser", "password": "SecurePassword123" }
```

Or equivalently:

```json
{ "username": "test@example.com", "password": "SecurePassword123" }
```

Typical successful login response:

```json
{ "accessToken": "...", "refreshToken": "...", "expiresIn": 3600 }
```

### Send a sign-in link (magic-link)

```json
{ "email": "testuser" }
```

Uniform response regardless of whether the account exists / is eligible:

```json
{ "message": "If an account exists for that email, a sign-in link will be sent." }
```

### Current User

`GET /api/auth/me` returns the authenticated user's identity, role, `email_verified_at`, and storage information.

## Distinguished error codes

The `error_type` field on 4xx responses lets frontends render specific UX. Codes surfaced by this subsystem:

| `error_type` | HTTP | Meaning |
| --- | --- | --- |
| `PasswordLoginDisabled` | 403 | `OXICLOUD_AUTH_METHODS` doesn't include `password` |
| `PasswordRegistrationDisabled` | 403 | Same, on `register` with a password field |
| `MagicLinkLoginDisabled` | 403 | `OXICLOUD_AUTH_METHODS` doesn't include `magic_link`, OIDC is enabled, or email-only signup is attempted on a password-only deployment |
| `EmailNotVerified` | 403 | Password validated, but `email_verified_at IS NULL` and `OXICLOUD_REQUIRE_VERIFIED_EMAIL=true`. Server has already sent a verification link |
| `RegistrationDisabled` | 403 | Global registration off |
| `RegistrationDomainNotAllowed` | 403 | Email domain outside `OXICLOUD_REGISTRATION_ALLOWED_EMAIL_DOMAINS` |
| `AccountLocked` | 429 | Too many failed login attempts for (account, IP) — see rate-limit config |

## DAV clients (WebDAV / CalDAV / CardDAV): app passwords only

DAV surfaces at `/webdav/`, `/caldav/`, and `/carddav/` accept HTTP
Basic Auth **only against app passwords** — the user's regular account
password is refused on those paths. This is intentional and cannot be
switched off.

Reasons:

- **Uniformity across account types.** Magic-link-only accounts (email-
  only signup) and OIDC-linked accounts have no local password to send
  over Basic Auth. App passwords are the one credential shape that
  works for every account type.
- **Revocable and scoped.** An app password can be revoked
  individually without touching the account password. Losing a phone
  or rotating a client only affects that client.
- **Bounded blast radius on phishing / leak.** A leaked account
  password grants web login (which the SPA can gate with 2FA / step-up
  in future); an app password grants only the DAV surface it was
  minted for.

**User workflow:** in the OxiCloud web UI, *Profile → App Passwords →
Create*, name it, copy the token shown once, and use `username +
token` in the DAV client. See
[DAV Client Setup](/guide/dav-client-setup#before-you-start-get-an-app-password).

## Security Model

- Local passwords hashed with Argon2id
- DAV surfaces (WebDAV / CalDAV / CardDAV) accept **app passwords only** — the account password is refused on `/webdav/`, `/caldav/`, `/carddav/` by design (see above)
- Access control is role-based (`admin` and `user`)
- Refresh tokens support session renewal without forcing frequent re-login
- Login endpoint uses anti-enumeration response shapes — bad-username and bad-password return the same 403
- Magic-link `send` returns a uniform 200 whether the account exists or not; the truth lands in the `audit` log target
- OIDC can coexist with local auth or disable password login entirely
- OIDC-enabled deployments have magic-link login hard-disabled to prevent IdP-MFA bypass

## Related Pages

- [OIDC / SSO](/config/oidc)
- [Admin Settings](/config/admin-settings)
- [Environment Variables](/config/env)
