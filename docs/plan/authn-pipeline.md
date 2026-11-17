# Authentication pipeline — one admission gate, many credentials

Branch: `feat/scope-and-session-elevation`. Companion to
`scope-gates.md`, which owns the **per-call** axes (Gate / Scope /
Grant). This document owns the **login phase**: what happens between a
credential being verified and a session existing.

Status: **DRAFT** — audit complete, design agreed in review, nothing
implemented.

## Goal

One place that decides "may this account have a session, and how wide",
reached by every credential type. Today that decision is spread across
eight authentication paths and four enforcement layers, and the result
is that `require_verified_email` is enforced on exactly one path.

The split that makes it cheap:

```
login phase   creds → scope (method ceiling) → admission → reject | narrow | mint
              many checks, once, no session created on reject

per call      one cached UserFlags read → still admitted? role? internal?
              + scope test (bitmask, free) + grant per resource
```

Login-phase checks cost nothing per request. Per-call checks are only
those that must take effect *mid-session*.

## The audit

Four account-state fields exist, enforced in four different places:

| field | enforced by |
|---|---|
| `active` | `ensure_user_is_active` (3 callers), `verify_credentials`, `refresh_token`, app-password verify, and per-call `LiveRole::Revoked` |
| `email_verified_at` | **one site** — `login()` ~1700, admins exempt |
| `is_external` | `require_internal_user`, per call, live |
| `force_password_change_at_next_login` | `require_no_password_change_pending_layer`, **5** mount points |

Which path applies which:

| path | `active` | email verified | force-pwd-change |
|---|---|---|---|
| classic `login()` | ✓ `:1626` | ✓ `:1700` (admins exempt) | sets/clears |
| **OPAQUE `ke3`** | **✗** | **✗** | ✗ |
| magic-link redeem | ✓ `:2184` | *marks* verified `:2176` | ✗ |
| OIDC | ✓ `:5795` | own logic from IdP claims | ✗ |
| **app password** | ✓ (≤30 s cache) | **✗** | ✗ |
| **device approve/poll** | **✗** | **✗** | ✗ |
| NC login v2 | ✓ `verify_credentials` | ✗ | ✗ |
| **refresh** | ✓ `:2346` | **✗** | ✗ |
| **per call** | ✓ `LiveRole` | **✗** | layer-dependent |

### Findings

1. **Email verification is enforced on 1 of 8 paths and never per
   call.** OPAQUE, app passwords, the device flow, NC login v2 and
   refresh all bypass `require_verified_email`.
2. **`refresh` undoes the one path that does check.** It re-mints
   without re-evaluating, so a session established once outlives the
   requirement indefinitely.
3. **OPAQUE bypasses `active` too.** `ke3` fetches the user and calls
   `mint_session_for_authenticated_user` directly. Per-call
   `LiveRole::Revoked` kills the *next* request, but the login itself
   succeeds — a successful login for a deactivated account, which also
   confirms the account exists.
4. **Device approval inherits nothing.** The approving session passed
   gates; the minted token records none of them.
5. **The cause is structural, not a missing `if`.** The gate lives
   *inside* `login()`, while `mint_session_for_authenticated_user` was
   factored out "so the OPAQUE login handler" could reuse it — reusing
   the minting and leaving the gate behind. A ninth path will repeat it.

## The phases, and when each runs

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

The ordering is load-bearing: **scope before admission**, so admission
can only narrow what the credential kind already bounded. Reversed, a
restricted account could be granted more than its credential allows.

### Which phases run, and when

Canonical home once § Required: documentation is discharged is
`docs/architecture/auth-model.md` § Phases; the copy below is the
draft it is cut from.

| entry point | 1 creds | 2 scope | 3 admit | 4 mint | notes |
|---|---|---|---|---|---|
| initial login — password, OPAQUE, magic-link, OIDC | ✓ | ✓ | ✓ | ✓ | the reference path |
| device approval | ✓ (approver's session) | ✓ | ✓ **of the approver** | ✓ | the minted token belongs to the approver; admission must run against them, which today it does not |
| **refresh** | ✓ (refresh token) | ✓ **re-derived** | ✓ **re-run** | ✓ (rotation) | the re-verification ceremony — see below |
| **app password** | ✓ | ✓ | ✓ | ✗ | **no session exists**; every request is the login phase |
| anonymous share link | ✓ (link token) | ✓ from the share grant | **skipped** | ✓ | no `auth.users` row, so nothing to admit |
| obligation discharge | — | — | — | — | not a phase: an ordinary authenticated call inside the narrowed scope, followed by a refresh to re-admit |
| **per call** | — | — | — | — | no phases: one cached read + scope test + grant |

### Refresh re-derives from `sessions.origin`, not from "it is a refresh"

The ceiling must come from the **credential that minted the session**,
not from the refresh itself. Otherwise refreshing a device-flow session
would widen it to the interactive ceiling — a privilege escalation
reached by waiting an hour.

The column already exists: `auth.sessions.origin`
(`20261013000000_sessions_origin.sql`), typed
`SessionOrigin { Password, Opaque, MagicLink, Oidc, Device, Unknown }`.
So refresh computes

```
scope = ceiling_of(session.origin) ∩ admit(current account state)
```

and never reads the presented token's scope. `Unknown` — rows predating
that migration — must resolve to the **narrowest** ceiling, not the
widest; it is the `_ =>` arm, and § Dependency applies.

### App passwords have no login phase because every request is one

`verify_basic_auth` returns `(user_id, username, email, role)` and
creates no session row; there is no `AppPassword` variant in
`SessionOrigin` and there should not be one. Stages 1-3 therefore run on
**every request**, which has two consequences:

- it is the credential that most needs `admit()`, because the gap found
  in § The audit is executed thousands of times rather than once;
- admission must sit behind the existing 30 s result cache, caching the
  `AdmittedCaller` rather than only the flags. Re-running Argon2id and
  three account checks per request is not an option — the cache is why
  this path is affordable at all, and its documented bound ("expiry and
  deactivation are bounded by the TTL") becomes the bound on admission
  too.

## Login phase

### Stage 1 — credentials

Each method answers one question: is this credential valid? No account
checks. Output:

```rust
struct AuthenticatedPrincipal {
    user_id: Uuid,
    method: AuthMethod,            // widen the existing enum (config.rs ~1616)
    credential_id: Option<Uuid>,   // app-password / device-code row
}
```

`AuthMethod` exists today with `Password | MagicLink | Oidc` and needs
`Opaque | AppPassword | Device | Refresh`.

### Stage 2 — scope, forced not requested

`ScopeSet` derived from `(method, credential) ∩ type_ceiling`
(`scope-gates.md` § Defaults). The method *determines* the maximum; a
credential cannot ask for more than its kind allows.

Scope comes **before** admission so that admission can only narrow
further — it never widens what the credential kind already bounded.

### Stage 3 — admission: reject or narrow

One function, every path:

```rust
fn admit(p: AuthenticatedPrincipal, s: ScopeSet)
    -> Result<AdmittedCaller, AppError>
```

| condition | action | safe to decide here because |
|---|---|---|
| inactive / deleted | **reject**, no session minted | fails fast; deactivation *after* login is caught per call |
| email not verified | **narrow** to `{account}` so the user can resend verification, or reject (today's behaviour) | email is not user-mutable, so it cannot change under a live session |
| force password change | **narrow** to `{security}` | `set_force_password_change` already calls `revoke_all_user_sessions` (`:4413`), so no stale full-scope session can exist |

Narrowing rather than rejecting is what makes the restricted states
usable: the session is real, the discharge endpoint is an ordinary
authenticated route, and the client gets a session it can act within.

**`require_no_password_change_pending_layer` is deleted** — five mount
points, and with them the "nine layers, tenth nest" fragility. The
narrowing plus the existing session revocation covers it completely, and
`force_password_change` leaves the per-call path entirely.

### Stage 4 — mint, and why bypass stops compiling

```rust
// today — the hole
mint_session_for_authenticated_user(user_id, …)
// proposed
mint_session_for_authenticated_user(caller: AdmittedCaller, …)
```

`AdmittedCaller` is constructible only by `admit()`. OPAQUE's current
code stops compiling, and a ninth path cannot repeat finding 5. Rank 1
of `scope-gates.md` § Maximise deny-by-default — won't compile — rather
than rank 5, a check a reviewer must notice.

## Mandatory actions (obligations)

Forward-looking; not for the first implementation. The use cases are
"accept the new terms of service" and "confirm your personal data are
correct".

Narrowing generalises to them, and that is the reason to prefer it over
gating before session creation: with no session, discharging the
obligation needs a **half-authenticated principal** — a new credential
kind with its own ceiling, gates and tests, and a fresh attack surface.
Narrowing adds no new concepts.

Four consequences, recorded so the shape is right the first time:

1. **A set, not another boolean.** `force_password_change` is a bool.
   Terms-acceptance, data-confirmation and password-reset can all be
   pending at once, so the input is a set of obligations and the minted
   scope is the union of what discharges them. Decide this before a
   second `force_*` column exists.
2. **Each obligation declares the scope that discharges it**, or
   narrowing bricks the account — the same deadlock risk as the forced
   reset below.
3. **Each obligation declares its audience.** An app password or device
   token cannot show a dialog. "Accept new terms" may need to block
   every credential; "confirm your data" should not silently break every
   DAV client. These answer differently, so it is per-obligation.
4. **The client is told why, in the API.** The login and refresh
   responses (or `/api/auth/me`) carry the pending obligations.
   Otherwise the SPA infers its state from 403s.

### The forced-reset deadlock

A session narrowed to `{security}` must be able to discharge the reset.
But `scope-gates.md` § `security` requires `Gate::Elevated` for security
mutations — so the narrowed session must be able to elevate, using the
very password it is being forced to change. That works today (the user
just authenticated with it), but it is a path that must be pinned: if
elevation ever requires something a narrowed session cannot obtain, a
forced reset bricks the account.

## Refresh is a re-verification ceremony

Refresh re-runs stages 2 and 3. Two rules make it work:

**It re-derives scope and never inherits it.** Scope comes from
`(method ceiling ∩ current admission)`, ignoring the presented token's
scope entirely. Both directions matter:

- inherit, and a session narrowed to `{security}` can never widen after
  discharge — the user is stuck permanently;
- trust the old token's claims, and a narrowed token could ask for more.

Re-deriving makes refresh the **only** widening path, and safe because
it re-runs admission.

**An obligation created mid-session binds at the next ceremony**, and
the latency is bounded by the access-token lifetime rather than the
refresh lifetime. `refresh_token_expiry_secs` is 604800 (7 days) but
"with rotation, active sessions auto-renew", and the access token lives
~1 h — so an active client re-admits hourly and an idle one re-admits
the moment it returns. The only escape is a session riding out its
current access token.

That is a bounded latency, not a hole. Where an obligation needs
immediacy, the lever is `revoke_all_user_sessions` at publish time — the
mechanism `set_force_password_change` already uses.

## Revocation

### The rule that makes login-phase evaluation sound

> Every state change that **tightens** must either revoke sessions or be
> in the per-call read.

Satisfied today by accident rather than by design, and written nowhere —
so the next `force_*` flag will get it wrong:

| change | revokes sessions? |
|---|---|
| `change_password` | ✓ |
| `set_force_password_change` | ✓ (`:4413`) |
| deactivation (`set_user_active`) | ✗ — relies on per-call `LiveRole`, and the flags cache is invalidated (`:4577`) |
| OIDC backchannel logout | ✓ — a third revocation source |
| email change | n/a — not user-mutable |
| **new obligation published** | **must choose**, per § Mandatory actions |

### There is no true revocation today

`revoke_all_user_sessions` runs
`UPDATE auth.sessions SET revoked = true WHERE user_id = $1`. The
session row carries the **refresh token**, so revocation prevents
*renewal*. `auth_middleware` reads `claims.sid` only to feed the
last-seen tracker — explicitly "no I/O on this hot path" — and never
checks whether the session is revoked.

So revoking leaves the access token working for its remaining lifetime,
and the admin panel's `DELETE /api/admin/sessions/{id}` is "stop this
session renewing", not "stop this session".


### Revocation must target the record that grants access

The admin panel's `DELETE /api/admin/sessions/{id}` is the revoke-one
case above, and `Gate::Elevated` already covers it — it is a non-safe
method under `/api/admin/*` (`scope-gates.md` § Declaring session
elevation). Two neighbours are less well served.

**Device revocation is broken at the data source.** Four facts that only
bite in combination:

1. `approve()` creates a `Session::new(…)` — the durable record of a
   connected device, with `origin = Device`.
2. `poll()` deletes the `auth.device_codes` row once the tokens are
   collected, deliberately: "prevents plain-text tokens from lingering
   in the database".
3. `list_user_devices` reads `FROM auth.device_codes WHERE user_id = $1`,
   unfiltered by status.
4. `revoke_device` deletes a `device_codes` row.

So `GET /api/auth/device/devices` lists **uncollected authorizations**,
not connected devices, and `DELETE /api/auth/device/devices/{id}` can
only cancel a pending one. A device that has actually connected is
absent from its own list and unrevokable from it. Its only record is the
session row — admin-only, and today that stops renewal rather than
access.

The fix follows the data: device listing and revocation belong on
`auth.sessions WHERE origin = 'device'`, which also supplies the
last-seen and IP columns the admin panel already renders. The
`device_codes` table then means what its name says — in-flight
authorizations — and pending-cancel stays a separate, smaller action.

**Self-service session revocation does not exist.** `/api/admin/sessions`
is `Gate::Admin`, so "log out my other devices" is unavailable to a user
who is not an administrator. `scope-gates.md` reserves it under the
`security` scope; it needs the same durable-record treatment, not a
second mechanism.

**The general rule**, since this is the third instance of the same
mistake:

> A "revoke" action must target the record that actually grants access,
> and must use the revocation mechanism above rather than deleting a
> row and hoping.

App-password revocation is the one that gets this right today, and is
worth copying: it deletes the `app_passwords` row *and* explicitly
invalidates the verification cache, "so a revoked password never
survives in cache regardless of TTL".

### True revocation, at no per-call cost

`UserFlags` is already read per call and already cached, with eager
invalidation on mutation. Both forms ride that read:

- **Revoke-all** — add `sessions_valid_from: TIMESTAMPTZ` to the user
  row. Revoking sets it to `now()`; the middleware rejects any token
  whose `iat` precedes it. One comparison, no new query.
- **Revoke-one** — a bounded `revoked_sids` set in the same cached row.
  Bounded because a revoked `sid` only needs remembering until its
  access token would have expired anyway (~1 h), so the set self-prunes.
  `sid` is already on `JwtClaims`, present "precisely so the auth
  middleware can act per-session without a DB round trip".

Latency is the flags-cache TTL (30 s) unless the revoking call
invalidates eagerly — and that path already exists
(`user_flags_cache.invalidate`), so revocation becomes immediate by
calling it.

This keeps the per-call budget flat: one cached read, now answering
"still admitted, and is *this session* still valid" instead of only the
former.

## Anonymous sessions do not pass through `admit()`

A public-link session has no `auth.users` row — `share_service.rs` ~476,
"Anonymous link tokens always get the Viewer role (read-only)" — so
there is nothing to admit. It is a login phase producing a session from
a credential with no account behind it.

It gets an explicit arm rather than being an exception someone discovers:
stage 1 yields no `user_id`, stage 2 derives the scope from the share
grant, stage 3 is skipped, and the resulting principal fails
`Gate::NotAnonymous` everywhere except the declared allowlist. Without
the explicit arm, someone calls `admit()` on it and gets a `NotFound`
resolved by whichever fail-open arm they land on.

## Dependency: the two fail-open arms

Collapsing per-call account state to a single question makes one
existing bug load-bearing. `src/AGENTS.md`:

> **An auth helper's `_ =>` arm must DENY.** Two fail-open gates exist
> and are bugs, not patterns to copy: `require_internal_user` admits the
> caller on *any* `get_user_flags` error, and `decide_live_role`
> resurrects the claim role on a transient DB error.

Today several checks sit in different places, which partly masks it.
Under this design there is **one** per-call account check reading **one**
cache — so a fail-open arm there is the entire per-call account gate
failing open at once. `scope-gates.md` § Maximise deny-by-default
already decided the fold fixes these; this is why it is not optional.

## Audit events

Per AGENTS.md, every denial emits a line before returning. The new
transitions need stable `event` / `reason` keys, inside the existing
`auth.*` domain:

- `auth.admission_rejected`, `reason = "inactive" | "email_not_verified"`
- `auth.session_narrowed`, with the obligation that caused it — the only
  trace of why a user is restricted
- `auth.obligation_discharged`
- `auth.refresh_readmission_rejected`
- `auth.session_revoked`, distinguishing revoke-one from revoke-all

## Test plan

- Unit: `admit()` per condition — reject for inactive, narrow for
  force-password-change, and the email arm whichever way it is settled.
- Unit: **every path reaches `admit()`.** Enumerate the authn entry
  points and assert each produces an `AdmittedCaller`; the type makes
  bypass a compile error, and this pins that nobody re-introduces a
  `user_id`-taking variant.
- Hurl: the audit matrix inverted — OPAQUE login, app-password auth,
  device token and refresh each **refused** for an unverified email
  where classic login is refused. These four are the current bypasses,
  so they are the regression pins.
- Hurl: forced-reset discharge end to end — login with the flag set
  yields a `{security}`-scoped session, elevation succeeds with the
  current password, the change succeeds, and the *new* session is
  full-scope. This is the deadlock pin.
- Hurl: refresh re-derives rather than inherits — a narrowed session
  refreshes to a narrowed session while the obligation stands, and to a
  full one once discharged.
- Hurl: true revocation — after `DELETE /api/admin/sessions/{id}` the
  **access token** stops working, not merely the refresh. This fails
  today, which is the point.
- Hurl: device revocation reaches the device. Approve a device, confirm
  it appears in `GET /api/auth/device/devices`, revoke it, and confirm
  its **access token** stops working. All three legs fail today — the
  device is absent from its own list once connected, so the test cannot
  even reach the revoke step.
- Unit: the tightening inventory — a test that fails if a state-changing
  method is added without either revoking sessions or appearing in the
  per-call read. Table-driven, so the rule above has an executable form.

## Adding a new restriction — the recipe

The point of one admission function is that a new account condition is a
*decision*, not a search for somewhere to put a check. Answer six
questions; each has a default, and the defaults are the safe ones.

### 1. Login phase, or per call?

```
Can the condition change mid-session?
├─ no  → LOGIN PHASE. Evaluate in `admit()`. Costs nothing per request.
└─ yes → Must the change take effect before the session ends?
         ├─ no  → LOGIN PHASE, and it binds at the next refresh
         │        ceremony (≤ access-token lifetime, ~1 h).
         └─ yes → PER CALL, and it goes in `UserFlags` —
                  never in a new middleware layer.
```

Only two conditions have ever needed "yes/yes": deactivation and role
change. Both already ride the one cached read. **A new per-call
condition that cannot be answered from `UserFlags` is a design smell**:
it means a second per-request lookup, which is what § Goal exists to
avoid.

**Never add a middleware layer for an account condition.** That is how
`force_password_change` ended up at five mount points and
`auth_middleware` at nine — and how the tenth mount silently escapes.

### 2. Reject, or narrow?

| choose | when | example |
|---|---|---|
| **reject** | no useful session exists under the condition | inactive, deleted |
| **narrow** | the user can discharge it themselves | forced reset, terms acceptance |

Narrowing is preferred wherever discharge is possible: the session is
real, the discharge endpoint is an ordinary authenticated route, and the
client gets something it can act within. Rejecting forces you to invent
a half-authenticated principal — see § Mandatory actions.

If you narrow, you **must** name the scope that discharges it. A
narrowing that excludes its own discharge path bricks the account.

### 3. Does setting it tighten? Then prove the invariant.

> Every state change that tightens must either revoke sessions or be in
> the per-call read.

Pick one and say which in the commit. Setting a restriction without
doing either leaves every existing session at its old width — the
restriction applies to future logins only, which is almost never what
was meant.

### 4. Who is exempt, and say it out loud.

Exemptions are legitimate — admins skip email verification so an
operator cannot lock themselves out of their own instance — but they
belong as an **arm inside `admit()`**, where a reviewer sees them beside
the rule. The current bug is the opposite shape: the rule lives in one
path and the "exemption" is its absence from seven others.

### 5. Which credentials does it bind?

Interactive sessions can show a dialog; an app password and a device
token cannot. So a restriction that narrows scope will, for a DAV
client, simply break sync with no explanation. Decide per restriction
whether it binds every credential or only interactive ones, and expect
"accept new terms" and "confirm your data" to answer differently.

### 6. The tests are fixed, not negotiable

- **The path matrix.** Assert the new condition on *every* authn entry
  point — classic, OPAQUE, magic-link, OIDC, app password, device,
  NC login v2, refresh. This is the pin that the audit above would have
  caught: eight paths, one enforcement.
- **Refresh re-evaluates it.** Otherwise the condition is bypassable by
  waiting an hour.
- **Discharge works, and does not deadlock** — if you narrowed.

### Worked example: "accept the new terms"

1. *Login phase* — a published terms version cannot change inside a
   request. Binds at the next refresh.
2. *Narrow*, not reject — the user can accept. Discharge scope:
   whichever scope holds `POST /api/terms/accept`.
3. *Tightens on publish* — so publishing either calls
   `revoke_all_user_sessions` (immediate, heavy-handed) or accepts ≈1 h
   latency. A legal requirement probably picks the former; record which.
4. *Exempt*: nobody. Note that this differs from email verification, and
   why.
5. *Binds*: all credentials if legally required — which means accepting
   that rclone stops syncing until a human clicks, so say so in the
   release note rather than discovering it in an issue.
6. *Tests*: the eight-path matrix, refresh re-evaluation, accept →
   refresh → full scope.

## Required: documentation

As with `scope-gates.md`, this does not land as code alone. The audit
matrix in particular is the thing a future reader needs: "why did my
OPAQUE login succeed for a disabled account" is answerable from it and
from nothing else.

**Extend `docs/architecture/auth-model.md`** rather than adding a file.
It already owns § Identity model, § Login paths and § Audit events, so
it is where someone debugging a login already is:

| section | change |
|---|---|
| § Login paths | gains the per-path admission matrix — which path applies which account check. Replaces the current prose, which describes how each path authenticates but not what it then verifies |
| new § Phases | **the diagram and the when-each-runs table** from § The phases, and the `sessions.origin` rule for refresh. A reader must see credentials → session at a glance and be able to answer "what does refresh re-check" without reading code. Not redundant with the matrix above: the matrix says what each path checks, this says *when* it is checked |
| new § Admission | the four stages, reject-vs-narrow, and the tightening invariant |
| § Audit events | the five new `auth.*` events |
| § Related documents | `scope-gates.md` for the per-call axes, and the architecture page it produces |

**The when-each-runs table is the acceptance artefact**, and its
canonical home is that page — not this one. Copy it verbatim, including
the marks that carry meaning rather than decoration: app password
`4 mint ✗` (no session exists), device approval `3 admit ✓ of the
approver`, anonymous share `3 skipped`. A paraphrase loses exactly
those three, which are the non-obvious rows.

Once it lives in `auth-model.md`, **this plan's copy is the stale one**.
Plans record why a change was made and get archived; architecture docs
record how the system works and are maintained. So the plan should then
point at the page rather than restate it — the same single-source rule
`scope-gates.md` § Required: documentation applies to the scope
vocabulary.

**Cross-link from `docs/architecture/authorization-axes.md`** (the page
`scope-gates.md` § Required: documentation creates): scope is *minted*
in the login phase and *enforced* per call, and a reader arriving at
either page should find the other in one hop.

**`src/AGENTS.md` § New auth surfaces** already says "Any new endpoint
that mints or consumes credentials/tokens must consult one of the
`is_*_login_allowed()` helpers, not the raw allowlist." That is the
right place for the companion rule:

> A credential path must obtain an `AdmittedCaller` from `admit()` and
> pass it to the session mint. Taking a bare `user_id` is the bypass
> that let OPAQUE skip every account check; the signature is what
> prevents it, so do not add a `user_id`-taking variant back.

Acceptance, two things: the per-path matrix is in the architecture doc
and current, and a reader can trace **credentials → session** plus
answer "which phases does a refresh run" from that page alone. If
either only exists in this plan file, the next contributor adding a
login path has no way to know what their path must check or when.

## Non-goals

- Does not change `authz.require` or any grant check — that is the third
  axis, owned by ReBAC.
- Does not implement obligations, nor the terms-acceptance example in
  § Adding a new restriction. Those sections exist so the first
  restricted state is built in a shape the second can reuse.
- Does not add an operator knob. The admission rules are policy, not
  configuration.
