# Scope gates — one home for principal-kind checks

Branch: `feat/scope-and-session-elevation`. **Trigger: PR #853**
(calendars REST surface) for the gate half — see § When. The credential-
scope half (§ Credential scopes) has its own trigger: the device flow
already collects and displays scopes it does not enforce.

Status: **DRAFT** — amended 2026-11-15 to split Gate from Scope and to
make credential scopes resource-based rather than protocol-based, and to
split `account` from `security` the way Google separates those tabs. The
design questions raised in review are all resolved: no row in the
scope→route table is unassigned, and the two judgement calls
(`/remote.php/dav/avatars` under `account`, `/api/drives/{id}/members`
reachable with `files:rw`) are ratified with their reasoning recorded
rather than left open. Awaiting approval to implement.

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
    Admin,                   // role.at_least(Admin) — Owner satisfies it too
    Owner,                   // role.at_least(Owner): the instance owner
                             // (UserRole::Owner, NOT storage.grant_role)
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
///
/// NOT named `Resource`: `domain::services::authorization::Resource`
/// already owns that name for the GRANT axis, and its variants carry an
/// id — `Resource::Calendar(Uuid)` is *one* calendar, whereas
/// `Scope::Calendar` is the calendar *area*. Same word at two
/// granularities is exactly the ambiguity this plan exists to remove,
/// so the scope axis keeps the `Scope` name and the grant axis keeps
/// `Resource`.
pub enum Scope {
    Files, Calendar, Contacts, Photos, Music, Sharing,
    /// Profile and preferences — Google's "Account" tab.
    Account,
    /// Password, linked identities, credentials — Google's "Security"
    /// tab. In the vocabulary, but absent from every non-interactive
    /// credential's ceiling. § `security`.
    Security,
    // Reserved — parser REJECTS it until a mailbox exists. § `mail` is reserved.
    // Mail,
}
pub enum Access { ReadOnly, ReadWrite }

/// The set a credential carries. `Scope` is one entry, `ScopeSet` is the
/// whole grant — keeping them distinct avoids "scope" meaning both.
pub struct ScopeSet(BTreeMap<Scope, Access>);

pub trait ScopePolicy {
    /// Takes the CALLER CONTEXT, not a bare user id — see § What a scope
    /// is bound to. `CurrentUser` (request extension) carries both the
    /// user id and the session attributes.
    async fn require(&self, gate: Gate, caller: &CurrentUser) -> Result<(), AppError>;
    /// Conjunction. Deliberately NOT `Gate` as a bitflags set — see
    /// § The array is AND: a `|` reading admits an elevated non-admin
    /// to admin mutations.
    async fn require_all(&self, gates: &[Gate], caller: &CurrentUser)
        -> Result<(), AppError>;
    /// Credential half. `Access::ReadWrite` implies `ReadOnly`.
    async fn require_scope(&self, need: Scope, access: Access, caller: &CurrentUser)
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

### Worked example: sharing a file

All three axes, on one request:

| axis | check | where |
|---|---|---|
| Gate | `Gate::NotAnonymous` — enforced today by `anonymous_allowlist_layer` | router layer |
| Scope | `oxicloud:sharing:rw` | router layer / extractor |
| Grant | `authz.require(Subject::User(caller), Permission::Share, resource)` — owners short-circuit | `grant_handler.rs` |

The gate is `NotAnonymous`, **not** `Internal` — and the difference is
easy to blur. An external user has an `auth.users` row and legitimately
re-shares what was shared with them (§ Rejected: default-deny). An
anonymous public-link visitor has no user row at all: `UserRole::Anonymous`
"is never stored in `auth.users`… it exists only on a session and its
JWT". So there is no subject to record as `granted_by`, and permitting it
would turn a read-only link into an access-granting capability — anyone
holding the URL could mint persistent grants for third parties.

That is already refused twice over today: `/api/grants` is not on the
`anonymous_allowlist` (a 7-route, GET-only list covering folder and file
reads, thumbnails and metadata — deny-by-default outside it), and the
layer rejects non-GET regardless. The allowlist's own tests assert the
refusal for neighbours like `/api/users/{id}`, `/api/groups/search` and
`/api/auth/app-passwords`.

So this row folds in rather than appearing: per § Fold in,
`anonymous_allowlist_layer` becomes `gate_layer(Gate::NotAnonymous)`
and keeps its allowlist.

Neither the scope nor the grant substitutes for the other, and that is the
design working rather than redundancy:

- `sharing:rw` on a file the caller cannot `Share` → denied by the
  grant. The credential is allowed to share *in principle*; this file
  is not theirs to share.
- `Permission::Share` held on the file, but presented by a credential
  without `sharing` in its set → denied by the scope. The person may
  share; this *key* was not issued for it. An rclone app password
  scoped `files:rw` cannot hand your documents to a stranger even
  though the human behind it could do so from the browser.

Revocation is the same shape with a wider grant arm: the caller passes
if they are the original granter OR hold `Permission::Share`
(`grant_handler.rs` ~551), still gated by `sharing:rw`.

Reading who a resource is shared with needs only `sharing:ro`, which is
why recipient lookup sits in this scope too — see § Three scopes sound
alike.

**Note for the implementor:** that grant check lives in the *handler*,
not a service, which deviates from AGENTS.md § Authorization. It is
pre-existing and § Non-goals keeps grant checks out of this change — but
it means `/api/grants` will carry a scope check and a grant check side
by side in the same handler. Do not "tidy" that into one call; they are
different axes answering different questions.

## Credential scopes name data areas, not protocols

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

### Why an area and not a protocol

The tempting vocabulary is the protocol list: `webdav`, `caldav`,
`carddav`, `api`, `nextcloud`. Rejected, because in this codebase every
door maps to exactly one area:

| scope (shown without the `oxicloud:` prefix) | routes it opens (non-exhaustive) |
|---|---|
| `files` | `/webdav`, `/remote.php/dav/files`, `/wopi`, `/api/files`, `/api/folders`, `/api/uploads`, `/api/batch`, `/api/trash`, `/api/favorites`, `/api/recent`, `/api/dedup`, `/api/drives` |
| `calendar` | `/caldav`, `/.well-known/caldav`, `/api/calendars` (#853) |
| `contacts` | `/carddav`, `/.well-known/carddav`, `/api/address-books`, `/api/contacts` |
| `photos` | `/api/photos`, `/api/photos/geo` (Places), `/api/people` (faces) — and `/api/places` if that view is ever promoted to its own nest |
| `music` | `/api/playlists` |
| `account` | `/api/auth/me`, `/api/auth/me/profile`, `/api/auth/me/image`, `/api/notifications`, `/remote.php/dav/avatars` — profile and preferences |
| `security` | `/api/auth/change-password`, `/api/auth/oidc/link/start`, `/api/auth/oidc/unlink`, `/api/auth/upgrade-to-internal`, `/api/auth/opaque/register/start`, `/api/auth/opaque/register/finish`, `/api/auth/app-passwords`, `/api/auth/device/devices`, `/api/auth/device/verify` — and 2FA when it lands. **Never in a non-interactive ceiling; mutations also need `Gate::Elevated`** |
| `sharing` | `/api/grants`, `/api/shares`, `/api/s`, OCS sharing, and recipient lookup: `/api/users/{id}`, `/api/groups/search` |
| `mail` | **reserved — see below** |

**Three scopes sound alike and are not.** Getting this wrong is what
made an earlier draft of the table incoherent — user lookup sat under
`account`, group lookup under `sharing`, and the share dialog therefore
needed two unrelated scopes to populate one recipient picker:

- **`contacts`** — the caller's OWN address books: CardDAV collections
  and the vCards in them. Nothing to do with the instance's user
  roster. A `contacts:rw` credential must not be able to enumerate who
  else has an account here.
- **`account`** — the caller's OWN profile and settings:
  `/api/auth/me`, `/me/profile`, `/me/image`. Note there is no
  self-profile route under `/api/users` at all; that nest holds exactly
  one route and it looks up *other* people.

  `/remote.php/dav/avatars` sits here too, and it is the one row that
  does not fit the "own data" rule cleanly: the NC endpoint serves ANY
  user's avatar by username, so it is directory data by the argument
  that puts `/api/users/{id}` under `sharing`. Kept under `account`
  deliberately (ratified 2026-11-15): the NC desktop client fetches its
  own avatar through the same path, an avatar is the least sensitive
  thing the directory exposes, and splitting one route across two scopes
  costs more explanation than it buys. Revisit only if avatars ever
  carry something beyond an image.
- **`sharing`** — grants and shares, **plus recipient lookup**.
  `GET /api/users/{id}` returns a `PublicUserDto` for a target user
  "iff the authenticated caller has a legitimate relationship with
  them", rate-limited 60/min specifically so a stale JWT cannot iterate
  UUIDs; `/api/groups/search` exists "so the share dialog can offer
  groups as recipients". Both are the same capability — finding someone
  to share with — so both live here.

That last grouping is the load-bearing one: a credential that may create
a grant must be able to find recipients, and a credential that may not
share has no business enumerating the directory. Splitting users from
groups across two scopes would have broken the picker while protecting
nothing, since either half alone still reveals the roster.

`photos` covers Places and People because both are **derived from the
photos themselves** — place data from EXIF coordinates, faces from
pixel analysis. A credential that can read a photo can already recover
where it was taken and who is in it, so a separate scope would protect
nothing while implying it did. That is the same test applied to
`/api/search` and `/api/rt/ws`: a scope earns its place only when
withholding it actually withholds something.

Non-exhaustive on purpose: the mapping is "which resource does this
door touch", so a new route under an existing area inherits its scope
without editing this table. A new route that fits *no* row is the
signal that a scope is missing — and that is a design decision, not a
lookup.

Deliberately absent, and each for its own reason:

- `/api/admin/*`, `/api/admin/dedup/*` — `Gate::Admin`, never a scope.
- `/api/auth/app-passwords`, `/api/auth/device/devices` — credential
  management, excluded from the vocabulary entirely (see below).
- `/api/config`, `/api/i18n`, `/status.php`, `/api/auth/*` login flows —
  public or pre-authentication; no credential exists yet to scope.
- `/api/search` — reads across areas, so it is filtered by the scopes
  the caller already holds rather than carrying one of its own. A
  `files:ro`-only credential searching must not see calendar hits.
- `/api/rt/ws` — the message bus delivers events for whatever the
  caller can already see; same filtering argument as search.
- `/api/groups` mutations — **admin, not a scope.** The handler doc is
  explicit: "All mutating endpoints require admin role". Only
  `/api/groups/search` is scope-reachable, and it exists "so the share
  dialog can offer groups as recipients" — which is why it sits under
  `sharing` rather than `contacts`. These are ReBAC subject groups used
  as grant recipients, not address-book contact groups; the name is the
  only thing they share.

One consequence of `/api/drives` sitting under `files`, recorded because
it is not obvious from the table: the nest includes
`/{id}/members`, so a `files:rw` credential can **add a member to a
shared drive** — i.e. grant another subject access to data. That is a
sharing-shaped action reached through a files-shaped scope.

Accepted deliberately (ratified 2026-11-15): a drive *is* a file container, splitting one nest
across two scopes costs more in explanation than it buys, and the
member change is still bounded by `authz.require` on the drive itself —
so the caller must already hold rights there. Revisit if drive
membership ever becomes reachable by a credential that could not already
share the same content through `/api/grants`.

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

### `mail` is reserved, not implemented

There is no mailbox in OxiCloud today. SMTP is outbound only — the
`email_sender` port, magic-link delivery, job notification sinks — so
nothing a `mail` scope could gate currently exists.

It is reserved anyway, for the same reason the `Vault` drive kind is:
the name is the cheap part, and claiming it now costs nothing while
letting someone else claim it later costs a migration. Scope strings
are persisted on credentials and displayed on consent screens, so the
vocabulary is a wire format — renaming an entry means re-issuing every
credential that carries it.

Reserved means: the parser **rejects** `mail` as an unknown resource
until the feature lands, exactly as it rejects a typo. It does not mean
the parser accepts it and the gate ignores it — that is precisely the
shape of the device-flow bug this plan exists to fix, where a scope was
collected, consented to, and silently unenforced. A name that parses
but gates nothing is worse than a name that does not parse.

### Read-only is multiplied, not orthogonal

A global `readonly` flag cannot express asymmetry — "sync my files
read-write but my calendars read-only" is an ordinary ask and
`files,calendar,readonly` has no way to say it. Every major provider
multiplies instead (`drive.readonly`, `Files.ReadWrite`).

Multiply on the wire, not in the type:

```
files:rw,calendar:ro,contacts:ro
```

parsed into a `ScopeSet` (`BTreeMap<Scope, Access>`). Unqualified means `:rw`, so
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

### Wire format: namespaced, both delimiters

OxiCloud resource scopes are **prefixed `oxicloud:`**:

```
oxicloud:files:rw oxicloud:calendar:ro
```

The prefix is not decoration. `POST /api/auth/device/authorize`
implements RFC 8628, which inherits RFC 6749's `scope` parameter — so
OxiCloud is **already an OAuth 2.0 authorization server** for that
grant, and its scope values already live in the OAuth scope namespace.
`openid`, `profile`, `email`, `address`, `phone` and `offline_access`
are reserved there (OIDC Core §5.4), and OxiCloud already *requests*
`"openid profile email"` as an OIDC client (`config.oidc.scopes`).

An unprefixed `profile` scope would therefore have collided head-on:
same word, opposite semantics — OIDC `profile` delivers read-only
identity claims, ours would grant *write* access to `/api/users`. Worse,
a client sending the textbook `scope=openid profile email` would hit the
reject-unknown rule as: `openid` rejected, `email` rejected, `profile`
**accepted with the wrong meaning**. A partial parse that silently
mis-resolves the one token that matched is worse than a clean failure.

Prefixing removes OxiCloud's vocabulary from the reserved space
permanently, so a mixed request stays unambiguous
(`openid profile email oxicloud:files:rw`) and future OIDC evolution
cannot collide with it. Accordingly the resource formerly called
`profile` is **`account`** — unreserved even unprefixed, so the name is
safe in logs, docs and UI copy where the prefix is dropped.

A token without the `oxicloud:` prefix is not an OxiCloud resource and
is **rejected**, until and unless OxiCloud becomes an OIDC provider and
a separate layer claims the standard names.

**Delimiters: accept space AND comma.** RFC 6749 §3.3 specifies
space-delimited; `DeviceAuthorizeRequestDto` documents and parses
comma-separated (`"webdav,caldav,carddav"`). That is a live conformance
bug — a conformant client sending `scope=webdav caldav` yields one token
`"webdav caldav"` — currently invisible only because nothing reads the
field. Splitting on both fixes conformance without breaking existing
rows or any client already sending commas.

### Defaults

**Access defaults to `:rw` when unqualified.** Forced rather than
chosen: existing `device_codes.scopes` rows carry no qualifier and those
credentials have full write access today, so defaulting to `:ro` would
retroactively break every sync client in the wild. It also matches
convention — Google's bare `drive` is full access, `drive.readonly` is
the explicit narrowing.

The cost is consent clarity: a user approving "files" may not read that
as including deletion. That belongs in the UI, not the parser — **the
consent screen renders the resolved access** ("Files — read and write"),
never the raw token.

**An omitted scope does NOT mean every scope.** That is the fail-open
shape, and it is the bug this section exists to fix: scope ignored ⇒
full access. RFC 6749 §3.3 requires an omitted `scope` to resolve to a
*pre-defined* default or fail the request, and OxiCloud already has one
— serde's `default_scopes()` supplies `webdav,caldav,carddav` when the
field is absent, so today's documented behaviour is already a subset.

So the rule is per credential type, with every grant bounded by that
type's ceiling:

```
effective = (requested ?? type_default) ∩ type_maximum
```

Both halves bounded, so neither an omitted scope nor a greedy request
can escalate.

The ceiling is an **allowlist**, not a denylist — a credential type gets
only what its row names. A new type added without thought about
`security` therefore gets it withheld.

| credential | default when omitted | maximum (allowlist) |
|---|---|---|
| App password | `files,calendar,contacts,account` (`:rw`) | same — **no `security`** |
| Device flow | `files,calendar,contacts` (`:rw`) — today's `webdav,caldav,carddav`, namespaced | `files,calendar,contacts,account` — **no `security`** |
| Interactive web session | n/a — never requests scopes | every scope `:rw`, `security` included |

App password is not `files` alone: `app_password_handler` describes
these as credentials for "DAV / NC-Basic clients (DAVx⁵, Thunderbird,
rclone, NC desktop)", and NC desktop needs `/remote.php/dav/files` plus
avatars while DAVx⁵ needs calendars and contacts.

The device-flow default omits `account`, so enabling enforcement must
**widen existing rows** rather than only adding a check — otherwise a
device credential that reads avatars today starts failing.

`scope=` present-but-empty is **malformed** and rejected, not treated as
"use the default". Silently defaulting hides a client bug, and a
zero-scope credential would be useless anyway.

### `security`: named, but out of reach

`account` and `security` split the way Google's settings do — one tab for
"your account" (profile, preferences), another for "security" (password,
2FA, connected apps, linked identities). The split is not cosmetic: the
two carry completely different blast radius while sitting under the same
`/api/auth/*` prefix, which is exactly how a prefix-mapping mistake
would hand the dangerous half to an rclone credential.

`security` covers, today:

| route | what it really does |
|---|---|
| `/api/auth/change-password` | replaces the credential everyone else authenticates with |
| `/api/auth/oidc/link/start`, `/oidc/unlink` | adds or removes an identity provider that can log in as this user |
| `/api/auth/upgrade-to-internal` | changes what the principal IS (external → internal) |
| `/api/auth/opaque/register/start`, `/register/finish` | **enrols a new OPAQUE credential for the authenticated account** — session-authenticated because an envelope binds to a user_id |
| `/api/auth/app-passwords` | **mints credentials** |
| `/api/auth/device/devices`, `/device/verify` | lists, revokes and *approves* device credentials |

Plus 2FA/TOTP when it lands — none exists yet — and self-service session
revocation, which also does not exist: `/api/admin/sessions` is
`Gate::Admin`, not `security`.

**Why a named scope rather than an exception list.** An earlier draft
excluded these from the vocabulary entirely. Naming them is better on
three counts:

1. Every route now maps to a scope, so "is anything orphaned?" is
   answerable by reading the table instead of cross-checking an
   exception list — the question that found four of these in the first
   place.
2. Enforcement reuses the ceiling already defined in § Defaults
   (`effective = (requested ?? type_default) ∩ type_maximum`) rather
   than a special case. The ceiling is an **allowlist**, so a credential
   type whose author forgets to think about `security` gets it withheld,
   not granted. Forgetting is safe, which is the opposite of an
   exception list's failure mode.
3. It appears on consent screens if ever requested, instead of failing
   with an unexplained rejection.

**`Gate::Elevated` is the second factor, and this is its first real
consumer.** Holding `security:rw` is necessary and not sufficient: a
mutation also needs a recently re-authenticated session, the admin
sudo-mode state designed in `docs/plan/admin-elevation.md`. Scope answers
"was this key issued for security changes"; elevation answers "did a
human prove themselves in the last few minutes". The commented-out
`Elevated` variant in the enum above stops being hypothetical here.

Until elevation lands, `security` is interactive-session-only by ceiling
alone — correct but weaker, since an XSS-driven request from a live
browser session would pass. That ordering is a deliberate choice, not an
oversight: the ceiling is what keeps a *stolen app password* away from
the password-change endpoint, which is the threat this plan exists for.

**The worst of them is IdP linking**, and it deserves naming: an attacker
who links their own Google account keeps access *after* the credential
they used is revoked. It defeats revocation itself, which is the one
control every other mitigation here falls back on.


### Deliberately scopeless

Not every route takes a credential-scoped decision, and saying so beats
leaving blanks that read as oversights.

**Scopeless is DECLARED, never absent.** This is the distinction the
whole plan turns on: an ungated route and an explicitly-scopeless route
behave identically at runtime and mean opposite things in review.
`/api/auth/logout` is scopeless because someone decided any credential
may put itself beyond use; `create_address_book` was reachable by
externals because nobody decided anything. The second is the bug this
file exists to fix, and it is invisible unless "no scope" has to be
written down.

So the mapping is **exhaustive by construction**, enforced by a test
rather than by review attention:

> Every operation in `resources/gen/openapi.json` must appear either in
> the scope map or in an explicit `SCOPELESS` list. A route in neither
> fails the test, naming the path.

That works because the spec is already machinery we trust: 190 paths and
235 operations today, **all of them `/api`**, regenerated from the
`#[utoipa::path]` attributes and drift-checked by the
`openapi-spec-drift` CI job. A contributor therefore cannot add a REST
route without the spec changing, so the test cannot be bypassed by
forgetting — the same reasoning as the `InternalCaller` token in § Why 2,
applied to the scope axis: make the omission fail, not merely be
visible.

It also converts the orphan question from something a reviewer has to
think to ask into a CI failure. Nine routes were unassigned in an
earlier draft of this file — four of them escalation paths — and they
stayed unassigned until someone asked "are there routes still scope
orphan?". That question should not depend on someone having the idea.

Coverage boundary, stated rather than assumed: the spec contains no
non-`/api` paths, because `/webdav`, `/caldav`, `/carddav`,
`/remote.php` and `/wopi` dispatch through `any` handlers and carry no
utoipa annotations. Those are mapped wholesale per § scope table, so a
second assertion covers them — each mount prefix maps to exactly one
scope, and a new mount in `main.rs` without a mapping fails the same
test.

The current `SCOPELESS` list, each entry with its reason:

- **`/api/auth/dpop/bind`** — binds a proof-of-possession key to the
  CALLING session. It belongs here rather than in `security`, which is
  where an earlier draft put it, and the correction matters twice over:
  - It would have broken the feature. Non-interactive ceilings exclude
    `security`, so requiring it would make a DPoP-bound app password
    **impossible** — denying proof-of-possession to exactly the
    long-lived credentials that most need it.
  - It is safe to leave open. The operation is self-affecting (it cannot
    touch another session or the account), strictly *narrowing* (a bound
    token is harder to replay, not more capable), and **one-shot** —
    rebinding answers `409 "Session already bound"`, so it cannot be
    used to seize a session from a client that already bound.

  A session may therefore be required to carry DPoP while holding no
  right whatever to change the password. Those are different questions:
  `Gate::DpopBound` asks how strongly this credential is held,
  `Scope::Security` asks what it was issued to do.
- **`/api/auth/logout`** — ends the caller's own session. Any credential
  may do that; refusing would leave no way to put a key beyond use.
- **`/api/rt/ticket`** — message-bus ticket issuance, DPoP-gated and
  filtered like `/api/rt/ws`: it delivers events for whatever the caller
  can already see.
- **`/ocs/v1.php|v2.php/cloud/user`** — NC user-metadata probe, reached
  by clients that already hold `account`.
- **`/index.php/204`** — NC connectivity probe.
- **Login and pre-auth flows** — no credential exists yet to carry a
  scope: `/api/auth/login`, `/register`, `/refresh`, `/setup`,
  `/magic-link/send`, the OIDC quartet (`authorize`, `callback`,
  `exchange`, `backchannel-logout`), the OPAQUE **login** handshake
  (`login/lookup`, `login/ke1`, `login/ke3`, `params`) and the RFC 8628
  device pair (`device/authorize`, `device/token`).

  Note the asymmetry inside OPAQUE: *login* is pre-auth and scopeless,
  *registration* is session-authenticated and `security`. Same prefix,
  opposite classification — see the `security` table.
- **`/api/status`** — server status probe.
- **Ops surface** — `/health`, `/ready`, `/metrics`, `/version`,
  `/openapi.json`.

## Why

### Grant vs scope (AGENTS.md § Authorization)

`authz.require` answers "may this caller act on THIS resource". It
cannot answer the scope question — there is no resource to name. The two
are separate axes and the enum above must stay closed over **properties
of the caller alone**.

**Two different things are called "owner", and only one is a `Gate`.**

`storage.grant_role = 'owner'` is a row against a specific resource, i.e.
a grant. `scope.require(Owner)` on *that* has nowhere to put the
resource id: either the signature grows one and it is `authz.require`
under a new name, or it silently means "owner of *something*", which is
not a check. Same for `Editor` / `Viewer` and every `DriveRole`. If a
proposed variant needs a resource id, it belongs in
`AuthorizationEngine`, not here.

`UserRole::Owner` is the opposite case and **is** a `Gate`. It is a
column on `auth.users` (widened by
`20261028000000_userrole_add_owner.sql`, "ranked above 'admin'"), a
property of the caller with no resource to name — exactly parallel to
`Admin`. Its live callsite already exists:
`POST /api/admin/transfer-ownership` answers 403 "Only the current owner
may transfer ownership", which becomes `Gate::Owner` instead of a
hand-rolled check.

Two implementation notes, because both are traps:

- **`Gate::Admin` is `role.at_least(Admin)`, not `role == Admin`.** The
  entity doc is explicit that Owner "outranks `Admin`, so it satisfies
  every admin gate without those gates naming it". An implementer who
  writes equality locks the instance owner out of the admin panel.
  `UserRole::at_least` already exists; the gate delegates to it rather
  than re-deriving a comparison.
- **`UserRole::outranks` is NOT a `Gate`.** It is the strict companion,
  used for "may this admin act upon *that* admin" — it takes a target,
  so by the rule above it is a grant question. Do not try to express
  "may demote this user" as a gate.

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

### Fold in (the five existing layers, plus one hand-rolled check)

| Today | Becomes |
|---|---|
| `require_internal_user_layer` | `gate_layer(Gate::Internal)` |
| admin middleware on `/api/admin/*` | `gate_layer(Gate::Admin)` |
| `require_no_password_change_pending_layer` | `gate_layer(Gate::PasswordChangeSettled)` |
| `require_dpop_layer` | `gate_layer(Gate::DpopBound)` |
| hand-rolled owner check in `transfer_ownership` | `Gate::Owner` |
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

## Required: amend `src/AGENTS.md`

This plan does not land without a rule in `src/AGENTS.md`, because the
thing it fixes is not a missing mechanism — it is that nobody was
required to think about the question. `create_address_book` was reachable
by externals with a gate available and unused.

The rule to add, under § AuthZ enforcement points:

> **Every route declares all three axes explicitly. There is no default.**
>
> - **Gate** — which KIND of caller may reach this surface
>   (`Gate::Internal`, `Admin`, `Owner`, `NotAnonymous`, `DpopBound`,
>   `PasswordChangeSettled`). Declared even when the answer is "any
>   authenticated caller".
> - **Scope** — what the CREDENTIAL was issued for
>   (`oxicloud:<area>:<ro|rw>`), or an explicit `SCOPELESS` entry with
>   the reason. "No scope" is a decision that gets written down, never an
>   omission: an ungated route and a declared-scopeless route behave
>   identically at runtime and mean opposite things in review.
> - **Grant** — **delegated to the service, never checked at the route.**
>   Where the route touches a specific resource, it passes `caller_id`
>   into a service method that calls
>   `authz.require(subject, permission, resource)` — the `_with_perms`
>   convention. The route's obligation is to name which service method
>   owns the check, not to perform it: that is the existing rule in
>   AGENTS.md § Authorization, and the reason it exists is that REST and
>   DAV reach the same service methods, so a check at one door is a hole
>   at the other.
>
>   A route with no resource id has no grant to delegate, and inventing
>   one is how `Owner` ends up in a scope enum (§ Grant vs scope).
>
> A route that cannot answer all three does not merge. If the answer to
> any is "I don't know", that is the review, not a blocker to route
> around.

Which layer owns which axis, since the three are enforced in three
different places and conflating them is how the original hole happened:

| axis | enforced in | why there |
|---|---|---|
| Gate | router layer (or extractor) | no resource to name, and the DAV surfaces dispatch through one `any` handler |
| Scope | router layer / extractor, declared in the utoipa `security` array | property of the credential, resource-blind |
| Grant | **application service** (`_with_perms`) | REST and DAV reach the same service methods, so a check at one door is a hole at the other |

The pre-existing deviation stays flagged rather than fixed:
`grant_handler.rs` performs its `Permission::Share` check in the handler.
§ Non-goals keeps grant checks out of this change, so `/api/grants` will
carry a scope check and a handler-level grant check side by side. Do not
collapse them — and do not take that handler as the pattern for new
routes, which delegate.

Three existing bullets in that file already point here and should be
cross-referenced rather than duplicated:

- **"`security(("bearerAuth" = []))` — the empty array is the SCOPES
  list, not decoration."** That is the declaration site. Scopes land
  there as `security(("bearerAuth" = ["oxicloud:files:rw"]))`, so the
  declaration rides in the spec rather than a side table, and
  `openapi-spec-drift` keeps it fresh by construction.
- **"OpenAPI has no field for a minimum role… use a pseudo-scope
  (`["role:admin"]`)."** Gates therefore declare as `role:` pseudo-scopes
  alongside the real `oxicloud:` ones. The `oxicloud:` prefix (§ Wire
  format) is what keeps the two unambiguous in one array — a
  client-requestable scope and a non-requestable gate can share the
  field without sharing a namespace.
- **"Declaring is not enforcing. utoipa's `security` wires nothing, so it
  drifts from the real gate silently. Any scope worth declaring is worth
  a test cross-checking it against the actual mount."** That is
  precisely § Deliberately scopeless's exhaustiveness test. The file
  already asked for it; this plan discharges the request.

Two mechanisms, and the division matters:

| mechanism | enforces | status |
|---|---|---|
| Redocly `security-defined: error` | every operation declares *something* | **shipped** (`checks/openapi/redocly.yaml`, 2026-11-14) — it found 14 operations declaring no auth at all |
| exhaustiveness test | the declaration *matches* the mount | this plan |

The lint catches silence; the test catches lies. Neither substitutes for
the rule in `src/AGENTS.md`, which is what makes a contributor consider
the axes before writing either.

## Declaring session elevation

`docs/plan/admin-elevation.md` owns elevation and already decided how it
is **enforced**: a layer, keyed on HTTP method.

```
non-safe method (POST/PUT/PATCH/DELETE) under /api/admin/*  →  elevation required
```

Chosen so a new admin mutation is covered the moment it is mounted,
without anyone remembering — the same failure this plan exists to stop.

What that leaves open, and what belongs here, is how a route **declares**
it. Two reasons the declaration is not redundant with the layer:

1. **`src/AGENTS.md` forbids relying on the verb alone**: "A method check
   is not an authorization check… `GET /api/wopi/editor-url` returns a
   WOPI token usable for POST, `GET /api/s/{token}` writes via
   `register_shared_link_access`, `GET /api/auth/device/verify` is an
   oracle on live device codes. Never gate on verb alone." All those
   examples sit outside `/api/admin`, and a sweep of the 48 admin GETs
   found none that mutates — so the method rule is sound *today* and
   silently wrong the day someone adds `GET /api/admin/.../generate-key`.
2. The spec must be truthful per operation, and `#[utoipa::path]` is
   already per `(verb, path)` — so the declaration site is naturally
   verb-scoped, which a prefix layer is not.

**Declaration: a pseudo-scope in the existing `security` array.**

```rust
security(("bearerAuth" = ["role:admin", "elevated"])),
```

Consistent with the convention `src/AGENTS.md` already sets for roles
("OpenAPI has no field for a minimum role… use a pseudo-scope
`["role:admin"]`"). The `oxicloud:` prefix is what keeps this
unambiguous: `oxicloud:files:rw` is a client-requestable scope,
`role:admin` and `elevated` are declarations of caller state that **no
client may request**. A client cannot ask to be elevated; it re-auths
through `POST /api/auth/elevate` and the server stamps
`auth.sessions.elevated_at`.

**The layer is the default, the declaration is the override.** The
exhaustiveness test checks both directions, and the second is the one
that matters:

- an operation declaring `elevated` must be covered — catches a
  declaration that enforces nothing;
- **an operation the method rule covers must declare `elevated`** —
  catches the spec drifting from the gate, which is exactly what
  `src/AGENTS.md` means by "declaring is not enforcing".

A mutating GET under `/api/admin` then resolves cleanly instead of
silently: it declares `elevated`, the method rule does not cover it, the
test fails, and the fix is an explicit per-route entry. "Never gate on
verb alone" is satisfied without giving up automatic coverage.

### The array is AND, and getting that backwards is an escalation

`security(("bearerAuth" = ["role:admin", "elevated"]))` means
`Gate::Admin` **AND** `Gate::Elevated`. Not or. OpenAPI fixes this and
the two forms are easy to confuse:

| form | meaning |
|---|---|
| `("bearerAuth" = ["role:admin", "elevated"])` — one requirement, two scopes | **both** required (AND) |
| `("bearerAuth" = ["role:admin"]), ("bearerAuth" = ["elevated"])` — two requirements | **either** suffices (OR) |

Read as OR, the declaration above admits an **elevated non-admin
session to admin mutations** — a plain user who re-authenticated at
`POST /api/auth/elevate` would satisfy `elevated` and pass. That is a
privilege escalation produced entirely by notation. The fix is to put
the semantics somewhere a reader cannot misinterpret:

```rust
// Avoided — not because bitflags are wrong, but because `|` reads as
// boolean "or" to the next reader while meaning "and".
policy.require(Gate::Admin | Gate::Elevated, caller).await?;

// Chosen — conjunction is the only available reading.
policy.require_all(&[Gate::Admin, Gate::Elevated], caller).await?;

// Equally fine, and what a layer composes to anyway:
policy.require(Gate::Admin, caller).await?;
policy.require(Gate::Elevated, caller).await?;
```

To be fair to the bitflags reading: as a *representation* it is fine —
`a | b` builds a set, and `require` can perfectly well mean "every bit
in this set must be satisfied". The hazard is not the bits, it is that
the operator reads as boolean "or" to the next person, while the
semantics are "and". A slice removes the ambiguity at no real cost:
there are six gates, never more than two or three at a call site, and
§ Lookup cost already puts these checks on cold paths, so a loop with
`?` is not a performance question.

So `Gate` stays a plain enum. No combination in this design means
"either", and if a genuine OR ever appears it gets two requirement
objects in the spec plus an explicit `require_any` — the semantics in
the method name, where a reviewer cannot misread it, rather than in an
operator.

Note the asymmetry with real scopes, which is why they live in the same
array without confusion: `oxicloud:files:rw` and `oxicloud:calendar:ro`
in one requirement are also AND — a credential carrying both — and that
is exactly what a `ScopeSet` is. Conjunction is the right default for
every entry in the array; it is only the `|` operator that suggests
otherwise.
**Two consumers today**, so `Gate::Elevated` is not speculative:

| needs elevation | because |
|---|---|
| non-safe methods under `/api/admin/*` | holding the role is not enough to *change* things (`admin-elevation.md` § Goal) |
| mutations in the `security` scope | § `security` — scope says the key was issued for it, elevation says a human proved themselves recently |

The second is why `security:rw` is necessary and not sufficient: a
long-lived interactive session that has drifted away from its keyboard
should not be able to change the password, and the scope alone cannot
express "recently".

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

- Unit: the scope parser. Unqualified → `ReadWrite`; the legacy
  `webdav,caldav,carddav` string → `{Files, Calendar, Contacts}` all
  `:rw`; an unknown resource name is **rejected**, not ignored (a typo'd
  scope must not silently widen).
- Unit: namespace and delimiters — `oxicloud:files:rw` parses;
  space- and comma-separated lists both parse and agree; an unprefixed
  token is rejected, **including the standards-reserved `openid`,
  `profile` and `email`**, so a mixed OIDC request can never partially
  resolve; `scope=` present-but-empty is malformed, not defaulted.
- Unit: `effective = (requested ?? type_default) ∩ type_maximum` — a
  device-flow request for `account` is clamped, and an omitted scope
  yields the type default rather than every resource. This is the
  fail-open pin: if it ever passes by granting everything, the feature
  is decorative.
- Unit: `Access::ReadWrite` implies `ReadOnly`, and the DAV safe-method
  split — `PROPFIND` passes `:ro`, `PROPPATCH` does not.
- Hurl, new file: a credential scoped `files:rw` is refused on
  `/caldav` and `/carddav` and accepted on `/webdav`; one scoped
  `files:ro` passes `PROPFIND` and is refused `PUT`. Each pin asserts
  the refusal AND an allowed caller succeeding, per AGENTS.md, so a
  gate that refuses everyone cannot pass.
- Hurl: the `security` pins — an app-password or device-flow credential
  requesting `oxicloud:security:rw` is **clamped by the ceiling**, not
  merely refused at the route, and is then refused on
  `POST /api/auth/change-password`, `/api/auth/oidc/link/start`,
  `/api/auth/oidc/unlink`, `/api/auth/upgrade-to-internal` and
  `POST /api/auth/app-passwords`. Pinning the clamp AND the route
  matters: if only the route is pinned, a later ceiling bug passes the
  suite while handing a stolen app password the password-change
  endpoint. The IdP-link pin is the one that outlives revocation.
- Migration: existing `device_codes` rows keep working. Pin the legacy
  default string explicitly, since enforcement is what turns a
  previously-inert column into a lockout risk.
- Hurl: the counter-pin — a credential WITHOUT `security` can still
  `POST /api/auth/dpop/bind` and `POST /api/auth/logout`. Without this,
  a later tightening that folds session self-management into `security`
  would silently make DPoP-bound app passwords impossible, and every
  refusal pin above would still pass.
- Unit: **the exhaustiveness test** — every operation in
  `resources/gen/openapi.json` appears in the scope map or the
  `SCOPELESS` list, and every non-`/api` mount prefix in `main.rs` maps
  to exactly one scope. This is the pin that makes the other scope pins
  maintainable: without it, a route added later is silently ungated and
  nothing fails. Nine routes sat unassigned in an earlier draft of this
  plan — four of them escalation paths — and stayed that way until a
  reviewer thought to ask.
- Unit: the elevation cross-check, **both directions** — an operation
  declaring `elevated` is covered by the method rule, AND every
  operation the method rule covers declares it. The second direction is
  the one that catches drift; without it the spec can claim elevation
  that nothing enforces, which is precisely what `src/AGENTS.md` means
  by "declaring is not enforcing". A mutating GET under `/api/admin`
  must fail this test rather than pass quietly.
- Unit: `require_all` is **conjunction** — a caller holding `elevated`
  but not `Admin` is REFUSED on `[Admin, Elevated]`, and so is one
  holding `Admin` but not `elevated`. Pin both halves: an OR
  implementation passes the first case silently and hands admin
  mutations to any user who re-authenticated. This is the cheapest test
  in the plan and guards the most expensive mistake.

## When

At PR #853, which adds `POST /api/calendars` and the events surface —
the third hand-written copy of the per-route gate. Three is where the
duplication is real and the abstraction pays. Before that, this is
building the frame before there is a third wall.
