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

### Admission — what each door verifies after the credential

The table above says how each path proves *who you are*. This one says what is then checked about the *account*, which is a separate question and the one that used to be answered differently by every door.

"Door" is finer-grained than "login path" on purpose: a user-facing path can reach the server through more than one credential, and it was the extra ones — OPAQUE, the device flow, the refresh ceremony — that skipped the checks.

Every door now calls one function, `check_account_admission`, so this table has one column rather than nine opinions:

| Door | `active` | verified email | Notes |
|---|---|---|---|
| Classic login (`/api/auth/login`) | ✓ | ✓ | the early `active` check fires BEFORE the OPAQUE-migrated branch, so a disabled migrated user is told "deactivated" rather than "use the OPAQUE endpoint" |
| OPAQUE (`/api/auth/opaque/login/ke3`) | ✓ | ✓ | the handshake COMPLETES for a disabled account — its envelope is still valid — so the refusal is an account decision taken after the crypto |
| Magic-link redemption | ✓ | n/a | redeeming the link IS the verification; `mark_email_verified` runs before the mint |
| OIDC callback | ✓ | ✓ | the email half is decided from the IdP's `email_verified` claim and then stamped locally |
| App password (Basic auth) | ✓ | ✓ | every request is a login; see § Caching below for what that means |
| Device approval (`/device/verify`) | ✓ | ✓ | checks the APPROVER, who is the one holding a session |
| Refresh (`/api/auth/refresh`) | ✓ | ✓ | a refresh is a re-admission, not a formality |
| Refresh grace replay | ✓ | ✓ | re-issues without rotating, so it owes the same checks |
| NextCloud Login Flow v2 | ✓ | ✓ | runs AFTER the password verify; the flow ends by handing out an app password, so it owes a session mint's checks |
| Anonymous share session | n/a | n/a | no `auth.users` row exists; there is nothing to admit |

**Only the Owner is exempt from the verified-email requirement — not admins.** The exemption exists so the instance cannot lock itself out: the rule is enforced at login, so an account whose own address is unverified would have no way left to log in and fix it. One unlockable account is all that takes, and there is exactly one Owner. Admins are plural and replaceable, so exempting all of them would widen the hole for no extra recovery. Admin-created accounts are stamped verified at creation ("admin fiat counts"), so the admins this refuses are those promoted from a self-registered account — the case where an unverified address is worth questioning.

**The verified-email column is conditional on `OXICLOUD_REQUIRE_VERIFIED_EMAIL`** (default `false`). With it off, the column is vacuous everywhere.

#### Why a check can be present and still not protect you

Two surfaces verify the account on a cached read, and the cache is the thing to understand:

- **App passwords.** `verify_basic_auth` caches the credential→identity mapping for 300 s, and its own `is_active` check runs only on a cache MISS — which a client that just authenticated never produces. So the account state is NOT read from that cache: the middleware re-checks the live record on every request, exactly as the Bearer and cookie paths do. The password cache's job is skipping Argon2, nothing more.
- **The NextCloud surface** (`/remote.php/**`) has its own `basic_auth_middleware` and accepts no Bearer credential, so that middleware is its only gate. It reads the live flags and checks `active`, `is_external` and the role from one lookup.

### Per-call checks, and the ones that are not per-call

Admission answers "may this account have a session". Three things are then re-checked on **every** authenticated request, from the cached `UserFlags` row:

| Checked per call | Not checked per call |
|---|---|
| `active` — deactivation takes effect at once | the verified-email rule, which is a login-time gate only |
| the live `role` — a demotion takes effect at once, never the frozen JWT claim | |
| revocation, both forms — see § Revocation | |

A credential that carries no JWT reaches a narrower set of these: an app password has no `iat` and no `sid`, so neither revocation form applies to it — it is revoked by **deleting** it. A WebSocket session is handed only a `caller_id`, so an open socket notices deactivation but not a revoke-all.

## Phases — credential to session

Four phases sit between "a credential was presented" and "a session exists". Reading them in order is how you answer *when* something is checked, as opposed to *what* each door checks (§ Admission above).

```
  credential                                            session
      │                                                    ▲
      ▼                                                    │
 ┌─────────┐    ┌─────────┐    ┌───────────┐    ┌──────────┴──┐
 │ 1 creds │ ─► │ 2 scope │ ─► │ 3 admit   │ ─► │ 4 mint      │
 │ verify  │    │ ceiling │    │ reject or │    │ session +   │
 │         │    │         │    │ narrow    │    │ tokens      │
 └─────────┘    └─────────┘    └───────────┘    └─────────────┘
   who?          how wide        may they?        issue it
                 at most?        how wide now?

 reject at 3  ⇒  no session row is ever created
```

**Phase 2 is designed but NOT implemented.** Credential scopes live in `docs/plan/scope-gates.md`; today every session is full-scope and phase 3 can only reject, never narrow. The phase is shown because the ordering is load-bearing and the gap is easier to understand than to rediscover: scope comes *before* admission so that admission can only narrow what the credential kind already bounded. Reversed, a restricted account could be granted more than its credential allows.

### Which phases run, and when

| entry point | 1 creds | 2 scope | 3 admit | 4 mint | notes |
|---|---|---|---|---|---|
| initial login — password, OPAQUE, magic-link, OIDC | ✓ | — | ✓ | ✓ | the reference path |
| device approval | ✓ (approver's session) | — | ✓ **of the approver** | ✓ | the minted token belongs to the approver, so admission runs against them |
| **refresh** | ✓ (refresh token) | — | ✓ **re-run** | ✓ (rotation) | a re-admission, not a formality |
| refresh grace replay | ✓ (parent refresh) | — | ✓ **re-run** | ✗ (re-issues, no rotation) | inside the grace window only |
| **app password** | ✓ | — | ✓ | **✗** | **no session exists** — every request is the login phase |
| anonymous share link | ✓ (link token) | — | **skipped** | ✓ | no `auth.users` row, so nothing to admit |
| **per call** | — | — | — | — | no phases: one cached read, then the grant check |

Three rows carry the non-obvious facts, and a paraphrase loses exactly these:

- **app password, `4 mint ✗`** — it creates no session row. There is no `AppPassword` variant in `SessionOrigin` and there should not be one. Phases 1 and 3 therefore run on *every request*, which is why that path's cost profile matters (§ Admission).
- **device approval, `3 admit ✓ of the approver`** — the human approving is the one holding a session; the device holds nothing yet.
- **anonymous share, `3 skipped`** — not "passes", skipped. There is no account behind the credential.

### Refresh re-runs admission against current state

A refresh re-reads the account and applies the same `check_account_admission` as a fresh login. Without that, a session established once would outlive any later tightening: deactivate a user and their client keeps rotating tokens indefinitely.

`auth.sessions.origin` records which credential minted the session (`SessionOrigin { Password, Opaque, MagicLink, Oidc, Device, Unknown }`). It is not yet read at refresh — it becomes load-bearing when phase 2 lands, because the ceiling must come from the credential that minted the session rather than from the refresh itself. Otherwise refreshing a device-flow session would widen it to the interactive ceiling: a privilege escalation reached by waiting an hour.

## Admission

One function, `check_account_admission`, is the only place a per-login account rule lives. Every door calls it by that name; there is deliberately no per-service wrapper or convenience alias, because a rule added to a wrapper reaches only the doors using the wrapper — which is how the checks came to differ in the first place.

### A session cannot be built without proof of admission

The rule being in one place stops the checks *differing* between doors. It does not stop a new door from skipping them. That is enforced by the type system instead:

```rust
// domain/entities/session.rs
pub fn new(
    admitted: &AdmittedCaller,   // ← the enforcement
    refresh_token: String,
    …
) -> Self
```

`AdmittedCaller` has private fields and no public constructor, so the only way to obtain one is `admit`, which runs `check_account_admission` first. Therefore:

- **every session row in the codebase comes from an admitted account** — not by convention, but because nothing else compiles;
- a future login door that forgets the account checks is a **compile error**, not a bypass waiting to be found;
- `user_id` is taken *from* the proof rather than passed alongside it, so "admitted user A, session for user B" is unrepresentable rather than merely unlikely.

This is deny-by-default in the strongest form available — the rank the project's own ordering puts first: *won't compile* beats *won't start* beats *fails closed* beats *merely visible*. It exists because the weaker forms demonstrably failed here: OPAQUE reached the mint with a bare `User` and issued sessions for deactivated accounts, and the device flow did the same afterwards. Both were correct-looking code that no reviewer caught.

Two deliberate boundaries:

- **`Session::from_raw` requires no proof.** Rehydrating a row that already exists is not admitting anyone, and demanding one there would mean re-running login checks in order to *read* a session.
- **`check_account_admission` and `admit` are not synonyms.** The first answers "may this account have a session" and is what the doors that mint *no* session call — an app-password request, the NextCloud login flow. The second answers the same question and hands back the capability. One rule, two return types; a new account rule goes in the first only.

When the type was introduced it immediately found a latent ordering bug: `oidc_callback` ran its admission check *after* building the session object. Harmless, since nothing was persisted yet — but a proof cannot be produced later than the thing that needs it, so the check moved to where it always belonged.

**Order is part of the contract.** `active` is asked first, so a caller who is both deactivated and unverified is audited as `account_deactivated` — the more fundamental reason — rather than sending an operator to look at the mail flow.

### Reject, not narrow

Phase 3 can currently only **reject**. The narrowing half — admitting a caller with a reduced scope so they can discharge an obligation such as a forced password change — needs credential scopes and is designed in `docs/plan/authn-pipeline.md` § Mandatory actions. Until then, a forced password change is enforced by a separate router layer (`require_no_password_change_pending_layer`) rather than by a narrowed session.

### The tightening invariant

> Every state change that **tightens** must either revoke sessions or be in the per-call read.

This is the rule that makes login-phase evaluation sound at all. A check that runs only at login is worthless for a state that can change afterwards, unless changing it also invalidates the sessions that passed the old check.

| change | how it is honoured |
|---|---|
| `change_password` | revokes other sessions, keeps the caller's (§ Revocation) |
| forced password change set | revokes sessions |
| deactivation | **not** revoked — carried by the per-call `active` read instead |
| OIDC back-channel logout | revokes sessions |
| a NEW account rule | **must choose one of the two.** Adding a login-only check for mutable state is the mistake this invariant exists to prevent |

## Revocation

Revoking a session has to stop it *now*, and that is not what flipping `auth.sessions.revoked` achieves on its own: that row holds the **refresh** token, and the auth middleware reads `claims.sid` only to feed the last-seen tracker. On its own, revocation therefore means "cannot renew" while the access token already issued keeps working for the rest of its lifetime (default 1 h).

Two columns close that, and they answer different questions:

| | column | scope | mechanism |
|---|---|---|---|
| **user-wide** | `auth.users.sessions_valid_from` | every token this user holds | a token whose `iat` precedes the stamp is rejected |
| **per-session** | `auth.sessions.revoked_at` | one named session | the session's id is carried in a bounded set and matched against the token's `sid` |

Both ride the `UserFlags` row that is already read on every authenticated request, so neither adds a query. Both answer as an ordinary revocation (the caller sees the same 401 as a deleted account), so no call site needed new handling.

Which one each operation uses:

| operation | mechanism |
|---|---|
| `POST /api/auth/logout` | per-session — stops that session's access token at once |
| `DELETE /api/admin/sessions/{id}` | per-session |
| `change_password` (browser, cookie present) | per-session for the OTHER devices; the caller's own session is kept |
| `change_password` (no session identifiable) | user-wide cutoff |
| admin password reset | user-wide cutoff |
| deactivation | neither — carried by the per-call `active` read |

**Why `change_password` keeps the caller's session.** The SPA has to finish re-registering its OPAQUE envelope on the cookie it already holds; killing that session would leave the envelope bound to the OLD password. A user-wide cutoff cannot express "all except this one", which is precisely why the per-session form exists.

**Rotation is not revocation.** A refresh revokes the parent row but does NOT stamp `revoked_at`: the parent's access token is what any request already in flight is carrying, and the refresh-grace replay re-issues against the parent inside its window. Stamping would break both.

Latency is the flags-cache TTL (30 s) in principle, but every revoking path invalidates the entry eagerly, so the refusal lands on the next request.

**What revocation does not reach.** A credential with no `sid` cannot be attributed to a session, and one with no `iat` cannot be compared to the cutoff:

- an **app password** has neither — it is revoked by **deleting** it;
- a **WebSocket session** is handed only a `caller_id`, so an open socket notices deactivation and deletion but not a revoke-all.

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
| `POST /api/auth/login` | Uniform `403 "Invalid credentials"` for unknown user and bad password — **including a bad password on a deactivated account** | A password guesser learns only "not the password". |
| `POST /api/auth/login`, **right** password on a deactivated account | `403` with `error_type: "AccountDeactivated"` | The caller has already proven possession, so naming the reason tells an attacker nothing they could not learn anyway — and the user needs to know why they cannot get in. The account check therefore fires AFTER the password verify, never before. Both halves are pinned in `tests/api/admin_user_ops.hurl`. |
| NextCloud Login Flow v2, any credential failure | `303` to `?type=invalid-credentials` | The handler discards the error, so this surface cannot distinguish the reasons even though the service does. |

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
| `auth.nc_basic_rejected` | `external_user`, `account_deactivated`, `flags_unavailable` | `basic_auth_middleware` |
| `auth.token_revoked` | `deactivated`, `deleted`, `issued_before_revocation_cutoff`, `session_revoked` | `decide_live_role` — a valid token presented for an account or session that no longer accepts it |
| `auth.live_role_indeterminate` | `transient_backend`, `lookup_failed` | `decide_live_role` when the live record could not be read: the caller is neither honoured nor revoked, and the request answers 503 or 500 |
| `authz.evaluation_failed` | `transient_backend`, `lookup_failed` | `decide_internal_user` — the internal-user gate could not evaluate. Distinct from `authz.denied`, which is a decision |
| `auth.revocation_cutoff_failed` | `write_failed` | `stamp_revocation_cutoff`. **Worth alerting on**: the sessions were revoked but the cutoff was not stamped, so access tokens stay valid until they expire |
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
- `docs/plan/scope-gates.md` — the **per-call** axes (Gate / Scope / Grant), and the credential scopes that phase 2 of § Phases is waiting on. This page owns what happens up to a session existing; that one owns what the session may then reach.
- `docs/plan/authn-pipeline.md` — the design behind § Phases and § Admission, including the per-path audit that found the gaps, and the one part still pending (the WebSocket cutoff blind spot).
