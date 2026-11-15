# Scope gates — one home for principal-kind checks

Branch: `feat/scope-and-session-elevation`. **Trigger: PR #853**
(calendars REST surface) for the gate half — see § When. The credential-
scope half (§ Credential scopes) has its own trigger: the device flow
already collects and displays scopes it does not enforce.

Status: **DRAFT** — amended 2026-11-15 to split Gate from Scope and to
make credential scopes resource-based rather than protocol-based.

## Goal

One definition of "may this KIND of caller reach this surface at all",
callable from both a router layer and a service, replacing five
hand-wired middleware layers that each decide the same class of question
with their own fail-open policy.

```rust
// application/ports/scope_ports.rs
/// Properties of the CALLER. Never client-requestable — § Three axes.
pub enum Gate {
    // ── derived from the USER row, re-read live per request ──
    Internal,                // not is_external
    Admin,                   // privileged role
    PasswordChangeSettled,   // no force_password_change_at_next_login
    // ── derived from the SESSION / credential ──
    NotAnonymous,            // this credential maps to an auth.users row
    DpopBound,               // session carries a DPoP thumbprint
    // NOT a v1 variant — no session-elevation state exists in the
    // schema yet. Designed in `docs/plan/admin-elevation.md` (admin
    // sudo mode: holding the role is not enough to MUTATE), which adds
    // `auth.sessions.elevated_at` and needs `sid` on `CurrentUser`.
    // Listed here so the enum's session-bound half is visibly load-
    // bearing rather than hypothetical — it lands with no reshaping.
    // Elevated,
}

/// What the CREDENTIAL was issued for. Client-requestable, frozen at
/// mint, and a SEPARATE type from `Gate` on purpose — § Three axes.
pub enum Resource {
    Files, Calendar, Contacts, Photos, Music, Profile, Sharing,
}
pub enum Access { ReadOnly, ReadWrite }
pub struct Scope(BTreeMap<Resource, Access>);

pub trait ScopePolicy {
    /// Takes the CALLER CONTEXT, not a bare user id — see § What a scope
    /// is bound to. `CurrentUser` (request extension) carries both the
    /// user id and the session attributes.
    async fn require(&self, gate: Gate, caller: &CurrentUser) -> Result<(), AppError>;
    /// Credential half. `Access::ReadWrite` implies `ReadOnly`.
    async fn require_scope(&self, need: Resource, access: Access, caller: &CurrentUser)
        -> Result<(), AppError>;
}
```

Three consumers of the one predicate, picked by what the gate protects:

- **`InternalCaller` capability token** — for a top-level create. The
  only deny-by-default form: forgetting it does not compile. See § Why 2.
- **`gate_layer(state, Gate::Internal)`** — for a whole protocol
  subtree (`/webdav`, `/caldav`, `/carddav`, `/api/admin/*`), where the
  denial is the surface, not one method.
- **`InternalUser` / `AdminUser` extractors** — convenience on a REST
  handler that has no service method of its own to protect. Weakest of
  the three (forgetting = open), so prefer the token wherever ownership
  is stamped in a service.

## What a scope is bound to

**The session, not the user** — in the OAuth sense that a scope rides on
the credential presented, not on the person. Precisely: a scope is a
property of the **(session, user) pair**, i.e. what THIS credential may
do = what the session was issued for ∩ what the user is currently
allowed. Both halves are load-bearing:

- **Session half.** An app password is a narrower credential than a web
  session for the same user; an anonymous share session may map to no
  user row at all, or (self-grant) to a real one; DPoP binding is per
  session; `elevated_via_magic_link_at` means the same user holds a
  more-privileged session after a re-auth. If scope were user-bound,
  every one of these collapses — an app password would inherit the
  user's full web capability.
- **User half, read LIVE.** A token minted before an account was demoted
  (or deactivated, or upgraded) carries stale attributes. Caching
  `is_external` on the session at mint time is the trap: an invited guest
  who completes `POST /api/auth/upgrade-to-internal` would stay locked
  out of internal-only surfaces until they logged out and back in —
  precisely the dead end `is_upgradable` exists to prevent. So
  user-derived variants re-read per request, exactly as
  `require_internal_user` → `get_user_flags` does today, and as
  `decide_live_role` does for roles.

Hence `require(scope, caller: &CurrentUser)` rather than
`require(scope, caller_id: Uuid)`: a bare `Uuid` cannot express the
session half, and a signature that only takes one quietly forces every
scope to be user-bound. `CurrentUser` is already in request extensions
and already carries the session attributes.

**What `CurrentUser` carries today** (`application/dtos/user_dto.rs:621`):
`id`, `username`, `email`, `role` (**from the JWT claim**, hence
stale-prone, hence `decide_live_role`), and `dpop_jkt` — the one genuine
session attribute, threaded from `cnf.jkt` specifically to avoid a
session-row lookup on the hot path (`docs/plan/dpop.md` Gate 9).

The session id is **not on `CurrentUser`**, but it does exist one layer
down: `JwtClaims.sid` (RFC 8417 §4.1) carries `auth.sessions.id`
precisely so the auth middleware can act per-session without a DB round
trip. So surfacing it onto `CurrentUser` is a thread-through, not a new
concept — and `Gate::Elevated` needs it anyway
(`docs/plan/admin-elevation.md`). Left out of v1 only to keep the two
changes separable.

Follow the precedent already in that file: `CurrentUser::role`'s typed
view resolves an unrecognised value to `Anonymous` — "the *least*
privileged answer". Same posture here; an undetermined scope denies.

**Token freshness.** A token is only as fresh as the request that minted
it. Fine, because it is created and consumed inside one request — but it
must never be cached across requests or stored on a service, or it
becomes the stale-claims bug with a type wrapper.

## Three axes

Added 2026-11-15. This file originally had one `Scope` enum holding both
a caller property (`Internal`, `Admin`) and, implicitly, the idea of a
credential restriction. Separating them is not tidiness — merging them
is a privilege-escalation bug waiting for an author who parses a
client-supplied scope string into the enum and finds `Admin` in it.

| axis | question | source | client may request? |
|---|---|---|---|
| **Gate** | who is this caller? | `auth.users` + session, read live | **never** |
| **Scope** | what was this credential issued for? | frozen on the credential | yes |
| **Grant** | may they touch this object? | `storage.role_grants` | n/a |

Effective permission is the **intersection**. A scope never grants
beyond what the user already has; it only narrows. An admin's app
password scoped `files:ro` administers nothing — not because the scope
says "not admin", but because `/api/admin` requires the `Admin` gate and
no scope string can supply it.

Consequences, both agreed:

- **`/api/admin/*` is a role check, not a scope.** `Gate::Admin`, read
  live. It never appears in the scope vocabulary, so `scope=admin` has
  nothing to parse into.
- **`/api/dedup` is no longer admin.** `check_hash`,
  `check_hashes_batch` and `get_blob` are upload-path helpers; `/stats`
  and `/recalculate` moved to `/api/admin/dedup/*`. So `/api/dedup` sits
  under the `Files` resource like the rest of the upload flow.

## Credential scopes are resources, not protocols

### The feature is half-built and the missing half is the enforcement

`auth.device_codes.scopes` has existed since the initial schema
(`VARCHAR(512) NOT NULL DEFAULT 'webdav,caldav,carddav'`). The device
flow collects it, persists it, and the verify screen shows it to the
user for consent. **Nothing reads it.** The only Rust references to
`scopes` are OIDC provider config, and `auth.sessions` has no scope
column at all.

So today a client requests `scope=webdav`, the user approves "webdav",
and the issued token reaches every surface on the server. That is a
consent screen making a promise the code does not keep, and it is the
real motivation for this work — stronger than the five-duplicated-layers
argument above.

### Why resource and not protocol

The tempting vocabulary is the protocol list: `webdav`, `caldav`,
`carddav`, `api`, `nextcloud`. Rejected, because in this codebase every
door maps to exactly one resource:

| door | resource |
|---|---|
| `/webdav`, `/remote.php/dav/files`, `/wopi`, `/api/files`, `/folders`, `/uploads`, `/batch`, `/trash`, `/dedup` | `Files` |
| `/caldav`, `/api/calendars` (#853) | `Calendar` |
| `/carddav`, `/api/address-books`, `/api/contacts` | `Contacts` |
| `/api/photos`, `/api/people` | `Photos` |
| `/api/playlists` | `Music` |
| `/api/users` (self), `/remote.php/dav/avatars` | `Profile` |
| `/api/grants`, `/api/shares`, `/api/s`, OCS sharing | `Sharing` |

Note `/remote.php/dav` here serves `files/` and `avatars/` only — unlike
real NextCloud it does not carry calendars or contacts, so even the
compat surface stays 1:1.

Four reasons:

1. **PR #853 settles it.** Once calendars are reachable over REST, a
   `caldav` scope guards one of two doors to the same data. `Calendar`
   guards both, including doors nobody has built yet.
2. **Consent screens name data, not plumbing.** "DAVx⁵ wants access to
   your calendars and contacts" is a sentence a user can judge; "wants
   caldav" is not.
3. **No cross-product.** 7 protocols × 2 access levels as distinct
   variants is 14 and grows; 7 resources plus an access level is 7 + 2.
4. It is what Google (`calendar.readonly`) and Microsoft
   (`Calendars.Read`) do. Neither ships a protocol scope.

What this gives up is "calendars via CalDAV but not via REST". That
buys nothing: same data, same rights, and whoever holds the credential
can curl either door. A protocol restriction would be a speed bump, not
a boundary.

### Read-only is multiplied, not orthogonal

A global `readonly` flag cannot express asymmetry — "sync my files
read-write but my calendars read-only" is an ordinary ask and
`files,calendar,readonly` has no way to say it. Every major provider
multiplies instead (`drive.readonly`, `Files.ReadWrite`).

Multiply on the wire, not in the type:

```
files:rw,calendar:ro,contacts:ro
```

parsed into `BTreeMap<Resource, Access>`. Unqualified means `:rw`, so
the existing `device_codes.scopes = 'webdav,caldav,carddav'` rows keep
their present meaning and need no data migration — only a protocol→
resource rename.

**`Access::ReadOnly` is defined per surface, not by HTTP verb.** For DAV
it is clean: allow `GET/HEAD/OPTIONS/PROPFIND/REPORT`, deny
`PUT/DELETE/MKCOL/MKCALENDAR/MOVE/COPY/PROPPATCH/LOCK`. For REST the verb
is *not* a proxy for the effect — `POST /api/search` and
`POST /api/batch/files/get` are reads. A verb heuristic there would
silently permit writes, so v1 either classifies REST routes explicitly
or rejects `:ro` on REST-only resources as unsupported. Shipping the
heuristic is the one outcome to avoid.

### Freezing at mint is correct here

§ What a scope is bound to argues user-derived attributes must be
re-read live, never cached on the token. That rule does **not** extend
to credential scopes, and the distinction matters or someone will later
"fix" this into a per-request query on every WebDAV call:

- `is_external` must be live because a guest who upgrades would
  otherwise stay locked out until re-login — the attribute changes under
  a credential that keeps living.
- A scope cannot change under its credential. Narrowing is the entire
  point and there is no scope-upgrade path, so there is nothing to go
  stale against. Revocation is already handled by killing the session.

So scopes ride in the JWT claim, and the DAV doors stay free of a
lookup.

### Defaults

- **App password**: `files,calendar,contacts,profile` (all `:rw`).
  Not `files` alone — `app_password_handler` describes these as
  credentials for "DAV / NC-Basic clients (DAVx⁵, Thunderbird, rclone,
  NC desktop)", and the NC desktop client needs `/remote.php/dav/files`
  plus avatars, while DAVx⁵ needs calendars and contacts.
- **Device flow**: whatever the client asks for, intersected with the
  above. Existing rows default to `webdav,caldav,carddav`, which maps to
  `files,calendar,contacts` — note it omits `profile`, so enabling
  enforcement must widen existing rows rather than only adding a check.
- **Interactive web session**: all resources, `:rw`.

### Never in a client-requestable scope

Credential management — `/api/auth/app-passwords`,
`/api/auth/device/devices`, session revocation — must be reachable only
from an interactive session, never from a scoped credential.

This is the line the whole feature rests on. A credential that can mint
another credential can mint an unscoped one, and every restriction above
evaporates in one request. It is not a resource to be scoped `:ro`; it
is excluded from the vocabulary entirely, like `Admin`.

## Why

### Grant vs scope (AGENTS.md § Authorization)

`authz.require` answers "may this caller act on THIS resource". It
cannot answer the scope question — there is no resource to name. The two
are separate axes and the enum above must stay closed over **properties
of the caller alone**.

**`Owner` must NOT be a `Scope` variant.** It is a `Role::Owner` row in
`storage.role_grants` against a specific resource, i.e. a grant.
`scope.require(Owner)` has nowhere to put the resource id: either the
signature grows one and it is `authz.require` under a new name, or it
silently means "owner of *something*", which is not a check. Same for
`Editor` / `Viewer` and every `DriveRole`. If a proposed variant needs a
resource id, it belongs in `AuthorizationEngine`, not here.

### What this actually fixes

Not callsite brevity. The hole that motivated this
(`create_address_book` / `create_playlist` reachable by externals over
REST while the CardDAV door to the same service method was gated) was
not "the gate was hard to call" — it was "nobody noticed there was
none". A layer you must remember to add fails exactly the way a check
you must remember to write does. So the value is two narrower things:

1. **One predicate, two callers.** Same argument as
   `application/services/registration_policy.rs`: the domain allowlist
   was a hand-copied second implementation, and copies drift. Today's
   layers already disagree on policy — `decide_internal_user`
   (`interfaces/middleware/user.rs`) treats a `NotFound` as denial but a
   DB blip as pass-through, deliberately and with a long comment
   explaining why. That decision deserves one reviewable home, not five
   copies that each re-derive it.
2. **Make forgetting impossible, not merely visible.** An extractor
   (`caller: InternalUser`) only makes the gate *visible* — forget it
   and the route is open, which is the same failure mode in nicer
   clothes. Deny-by-default means the omission must not compile.

   Mint a capability token that only the policy can produce, and demand
   it where ownership is stamped:

   ```rust
   // application/ports/scope_ports.rs — field is private to this module,
   // so NOTHING outside can construct one without going through require().
   pub struct InternalCaller {
       user_id: Uuid,
   }

   impl InternalCaller {
       pub fn user_id(&self) -> Uuid { self.user_id }
   }

   impl dyn ScopePolicy {
       pub async fn require_internal(&self, caller: &CurrentUser)
           -> Result<InternalCaller, AppError>;
   }
   ```

   ```rust
   // The create methods stop taking a bare Uuid.
   async fn create_calendar(&self, dto: CreateCalendarDto, caller: InternalCaller)
   async fn create_address_book(&self, dto: CreateAddressBookDto, caller: InternalCaller)
   async fn create_playlist(&self, dto: CreatePlaylistDto, caller: InternalCaller)
   ```

   An implementor who forgets the check cannot call these at all — there
   is no way to obtain an `InternalCaller` except from `require_internal`,
   and `auth_user.id` no longer type-checks. That is the AGENTS.md axis
   taken literally: a guarantee the type system enforces beats one a
   reviewer must remember.

   The gain over an extractor is specifically that it holds for a door
   nobody anticipated. CalDAV `MKCALENDAR` and `POST /api/calendars`
   reach the same service method; a per-route gate protects whichever
   doors someone thought of, while the token protects the method itself.

## Scope of the change

### Fold in (the five existing layers)

| Today | Becomes |
|---|---|
| `require_internal_user_layer` | `gate_layer(Gate::Internal)` |
| admin middleware on `/api/admin/*` | `gate_layer(Gate::Admin)` |
| `require_no_password_change_pending_layer` | `gate_layer(Gate::PasswordChangeSettled)` |
| `require_dpop_layer` | `gate_layer(Gate::DpopBound)` |
| `anonymous_allowlist_layer` | `gate_layer(Gate::NotAnonymous)` + keeps its allowlist |

`anonymous_allowlist_layer` is the loose fit — it carries a route
allowlist and must stay `route_layer` (it needs `MatchedPath`). Fold the
*decision* and leave the allowlist where it is.

### Keep as layers, not extractors

`/webdav`, `/caldav`, `/carddav` dispatch through one `any` handler, so
a layer is both natural and cheaper there. Extractors are for REST
handlers only.

### Per-route gates to retire

- `routes.rs` — the two `contacts_create` / `music_create` sub-routers
  (added with the external-user fix) collapse into an extractor on the
  handler.
- `calendar_service.rs` — the `TODO(#853)` on `create_calendar`
  resolves by *deleting* it, not by adding copy number three.

## Lookup cost

`require_internal_user` calls `auth.get_user_flags(caller_id).await` —
a DB lookup per request. The live-read decision above means that cost is
**deliberate and stays**: putting `is_external` on the JWT claims to
make the check free is the one thing § What a scope is bound to rules
out, because an upgraded guest would then stay locked out until
re-login.

So the rule is placement, not caching:

- Ownerless creates and the DAV / admin subtrees are **cold** paths. One
  lookup there is free in practice. Keep the token and the layers there.
- Do **not** spread scope checks onto the hot paths — listing,
  thumbnails, auth (AGENTS.md § Performance). Those are grant questions
  anyway, answered by `authz.require` against a resource, and a scope
  gate on them would be both a per-request query and the wrong check.

If a scope check ever does become necessary on a hot path, the answer is
a short-TTL cache in the policy keyed by user id — not a claim on the
token. A cache can be invalidated on upgrade/demotion; a minted token
cannot.

## Rejected: default-deny across all of `/api`

The tempting reading of "secure by default" is
`gate_layer(Gate::Internal)` on the whole protected `/api` router plus
an allowlist of what externals may reach. Rejected, for a reason worth
recording so it isn't re-proposed:

That shape works for `anonymous_allowlist_layer` because an anonymous
share session's legitimate surface is **tiny** — a handful of routes.
Externals are the opposite: acting on resources shared with them is the
entire point, so they legitimately use most of the files / folders /
grants / search API, each call already guarded per-resource by
`authz.require`. The exemption list would therefore cover a large
fraction of `/api`, and a long allowlist is itself the hole-prone
artifact — plus it fails constantly in the annoying direction (guests
403'd on legitimate surfaces until someone notices).

Deny-by-default is right; the unit it applies to is the **ownerless
create**, not the HTTP surface. Hence the token.

## Non-goals

- Not a general policy engine. Target is ~10k users and five variants;
  a rules DSL is million-user machinery for a load that will not arrive.
- Does not touch `authz.require` / `_with_perms` or any grant check.
- Does not change any status code or audit `event` / `reason` key —
  operators key off those (`authz.external_user_blocked`,
  `authz.denied`). A refactor that renames them is a breaking change to
  log aggregators, so the audit lines move verbatim.

## Test plan

- Unit: the decision half (`decide_scope`) per variant, including the
  `NotFound`-denies / DB-blip-passes asymmetry that
  `decide_internal_user` documents today. Pure, no live auth service —
  same split as `decide_internal_user` / `decide_live_role`.
- Hurl: `tests/api/external_users.hurl` already pins 11h–11p (DAV
  surfaces, both REST creates, the system address book). Those pins must
  stay green **unchanged** through the refactor — that is the acceptance
  criterion. Per AGENTS.md each pin asserts the refusal AND an allowed
  caller succeeding, so a gate that refuses everyone cannot pass.
- No new Hurl file: if the refactor is behaviour-preserving, the
  existing pins are the regression suite.

The credential-scope half needs its own pins, because unlike the gate
refactor it is **not** behaviour-preserving — it turns an unenforced
field into an enforced one:

- Unit: the `protocol:access` parser. Unqualified → `ReadWrite`; the
  legacy `webdav,caldav,carddav` string → `{Files, Calendar, Contacts}`
  all `:rw`; an unknown resource name is **rejected**, not ignored
  (a typo'd scope must not silently widen).
- Unit: `Access::ReadWrite` implies `ReadOnly`, and the DAV safe-method
  split — `PROPFIND` passes `:ro`, `PROPPATCH` does not.
- Hurl, new file: a credential scoped `files:rw` is refused on
  `/caldav` and `/carddav` and accepted on `/webdav`; one scoped
  `files:ro` passes `PROPFIND` and is refused `PUT`. Each pin asserts
  the refusal AND an allowed caller succeeding, per AGENTS.md, so a
  gate that refuses everyone cannot pass.
- Hurl: the escalation pin — a scoped credential is refused on
  `POST /api/auth/app-passwords`. This is the one that matters most; if
  it ever goes green-by-passing, the whole feature is decorative.
- Migration: existing `device_codes` rows keep working. Pin the legacy
  default string explicitly, since enforcement is what turns a
  previously-inert column into a lockout risk.

## When

At PR #853, which adds `POST /api/calendars` and the events surface —
the third hand-written copy of the per-route gate. Three is where the
duplication is real and the abstraction pays. Before that, this is
building the frame before there is a third wall.
