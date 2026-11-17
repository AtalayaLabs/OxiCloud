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
- Unit: the tightening inventory — a test that fails if a state-changing
  method is added without either revoking sessions or appearing in the
  per-call read. Table-driven, so the rule above has an executable form.

## Non-goals

- Does not change `authz.require` or any grant check — that is the third
  axis, owned by ReBAC.
- Does not implement obligations. The section exists so the first
  restricted state is built in a shape the second can reuse.
- Does not add an operator knob. The admission rules are policy, not
  configuration.
