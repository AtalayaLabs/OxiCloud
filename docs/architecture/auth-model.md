# Authentication Model

OxiCloud's authentication is built on a single principle: **email is the identity, everything else is optional**. A user account is uniquely identified by their email address. Username, password, and OIDC linkage are each independent, optional slots — none of them is required, and none of them is the source of identity. Which slots a user has determines which login paths are available to them.

This page is the canonical reference for the identity and authentication surface. For the magic-link mechanism in detail (token lifecycle, invitation flow, kill switches), see [Magic-link external authentication](/architecture/magic-link-auth). For how grants are evaluated, see [ReBAC Authorization](/architecture/rebac-authorization).

## Identity model

Every user row in `auth.users` carries one identity field, three independent credential slots, and one derived signal (`email_verified_at`).

| Slot | Type | Required | Meaning |
|---|---|---|---|
| `email` | `String UNIQUE NOT NULL` | yes | The identity. Every login path ultimately resolves here. |
| `username` | `String UNIQUE NULL` | no | Optional handle. 2-64 chars, `[A-Za-z0-9._-]+`, **no `@`**. **Claim-once, immutable** (PR 24) — a user can claim a handle once if they have none, but cannot rename or unclaim. Multiple NULLs coexist under the UNIQUE index. |
| `password_hash` | `String NULL` | no | Argon2 hash if the user chose one. NULL = no password. No sentinel strings. |
| `oidc_subject` | `String NULL` | no | IdP subject claim if the user linked an external identity. NULL = no OIDC. |
| `is_external` | `bool` | yes (default false) | Provisioning origin marker. `true` = created via email-invitation. Affects home-folder provisioning and DAV access. |
| `email_verified_at` | `Timestamp NULL` | no | When the user demonstrated control of their email. NULL = unverified. Stamped on first magic-link redemption OR OIDC JIT with verified claim OR admin-created / setup-admin accounts (admin fiat). Idempotent: the first proof timestamp is preserved. Gated by `OXICLOUD_REQUIRE_VERIFIED_EMAIL` — see below. |

The **`@` ban on usernames** is what makes the username and email namespaces provably disjoint. The login dispatcher relies on this — input containing `@` is unambiguously an email lookup, input without is a username lookup. No fallback chain, single DB hit.

Eligibility predicates derive from the slots:

```rust
fn has_password(&self) -> bool      { self.password_hash.is_some() }
fn has_oidc(&self) -> bool          { self.oidc_subject.is_some() }
fn has_login_credential(&self) -> bool {
    self.has_password() || self.has_oidc()
}
```

## Login dispatcher

`POST /api/auth/login` accepts one identifier field that holds **either** a username or an email. The server dispatches in one branch:

```
input contains '@' → lookup by email,    verify password
input does not     → lookup by username, verify password
```

The `@` ban on usernames makes this unambiguous. A single DB lookup, no fallback chain, no cross-column scan.

The frontend's "Username or email" field submits whatever the user typed; the JSON field is still named `username` for backwards compatibility, with a docstring noting the dual semantics.

The same dispatch applies to `POST /api/auth/magic-link/send` — its `email` field also accepts either an email or a username. When a username is supplied, the server resolves it to the account's registered email BEFORE rate-limiting so `alice` and `alice@example.com` share one budget (otherwise alternating shapes would double the effective per-target budget).

## Deployment auth policy

Two env vars control the auth surface. OIDC is a first-class allowlist token, no longer orthogonal:

- `OXICLOUD_AUTH_METHODS` — allowlist of enabled methods (`password`, `magic_link`, `oidc`, or any comma-separated combination). Default (unset): `password,magic_link`. Removing a token produces distinct error_type codes so the SPA can render specific UX:
  - Removing `password` → `POST /api/auth/login` → 403 `PasswordLoginDisabled`; password-based `register` → 403 `PasswordRegistrationDisabled`.
  - Removing `magic_link` → `magic-link/send` → 403 `MagicLinkLoginDisabled`; login-purpose token redemption refuses.
  - Setting to just `oidc` → SSO-only posture, local login surface disabled.
  - **Fail-fast on boot:** unknown token, empty allowlist, or `oidc` listed without `OXICLOUD_OIDC_ENABLED=true` all panic startup.
  - **Startup gate:** `magic_link` as the only working method (no `password`, no `oidc`) with no SMTP wired → server refuses to start.
- `OXICLOUD_AUTH_POLICIES` — additive policy switches. Today: `permit_magic_link_for_password_users`. Future variants (`Require...`, `Deny...`) reuse the same vector-shaped env var — no per-policy env-var proliferation.
- `OXICLOUD_REQUIRE_VERIFIED_EMAIL` — when true, `POST /api/auth/login` returns 403 `EmailNotVerified` for accounts with `email_verified_at IS NULL`. Checked AFTER password validation (anti-enum — an attacker without the password can't probe verification state). **Admin accounts are exempt** from this gate to prevent a config flip from locking pre-existing admins out of their own instance.

**Verification piggyback.** When the `EmailNotVerified` branch fires (password OK + email unverified), the login handler auto-sends a verification magic-link to the account via a distinct service method that bypasses the `has_password` eligibility gate — the password itself just proved identity, so mailbox-only trust isn't being extended beyond what the password already established. Response is 403 `EmailNotVerified` with "check your inbox"; re-submitting the same login re-triggers the send. This is why there is no unauthenticated "resend verification" endpoint — one would leak `has_password` state to unauthenticated callers.

**OIDC-master rule.** When OIDC is enabled (either explicitly in `OXICLOUD_AUTH_METHODS` or via `OXICLOUD_OIDC_ENABLED=true`), magic-link login is hard-off regardless of what the allowlist says. Magic-link would bypass any 2FA / step-up the IdP enforces.

## Login paths

| Path | How it works | When available |
|---|---|---|
| **Username + password** | Type a handle and a password. Backend looks up by username, verifies the Argon2 hash. | User has both `username` and `password_hash` set. |
| **Email + password** | Type an email and a password. Backend looks up by email, verifies the hash. | User has `password_hash` set (username optional). |
| **Email + magic-link** | Type an email, click "Send sign-in link", receive a magic-link in the inbox, click it. | Magic-link eligibility (below). |
| **OIDC redirect** | Click "Sign in with {IdP}", redirect to IdP, return to OxiCloud authenticated. | User has `oidc_subject` set OR JIT-provisioning is enabled. |

### Magic-link eligibility

```
1. has_oidc()        → reject "oidc_user"   (unconditional)
2. has_password()    → reject "has_password" by default
                       allow when OXICLOUD_AUTH_POLICIES contains
                       `permit_magic_link_for_password_users`
3. neither           → allow
```

| User state | Magic-link eligible? |
|---|---|
| No password, no OIDC (typical external / fresh email-only signup) | Yes — always |
| Has password, no OIDC | Default no; flag flips to yes for lenient mode |
| Has OIDC (with or without password) | **No — always.** Flag has no effect. |

**OIDC is excluded unconditionally** because the IdP is the security boundary and may enforce MFA (TOTP, WebAuthn, conditional access, etc.) that a magic-link would bypass. Even when the operator wants lenient magic-link for password users, OIDC-linked accounts must stay on the IdP path.

## Device-bound magic-link redemption

PR 22 binds **login-via-email** magic-links to the originating browser via a challenge cookie. The mechanism closes the mailbox-as-bearer-token attack class on this surface — mailbox compromise alone is no longer enough to redeem a session.

**Asymmetric scope.** Binding applies only to login-via-email, not invitations:

| Flow | Initiator | Bound to browser? | TTL | Env |
|---|---|---|---|---|
| `POST /api/auth/magic-link/send` (login-via-email) | The user themselves, in a browser | **Yes** (challenge cookie) | **10 minutes** | `OXICLOUD_MAGIC_LINK_LOGIN_TTL_MINUTES` |
| `POST /api/grants subject.type=email` (invitation) | A sharer; recipient has no prior browser context | No (cross-device by design) | **24 hours** | `OXICLOUD_MAGIC_LINK_INVITE_TTL_HOURS` |

The legacy `OXICLOUD_MAGIC_LINK_TTL_HOURS` env is preserved as a deprecated alias for the invitation TTL.

**Cookie mechanism.** `POST /api/auth/magic-link/send` generates a random per-request challenge, sets it as `oxicloud_magic_request=<value>` (HttpOnly, SameSite=Strict, Path=`/magic`, Max-Age = login TTL), and mirrors it into the new `auth.magic_link_tokens.request_challenge` column. On redemption (`GET /magic/v1/{token}`):

- **Cookie matches** → redeem instantly. The user clicked the link in the same browser they requested it from.
- **Cookie absent or mismatched** → render a confirmation HTML page warning that the link was opened on a different device. The Continue button submits back to `/magic/v1/{token}?confirm=1`; the redemption proceeds with audit `magic_link.redeemed cross_browser_confirmed=true`.
- **Invitation tokens** (no `request_challenge`) bypass the check entirely. Invitations are cross-device by design — the recipient was never going to have a matching cookie.

The cookie is **set on every 200 response** from `POST /api/auth/magic-link/send`, including the silently-absorbed rate-limit and ineligibility paths. The DB row only stores the challenge when a token is actually minted, so the cookie's presence alone isn't an enumeration oracle.

## Profile editing

PR 24 adds `PATCH /api/auth/me/profile` for the user to edit their own profile. Three optional fields:

| Field | Mutability |
|---|---|
| `username` | **Claim-once, immutable**. Accepted only when the caller has no username; subsequent calls (whether claiming a different handle or the same value) return `409 UsernameImmutable`. Admin override is the only escape hatch for genuine typos. |
| `given_name` | Freely settable. Empty string is rejected — use field absence for "no change". |
| `family_name` | Same as `given_name`. |

**OIDC users are rejected wholesale with 403.** Their profile is owned by the IdP and changes there propagate on next sign-in.

**Why claim-once on username?** The DAV / NextCloud compat layer at `/remote.php/dav/files/{user}/…` and the `verify_url_user` check both bake username in as a stable identifier in URL paths. Allowing renames would silently break every configured NC client (clients build URLs from the username they were given at login, and don't re-fetch a URL template). The immutability decision sidesteps the whole problem — usernames stay stable for the lifetime of the account, NC clients keep working forever. Native OxiCloud surfaces (`/api/*`, `/webdav/*`, `/caldav/*`) don't include username in the path and would have been fine with renames; the NC surface drives the policy.

## Registration paths

| Path | Pre-condition | What happens |
|---|---|---|
| `POST /api/auth/register` with `{email, password}` | Public registration enabled | User row created with both slots; classic path. |
| `POST /api/auth/register` with `{email}` only | Public registration enabled + SMTP configured | User row created with `password_hash = NULL`; welcome magic-link mailed. |
| `POST /api/grants` with `{ subject: { type: "email", email: "..." } }` | Sharer has Share permission | Recipient lazily provisioned as external; invitation magic-link mailed. |
| OIDC JIT | First IdP-mediated login + auto-provisioning enabled | User row created with `oidc_subject` set, no password. |

Anti-enumeration applies to the public `register` endpoint — see below.

## Anti-enumeration

The endpoint responses are tuned per attacker model:

| Endpoint | Response shape | Why |
|---|---|---|
| `POST /api/auth/register` (SMTP wired) | Uniform 200 on success **and** collision: `{"message": "Registration request received."}` | Per-user oracle on `email` / `username` would let an attacker probe account existence. The "check your email" cover story is honest because successful email-only signups receive a welcome mail. |
| `POST /api/auth/register` (SMTP not wired) | `201 + UserDto` on success, `409` on collision (classic) | Without the email cover story, a uniform response is misleading UX with no security benefit. |
| `POST /api/auth/magic-link/send` | Uniform 200 regardless of outcome | The mailbox owner is the only one who'd see whether mail arrived. |
| `POST /api/auth/login` | Uniform `403 "Invalid credentials"` | Same shape for unknown user / bad password / deactivated account. |

In all four cases the real reason is recorded in the `audit` channel — operators see the truth; attackers see the same response.

**Instance-wide policy stays visible** in every flow. `OXICLOUD_ENABLE_REGISTRATION=false`, OIDC-only mode, and SMTP-not-configured for email-only signup all return clear errors (403 / 503) — these are not per-user oracles, so hiding them would just frustrate legitimate users.

## Security trade-offs

| Concern | Current treatment |
|---|---|
| **Mailbox compromise = account compromise (lenient mode)** | When `OXICLOUD_AUTH_POLICIES` contains `permit_magic_link_for_password_users`, a user's mailbox is as strong as their password — flip the password by mail. Operator opt-in only; off by default. Aligns with modern SaaS norms (Slack, Notion, Substack). |
| **Mailbox compromise = account compromise (strict mode)** | Only applies to magic-link-eligible users (no other credential). Their mailbox **is** their credential by design. Password-secured accounts are unaffected. |
| **No native MFA** | Today OIDC delegation is the only path to MFA — the IdP (Keycloak, Authentik, Okta) enforces TOTP/WebAuthn/etc., OxiCloud sees only the resulting ID token. This is why OIDC users are unconditionally excluded from magic-link. Native TOTP / WebAuthn enrolment is a future feature. |
| **Magic-link as bearer token (login-via-email)** | Closed (PR 22). Login tokens carry a per-request challenge mirrored into the originating browser's `oxicloud_magic_request` cookie. Redemption from a different browser shows a confirmation page rather than auto-signing. Asymmetric TTL: login tokens expire in 10 min, invitations in 24 h. |
| **Magic-link as bearer token (invitations)** | Open by design. Invitations have no `request_challenge` because the recipient has no prior browser context — Alice can't pre-authorise Bob's device. The shorter TTL on login tokens (10 min) does most of the work; invitations get the longer 24 h window because recipients may not check their email immediately. |
| **Enumeration via timing** | Best-effort. `register` collision is the same code path as success (uniform response, similar latency); `magic-link/send` is bounded by per-target-email and per-IP rate limits. |

## Rate limits

Three caps protect the magic-link surface, two protect classic auth:

| Cap | Keyed on | Default | Env |
|---|---|---|---|
| Login attempts | client IP | 360/hour (test env) — production should tighten | `OXICLOUD_RATE_LIMIT_LOGIN_MAX` |
| Register attempts | client IP | 360/hour (test env) | `OXICLOUD_RATE_LIMIT_REGISTER_MAX` |
| Email-invite per sharer | `caller_id` | 50/hour | `OXICLOUD_MAGIC_LINK_INVITE_PER_CALLER_PER_HOUR` |
| Magic-link send per target email | normalised email | 5/hour | `OXICLOUD_MAGIC_LINK_SEND_PER_EMAIL_PER_HOUR` |
| Magic-link send per IP | client IP | 200/hour | `OXICLOUD_MAGIC_LINK_SEND_PER_IP_PER_HOUR` |

The two `magic-link/send` caps are **silently absorbed** when exceeded (uniform 200, no mail dispatched). The other caps surface 429 to the authenticated caller.

## Audit events

Every meaningful denial / suppression / outcome emits a structured event on the `audit` tracing target. Reason keys are stable — log aggregators key off them.

| Event | Reasons / fields (subset) | Where it fires |
|---|---|---|
| `auth.login` | `created` | success path, `register` service |
| `auth.login_rejected` | `unknown_user`, `bad_password`, `account_deactivated` | `login` |
| `auth.register` | `created`, `email_taken`, `username_taken` | `register` service |
| `auth.magic_link_send` | `sent`, `no_account`, `oidc_user`, `has_password`, `account_deactivated`, `malformed_email`, `rate_limited_ip`, `rate_limited_email` | `send_login_link` + handler |
| `magic_link.invitation_suppressed` | `oidc_user`, `has_password` | `issue_invitation` |
| `magic_link.cross_browser_prompt` | `incoming_present` flag, token_id, user_id | `redeem` when the challenge cookie is absent or mismatched (PR 22) |
| `magic_link.redeemed` | `cross_browser_confirmed` flag, `is_external`, resource fields | `redeem` success path |
| `magic_link.redemption_rejected` | `token_not_found`, `token_used`, `token_expired`, `account_deactivated` | `redeem` |
| `auth.profile_updated` | `fields` (list of changed names) | `update_profile_with_perms` (PR 24) |
| `auth.profile_update_rejected` | `oidc_user`, `username_immutable`, `username_taken` | `update_profile_with_perms` (PR 24) |
| `auth.app_password_create_rejected` | `external_user`, `no_username` | `create_app_password` |
| `authz.external_user_blocked` | `internal_only_surface` | `require_internal_user_layer` (CalDAV/CardDAV/WebDAV) |
| `auth.nc_basic_rejected` | `external_user` | `basic_auth_middleware` |
| `groups.search_rejected` | `external_user` | `search_groups` |
| `user_profile.rejected` | `external_no_relationship`, `target_external_hidden`, `target_hidden` | `get_user_profile` |
| `authz.denied` | resource-specific | `AuthorizationEngine::require` |

## Migration path for existing instances

The auth model lands across PR 16-24, all forward-only and non-destructive.

**PR 16 — schema cleanup.**

- `username` and `password_hash` drop their `NOT NULL` constraints; existing rows keep their values.
- Email-shaped usernames on `is_external = true` users are NULL'd (they were redundant duplicates of the email column).
- Sentinel password strings (`__EXTERNAL_NO_PASSWORD__`, `__OIDC_NO_PASSWORD__`) are replaced with `NULL`.
- A CHECK constraint bans `@` in usernames going forward. Existing usernames are pre-validated as compliant.

**PR 22 — device-bound login tokens.** Adds `auth.magic_link_tokens.request_challenge TEXT NULL`. Invitation tokens already in flight keep NULL and continue to redeem cross-device. New login tokens get the challenge and the cookie binding.

**PR 23 — email-verified signal.** Adds `auth.users.email_verified_at TIMESTAMPTZ NULL`. Backfill stamps OIDC users (`oidc_subject IS NOT NULL`) and externals who have logged in at least once (`is_external = TRUE AND last_login_at IS NOT NULL`) — for both groups, the proof-of-control event happened in the past. Everyone else stays NULL until they go through a magic-link flow.

**Continuity guarantees.** Existing internal users with `username` + `password_hash` continue to work unchanged. External users keep their session UUIDs; their JWTs reference `user_id`, not `username`, so session continuity is preserved. The address-book and share-modal use the `username → given_name family_name → email` fallback chain for display. NextCloud clients keep working because (a) the URL path uses username, which is now immutable for the lifetime of the account, and (b) app passwords are tied to `user_id` and survive every other change to the user record.

## Future direction — per-user `login_strategy`

The current model has moved from fully-implicit toward **instance-scoped explicit** via `OXICLOUD_AUTH_METHODS` and `OXICLOUD_AUTH_POLICIES` (see above). The next step is **per-user explicit** — a policy enum on the user row that overrides the deployment default:

| Strategy | Login requires |
|---|---|
| `passwordless` | magic-link only (current external default) |
| `password` | password only |
| `password_or_magic_link` | either (today's `permit_magic_link_for_password_users` per-account) |
| `password_and_magic_link` | both — true 2FA, mailbox-as-second-factor |
| `oidc` | IdP redirect (existing) |
| `password_and_totp` | once native TOTP enrolment ships |
| `password_and_webauthn` | once native WebAuthn enrolment ships |

`password_and_magic_link` is particularly interesting: it turns the parallel single-factor paths we have today into a real MFA primitive (something you know + access to a mailbox). No new auth code required — just a policy gate.

The instance-scoped equivalents are already deployed via `OXICLOUD_AUTH_METHODS` / `OXICLOUD_AUTH_POLICIES`; per-user overrides would need a new column and an eligibility branch that reads it. Stays out of the current PR sequence.

## What is deliberately out of scope

- **Native TOTP / WebAuthn enrolment.** The eligibility predicate has room for a `Reject("mfa_enrolled")` branch once native MFA lands. OIDC delegation is the only MFA path today.
- **External-user → internal-user promotion — SHIPPED.** `POST /api/auth/upgrade-to-internal` flips `is_external` to false, optionally sets a password (optional iff the deployment offers magic-link login), and provisions a personal drive via `PersonalDriveLifecycleHook::on_upgraded_to_internal`. Refused with distinguished `error_type` codes: `AlreadyInternal`, `ManagedByIdP` (OIDC users), `PasswordRequired`, `RegistrationDomainNotAllowed` (domain outside the register allowlist — invitations must not become a bypass of the operator's self-registration policy). Self-service only; admin-side upgrade endpoint is a follow-up.
- **Session-kind discriminator.** A magic-link session is indistinguishable from a password session today. Scoped sessions (Option-B style: "magic-link sessions only access granted resources") are deferred.
- **Differentiated session TTL for externals.** Refresh-token expiry is uniform today. Future env: `OXICLOUD_EXTERNAL_REFRESH_TOKEN_EXPIRY_DAYS`.
- **Open Cloud Mesh (OCM) federation.** A third source for external provisioning. The `ExternalIdentityLifecycleHook::on_user_created` design accommodates the `source` discriminator (`magic_link` / `oidc` / `ocm`).
- **Email-verified login gate — SHIPPED.** `OXICLOUD_REQUIRE_VERIFIED_EMAIL=true` gates login on `email_verified_at IS NOT NULL` (admins exempt). Gating other features (uploads, shares, etc.) on the same signal is future work; the plumbing is in place.
- **Username rename via the API.** PR 24 makes `username` claim-once-immutable on `/api/auth/me/profile`. A future admin endpoint at `PATCH /api/admin/users/{id}` can override for typo correction; that surface is admin-policy territory, not user-self-service.
- **Anti-enumeration latency parity.** The success and collision branches of `register` already use similar code paths, but a sophisticated attacker could still time-distinguish. Deferred; rate-limiting bounds the damage.
- **Per-user opt-out of magic-link.** The `OPEN_TO_PASSWORD_USERS` flag is instance-wide today. A future per-account toggle for high-privilege users (admins, etc.) would need a column + extra eligibility branch.
- **Clearing `given_name` / `family_name`.** PR 24's profile endpoint can SET them but not CLEAR them back to NULL. A future patch with `Option<Option<String>>` serde semantics or a dedicated DELETE endpoint can add that.
- **`login_strategy` enum** (above) — the data model accommodates it but the policy code is future work.

## Related documents

- [Magic-link external authentication](/architecture/magic-link-auth) — the magic-link mechanism in depth: token lifecycle, invitation flow, kill switches, defence-in-depth boundary protections.
- [ReBAC Authorization](/architecture/rebac-authorization) — how grants are evaluated against `auth.users` rows (including externals).
- [Share Integration](/architecture/share-integration) — how share-link flow relates to the email-invite flow.
- [Environment Variables](/config/env) — the full set of `OXICLOUD_*` knobs referenced in this page.
