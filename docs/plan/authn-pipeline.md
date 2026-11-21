# Authentication pipeline — one admission gate, many credentials

Branch: `feat/scope-and-session-elevation`. Companion to
`scope-gates.md`, which owns the **per-call** axes (Gate / Scope /
Grant). This document owns the **login phase**: what happens between a
credential being verified and a session existing.

## Status

**Part A is COMPLETE and green against a live server**; part B has not
started. Keep this table current — it is the first thing a reader
checks, and a plan that still says "nothing implemented" after the work
has shipped is worse than no status at all.

| item | status | commits |
|---|---|---|
| Fail-open arms answer 503 | **done** | `ed427185e` `a15455f14` |
| One admission check, all nine doors | **done** | `2f9101138` `c2d6045cc` |
| `AdmittedCaller` required by `Session::new` — every door | **done** | `2f9101138` `ab109e66a` |
| Revocation — user-wide `iat` cutoff | **done** | `fe12a4739` |
| Revocation — per-session (`revoked_at`) | **done** | `ae956005b` |
| `auth-model.md` documentation — phases, admission, revocation | **done** | see § Required: documentation |
| Session IP pair — `created_ip` / `last_seen_ip` | **done** | `b299f184f` `5a31e1f29` |
| Connected-device list + revoke reading `auth.sessions` | **done** | `5c3da3ac7` |
| WebSocket revocation — both upgrade paths + open-socket re-check | **done** | `75c806b5a` |
| Scope narrowing, obligations (part B) | **not started** | needs `scope-gates.md` |

Verified at the wire, not only in unit tests: the deactivated-account
refusal on every door (`e3ead258c` `52b2179a9`, OPAQUE's in
`opaque-hurl-helper` at `4355aec71`) and both revocation forms
(`6a857cc20`), and the session IP pair with spoofed addresses
(`b953ec174`), and the verified-email policy in its own server pass
(`9abe7b742`, `301606a5d`). The device list and its revoke are covered by the
pre-existing `device_auth_flow.hurl`, updated to assert the new fields
and that a device which COLLECTED its tokens is still listed — its
absence was the defect.

Two defects the RUNS found, which neither `cargo` nor clippy could see —
the reason the wire pins were worth writing:

- the Basic-auth path had **no** live account check and served a 300 s
  cached verification, so a deactivated account kept WebDAV for up to
  five minutes (`102af56b3`);
- the NextCloud surface has its **own** middleware, which ignored
  `active`, used a cached role and failed open on a lookup error
  (`9add3034b`). That surface accepts no Bearer credential, so nothing
  else would have caught it.

One thing is deliberately NOT done: the device-flow service check,
because middleware refuses a deactivated caller first and a wire test
there would assert the wrong layer (§ Test plan).

The WebSocket blind spot is CLOSED — both upgrade paths carry the claims,
and open sockets are re-checked on the keepalive tick. An eviction signal
was considered and deferred: the bus is in-process, so it would silently
fail across nodes. See `docs/architecture/auth-model.md` § Revocation for
the upgrade path.

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
| NC login v2 | ✓ `verify_credentials`, but **pre-credential** | ✗ | ✗ |
| **refresh** | ✓ `:2346` | **✗** | ✗ |
| **refresh grace replay** | ✓ hand-copied | **✗** | ✗ |
| **per call** | ✓ `LiveRole` | **✗** | layer-dependent |

### Findings

1. **Email verification is enforced on 2 of 9 paths and never per
   call** — `login()` and, by its own claim logic, OIDC. OPAQUE, app
   passwords, the device flow, NC login v2, refresh and the grace
   replay all bypass `require_verified_email`.
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
   the minting and leaving the gate behind. A tenth path will repeat it.
6. **The grace replay is a door nobody counts.** `try_grace_replay`
   re-issues an access token without rotating, and hand-rolls
   `is_active` under the comment "the user has to pass the same gates a
   normal refresh applies" — so it verifies one of the two gates it
   claims to mirror. Only a sweep of every `generate_access_token`
   caller finds it; it is reachable by anyone holding a
   just-superseded refresh token.
7. **NC login v2 checks `active` before the password verify**, and
   answers "Account deactivated" to a caller who has proven nothing —
   so a prober learns the username exists AND is deactivated.
   `ensure_user_is_active`'s own doc calls that the weaker posture. The
   flow ends by handing the client an app password, so it owes a
   session mint's checks.

### Resolved

`check_account_admission` is the single definition; `ensure_admissible`
reaches it inside the service, and the two services holding no
`AuthApplicationService` (device, app password) call it directly. Every
door calls the **composed** check, never the individual ones — asking
them one at a time per door is how they drifted, and a rule added later
would otherwise land in `admit` and miss `refresh`.

`AdmittedCaller` makes it unforgeable at the **entity**, which is what
closes finding 5 for good: `Session::new` requires one, its fields are
private with no public constructor, and only `admit` produces it. So
every session row in the codebase — present and future — comes from an
admitted account, and a tenth door that forgets the checks does not
compile rather than shipping a bypass. `user_id` comes from the proof
rather than beside it, so "admitted user A, session for user B" is
unrepresentable too.

That is the strongest rank in `scope-gates.md` § Maximise
deny-by-default — *won't compile* — and it is there because the weaker
ranks demonstrably failed on this exact surface twice: OPAQUE reached
the mint with a bare `User`, and the device flow did the same
afterwards. Both read as correct code.

Introducing the type found a latent ordering bug immediately:
`oidc_callback` ran its admission check AFTER constructing the session.
Harmless, since nothing was persisted yet — but a proof cannot be
produced later than the thing that needs it.

`Session::from_raw` deliberately requires no proof: rehydrating an
existing row is not admitting anyone, and demanding one there would mean
re-running login checks in order to READ a session.

Two carve-outs are deliberate and pinned:

- **Only the Owner is exempt from the email rule** — not `at_least(Admin)`.
  It is enforced at login, so the exempt account is the one that could
  otherwise never log in to fix its own address; one such account is
  enough, and there is exactly one Owner.
- **`login` keeps a separate early `active` check** before the
  OPAQUE-migrated branch, so a deactivated migrated user is told
  "deactivated" rather than "use the OPAQUE endpoint". App-password
  verify keeps its early check too, as a pre-filter that saves an
  Argon2 hash and leaks nothing.

No lockout risk from uniform enforcement:
`OXICLOUD_REQUIRE_VERIFIED_EMAIL` already exists and defaults to
`false`.

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

**Canonical home: `docs/architecture/auth-model.md` § Phases.** That
page is the one a reader debugging a login reaches for, and it is kept
current against the code; the copy below is the draft it was cut from
and will drift. Prefer the architecture page.

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
- the caching question is the whole design, and this draft got the
  answer WRONG. It proposed caching the `AdmittedCaller` alongside the
  credential result, so "deactivation is bounded by the TTL" would have
  become the bound on admission too.

  **That is the bug, not the fix.** The cache is keyed on
  `blake3(username + ":" + password)` and lives 300 s, not 30; its
  account check runs only on a MISS, which a client that just
  authenticated never produces. Caching admission beside it means a
  disabled account keeps the whole DAV surface for five minutes — which
  is exactly what was found on this path once a wire test existed
  (`102af56b3`).

  What shipped instead: the credential cache holds ONLY
  credential→identity, and the live account state is read per request
  from the `UserFlags` cache, which is eagerly invalidated on
  deactivation and role change. Argon2id is still skipped — that is the
  cache's real job — while the refusal is immediate. Cost is one moka
  hit, the same one every Bearer request already pays.

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
code stops compiling, and a tenth path cannot repeat finding 5. Rank 1
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

Satisfied today by accident, and written nowhere — so the next `force_*`
flag will get it wrong:

| change | revokes sessions? |
|---|---|
| `change_password` | ✓ |
| `set_force_password_change` | ✓ (`:4413`) |
| deactivation (`set_user_active`) | ✗ — relies on per-call `LiveRole`; flags cache invalidated (`:4577`) |
| OIDC backchannel logout | ✓ — a third revocation source |
| email change | n/a — not user-mutable |
| **new obligation published** | **must choose**, per § Mandatory actions |

### There is no true revocation today

`revoke_all_user_sessions` sets `revoked = true` on the session row, and
that row carries the **refresh token** — so it prevents *renewal*.
`auth_middleware` reads `claims.sid` only to feed the last-seen tracker
("no I/O on this hot path") and never checks whether the session is
revoked. The access token therefore keeps working for its remaining
lifetime, and `DELETE /api/admin/sessions/{id}` means "stop this session
renewing", not "stop this session".

### Device revocation is broken at the data source

Four facts that only bite in combination:

1. `approve()` creates a `Session::new(…)` with `origin = Device` — the
   durable record of a connected device.
2. `poll()` deletes the `auth.device_codes` row once tokens are
   collected, deliberately: "prevents plain-text tokens from lingering".
3. `list_user_devices` reads `FROM auth.device_codes`, unfiltered.
4. `revoke_device` deletes a `device_codes` row.

So `GET /api/auth/device/devices` lists **uncollected authorizations**,
and `DELETE …/{id}` can only cancel a pending one. A device that has
actually connected is absent from its own list and unrevokable from it.

The fix follows the data: list and revoke from
`auth.sessions WHERE origin = 'device'`. `device_codes` then means what
its name says — in-flight authorizations — and pending-cancel is a
separate, smaller action.

**The list needs no new columns**: `user_agent` carries
`device:{client_name}`, and `last_seen_at`, `created_at`, `expires_at`,
`revoked` and `dpop_jkt` are all on the row. It is a query change that
turns a list of things which are *not* connected into a list of things
that are.

**But two IP columns rather than one.** `ip_address` is commented
"approver's browser IP" — where the human authorised, not where the
device is:

| column | written | means |
|---|---|---|
| `ip_approved` | once, at `approve()` | where a human authorised this device — immutable, forensic |
| `ip_seen` | on the existing last-seen write | where the credential is actually used |

A mismatch between them is itself a theft signal, which no single column
can express. `ip_seen` is nearly free — the tracker already writes that
row on a batched 10 s interval (`main.rs` ~200), so the IP rides an
UPDATE that is already happening; the corollary is that it is accurate
to within that interval, and the display should not imply otherwise.
Applies to every origin, so the admin panel's IP column becomes
unambiguous for browser sessions too.

### True revocation, at no per-call cost

`UserFlags` is already read per call and cached with eager invalidation.
Both forms ride that read:

- **Revoke-all** — `sessions_valid_from: TIMESTAMPTZ` on the user row.
  Revoking sets it to `now()`; the middleware rejects any token whose
  `iat` precedes it. One comparison, no new query.
- **Revoke-one** — a bounded `revoked_sids` set in the same cached row.
  Bounded because a revoked `sid` need only be remembered until its
  access token would have expired (~1 h). `sid` is already on
  `JwtClaims` "precisely so the auth middleware can act per-session
  without a DB round trip".

  **It does not self-prune yet: `auth.sessions` has no `revoked_at`.**
  The column is a bare `revoked BOOLEAN`, so there is nothing to bound
  the set BY. Filtering on `expires_at` instead bounds it by the REFRESH
  expiry (7 days default), not the access-token hour — and that set is
  carried inside a hot-path cache entry, so a user cycling sessions
  across several devices could put hundreds of UUIDs there.

  So revoke-one needs `ALTER TABLE auth.sessions ADD COLUMN revoked_at
  TIMESTAMPTZ` first, after which the plan's design works as written and
  the set is a handful of rows. Revoke-all needed no such column, which
  is why it shipped first (`fe12a4739`).

Latency is the 30 s cache TTL unless the revoking call invalidates
eagerly — `user_flags_cache.invalidate` already exists, so calling it
makes revocation immediate. The per-call budget stays flat: one cached
read, now answering "still admitted, **and is this session still
valid**".

#### Why revoke-one is the only way to close the password-change window

`change_password` keeps the caller's session alive on purpose, so the
SPA can finish the OPAQUE envelope re-registration on the cookie it
already holds. A cutoff is one `iat` watermark for the whole user, so
stamping it there would invalidate the kept session too.

The tempting escape hatch does not exist. `/api/auth/refresh` carries no
`auth_middleware`, so a cutoff cannot block a refresh — which suggests
stamping it anyway and letting the SPA turn the caller's 401 into a
transparent refresh-and-retry. But `/api/auth/opaque/` sits on the
`AUTH_PRIMITIVES` bypass list in `frontend/src/lib/api/transport.ts`,
where a 401 is deliberately genuine and never retried, so the sync would
hard-fail and leave the envelope on the OLD password.

Hence the residual window — the other devices cannot renew, but their
already-issued access tokens work until they expire — closes only with
per-session revocation, and therefore only after `revoked_at`.
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

## The two fail-open arms

`require_internal_user` admits on any `get_user_flags` error;
`decide_live_role` resurrects the claim role on a transient one. The
resolution is in `scope-gates.md` § The two fail-open arms, and it is
short: a transient lookup failure is a server fault, so answer **503**
rather than yes or no. `NotFound` stays a definitive deny.

Two consequences for this document:

- **It needs a transient error kind.** `ErrorKind` has no such variant
  and `Timeout` maps to 408, which means the client was slow. Part A
  adds it, since part A is where `admit()` makes the first
  distinction between "this account is refused" and "I cannot tell".
- **The arms' stated defence was "login/refresh already enforce `active`
  at the canonical layer", and § The audit found that false** — OPAQUE's
  `ke3` never checks it, nor does device approval. Part A makes the
  premise true. That is worth knowing but no longer changes the
  ordering, because 503 is right either way.

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
  device token, NC login v2, refresh and the grace replay each
  **refused** for an unverified email where classic login is refused.
  These six are the current bypasses, so they are the regression pins.

  **Split them by which half you are pinning.** The deactivated-account
  half needs nothing special and is a plain regression pin per door
  (OPAQUE's lives in `opaque-hurl-helper`, because OPRF blinding cannot
  sit in a `.hurl` body).

  The unverified-email half **cannot run in the current suite**, and the
  reason is structural rather than a missing fixture:

  - `OXICLOUD_REQUIRE_VERIFIED_EMAIL` is start-up config, not a runtime
    setting, so one server cannot have it off for the rest of the suite
    and on for this scenario. `/api/config` exposes it, so a test can
    read the policy but not change it.
  - Admin-created users get `email_verified_at = NOW()` ("admin fiat
    counts"), so the fixture cannot come from `POST /api/admin/users`.
  - And with the flag ON, an unverified user can never acquire a
    credential to test with: the OPAQUE register endpoints sit behind
    `auth_middleware`, app passwords are minted from a session, and
    every login door now refuses them. The only reachable state is a
    credential issued BEFORE the flag was turned on.

  **Covered** — `tests/api/auth_email_verified_required.hurl`, green, in
  its own server pass (`run.sh` phase 8).

  This section previously said it was not worth building, on the
  reasoning that it needed a second server with its own config file. Two
  parts of that were wrong. `--config` does NOT beat the environment —
  `load_config_file` uses `dotenvy::from_filename`, and the precedence is
  defaults < file < environment — so no second file is needed; and the
  pass costs about twenty lines because `run.sh` already restarts nothing
  else and can reuse the same postgres.

  The override is a per-command env prefix at the launch line:

  ```sh
  OXICLOUD_REQUIRE_VERIFIED_EMAIL=true "$OXICLOUD_BIN" --config server.env &
  ```

  Explicitly NOT a blanket passthrough of the caller's `OXICLOUD_*`,
  which was the first design and would have let a developer's exported
  `OXICLOUD_DATABASE_URL` or `OXICLOUD_STORAGE_PATH` aim a test run at
  their sandbox. An explicit prefix reads nothing ambient, and `--config`
  keeps suppressing the repo-root `.env` probe.

  The scenario asserts the refusal AND both exemptions, since a policy
  that refused everyone would satisfy the refusal alone: the Owner logs
  in (first, because without it an operator enabling the flag would have
  bricked the instance), an admin-created account logs in (verified by
  fiat), a self-registered one is refused with `EmailNotVerified`, and a
  magic link then makes the same password login work.

  One product observation came out of writing it, recorded in the file:
  the refused login auto-sends a verification link but attaches no
  binding cookie, so that link is always a cross-browser redemption —
  even in the browser that just attempted the login.
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
  NC login v2, refresh, grace replay. This is the pin that the audit
  above would have caught: nine doors, one enforcement.
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

**Discharged.** `auth-model.md` now carries § Phases (with the diagram
and the when-each-runs table), § Admission (the stages, reject-vs-narrow,
and the tightening invariant), § Revocation (both mechanisms and what
they do not reach), the per-door admission matrix under § Login paths,
and the five new events under § Audit events. This plan's copies are now
the stale ones.

Two corrections went in with it, because the page described behaviour
that had since changed: § Anti-enumeration claimed `/api/auth/login`
answers uniformly for a deactivated account, which is true only for a
WRONG password — the Gap-3 pin in `admin_user_ops.hurl` asserts a
distinct `AccountDeactivated` for the right one; and NC Login Flow v2's
303-to-generic-page behaviour was undocumented.
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

### Hurl is mandatory for everything observable at the wire

This is security work, so the acceptance bar is not "a unit test exists".
Every behaviour a client can observe gets a Hurl pin under `tests/api/`,
and per AGENTS.md each pin asserts **the refusal and an allowed caller
succeeding** — a gate that refuses everyone passes a refusal-only test.

Mandatory Hurl coverage, by subject:

| subject | pin |
|---|---|
| the audit matrix inverted | the verified-email half is covered for classic login in its own server pass (`auth_email_verified_required.hurl`); the per-door variants would each need the same pass and are not built |
| admission reject | an inactive account gets no session from **any** path, and the response shape is identical across paths (anti-enumeration) |
| admission narrow | a forced-reset login yields a `{security}`-scoped session: the discharge endpoint works, everything else 403s |
| forced-reset discharge | login → elevate → change → **new** session is full-scope. The deadlock pin |
| refresh re-derivation | narrowed stays narrowed while the obligation stands, widens once discharged, and a device-origin session never widens past its ceiling |
| true revocation | after revoke, the **access token** stops working — not merely the refresh |
| device list + revoke | approve → appears in the list → revoke → access token dead. Three legs, all failing today |
| the two IP columns | approve from one address, use the credential from another, and assert the list shows both distinctly — the mismatch is the signal, so a pin that only checks one column misses the point |
| scope enforcement | `scope-gates.md` § Test plan owns these; they belong to the same suite |

### Where Hurl is the wrong layer

AGENTS.md: "the test goes in the lowest layer that would have failed …
the lower a test lives, the faster it runs and the more precisely it
names the invariant." So do **not** push these up into Hurl, where they
would be slower and vaguer:

- the scope-string parser, delimiters and the reject-unknown rule;
- `require_all` conjunction and the per-axis reduction;
- the `ScopeSet` mask invariant (`rw` implies `ro`);
- `admit()`'s per-condition arms, including the exemptions;
- the subsumption lattice and the DB constraint under it.

Each of those is a pure function. A Hurl test can only observe their
*consequence*, so it would pass for the wrong reason the moment the
plumbing changed.

### Operational notes for whoever writes them

- `tests/api/run.sh` takes selectors; the suite runs against the test
  Postgres on **5433**, which is shared — so a pin that leaks fixtures
  breaks a sibling, and every file needs its teardown.
- Login sets a cookie, so a session captured in one request persists;
  assert the credential being tested rather than inheriting one.
- Do not start the suite casually. It is the shared-database one.

## Implementation order

This document first, and specifically the half of it that has **no scope
dependency** — because that half closes five of the seven live defects
and needs nothing from `scope-gates.md`.

The pipeline splits cleanly:

| part | needs scopes? | contents |
|---|---|---|
| **A** | no | `admit()` with the *reject* arm only, `AdmittedCaller` required by the session mint, true revocation, the connected-device list and its two IP columns |
| **B** | yes | narrowing (forced reset → `{security}`), obligations |

Part A closes defects 3-7: OPAQUE logging in deactivated accounts, email
verification reaching two paths in nine, refresh undoing it, revoke
meaning "cannot renew", and device revocation being unreachable.

Suggested sequence:

1. **Pipeline part A.** Highest live-defect density, contained — one new
   function plus a signature change at five or six call sites. No
   routing restructure, no migration beyond the two IP columns.

   Current state is in § Status at the top — one home for it, so the
   two cannot disagree.

2. **`scope-gates.md` PR 1** — the gates fold. Behaviour-preserving, so
   its acceptance criterion is existing pins green *unchanged*.
3. **`scope-gates.md` PR 2** — credential scopes. First
   non-behaviour-preserving step; carries the lockout risk and needs the
   grandfathering table.
4. **Pipeline part B** — narrowing and obligations, which need the
   `ScopeSet` from step 3.
5. **`scope-gates.md` PR 3** — authentication-by-default. Restructures
   how auth is mounted, so it goes last and alone.

Why not scopes first: every scope decision is reversible in a way an
open account check is not. A wrong ceiling locks a user out and is
visible within minutes; OPAQUE admitting a deactivated account has been
silently true for as long as OPAQUE has existed.
## Non-goals

- Does not change `authz.require` or any grant check — that is the third
  axis, owned by ReBAC.
- Does not implement obligations, nor the terms-acceptance example in
  § Adding a new restriction. Those sections exist so the first
  restricted state is built in a shape the second can reuse.
- Does not add an operator knob. The admission rules are policy, not
  configuration.
