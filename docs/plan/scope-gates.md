# Scope gates — one home for principal-kind checks

Branch: `feat/scopes`. **Trigger: PR #853**
(calendars REST surface) for the gate half — see § When. The credential-
scope half (§ Credential scopes) has its own trigger: the device flow
already collects and displays scopes it does not enforce.

Status: **DRAFT** — amended 2026-11-15 to split Gate from Scope, make
credential scopes resource-based rather than protocol-based, and split
`account` from `security` the way Google separates those tabs. The design
questions raised in review are all resolved: every one of the 235 API
operations is classified with zero orphans, and the judgement calls are
ratified with their reasoning recorded rather than left open.
Deliverables now include a `src/AGENTS.md` rule and a new architecture
document — see the two § Required sections.

## Status

**Nothing here is implemented.** Keep this current as that changes.

| item | status |
|---|---|
| Gates fold — `Gate`, `ScopePolicy`, the three consumers | **not started** — PR 1, and 2 and 3 below depend on it |
| Credential scopes — `Scope`, `ScopeSet`, the 235-operation map | **not started** — PR 2 |
| Authentication by default (`NotAnonymous` inverted) | **not started** — needs the fold |
| A password-change facet | **will not ship with PR 1** — deadlocked on `authn-pipeline.md` part B; § Fold in |
| `src/AGENTS.md` rule + `authorization-axes.md` | **not started** |

The companion document's work — `authn-pipeline.md` part A — is MERGED
(PR #951). It touched the same middleware this plan folds, so four of its
consequences land before the first line here:

- **The two fail-open arms are already resolved**, as 503 rather than as
  a denial (§ The two fail-open arms). The gates fold inherits that
  answer rather than deciding it.
- **`resolve_live_role` now takes a `PresentedToken`** and `UserFlags`
  carries `sessions_valid_from` and `revoked_session_ids`. The fold
  touches those call sites, so expect them to look different from the
  snippets here.
- **`CurrentUser` carries `token: PresentedToken`**, so `sid` is on the
  caller. That retires the choice § `sid` is no longer deferrable framed,
  and it is the prerequisite the session facet's `Elevated` was waiting for.
- **`require_no_password_change_pending_layer` cannot be deleted by this
  plan** — the mechanism meant to replace it did NOT land. See the
  fold-in table.

## Goal

One definition of "may this KIND of caller reach this surface at all",
callable from a router layer or a service, replacing the hand-wired
middleware layers that each decide the same class of question with their
own fail-open policy. Five exist; PR 1 folds four (§ Fold in).

A gate has FOUR independent facets — role, credential-kind, anonymity,
session-binding — and the type is one field per facet. ("Facet" because
this document uses *axis* for Gate-vs-Scope-vs-Grant, § Three axes.)

A slice would let `[NotAppPassword, PermitAppPassword]` be constructed
and need catching at startup; one field per facet cannot hold it. That is
§ Maximise deny-by-default's "won't compile" over "won't start".

```rust
// application/ports/scope_ports.rs

/// Properties of the CALLER. Never client-requestable — § Three axes.
///
/// Every facet defaults to its SAFE value, so `Gates::default()` is the
/// strictest gate rather than the loosest: an authenticated internal
/// user, no app password, no anonymity. A route widens by naming what it
/// permits, never by omission.
#[derive(Default)]
pub struct Gates {
    /// Role facet. `None` = any authenticated principal. A conjunction
    /// reduces to the MAXIMUM, which is a field assignment, not a
    /// lattice walk — § Reduction is per facet.
    pub role: Option<RoleGate>,
    /// Credential-kind facet. § Credential-kind gating.
    pub app_password: AppPasswordGate,
    /// Anonymity facet. § Authentication by default.
    pub anonymous: AnonymityGate,
    /// Session-binding facet. `None` = unbound credentials accepted.
    pub dpop: Option<DpopGate>,
}

/// From the USER row, re-read live per request. ORDERED: each variant
/// implies the one before it, which is what makes the reduction a `max`.
///
/// `Admin ⇒ Internal` is provable from the `users_external_not_privileged`
/// CHECK constraint rather than conventional, so dropping `Internal`
/// beside `Admin` is sound. A test pins that dependency — removing the
/// constraint would quietly make the reduction wrong.
pub enum RoleGate {
    Internal,   // not is_external
    Admin,      // role.at_least(Admin); Owner satisfies it
    Owner,      // UserRole::Owner, NOT storage.grant_role
}

/// Deny-by-default: `Denied` is the `Default`, so a static Basic-auth app
/// password is refused on every route that does not say otherwise.
/// `Permitted` is the only variant that appears at a callsite, so
/// `grep Permitted` prints the whole app-password attack surface.
#[derive(Default)]
pub enum AppPasswordGate {
    #[default]
    Denied,
    Permitted,
}

/// Deny-by-default likewise — § Authentication by default. The handful of
/// genuinely public routes declare `Allowed`; publicness stops being
/// inferred from which router a handler was mounted on.
#[derive(Default)]
pub enum AnonymityGate {
    #[default]
    Denied,
    Allowed,
}

pub enum DpopGate {
    Bound,
    // Not v1: no elevation state in the schema yet. Designed in
    // `session-elevation.md`, which adds `auth.sessions.elevated_at`.
    // Its `CurrentUser` prerequisite is DONE — the caller carries
    // `token: PresentedToken`, so this reads `caller.token.session_id`.
    // Lands as a variant here with no reshaping.
    // Elevated,
}

/// `const` builders, so a route declares its gate as one expression and
/// the whole thing is a compile-time constant at the mount point.
///
/// Each returns `Self` with the named facet set and every other facet at
/// its SAFE default, so widening is always explicit and always local to
/// the callsite — `Gates::anonymous_allowed()` says exactly one thing and
/// cannot accidentally relax the app-password facet too.
impl Gates {
    pub const fn role(r: RoleGate) -> Self;
    /// The whole public surface, greppable: § Authentication by default.
    pub const fn anonymous_allowed() -> Self;
    /// The whole app-password attack surface, greppable:
    /// § Credential-kind gating.
    pub const fn permit_app_password(self) -> Self;
    pub const fn dpop_bound(self) -> Self;
    // pub const fn elevated(self) -> Self;  // with the session facet
}
```

`/webdav` then mounts as
`Gates::role(RoleGate::Internal).permit_app_password()`.

No password-change facet — § Fold in.

```rust
/// What the CREDENTIAL was issued for. Client-requestable, frozen at
/// mint. NOT named `Resource`, which the GRANT axis owns — and whose
/// variants carry an id, so `Resource::Calendar(Uuid)` is *one*
/// calendar where `Scope::Calendar` is the calendar *area*.
pub enum Scope {
    Files, Calendar, Contacts, Photos, Music, Sharing,
    Account,   // profile and preferences — Google's "Account" tab
    Security,  // password, identities, credentials — Google's "Security"
               // tab; absent from every non-interactive ceiling
    // Mail,   // reserved — the parser REJECTS it until a mailbox exists
}
pub enum Access { ReadOnly, ReadWrite }

/// Two bitmasks rather than a map: unlike a gate this is checked on hot
/// paths with no DB round trip to hide behind (§ Lookup and checking
/// cost). Eight scopes fit a `u8`, and `rw` implies `ro` by
/// construction.
pub struct ScopeSet {
    readable: u8,
    writable: u8,   // ⊆ readable, maintained on insert
}

pub trait ScopePolicy {
    /// ONE method, not a `require` / `require_all` pair. A single gate is
    /// a `Gates` with one facet set, so the pair was an artefact of the
    /// slice: `require_all` existed to normalise what a slice could
    /// misrepresent, and the struct cannot misrepresent it.
    ///
    /// Takes the CALLER CONTEXT, not a bare user id — § What a scope is
    /// bound to.
    async fn require(&self, gates: Gates, caller: &CurrentUser)
        -> Result<(), AppError>;
    /// Separate because Scope is a different AXIS, not another facet:
    /// client-requestable, frozen at mint, and checked on hot paths with
    /// no DB round trip to hide behind. § Checking cost.
    async fn require_scope(&self, need: Scope, access: Access, caller: &CurrentUser)
        -> Result<(), AppError>;
}
```

Three consumers, picked by what the gate protects:

- **`InternalCaller` capability token** — for a top-level create. The
  only deny-by-default form: forgetting it does not compile. § Why 2.
- **`gate_layer(state, Gates::role(RoleGate::Internal))`** — for a protocol subtree
  (`/webdav`, `/caldav`, `/carddav`, `/api/admin/*`), where the denial
  is the surface rather than one method.
- **`InternalUser` / `AdminUser` extractors** — for a REST handler with
  no service method of its own. Weakest (forgetting = open), so prefer
  the token wherever a service stamps ownership.

## What a scope is bound to

**The session, not the user.** Precisely: the **(session, user) pair** —
what this credential may do = what the session was issued for ∩ what the
user is currently allowed. Both halves are load-bearing:

- **Session half.** An app password is narrower than a web session for
  the same user; an anonymous share session may map to no user row, or
  (self-grant) to a real one; DPoP binding is per session. If scope were
  user-bound, an app password would inherit full web capability.
- **User half, read LIVE.** Caching `is_external` at mint is the trap:
  a guest who completes `POST /api/auth/upgrade-to-internal` would stay
  locked out until re-login — the dead end `is_upgradable` exists to
  prevent. So user-derived variants re-read per request, as
  `require_internal_user` and `decide_live_role` already do.

Hence `require(gate, caller: &CurrentUser)`: a bare `Uuid` cannot
express the session half, and a signature taking one quietly forces
every gate to be user-bound.

`CurrentUser` (`dtos/user_dto.rs:621`) carries `id`, `username`,
`email`, `role` (**from the JWT claim**, hence stale-prone, hence
`decide_live_role`), `dpop_jkt` — and now `token: PresentedToken`, the
`sid` and `iat` the credential presented. **This question is closed; the
field is in.** the session facet's `Elevated` can read `caller.token.session_id`
without reshaping `require`.

It carries the whole `PresentedToken` rather than a bare `sid` because
both claims come from the same credential and `resolve_live_role` needs
them together; a second encoding of the same fact is how two channels
drift apart. Which is why the field replaced a request-extension publish
that had briefly served the same purpose: `CurrentUserId` is itself
derived from `extensions.get::<Arc<CurrentUser>>()`, so the caller struct
was already reachable everywhere the extension was, and one channel beats
two.

Extensions could not have served a `Gate` in any case — `require(gate,
caller: &CurrentUser)` passes the caller and nothing else, by design, so
a gate has no route to the request. That constraint is what decided the
question: the fact has to be on the caller.

`#[serde(skip)]`, like `dpop_jkt` — this is a serialized DTO with an
OpenAPI schema, and internal request state must not reach the wire.
Verified by regenerating `resources/gen/openapi.json`: no diff. CI diffs
that file, so the skip is guarded by the artifact rather than by a unit
test.

Follow the precedent in that file: `CurrentUser::role` resolves an
unrecognised value to `Anonymous`, "the least privileged answer". Same
posture — an undetermined scope denies.

**Token freshness.** A capability token is only as fresh as the request
that minted it. Fine while created and consumed inside one request, but
caching one across requests re-creates the stale-claims bug with a type
wrapper.

## Three axes

Caller properties and credential restrictions are separate types.
Merging them is not untidiness — it is a privilege-escalation bug
waiting for an author who parses a client-supplied scope string into the
enum and finds `Admin` in it.

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

**Vocabulary.** The three rows above are the **axes** — separate types.
Inside the Gate axis are four independent **facets** — role,
credential-kind, anonymity, session-binding — which are fields of one
type (§ Goal), and reduction never crosses between them (§ Reduction is
per facet).

Consequences, both agreed:

- **`/api/admin/*` is a role check, not a scope.** `RoleGate::Admin`, read
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
| Gate | the anonymity facet at its default `Denied` — enforced today by `anonymous_allowlist_layer` | router layer |
| Scope | `oxicloud:sharing:rw` | router layer / extractor |
| Grant | `authz.require(Subject::User(caller), Permission::Share, resource)` — owners short-circuit | `grant_handler.rs` |

The facet is ANONYMITY, **not** role — and the difference is easy to
blur. `AnonymityGate::Denied` refuses a visitor with no user row;
`RoleGate::Internal` refuses an external USER, who has one.

An external user has an `auth.users` row and legitimately re-shares what
was shared with them (§ Rejected: default-deny). An anonymous
public-link visitor has no user row at all: `UserRole::Anonymous`
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
`anonymous_allowlist_layer` becomes `gate_layer(Gates::default())`
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

### Areas, not protocols

Every door maps to exactly one area, so a protocol vocabulary
(`webdav`, `caldav`, …) would name the same permissions twice — and once
#853 adds `POST /api/calendars`, a `caldav` scope guards one of two
doors to the same data while `calendar` guards both. Google
(`calendar.readonly`) and Microsoft (`Calendars.Read`) are area-shaped
for the same reason.

| scope (shown without the `oxicloud:` prefix) | routes it opens (non-exhaustive) |
|---|---|
| `files` | `/webdav`, `/remote.php/dav/files`, `/wopi`, `/api/files`, `/api/folders`, `/api/uploads`, `/api/batch`, `/api/trash`, `/api/favorites`, `/api/recent`, `/api/dedup`, `/api/drives` |
| `calendar` | `/caldav`, `/.well-known/caldav`, `/api/calendars` (#853) |
| `contacts` | `/carddav`, `/.well-known/carddav`, `/api/address-books`, `/api/contacts` |
| `photos` | `/api/photos`, `/api/photos/geo` (Places), `/api/people` (faces) |
| `music` | `/api/playlists` |
| `account` | `/api/auth/me`, `/me/profile`, `/me/image` — **enumerated, never the `/api/auth/*` prefix** — `/api/notifications`, `/remote.php/dav/avatars` |
| `security` | `/api/auth/change-password`, `/oidc/link/start`, `/oidc/unlink`, `/upgrade-to-internal`, `/opaque/register/*`, `/app-passwords`, `/device/devices`, `/device/verify` |
| `sharing` | `/api/grants`, `/api/shares`, `/api/s`, OCS sharing, and recipient lookup: `/api/users/{id}`, `/api/groups/search` |
| `mail` | **reserved** — no mailbox exists; the parser REJECTS it, since a name that parses but gates nothing is the device-flow bug again |

Non-exhaustive by design: a new route under an existing area inherits
its scope. A route fitting no row means a scope is missing, which is a
decision rather than a lookup.

Three naming traps:

- **`contacts` is the caller's own address books**, not the user
  roster — a `contacts:rw` credential must not enumerate who has an
  account here.
- **`account` is the caller's own profile.** `/api/users` holds exactly
  one route and it looks up *other* people, so it is `sharing`.
- **`sharing` includes recipient lookup**, both users and groups: one
  capability, one scope. Splitting them breaks the share picker while
  protecting nothing, since either half reveals the roster.

`photos` covers Places and People because both are derived from the
photos — EXIF coordinates, pixel analysis. A separate scope would
protect nothing while implying it did. That is the test a scope must
pass: withholding it has to withhold something.

Deliberately absent: `/api/admin/*` and `/api/groups` mutations are
`RoleGate::Admin`; `/api/search` and `/api/rt/ws` read across areas and are
filtered by what the caller already holds; credential management and
account-security routes are in `security`; login, public and ops routes
are scopeless (§ Deliberately scopeless).

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
| App password | `files,calendar,contacts` (`:rw`) | same — **no `security`**, **no `account`** (§ Credential-kind gating) |
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
`RoleGate::Admin`, not `security`.

**Why a named scope rather than an exception list**, on three counts:

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

**the session facet's `Elevated` is the second factor, and this is its first real
consumer.** Holding `security:rw` is necessary and not sufficient: a
mutation also needs a recently re-authenticated session, the admin
sudo-mode state designed in `docs/plan/session-elevation.md`. Scope answers
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
  CALLING session. It belongs here rather than in `security`, for two
  reasons:
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
  `DpopGate::Bound` asks how strongly this credential is held,
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
may transfer ownership", which becomes `RoleGate::Owner` instead of a
hand-rolled check.

Two implementation notes, because both are traps:

- **`RoleGate::Admin` is `role.at_least(Admin)`, not `role == Admin`.** The
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

### Fold in (four of the five layers, plus one hand-rolled check)

| Today | Becomes |
|---|---|
| `require_internal_user_layer` | `gate_layer(Gates::role(RoleGate::Internal))` |
| admin middleware on `/api/admin/*` | `gate_layer(Gates::role(RoleGate::Admin))` |
| `require_no_password_change_pending_layer` | **left alone by PR 1** — see the deadlock below |
| `require_dpop_layer` | `gate_layer(Gates::default().dpop_bound())` |
| hand-rolled owner check in `transfer_ownership` | `RoleGate::Owner` |
| `anonymous_allowlist_layer` | `gate_layer(Gates::default())` + keeps its allowlist — and is the mechanism § Authentication by default would promote to the root |

`anonymous_allowlist_layer` is the loose fit — it carries a route
allowlist and must stay `route_layer` (it needs `MatchedPath`). Fold the
*decision* and leave the allowlist where it is.

**PR 1 does not touch the password-change layer.** Its intended
replacement — forced-reset as a scope narrowed to `{security}` at mint —
is `authn-pipeline.md` part B, which is *not started* and whose status
line reads "needs `scope-gates.md`". Each plan assigned the removal to
the other; no narrowing exists at the mint today.

So the eleven references stay, no password-change facet exists, and the
removal belongs to whichever plan lands second. Folding it as a facet
meanwhile would be a third home for the check, built to be deleted,
against an acceptance criterion of unchanged behaviour.

Distinct from `authn-pipeline.md` § The forced-reset deadlock, which is a
runtime risk inside part B (a `{security}`-narrowed session needs
elevation via the password it must change) and only exists once narrowing
does.

### Keep as layers, not extractors

`/webdav`, `/caldav`, `/carddav` dispatch through one `any` handler, so
a layer is both natural and cheaper there. Extractors are for REST
handlers only.

### Two fields `CurrentUser` must carry first

Every facet is a field read only if the caller actually carries it. Two
do not, and both block a facet rather than merely slowing it:

| field | why | today |
|---|---|---|
| `is_external` | `RoleGate::Internal` has nothing to read | `UserFlags` has it, but `LiveRole::Active(SmolStr)` returns only the role, so `require_internal_user` calls `get_user_flags` a SECOND time per request |
| credential-kind | `AppPasswordGate::Denied` cannot be evaluated at all | the Basic-auth branch knows (`origin = "app_password"`) and discards it |

Both resolve in the pass that already holds the flags, and both follow
`token: PresentedToken` (`9a42e4299`): the middleware knows the fact,
drops it, and a consumer re-derives it or cannot ask. Adding
`is_external` also removes the duplicate lookup.

### Per-route gates to retire

- `routes.rs` — the two `contacts_create` / `music_create` sub-routers
  (added with the external-user fix) collapse into an extractor on the
  handler.
- `calendar_service.rs` — the `TODO(#853)` on `create_calendar`
  resolves by *deleting* it, not by adding copy number three.

## Lookup and checking cost

`require_internal_user` calls `auth.get_user_flags(caller_id).await`.
That is a **cached read, not a query per request**: `user_flags_cache` is
a moka cache with a 30 s TTL and `try_get_with`, so concurrent callers
for one user collapse into a single lookup and a user's row is read at
most twice a minute. The DB appears on a miss.

The live-read decision stands and the cache makes it affordable: putting `is_external` on the JWT claims is the one thing
§ What a scope is bound to rules out, because an upgraded guest would
then stay locked out until re-login. A cache can be invalidated on
upgrade or demotion; a minted claim cannot — and `invalidate_user_flags`
is already the mechanism every revocation path uses.

So the rule is placement, and the reason is not cost:

- Ownerless creates and the DAV / admin subtrees are cold. Keep the
  token and the layers there.
- Keep GATE checks off the hot paths — listing, thumbnails, auth
  (AGENTS.md § Performance) — because those ask a **grant** question,
  answered by `authz.require` against a resource. A gate there is the
  wrong check, not an expensive one.

The 30 s TTL carries a second consequence worth stating: a gate's answer
can be up to 30 s stale. For `is_external` and role that is the intended
trade (§ What a scope is bound to), and every path that tightens already
calls `invalidate_user_flags` so the window closes immediately rather
than on expiry. A future facet whose staleness is NOT acceptable must
invalidate on change, not shorten the TTL for everyone.

This paragraph is about **gates only**, and said "scope" before the two
axes were separated. Scopes resolve the other way round on purpose: they
ARE a claim on the token, because unlike `is_external` a scope cannot
change under its credential (§ Freezing at mint), so there is nothing to
invalidate and a hot-path check is free.

### Checking cost: gates and scopes are not alike

The two axes have opposite cost profiles, and representing them the same
way gets one of them wrong.

| | gate | scope |
|---|---|---|
| source | `auth.users`, re-read live | JWT claim, frozen at mint |
| per check | cached read — moka, 30 s TTL, single-flight; DB on a miss | pure CPU |
| staleness | up to the TTL, closed early by `invalidate_user_flags` | none — cannot change under its credential |
| paths | cold by design, EXCEPT the defaults — see below | **hot** — every `/webdav` request, every `/api/files` listing |

Neither axis is dominated by a query, so **representation is not a
performance question for either of them**, and the choice of `Gates` is
made on representability alone: it makes a contradictory pair impossible
to construct, where a slice makes it impossible to miss only if something
checks. § Reduction is per facet.

The scope masks are still the right call, for their own reason rather
than by contrast with a round trip: a scope check runs on paths AGENTS.md
names as hot, several times per DAV request, and `u8` masks make it one
`&` and a compare with `writable ⊆ readable` held as an invariant — so
`rw` implying `ro` is not a branch at every callsite.

### Where to evaluate a facet — two questions

| facet | source | changes mid-credential? | evaluated |
|---|---|---|---|
| role (`Internal`/`Admin`/`Owner`) | `auth.users`, via `user_flags_cache` | **yes** — promote, demote, guest→internal | per request; already resolved by `auth_middleware` into `CurrentUser.role` |
| anonymity | the credential | no | per request, field read |
| credential-kind | the credential | no | per request, field read |
| DPoP binding | the credential (`cnf.jkt`) | no | per request, `CurrentUser.dpop_jkt` |

So a root gate layer adds **no lookup** to an authenticated request: the
one cached read already happens in `auth_middleware`, and the rest are
field reads — given the two fields § Two fields `CurrentUser` must carry
first adds.

Cost does not decide this; staleness does. The gate-side form of
`authn-pipeline.md`'s rule ("every state change that **tightens** must
either revoke sessions or be in the per-call read"):

1. **Can the fact change while the credential is alive?**
   - **No** — it is a property of the credential. Read it from
     `CurrentUser` per request: free, no staleness, nothing to
     invalidate. Anonymity, credential-kind and DPoP binding are all
     here.
   - **Yes** — question 2.
2. **Must a TIGHTENING change bite before the session ends?**
   - **Yes** — per-call read. `user_flags_cache` makes it free and
     `invalidate_user_flags` closes the TTL window. Role and
     `is_external` are here.
   - **No** — it may be evaluated at a ceremony (login, refresh), and it
     then OWES the obligation above: the change must revoke sessions.
     Nothing in v1 takes this branch.

The test an implementor can actually run: *state what happens if this
fact changes one second after login.* If the answer is "the caller keeps
something they should have lost", it is per-call or it revokes.

Scope is the counter-example that makes the rule legible: frozen at mint
is sound there because a scope cannot change under its credential
(§ Freezing at mint), so question 1 answers "no".

This belongs in `src/AGENTS.md` — § Required: amend.

### The defaults put a gate on every request

"Cold by design" holds for the named gates, not for the defaults.
`AnonymityGate::Denied` and `AppPasswordGate::Denied` only bite if every
route is gated — the root choke point § Authentication by default argues
for — so the design does put a gate check on listing and thumbnails.

The cache makes that affordable, but **measure it before wiring a root
layer**, and measure the right thing: not steady-state hit cost but
single-flight at the 30 s boundary under K concurrent requests for one
user, which is a sync client's shape. `examples/bench_auth_herd.rs` is
that shape for the sibling cache. Benches run against 5433, not 5432.

(§ Rejected: default-deny across all of `/api` refuses a root
`RoleGate::Internal` on correctness grounds — the external exemption list
would cover most of `/api` — which is a separate argument from cost.)

A scope check has no query to hide behind. It runs on paths AGENTS.md
names as hot (listing, thumbnails, auth), on every DAV request a sync
client makes, so a map lookup per check is real work for no reason.
Hence the two `u8` masks: one `&` and a compare, with `writable ⊆
readable` held as an invariant so `rw` implying `ro` is not a branch at
every callsite.

The parse stays string-shaped — `oxicloud:files:rw` is what rides on the
wire and appears on a consent screen — and is converted once at token
validation. Masks are an internal representation, never a wire format:
bit positions would make the vocabulary order-dependent, so adding a
scope could silently reinterpret an existing credential.

## Rejected: default-deny across all of `/api`

The tempting reading of "secure by default" is
`gate_layer(Gates::role(RoleGate::Internal))` on the whole protected `/api` router plus
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

## Maximise deny-by-default

The governing principle, and the tie-breaker whenever a choice in this
plan is otherwise balanced. Ranked, because not all denial is equal:

| strength | shape | example in this plan |
|---|---|---|
| 1. **Won't compile** | the omission is a type error | `InternalCaller` token — `auth_user.id` stops type-checking on an ownerless create |
| 2. **Won't start** | boot panics on a misconfiguration | an unknown job name in `OXICLOUD_JOBS_SCHEDULED` already does this; the scope parser rejecting an unknown resource is the same posture |
| 3. **Won't pass CI** | the omission fails a test | exhaustiveness test — a route in neither the scope map nor `SCOPELESS` |
| 4. **Fails closed at runtime** | the default answer is refusal | `AppPasswordGate::Denied` as the facet default; ceilings as allowlists; `CurrentUser::role` resolving an unknown value to `Anonymous`, "the *least* privileged answer" |
| 5. **Merely visible** | a reviewer must notice | an extractor on a handler — forget it and the route is open. Use only where 1-4 do not fit |

Prefer the lowest number available. Where this plan chose 4 over 1 it
was because no type can express the question (a router-level gate has no
callsite to constrain), not because 4 is good enough.

### Where to push it further than drafted

- **Scope parse rejects, never ignores.** An unrecognised resource name
  is an error, not a dropped token. A typo must not silently widen a
  credential, and `mail` must not parse until it exists (§ `mail` is
  reserved).
- **Ceilings are allowlists** (§ Defaults). A credential type added
  without a row gets nothing, so forgetting withholds.
- **Absent scope claim is the one deliberate exception**, and it earns
  it: absent means FULL (§ Rollout). Deny-by-default here would lock out
  every live session on upgrade — which is availability damage inflicted
  by a security reflex. The exception is bounded by being a
  *grandfathering* rule with a defined end: once no pre-enforcement
  token can still be valid, absent becomes empty.

### Reduction is per facet

**Reduction is per facet, never global.** The facets are independent, so
a role conjunction collapses to its maximum while the app-password facet
stays untouched; reducing across facets would silently drop a gate.

One field per facet makes both problems vanish instead of needing checks:
the contradictory pair is unrepresentable, and the role reduction is a
field assignment (`max`) rather than a lattice walk.

`Admin ⇒ Internal` makes that `max` sound, and it is provable from the
`users_external_not_privileged` CHECK rather than conventional — so a
test pins it, since removing the constraint would make the reduction
wrong.

### Authentication by default

The same inversion, applied consistently: **`NotAnonymous` is the
default and `AnonymousAllowed` is declared**, rather than routes being
public by omission. Arguing the positive-marker pattern for app
passwords commits us to it here too.

Today the server is the other way round, and more loosely than
"unsupported by the framework" suggests:

- `auth_middleware` is applied **nine separate times**, once per
  protected nest. There is no root choke point, which is the structural
  version of what `src/AGENTS.md` already warns about: "The extractors
  are NOT a choke point… A rule added to an extractor does not hold
  until it is added to these too."
- Public routes are public **by mount location**: `main.rs` ~1271 nests
  `public_api_routes` at `/api` without the layer, beside
  `protected_api` which has it. Nothing declares publicness; it is
  inferred from which router a handler was added to.

So the tenth nest added without `auth_middleware` is silently
unauthenticated and no test fails. That is the `create_address_book`
failure mode one level up — a gate available and unused, because nobody
was required to think about it.

**The mechanism already exists in-house.**
`anonymous_allowlist_layer` (mounted `main.rs` ~1110) consults
`MatchedPath` against a route list and refuses anything absent from it.
That is exactly the shape needed; it governs one nest where it should
govern the root. Inverting means: one layer over the merged router,
consulting a declared public set, with absence meaning deny.

**The failure mode is loud, and that is the point.** Get a declaration
wrong and `POST /api/auth/login` stops working — a total outage, not a
quiet hole. Preferable to the current failure mode, which is an open
route nobody notices, but only because § Deliberately scopeless's
exhaustiveness test catches a missing declaration in CI rather than in
production. Without that test this inversion would be reckless; with it,
the worst case is a red build.

**Sequencing.** This restructures how authentication is mounted, so it
does not belong inside the gate fold — PR 1's whole claim is to change
no behaviour. It is PR 3, or its own RFC, and the `Gate` enum carries
the pair from the start so the later change is a wiring exercise rather
than a redesign.

### The two fail-open arms: answer 503, not yes or no

`require_internal_user` admits the caller on any `get_user_flags` error;
`decide_live_role` resurrects the claim role on a transient one.
`src/AGENTS.md` calls both bugs. The apparent choice — fail open and
lock nobody out, or fail closed and deny on a DB blip — is a false one.

**Both answer the authorization question when the server does not know
the answer.** A transient lookup failure is a server fault, so the
honest response is neither yes nor no:

| case | today | should be |
|---|---|---|
| `NotFound` — definitive revocation | deny | deny — correct already |
| transient failure | admit (fail open) | **503, retryable** |

A 403 would be the other wrong answer: it tells the client "you are not
allowed", so the SPA shows a permissions error or bounces to login
rather than retrying. 503 says "ask again", which is what is true.

The availability objection then disappears, because during a database
outage the request was going to fail anyway at the next query —
fail-open admits a caller into a system that cannot serve them, one
status code less honestly.

One gap: there is no transient error class. `ErrorKind` has
`NotFound | AlreadyExists | InvalidInput | AccessDenied | Timeout`, and
`Timeout` maps to `408 Request Timeout` (`interfaces/errors.rs` ~236) —
which means "the client was slow", not "my database is down". So this
needs a transient kind mapping to 503, which is a small addition with a
clear consumer, and `authn-pipeline.md` part A is where it lands.

## Direction: app passwords are terminal

Recorded so the next reader does not treat the permit set as permanent
architecture. The intent is that **app passwords disappear in favour of
OIDC for interactive login and the device flow for everything else**,
with OxiCloud's own client replacing the NextCloud one.

That makes `PermitAppPassword` a **deprecation instrument** rather than a
permanent gate: five mounts today, three once NC support goes, zero at
the end. Each removal is a reviewable step instead of a flag day.

Two notes for whoever picks that up:

- **The successor is RFC 8628, not OIDC alone.** OIDC covers interactive
  login, but generic DAV clients cannot use it — DAVx⁵, Thunderbird and
  rclone authenticate with Basic, and their OAuth support is
  provider-specific where it exists. The device flow is already
  implemented (`/api/auth/device/authorize`, `/device/token`) and already
  mints a JWT: a scoped, revocable, per-device credential with no static
  secret a user can paste anywhere. It is what app passwords should die
  into, which also makes the device-flow half of this plan the
  strategically important one.
- **AGENTS.md § Purpose currently says the opposite** and will need
  amending in the same change: "Customer target: NextCloud users. Hence
  the NextCloud-compatible API surface (`/remote.php`, `/ocs`,
  `/status.php`) — compatibility is a feature, and breaking it costs
  adopters." Until that line changes, every contributor is being told to
  preserve what this direction intends to drop.

## Companion: the login phase

This document owns the **per-call** axes. The login phase — what happens
between a credential being verified and a session existing — is
`docs/plan/authn-pipeline.md`, and the two meet at three points:

- **Scope is minted there, enforced here.** The login phase derives a
  `ScopeSet` from `(method, credential) ∩ ceiling` and may narrow it
  further for a restricted account; this document only ever reads it.
- **The password-change check is deleted by it.** The forced-reset state
  becomes a scope narrowed to `{security}` at mint, so
  `require_no_password_change_pending_layer` and its five mount points go
  away rather than becoming a facet. Until part B lands, PR 1 leaves the
  layer untouched — § Fold in.
- **The two fail-open arms fold into it.** Their resolution is 503 on a
  transient failure (§ The two fail-open arms), which needs a transient
  error kind — and part A is where `admit()` first has to distinguish
  "this account is refused" from "I cannot tell".

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
> - **Gate** — which KIND of caller may reach this surface, as a `Gates`
>   value naming the facets it needs (role, credential-kind, anonymity,
>   session-binding). Declared even when the answer is "any
>   authenticated caller", which is `Gates::default()` and still written
>   down.
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

And the deny-by-default ranking, which is guidance for every contributor
rather than only for this change — so it belongs in `src/AGENTS.md`
beside the existing "an auth helper's `_ =>` arm must DENY" bullet, not
only in a plan file:

> **Prefer the strongest available form of denial**, in this order:
>
> 1. **Won't compile** — the omission is a type error (a capability
>    token the policy alone can mint).
> 2. **Won't start** — boot panics on a misconfiguration, as an unknown
>    job name in `OXICLOUD_JOBS_SCHEDULED` already does.
> 3. **Won't pass CI** — the omission fails a test, as a route absent
>    from both the scope map and `SCOPELESS` does.
> 4. **Fails closed at runtime** — the default answer is refusal
>    (`AppPasswordGate::Denied` as the facet default; ceilings as allowlists;
>    `CurrentUser::role` resolving an unknown value to `Anonymous`).
> 5. **Merely visible** — a reviewer must notice. An extractor on a
>    handler is this: forget it and the route is open. Use only where
>    1-4 cannot express the question.
>
> Choosing a weaker form is a decision to record, not a default. The
> existing `OptionalUserId` is the cautionary tale this file already
> names: it exists, it is dead code, and nothing ever used it.

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

`docs/plan/session-elevation.md` owns elevation and already decided how it
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

### The array is AND

`security(("bearerAuth" = ["role:admin", "elevated"]))` requires
`RoleGate::Admin` **and** the session facet's `Elevated`. OpenAPI puts conjunction inside
one requirement object and disjunction between objects, and the two
forms look nearly identical:

| form | meaning |
|---|---|
| `("bearerAuth" = ["role:admin", "elevated"])` — one requirement, two entries | **both** required |
| `("bearerAuth" = ["role:admin"]), ("bearerAuth" = ["elevated"])` — two requirements | **either** suffices |

Taking the first form as "either" would admit an **elevated non-admin
session to admin mutations**: any user who re-authenticated at
`POST /api/auth/elevate` satisfies `elevated` and passes. So the
conjunction belongs in a method name, where it cannot be misread:

```rust
// One call, and the conjunction is in the value rather than in the
// reader's head. `.elevated()` joins the builders in § Goal when the
// session facet gains its variant — not v1.
policy.require(Gates::role(RoleGate::Admin).elevated(), caller).await?;

// Two calls are EQUIVALENT here, and that is a property of the facets
// being independent: `role` and the session facet cannot overwrite one
// another, so sequencing two requires composes rather than conflicts.
// The single call is preferred for being one audit line, not because
// the pair is wrong.
policy.require(Gates::role(RoleGate::Admin), caller).await?;
policy.require(Gates::default().elevated(), caller).await?;
```

No gate combination in this design means "either". If one ever appears
it gets two requirement objects in the spec and an explicit
`require_any`, named so the reviewer sees which it is.

Real scopes behave the same way, which is why they share the array
without confusion: `oxicloud:files:rw` and `oxicloud:calendar:ro` in one
requirement means a credential carrying both — exactly what a `ScopeSet`
is. Conjunction is the right default for every entry.

**Elevation is not an admin feature.** `session-elevation.md` is named for
its first consumer, and the name is now too narrow: elevation is a
property of any session, and an ordinary user needs it for the same
reason an admin does. Three consumer classes, so the session facet's `Elevated` is not
speculative:

| needs elevation | audience | because |
|---|---|---|
| non-safe methods under `/api/admin/*` | admins | holding the role is not enough to *change* things (`session-elevation.md` § Goal) |
| mutations in the `security` scope | **any user** | scope says the key was issued for it, elevation says a human proved themselves recently |
| irreversible deletions | **any user** | no undo path exists afterwards, so a stolen session gets one shot at permanent damage |

Three consequences worth settling before implementation:

1. **`POST /api/auth/elevate` is for any authenticated user**, not just
   admins. `session-elevation.md` § Elevating mirrors `OXICLOUD_AUTH_METHODS`
   with a branch per method, which already generalises — but the plan's
   framing is admin-only and should be widened, or renamed.
2. **The method-based layer cannot cover classes 2 and 3.** It is keyed
   on `/api/admin/*`; `security` routes live under `/api/auth/*` and the
   deletions are scattered across `/api/trash`, `/api/people`,
   `/api/recent`. Those rely on the per-operation declaration
   (§ Declaring session elevation), which is why the declaration is not
   redundant with the layer.
3. **"Important deletion" needs a criterion**, or it decays into
   "whatever felt frightening on the day". The usable one is
   **irreversible, not merely destructive**: the action destroys data
   that no undo path can recover.

   `DELETE /api/files/{id}` does *not* qualify — it moves to trash, and
   trash *is* the undo path. These do:

   | route | why |
   |---|---|
   | `DELETE /api/trash/empty`, `/api/trash/drive/{id}` | destroys the undo path itself, in bulk |
   | `DELETE /api/trash/{id}` | permanent deletion of one item |
   | `DELETE /api/people/data` | erases all face-recognition data for the caller |
   | `DELETE /api/drives/{id}` | destroys a shared drive and everyone's access to it |

   `DELETE /api/recent/clear` is deliberately **not** on the list: it
   discards a convenience index, not content. The line is "can the user
   get the bytes back", not "does the word delete appear".

## Credential-kind gating — app passwords are deny-by-default

### The hole this closes

`auth_middleware` accepts three credential kinds for the **whole** `/api`
surface, and the second is the problem (`middleware/auth.rs` ~305-313):

> 1. **Bearer JWT** … 2. **Basic Auth with App Passwords** — for DAV
> clients (DAVx⁵, Thunderbird, rclone) … 3. **HttpOnly Cookie**

So today an app password is a full-power REST credential. It can do
everything the SPA can — including `POST /api/auth/app-passwords` to mint
another one. Every ceiling in § Defaults is bypassable in one request by
the credential type most likely to be pasted into a config file, mailed
to oneself, or committed by accident.

Scopes cannot fix this. The vocabulary is resource-shaped on purpose
(§ Areas, not protocols), and `/api/files` and `/webdav` both
need `files` — so no scope distinguishes the browser API from the sync
door. This is a question about the KIND of caller, which is the
definition of a gate.

### Deny by default, permit explicitly

`AppPasswordGate::Denied` is the facet DEFAULT, so **every** route
refuses a Basic-auth app password until something says otherwise. Mounts
that legitimately accept one declare `.permit_app_password()`.

Positive opt-in rather than a `remove`-style subtraction, for a reason
beyond taste: `grep PermitAppPassword` then prints the **complete**
app-password attack surface in one command. A subtractive form cannot be
audited that way — you would be grepping for an absence, and a route
that silently inherits the default looks identical to one someone
deliberately opened. Only the positive marker ever appears at a
callsite, so the double negative never shows up in route code.

The permit set is exactly the sync surfaces, and nothing else:

| mount | why it needs the opt-in |
|---|---|
| `/webdav`, `/caldav`, `/carddav` | these handlers do **not** verify app passwords themselves — they rely on `auth_middleware`'s Basic branch, so removing the branch would break DAVx⁵, Thunderbird and rclone |
| `/remote.php`, `/ocs` | verify independently (`nextcloud/basic_auth_middleware.rs` ~171), but are still app-password surfaces by design |

`/api` is not on that list, which is the whole point. The Basic branch
stays — DAV needs it — and the gate rejects the resulting principal at
the `/api` door.

### What it requires

The principal must carry **how it authenticated**, not just who it is.
`CurrentUser` today cannot answer "was this a Basic app password or a
Bearer JWT", so the credential kind is threaded alongside `dpop_jkt` —
which is the precedent: that field exists precisely to answer a
session-shaped question without a DB round trip.

### Not in the spec's security array

The other gates declare as pseudo-scopes (`role:admin`, `elevated`)
because they are *requirements*. `PermitAppPassword` is a
**relaxation**, and an array of required scopes is the wrong place for
one — it would read as "this route requires an app password".

It also does not need to be there. The permit set is five mounts, none
of which appears in `resources/gen/openapi.json` (§ Deliberately
scopeless records that the spec holds no non-`/api` paths). So the
assertion is mount-level, not per-operation:

> No `/api` operation carries `PermitAppPassword`, and the permit set is
> exactly `{webdav, caldav, carddav, remote.php, ocs}`.

### Ceiling tightening that follows

With `/api` closed to app passwords, their scope ceiling drops `account`
— `/api/auth/me*` is unreachable by them anyway, and the NC avatar path
sits under `/remote.php/dav/avatars`, which is permitted. New ceiling:
`files,calendar,contacts`.

## Required: documentation

Not optional follow-up. Two of the three axes have no architecture doc
today, and the plan is not done while the only description of them is a
plan file — plans record *why a change was made*, architecture docs
record *how the system works now*, and a reader debugging a 403 needs
the second.

### New: `docs/architecture/authorization-axes.md`

One page covering all three axes and how a request is decided, with
ReBAC remaining the deep dive for Grant.

A new file rather than an extension of `rebac-authorization.md`, because
that document is focused and already disclaims this material. Its
§ What ReBAC does *not* cover says of global roles: "They live **outside
ReBAC** because they're cross-cutting concerns, not per-resource
permissions." That bullet *is* the Gate axis — correctly reasoned and
homeless. The new page is where it lands.

Contents, no more:

- the three axes and their one-line questions (Gate / Scope / Grant),
  plus the intersection rule;
- the worked example (§ Worked example: sharing a file) — the fastest
  way to make the three concrete;
- the scope vocabulary table with its routes;
- where each axis is enforced (router / router+spec / service) and why
  Grant is delegated;
- elevation, its three consumer classes, and the irreversibility
  criterion;
- links out: ReBAC for Grant detail, `auth-model.md` for how a
  credential is obtained, `session-elevation.md` for the elevation
  mechanism.
- the deny-by-default ranking (§ Maximise deny-by-default). It belongs
  in the architecture doc as well as `src/AGENTS.md`: the latter tells a
  contributor what to do, the former explains to a reader why a refusal
  is the default answer rather than a bug.

### Edits to existing docs — links, not copies

| document | change |
|---|---|
| `docs/architecture/rebac-authorization.md` | § What ReBAC does *not* cover — the "global roles in the JWT" bullet points at the new page instead of trailing off; § File map gains the policy module |
| `docs/architecture/auth-model.md` | scopes are minted where credentials are, so the app-password and device-flow material states what scope set each gets; § Related documents gains the link |
| `docs/architecture/index.md` | one entry beside the existing ReBAC line |
| `src/AGENTS.md` | the three-axis rule — see § Required: amend `src/AGENTS.md` |

Duplication is the failure mode to avoid: the scope vocabulary must
appear in exactly one place. Everything else references it, because a
second copy is a second thing to forget when `mail` stops being
reserved.

### User-facing

`docs/guide/dav-client-setup.md`, `webdav.md` and `caldav-carddav.md`
all walk a user through creating an app password. Once scopes exist,
those pages need to say **which scopes each client needs** — DAVx⁵ wants
calendar and contacts, rclone wants files, the NC desktop client wants
files plus account — or users will tick everything and the feature
achieves nothing. Follow `feedback_enduser_docs` style: what to click,
not what the server does.

The consent screen copy is documentation too, and the plan's § Defaults
already constrains it: render the *resolved* access ("Files — read and
write"), never the raw token.

### Already automatic

The published API reference needs no manual work. Scopes are declared in
`security(("bearerAuth" = [...]))`, so `resources/gen/openapi.json`
carries them, the Swagger page renders them, and `openapi-spec-drift`
keeps them honest. That is the second reason the declaration lives in
the annotation rather than a side table.

### Deliberately nothing

`docs/config/env.md` needs no entry: this design adds **no** operator
knob. The scope vocabulary is compiled in, the ceilings are per
credential type in code, and the one tunable in the area — the elevation
TTL — belongs to `session-elevation.md`. If a reviewer finds themselves
writing an `OXICLOUD_SCOPE_*` variable, something has gone wrong with
§ Non-goals.

### Acceptance

A reader who lands on `docs/architecture/index.md` asking "why was my
request refused?" reaches the three axes in one hop, and from there the
specific axis in two. If answering that needs a plan file, the
documentation is not done.

## Rollout

### Two PRs, not one

The halves have opposite risk profiles and must not share a green suite:

| PR | change | acceptance |
|---|---|---|
| 1 — gates | adds `is_external` and credential-kind to `CurrentUser` (§ Two fields), then `Gates` + the facet enums, then folds four layers plus the `transfer_ownership` check into them | **behaviour-preserving** — `external_users.hurl` stays green *unchanged*, which is the whole criterion |
| 2 — scopes | turns `device_codes.scopes` from an inert column into enforcement | **not** behaviour-preserving; carries every lockout risk below |

Landing them together means PR 2's risk rides in on PR 1's green suite.

### Absent means FULL, everywhere

Enforcement day finds three populations with no scope information, and
each must grandfather to the complete set rather than the empty one:

| population | why it has none | rule |
|---|---|---|
| JWTs in flight | minted before the claim existed | absent claim ⇒ every scope `:rw`. Precedent: `sid` is optional for exactly this reason, "None on tokens minted by pre-`sid` builds" — but note it is declared THREE times in two types (`auth_ports.rs:73` as `Option<Uuid>`, `auth_ports.rs:389` and `jwt_service.rs:68` as `Option<String>`), so "absent" is spelled differently depending on which one you are holding. A scope claim should be declared once |
| `auth.app_passwords` rows | no scope column exists today | migration backfills the full app-password ceiling, never NULL-as-empty |
| `auth.device_codes` rows | default is `webdav,caldav,carddav` | maps to files+calendar+contacts and **omits `account`** — widen existing rows, or a device credential that reads an avatar starts 401ing |

The third is the one that bites: the column is already populated, so it
looks like data to respect rather than data to migrate.

### Observation belongs on the dashboard, not in a log

A log-only phase was the obvious first answer and it is the wrong one:
"the operator is not an SRE team" (AGENTS.md § Purpose) means nobody
tails logs waiting for a line that may never come. An observation nobody
reads is not a safety mechanism.

So the pre-enforcement signal surfaces where the operator already looks
— the admin dashboard, in the shape the OPAQUE advisory banner already
uses:

> *N requests in the last 7 days would have been refused by scope
> enforcement*, broken down by credential and scope.

Zero is then a positive, visible signal that flipping is safe, rather
than an absence of evidence. Anything severe can also reach the existing
alert sinks, which are already wired for job findings.

### Wire contracts — fixed before the first line

Both are observable by clients, so changing them later is a breaking
change:

- **Status.** `403` + `WWW-Authenticate: Bearer error="insufficient_scope"`
  (RFC 6750 §3.1). Mirrors `middleware/dpop.rs`, which already answers
  `401` + `WWW-Authenticate: DPoP error="use_dpop_nonce"` — so the
  header convention exists in-house and this follows it rather than
  inventing one.
- **Audit.** Stay inside the `authz.*` domain aggregators already filter
  (`authz.denied`, `authz.admin_denied`, `authz.external_user_blocked`
  exist) rather than opening a `scope.*` one:
  `authz.scope_denied` with `reason = "scope_not_granted" |
  "access_insufficient"`, `authz.app_password_blocked`, and
  `authz.elevation_required`. Per AGENTS.md every denial emits one
  before returning.

### `sid` is on the caller

`CurrentUser` carries `token: PresentedToken`, whose `session_id` is the
JWT `sid` (merged with `authn-pipeline.md` part A). So `security` gets
the per-session form rather than ceiling-only, and the session facet's
`Elevated` reads `caller.token.session_id` once
`auth.sessions.elevated_at` exists.

It must live on the caller, not in request extensions: a gate is handed
`&CurrentUser` and nothing else, so extensions can serve a handler but
never a gate.

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
  nothing fails. Nine routes were unassigned until this table was
  cross-checked, four of them escalation paths.
- Unit: the elevation cross-check, **both directions** — an operation
  declaring `elevated` is covered by the method rule, AND every
  operation the method rule covers declares it. The second direction is
  the one that catches drift; without it the spec can claim elevation
  that nothing enforces, which is precisely what `src/AGENTS.md` means
  by "declaring is not enforcing". A mutating GET under `/api/admin`
  must fail this test rather than pass quietly.
- Hurl: elevation for a **non-admin**. A plain user with a fresh session
  is refused `DELETE /api/trash/empty` and `POST /api/auth/change-password`,
  succeeds after `POST /api/auth/elevate`, and is refused again once the
  TTL lapses. Without this the whole feature can ship admin-only while
  every admin pin stays green — which is how it is framed in
  `session-elevation.md` today.
- Hurl: app-password **deny-by-default**. The same app password is
  accepted on `/webdav` and `/remote.php/dav/files` and refused on
  `/api/files`, `/api/grants` and `POST /api/auth/app-passwords`. The
  last is the escalation that matters — a leaked app password must not
  be able to mint another.
- Unit: the permit set is exactly `{webdav, caldav, carddav,
  remote.php, ocs}` and **no `/api` operation carries
  `PermitAppPassword`**. A new mount that opts in without review fails
  here, which is the point of the positive marker.
- Unit: the public set — once § Authentication by default lands, every
  route reachable without a session carries `AnonymousAllowed`, and the
  set is exactly the login flows, the share-link surface, `/status.php`,
  `/.well-known` and the ops probes. A missing declaration must fail
  here rather than in production, because the failure mode is `POST
  /api/auth/login` returning 401.
- Migration: grandfathering, all three populations — a JWT with no scope
  claim, an app-password row backfilled from NULL, and a `device_codes`
  row still holding the legacy `webdav,caldav,carddav`. Each keeps
  working. This is the pin that prevents a silent lockout on upgrade.
- Unit: `require_all` is **conjunction** — a caller holding `elevated`
  but not `Admin` is REFUSED on `[Admin, Elevated]`, and so is one
  holding `Admin` but not `elevated`. Pin both halves: a disjunctive
  implementation passes the first silently and hands admin mutations to
  any user who re-authenticated.
- Unit: normalisation — `[Internal, Admin, Owner]` reduces to `[Owner]`
  and still refuses a plain user; `[PermitAppPassword]` cancels the
  default `NotAppPassword`; `[NotAppPassword, PermitAppPassword]` fails
  at startup rather than resolving silently.
- Unit: per-axis reduction. `[Internal, Admin, Owner]` yields exactly
  `[Owner, NotAppPassword]` — the role axis collapses to its strongest,
  the anonymity axis collapses because `Owner ⇒ NotAnonymous`, and the
  credential axis keeps its default because no role can imply it. Pin
  the last part especially: an Owner presenting an app password is a
  coherent caller this plan intends to refuse, so a reduction that drops
  `NotAppPassword` is a privilege widening.
- Unit: **the subsumption lattice and the invariant under it.** Assert
  `Owner.at_least(Admin)` AND that `users_external_not_privileged` still
  reads `NOT (is_external AND role <> 'user')`. The reduction
  `Admin ⇒ Internal` is only sound while that CHECK holds — relax it to
  permit an external admin and the flattening silently drops the
  internal requirement, which is a privilege widening produced by an
  optimisation.
- Unit: the `ScopeSet` mask invariant — `grant(s, ReadWrite)` sets BOTH
  bits, so `allows(s, ReadOnly)` is true afterwards; `grant(s,
  ReadOnly)` sets only `readable`. Pin it at the type, not per
  callsite: `rw` implying `ro` is the kind of invariant that holds in
  seven places and not the eighth.
- Unit: masks never reach the wire. Round-trip
  `"oxicloud:files:rw oxicloud:calendar:ro"` → `ScopeSet` → string and
  assert equality, so bit positions stay an internal detail. If an
  ordinal ever leaked into a token, adding a scope would silently
  reinterpret every credential already issued.

## When

At PR #853, which adds `POST /api/calendars` and the events surface —
the third hand-written copy of the per-route gate. Three is where the
duplication is real and the abstraction pays. Before that, this is
building the frame before there is a third wall.
