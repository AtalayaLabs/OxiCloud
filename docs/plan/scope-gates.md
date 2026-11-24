# Scope gates — one home for principal-kind checks

Branch: `feat/scopes`. **Trigger: PR #853**
(calendars REST surface) for the Gate-axis half — see § When. The credential-
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
| Policy engine — `Condition`, the compile step, the operation table | **not started** — PR 1, and 2 and 3 below depend on it |
| Credential scopes — `Scope`, `ScopeSet`, the 235-operation map | **not started** — PR 2 |
| Authentication by default (`NotAnonymous` inverted) | **not started** — needs the fold |
| A password-change condition | **never** — it is a scope narrowing, not a condition; the layer is deleted by `authn-pipeline.md` part B after PR 2. § Fold in |
| `authorization-axes.md` | **written** — gate/grant distinction, request diagram, enforcement points; scope section marks itself unenforced |
| `src/AGENTS.md` rule | **not started** |

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
- **`CurrentCaller` carries `token: PresentedToken`**, so `sid` is on the
  caller. That retires the choice § `sid` is no longer deferrable framed,
  and it is the prerequisite `Elevation` was waiting for.
- **`require_no_password_change_pending_layer` cannot be deleted by this
  plan** — the mechanism meant to replace it did NOT land. See the
  fold-in table.

## Glossary

Eight words, one meaning each. The design arrived at these after several
of them had been doing double duty, so they are used strictly below —
and the retired ones are listed so a stale usage is recognisable rather
than merely odd.

| word | means | does NOT mean |
|---|---|---|
| **axis** | one of the three authorization questions — Gate / Scope / Grant (`docs/architecture/authorization-axes.md`) | a field of a `Policy` — those are *conditions* |
| **`Condition`** (and "condition") | ONE requirement on ONE axis: `MinRole`, `MinPrincipal`, scope, elevation. An operation declares a SLICE of them; the compile step resolves each into a field of `Policy` | an axis — a `Policy` spans two axes, so its fields cannot be axes |
| **`Policy`** | declared `Condition`s folded root→leaf. Total, one per operation | a declaration |
| **`RequestPolicy`** | the trait evaluated PER REQUEST — axum, DAV dispatch, WS bus | anything at login |
| **`AdmissionPolicy`** | the trait evaluated PER AUTHN PHASE — login, refresh | anything per request |
| **`Admission`** | what `AdmissionPolicy` returns: `AdmittedCaller` + the minted `ScopeSet` | a check |
| **authn phase** | login, refresh — where `AdmissionPolicy` runs | a router layer, or "session phase": an app password creates no session yet still runs phases 1 and 3 on every request |
| **grant** | may this caller touch THIS resource — service layer, `authz.require` | a `Policy`, or a condition |
| **integrity** | signature, DPoP proof, CSRF — is the REQUEST authentic | an axis or a condition (§ Not axes) |

**Retired, and why:**

- **"gate" as a TYPE name** — it named three different things: the Gate
  axis, any caller-kind check, and `gate_layer`. No type carries it now
  (`MinRole`, `MinPrincipal`, `RequestPolicy`). **"Gate axis"** and
  **"Gate-axis condition"** remain correct, since Gate *is* one of the
  three axes; a bare "a gate" meaning "a check" is tolerated in prose but
  *condition* or *check* is preferred. This document's own title is
  historical.
- **"facet"** — introduced to separate a `Policy`'s fields from the three
  axes; superseded by *condition*, which does not reuse the word.
- **"`ScopePolicy`"** — one trait covering both moments, and named after
  one of four conditions.
- **bare "phase"** — always *authn phase*.

## Goal

One definition of "may this KIND of caller reach this surface at all",
callable from a router layer or a service, replacing the hand-wired
middleware layers that each decide the same class of question with their
own fail-open policy. Five exist; PR 1 folds THREE (§ Fold in) — of the
other two, one is a scope narrowing and one is an integrity check.

Two shapes: an operation declares a **slice of `Condition`s**, and the
compile step folds those into a `Policy` — total, every axis resolved.
§ Declare, compile, evaluate.

A `Condition` is one requirement on one axis. Declaring is therefore
order-free and additive, and the slice needs no entry point: `[a, b]` and
`[b, a]` compile to the same `Policy`.

```rust
// application/ports/scope_ports.rs

/// ONE requirement on ONE axis. An operation declares a slice of these.
///
/// Every variant TIGHTENS except `AnonymousAllowed`, which widens — and
/// whose conflict with a role is a boot error (§ 2b). So a slice cannot
/// express a silent contradiction; the only two it can express are both
/// refused at startup.
pub enum Condition {
    Role(MinRole),
    Principal(MinPrincipal),
    Scope(Scope, Access),
    // Elevated,                  // with auth.sessions.elevated_at
}

impl Condition {
    pub const fn role(r: MinRole) -> Self;
    /// No credential at all. The whole public surface, greppable:
    /// § Authentication by default.
    pub const fn public() -> Self;
    /// A public-share session (the ring) suffices. Also admits a user —
    /// `/api/files/{id}` serves both.
    pub const fn visitor_allowed() -> Self;
    pub const fn scope(s: Scope, a: Access) -> Self;
    // pub const fn elevated() -> Self;
}

/// What the engine EVALUATES: the declared `Condition`s folded root→leaf.
/// Total — the compile step resolves every axis, so no `Option` survives
/// and the check is field reads.
///
/// Properties of the CALLER. Never client-requestable — § Three axes.
pub struct Policy {
    /// `None` = any authenticated principal. Folds by MAXIMUM; a
    /// declaration that would widen is a boot error.
    pub role: Option<MinRole>,
    /// The least privileged principal admitted. NOT an `Option`: unlike
    /// role, there is no "any" answer — every operation admits some
    /// least principal, and the default is `User`, which is what makes
    /// "no declaration ⇒ authentication required" fall out rather than
    /// be enforced. § Authentication by default.
    pub principal: MinPrincipal,
    // pub elevation: Option<Elevation>,   // with auth.sessions.elevated_at
    /// Resolved scope requirement. Scope is its own AXIS, not a Gate-axis
    /// condition: client-requestable and frozen at mint, so it is checked
    /// against the credential's `ScopeSet` rather than the user row.
    pub scope: Option<(Scope, Access)>,
}

/// From the USER row, re-read live per request. ORDERED: each variant
/// implies the one before it, which is what makes the reduction a `max`.
///
/// `Admin ⇒ Internal` is provable from the `users_external_not_privileged`
/// CHECK constraint rather than conventional, so dropping `Internal`
/// beside `Admin` is sound. A test pins that dependency — removing the
/// constraint would quietly make the reduction wrong.
pub enum MinRole {
    Internal,   // not is_external
    Admin,      // role.at_least(Admin); Owner satisfies it
    Owner,      // UserRole::Owner, NOT storage.grant_role
}

/// The LEAST privileged principal an operation admits. Three states, not
/// two — "no credential at all" and "a share session" are different
/// things today and were both called *anonymous*:
///
///   * `Public`  — no credential. Public by MOUNT today (no
///     `auth_middleware`): `/api/auth/login`, `/api/setup`,
///     `/api/s/{token}`.
///   * `Visitor` — a public-share session exists, carrying a
///     `visitor_id` that "matches no `auth.users` row"
///     (`middleware/auth.rs`), password-challenged or not. Confined
///     today by `anonymous_allowlist_layer`, whose route list IS this
///     set: `/api/files/{id}`, `/api/folders/{id}/download`, …
///   * `User`    — an authenticated user session. The DEFAULT.
///
/// ORDERED ascending in strictness, so the fold takes the MAX, like
/// `MinRole` — and admitting a visitor also admits a user, which it must,
/// since `/api/files/{id}` serves both.
///
/// The password challenge on a protected share is NOT a fourth state: it
/// runs before the ring is minted, so by the time any operation sees the
/// caller there is simply a ring. That makes it an `AdmissionPolicy`
/// concern — no operation has an opinion about whether the share had a
/// password.
#[derive(Default)]
pub enum MinPrincipal {
    Public,
    Visitor,
    #[default]
    User,
}

/// NOT a v1 axis — there is no elevation state in the schema yet.
/// Designed in `session-elevation.md`, which adds
/// `auth.sessions.elevated_at`. Its `CurrentCaller` prerequisite is DONE:
/// the caller carries `token: PresentedToken`, so this reads
/// `caller.token.session_id`.
///
/// DPoP binding is deliberately NOT here — § Not axes: integrity checks.
// pub enum Elevation { Required }

```

### At the declaration site

```rust
// A SUBTREE: declared once; every child inherits, including later ones.
admin_handler::admin_routes(app_state)
    .policy([Condition::role(MinRole::Admin)])

// ONE CHILD TIGHTENS: says nothing about role, so Admin is kept.
.route("/sessions/{id}", delete(revoke_session)
    .policy([Condition::elevated()]))          // → [role(Admin), elevated]

// TWO CONDITIONS, order free.
.route("/change-password", put(change_password)
    .policy([Condition::elevated(),
             Condition::scope(Scope::Security, Access::ReadWrite)]))
```

Fully worked across `/api/admin`, `/api/auth` and `/api/files` — the
three principals, a scope floor tightened per method, and the visitor
surface — in `docs/architecture/authorization-axes.md` § How the axes
compose. Kept there rather than duplicated here: that page is where a
reader debugging a refusal looks.

**One `.policy()` per operation, and each axis at most once inside it.**
Both duplicates are boot errors (§ 2b):

- a second `.policy()` would be ambiguous, because repeated builder calls
  in Rust conventionally OVERRIDE — a reader would take the second as
  replacing the first;
- the same axis twice in one slice —
  `[Condition::role(Admin), Condition::role(Owner)]` — has no legitimate
  meaning. Across a NEST CHAIN that same pair is ordinary inheritance and
  folds to `Owner`; within one declaration it is someone writing two
  things where they meant one.

The distinction is the scope of the rule: **folding is for chains,
not for the slice.**

**`.policy()` has to RECORD, and that is the open implementation
question.** A `Policy` is not a layer — it goes into the compiled table —
but axum's `Router` is opaque, with nowhere to accumulate a
`(path, method, [Condition])` triple. Two shapes, and PR 1 should try the
first before settling:

1. an extension trait on `MethodRouter` / `Router` that records into a
   registry while returning the router, keeping the syntax above;
2. failing that, an explicit builder we own —
   `api.route("/auth/change-password", put(h), [Condition::scope(…)])`,
   returning `(Router, PolicyTable)` from `build()`.

The second always works and reads worse. Since the ergonomics of the
declaration site is half of why this design is worth building, the
difference is worth an afternoon's attempt rather than an assumption.

Either way the registry must be constructible **without a running app**,
because `generate-openapi` has no database — § The spec is rendered FROM
the policy table.

Adding an axis means one `Option` field, one constructor and one fold
rule. It cannot change how the existing axes fold, and it cannot change
any existing declaration — which is the property that makes this worth
building rather than hand-wiring the next check too.

**Four axes at request time: scope, role, principal, elevation.** The
first three ship in PR 1; elevation is not optional, only blocked —
`auth.sessions.elevated_at` does not exist yet, and three classes of
operation need it (§ Declaring session elevation: non-safe
`/api/admin/*`, `security` mutations, irreversible deletions). It lands
as a fourth `Option` field with no reshaping.

Two things that look like axes are not, and keeping them out is most of
why the list is this short:

- **credential kind is a MINT-time input, not a request-time axis.** The
  login phase already narrows by credential type —
  `effective = (requested ?? type_default) ∩ type_maximum`, § Defaults —
  so an app password reaching `/api/auth/password` is refused because its
  `ScopeSet` lacks `security`, not because something noticed it was an
  app password. `Condition::scope(Scope::Security, …)` is the whole
  mechanism; a credential-kind condition would be a second implementation of
  a decision already made once.
- **integrity checks** — JWT signature, DPoP proof, CSRF — § Not axes.

No password-change condition either: that is a scope narrowing too — § Fold in.

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

/// Two bitmasks rather than a map: unlike the role condition this is
/// paths with no DB round trip to hide behind (§ Lookup and checking
/// cost). Eight scopes fit a `u8`, and `rw` implies `ro` by
/// construction.
pub struct ScopeSet {
    readable: u8,
    writable: u8,   // ⊆ readable, maintained on insert
}

/// PER REQUEST — axum, the DAV dispatchers, the WS bus. The caller
/// already exists; this consumes one.
///
/// ONE method. A single requirement is a `Policy` with one condition set,
/// so a `require` / `require_all` pair would be an artefact of
/// representing requirements as a list.
pub trait RequestPolicy {
    /// Takes the CALLER CONTEXT, not a bare user id — § What a scope is
    /// bound to.
    async fn require(&self, policy: &Policy, caller: &CurrentCaller)
        -> Result<(), AppError>;
}

/// PER AUTHN PHASE — login and refresh. There is no caller yet; this
/// PRODUCES one.
///
/// Separate trait rather than a second method, so the phases cannot be
/// confused by construction: this one takes a `&User` and no
/// `CurrentCaller`, so it cannot be called per request; `RequestPolicy`
/// needs a `CurrentCaller`, so it cannot be called at login. The handoff is
/// one-directional — admission mints the credential the request side
/// reads.
pub trait AdmissionPolicy {
    async fn admit(
        &self,
        user: &User,
        kind: CredentialKind,
        requested: Option<ScopeSet>,
    ) -> Result<Admission, DomainError>;   // AdmittedCaller + minted ScopeSet
}
```

Note the asymmetry, which is the structural reason these are two traits:
**only the request side is declarative.** Operations declare conditions,
compiled to a per-operation `Policy`. Admission has no per-operation
declaration — it is global config plus the credential's ceiling
(§ Defaults). The absence is correct, not missing.

## Declare, compile, evaluate

Three stages, because the two things wanted of this design pull in
different directions: declaring a requirement must be easy, and deciding
one must be cheap and total.

### 1. Declare — a slice of `Condition`s

An implementor says only what they constrain. One method per axis:

```rust
/admin              [Condition::role(MinRole::Admin)]
/admin/sessions/*   [Condition::elevated()]            // inherits Admin
/admin/fake-route   [Condition::role(MinRole::Owner)]  // tightens role only
/api/auth/password  [Condition::scope(Scope::Security, RW)]
```

A declaration names only the axes it constrains. An axis it omits is
**inherited**, which is what lets a child add elevation without
re-asserting — and thereby overtightening — what a parent had widened.

A new axis is a new `Condition` variant plus a constructor and a fold
rule. It cannot change how existing axes fold, nor any existing
declaration, which is what makes the engine extensible without a rewrite.

### 2. Compile — fold root→leaf, once at startup

Per axis, **tighten-only**:

| axis | fold | widening attempt |
|---|---|---|
| role | `max` (`Owner > Admin > Internal`) | **boot error** |
| app-password | `Denied` wins | needs an explicit `Override` |
| principal | strictest wins (`User` > `Visitor` > `Public`) | a declaration that WIDENS is a boot error — which is what `anonymous_allowlist_layer` already is |
| scope | union of what is required | — |

The result is a `Policy`: **total**, every axis resolved, no `Option`
left. One per operation.

### 2b. Compile-time validation — the reason the split pays

A fold has to walk every declaration anyway, so it can refuse to produce
a `Policy` that no caller could satisfy or that contradicts itself. These
are **boot errors**: "won't start", the second tier of § Maximise
deny-by-default, and unreachable today because a stack of independent
layers has no moment where all the declarations are in one place.

| refused at boot | why |
|---|---|
| `public()` or `visitor_allowed()` **+ any role** | structurally unsatisfiable: `UserRole::Anonymous` is "**never stored in `auth.users`**… it exists only on a session and its JWT, and `from_stored` deliberately cannot produce it" (`user.rs:21`). No role can ever be satisfied, so the route is unreachable and one of the two declarations is a lie. |
| `public()` or `visitor_allowed()` **+ elevation** | elevation is a human re-proving themselves against an account. There is no account. |
| `public()` or `visitor_allowed()` **+ a scope** | a share-ring visitor carries no `ScopeSet`; the requirement can never be met. |
| a role declaration that **widens** its parent's | `Owner` nest, `Internal` child: the fold is `max`, so the child's intent is silently discarded. Better to refuse than to ignore. |
| an operation with **no** policy | from the spec cross-check below. Deny-by-default means an undeclared operation is a boot failure, not an open door. |
| a policy naming **no live operation** | a policy left behind by a deleted route, which would otherwise sit unmatched forever. |
| **the same axis twice in ONE declaration** | `[Condition::role(Admin), Condition::role(Owner)]` has no legitimate meaning — someone wrote two things where they meant one. Across a NEST CHAIN the same pair is ordinary inheritance and folds to `Owner`; folding is for chains, not for the slice. |
| **two `.policy()` declarations** on one operation | ambiguous: repeated builder calls conventionally override, so accumulating would read as the second replacing the first. Put both conditions in one slice. |

The first row is the one worth having. An implementor writing
`public()` on a route under `/admin` means *"make this
public"*, while the nest says *"admins only"* — and whichever wins, the
other declaration was wrong. Stacked layers resolve that to
"unreachable" in silence; the compile step makes someone decide.

Note what is NOT a contradiction: an admin whose credential is scoped
`files:ro`. Role and scope intersect by design (§ Three axes) — the
admin simply cannot use *that credential* for admin work.

### 3. Evaluate — one `Policy`, one check

Field reads only. Role's cached read already happened in
`auth_middleware` (§ Lookup and checking cost), so a policy check adds
nothing per request — and it runs **once**, not once per nesting level.

## Keyed by operation, not by route

A `Policy` cannot hang off an axum route, because on three of the four
surfaces a route is not the unit of authorization:

| surface | axum routes | operation identity |
|---|---|---|
| REST | 279, one per operation | (method, path) — a layer works |
| WebDAV / CalDAV / CardDAV | **one** `any` handler each | the DAV method (PROPFIND, MKCOL, MOVE…) |
| WS bus | **one** route, `/api/rt/ws` | the `rt.*` method name |
| collab data plane | the same single route | the binary frame kind (`0x01`, `0x02`, SYNC) |

So the compiled table is keyed by operation identity, and the axum layer
is **one lookup site among several**. The DAV dispatcher,
`handle_text_frame` and `handle_binary_frame` each look up the same table
with their own key.

This is not a generalisation for its own sake — it is where the bugs
were. The AWARENESS frame kind (`36c8c6a37`) and the CardDAV door to
`create_address_book` were both ungated precisely because neither has a
route to put a layer on.

It also corrects an earlier reading of the three `.require(...)` calls in
`rt_ws.rs`: they are **not** misplaced grant checks that belong in a
service. `handle_subscribe`, `handle_collab_capabilities` and
`handle_collab_flush` are method dispatch inside one route, so no layer
can reach them. They are the WS surface's policy checks written by hand
because this mechanism does not exist yet, and they become table lookups.

### Both surfaces are already enumerated

`resources/gen/openapi.json` lists 279 REST operations;
`resources/gen/asyncapi.json` lists the `rt.*` methods. So the table is
cross-checkable in both directions: every operation in either spec must
have a compiled policy, and every policy must name a live operation. An
ungated frame kind fails that check as readily as an ungated route.

### Consumers that are not a lookup

- **`InternalCaller` capability token** — for a top-level create, which
  has no parent resource and therefore no grant to check. The only
  deny-by-default form where forgetting does not compile. § Why 2.
- **Extractors** (`InternalUser` / `AdminUser`) — weakest, since
  forgetting leaves the route open. Prefer the token wherever a service
  stamps ownership.

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

Hence `require(gate, caller: &CurrentCaller)`: a bare `Uuid` cannot
express the session half, and a signature taking one quietly forces
every condition to be user-bound.

`CurrentCaller` (`dtos/user_dto.rs:621`) carries `id`, `username`,
`email`, `role` (**from the JWT claim**, hence stale-prone, hence
`decide_live_role`), `dpop_jkt` — and now `token: PresentedToken`, the
`sid` and `iat` the credential presented. **This question is closed; the
field is in.** `Elevation` can read `caller.token.session_id`
without reshaping `require`.

It carries the whole `PresentedToken` rather than a bare `sid` because
both claims come from the same credential and `resolve_live_role` needs
them together; a second encoding of the same fact is how two channels
drift apart. Which is why the field replaced a request-extension publish
that had briefly served the same purpose: `CurrentUserId` is itself
derived from `extensions.get::<Arc<CurrentCaller>>()`, so the caller struct
was already reachable everywhere the extension was, and one channel beats
two.

Extensions could not have served a `Gate` in any case — `require(gate,
caller: &CurrentCaller)` passes the caller and nothing else, by design, so
a condition evaluator has no route to the request. That is what decided
question: the fact has to be on the caller.

`#[serde(skip)]`, like `dpop_jkt` — this is a serialized DTO with an
OpenAPI schema, and internal request state must not reach the wire.
Verified by regenerating `resources/gen/openapi.json`: no diff. CI diffs
that file, so the skip is guarded by the artifact rather than by a unit
test.

Follow the precedent in that file: `CurrentCaller::role` resolves an
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
Inside the Gate axis are four independent **conditions** — role,
role, principal, scope, elevation — which are fields of one
type (§ Goal), and reduction never crosses between them (§ Reduction is
per condition).

Consequences, both agreed:

- **`/api/admin/*` is a role check, not a scope.** `MinRole::Admin`, read
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
| Gate | the principal condition at its default `User` — enforced today by `anonymous_allowlist_layer` | router layer |
| Scope | `oxicloud:sharing:rw` | router layer / extractor |
| Grant | `authz.require(Subject::User(caller), Permission::Share, resource)` — owners short-circuit | `grant_handler.rs` |

The condition is ANONYMITY, **not** role — and the difference is easy to
blur. `MinPrincipal::User` refuses a visitor with no user row;
`MinRole::Internal` refuses an external USER, who has one.

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
`anonymous_allowlist_layer`'s route list IS the `Visitor` set, so each of
its entries becomes `[Condition::visitor_allowed()]` on that route and the
layer goes away.

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
| `mail` | **reserved** — no mailbox exists; the parser REJECTS it, since a name that parses but guards nothing is the device-flow bug again |

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
`MinRole::Admin`; `/api/search` and `/api/rt/ws` read across areas and are
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

Plus 2FA/TOTP when it lands — none exists yet.

Self-service session management SHIPPED (`7fab8811c`) and belongs here:
`GET /api/auth/sessions` is `security:ro`, `DELETE /api/auth/sessions/{id}`
is `security:rw` — revoking a credential is a credential operation.
`/api/admin/sessions` is the other exposition of the same low-level
functions and is `MinRole::Admin` instead, which is the pattern worth
noting: two operations, two policies, one implementation
(`list_sessions_scoped`, `revoke_session_and_invalidate`). The policy is
per-operation; the logic is shared.

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

**`Elevation` is the second factor, and this is its first real
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
  right whatever to change the password. Those are different questions,
  and not even the same KIND of question: the DPoP proof is an integrity
  check (§ Not axes), `Scope::Security` is what the credential was issued
  to do.
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
may transfer ownership", which becomes `MinRole::Owner` instead of a
hand-rolled check.

Two implementation notes, because both are traps:

- **`MinRole::Admin` is `role.at_least(Admin)`, not `role == Admin`.** The
  entity doc is explicit that Owner "outranks `Admin`, so it satisfies
  every admin check without those checks naming it". An implementer who
  writes equality locks the instance owner out of the admin panel.
  `UserRole::at_least` already exists; the condition delegates to it rather
  than re-deriving a comparison.
- **`UserRole::outranks` is NOT a `Gate`.** It is the strict companion,
  used for "may this admin act upon *that* admin" — it takes a target,
  so by the rule above it is a grant question. Do not try to express
  "may demote this user" as a condition.

### What this actually fixes

Not callsite brevity. The hole that motivated this
(`create_address_book` / `create_playlist` reachable by externals over
REST while the CardDAV door to the same service method was gated) was
not "the check was hard to call" — it was "nobody noticed there was
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
   (`caller: InternalUser`) only makes the check *visible* — forget it
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

   impl dyn RequestPolicy {
       pub async fn require_internal(&self, caller: &CurrentCaller)
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

### Fold in (three of the five layers, plus one hand-rolled check)

| Today | Becomes |
|---|---|
| `require_internal_user_layer` — SIX sites, not 11: three DAV nests in `main.rs` (**removed**, `06a046593`) plus three per-METHOD sites in `routes.rs` | `[Condition::role(MinRole::Internal)]` — on the six DAV operations, and method-qualified for the three REST sites, which `.policy()` cannot express because a `MethodRouter` hides its methods |
| admin middleware on `/api/admin/*` (3 sites) | `[Condition::role(MinRole::Admin)]` |
| hand-rolled owner check in `transfer_ownership` | `[Condition::role(MinRole::Owner)]` |
| `anonymous_allowlist_layer` (1 site) | `[Condition::visitor_allowed()]` on the allowlisted routes; keeps the allowlist itself, and is the mechanism § Authentication by default would promote to the root |
| `require_no_password_change_pending_layer` (8 sites) | **untouched** — a scope narrowing, deleted later by `authn-pipeline.md` part B. Below. |
| `require_dpop_layer` (8 sites) | **untouched** — an integrity check, not an axis. § Not axes: integrity checks |

`anonymous_allowlist_layer` is the loose fit — it carries a route
allowlist and must stay `route_layer` (it needs `MatchedPath`). Fold the
*decision* and leave the allowlist where it is.

**A DAV operation is `(area, access)`, not `(surface, method)`.** The
method carries exactly one authorization-relevant bit — read or write —
so keying by it spends 36 declarations on six facts; and the AREA rather
than the protocol is what a scope names, since a protocol vocabulary
"would name the same permissions twice" (§ Areas, not protocols). An
unlisted method classifies as a WRITE, fail-closed.

**The NextCloud DAV doors are NOT in PR 1's fold.** They reach the same
areas, but no folded layer covers `/remote.php/*`: the NC router carries
only `access_log`, and `require_internal_user_layer` appears nowhere under
`interfaces/nextcloud/`. Declaring `MinRole::Internal` for an NC
operation would ADD a gate rather than fold one.

That exposes an asymmetry worth its own diff: native `/webdav` refuses an
external user and `/remote.php/dav/files` appears not to — the same
"a gate on one door is a hole in the other" shape as
`create_address_book`. `/remote.php/webdav` is a 301 to the former and
serves no data itself. `/remote.php/ocs` is likewise untouched by PR 1;
its SCOPES (`sharing`, `account`) are PR 2.

**Gate and Scope on the declaration; Grant in the service.** This is what
the whole design reduces to, and it is why the table is keyed by
OPERATION rather than by route. AGENTS.md puts AuthZ in the service layer
for coverage — REST, DAV and WS share service methods, so a gate on one
door is a hole in the other — and a router layer can only gate the door
it is attached to. `Op::Rest` / `Op::Dav` / `Op::Ws` / `Op::CollabFrame`
name every door of one service method, so **declaring each door is the
remedy, not keeping the gate in the service.**

A role check inside a service is therefore misfiled rather than
deliberate. Count the doors before moving one —
`git grep '<method>(' -- src/ | grep interfaces/` — to be sure every one
is declared; declaring a single door and deleting the service check is
the `create_calendar` / `create_address_book` bug again.

`transfer_ownership` is the easy case, verified:
`auth_application_service::transfer_ownership` has exactly one caller in
`interfaces/` (`admin_handler.rs`), so one `Op::Rest` declaration
replaces its `caller.role != UserRole::Owner` with nothing left
uncovered.

**But folding it changes an audit event, and that needs a decision.**
Today the denial emits `event = "server_owner.transfer_rejected"`,
`reason = "caller_not_owner"`. A router policy denial emits an `authz.*`
event instead. AGENTS.md: `event` "stays stable — log aggregators key off
it". So this fold silently retires an event an operator's alerting may
key on — and neither the policy dump (fields only) nor a status-code
matrix (HTTP only) would notice. Either keep emitting the old event from
the policy denial path for this operation, or treat it as a breaking
audit change and say so in the release notes.

### Not axes: integrity checks

A separate category, and keeping it separate is what stops the axis list
growing without limit. These ask **"is this request authentic?"** — a
precondition to authorization, not a part of it:

| check | asks |
|---|---|
| JWT signature + expiry | was this token issued by us, and is it still valid? |
| DPoP proof (RFC 9449) | does the holder of this bound session possess the key? |
| CSRF double-submit | did this state-changing request come from our own page? |

Three properties they share, none of which a declarable axis has:

- **the operation has no opinion.** `require_dpop_layer` is applied to
  all 8 nests identically, and `csrf_middleware` to 7; what varies is
  `OXICLOUD_DPOP_MODE` — an env var, not a route.
- **`None` is meaningless.** "This route does not care whether a bound
  session proved possession" is not a sentence anyone wants to write.
- **failure means the REQUEST is untrustworthy**, not that the caller is
  the wrong kind. The response says so: `401` +
  `WWW-Authenticate: DPoP error="invalid_dpop_proof"`, never a 403.

So `require_dpop_layer` is not folded. It belongs at the root choke point
§ Authentication by default creates, beside CSRF — declared once for the
whole surface rather than per operation.

What survives of the session axis is **elevation**, which does vary per
operation (`/admin/sessions/*` needs it, most routes do not) and where
`None` means something. It waits on `auth.sessions.elevated_at`.

**The password-change layer is not a Gate-axis condition at all** — it is a SCOPE
narrowing, so it never needed a condition. Admission narrows a forced-reset
session to `{security}`; anything outside that scope is refused, and the
FE redirects to password settings rather than showing an error.

Hence the order, with no cycle:

1. **PR 1** leaves the layer's eight mount points untouched. No
   `PasswordChangeSettled` condition is built — it would be a third home for
   a check that is about to disappear.
2. **PR 2** ships `Scope` and its enforcement.
3. **`authn-pipeline.md` part B** then narrows at admission and deletes
   the layer. Its "needs `scope-gates.md`" means needs PR 2, and PR 2
   needs nothing from it.

(`authn-pipeline.md` § The forced-reset deadlock is a different, runtime
concern: a `{security}`-narrowed session needs elevation via the password
it is being forced to change. It only exists once step 3 does.)

### Keep as layers, not extractors

`/webdav`, `/caldav`, `/carddav` dispatch through one `any` handler, so
a layer is both natural and cheaper there. Extractors are for REST
handlers only.

### The field `CurrentCaller` must carry first

An axis-policy is a field read only if the caller actually carries it,
and the caller now carries every field the four conditions read:

| condition | reads | carried as |
|---|---|---|
| `MinRole` | the role | `CallerKind::Internal(UserRole)` |
| `MinPrincipal` | public / visitor / user | the `CallerKind` variant itself |
| scope | the token's scope set | `PresentedToken` |
| elevation | the elevation stamp | `PresentedToken` |

`CurrentCaller.kind: CallerKind` is one `Copy` enum:

```rust
pub enum CallerKind {
    Visitor,              // public-share session: no auth.users row
    External,             // role is necessarily User (a schema CHECK)
    Internal(UserRole),   // user | admin | owner
}
```

It replaced a `role: SmolStr` + `is_external: bool` pair that could
express three states which cannot exist (anonymous+internal,
anonymous+external, admin/owner+external — the last already forbidden by
`CHECK (NOT (is_external AND role <> 'user'))`). `MinPrincipal` therefore
compares variants rather than parsing a string and consulting a bool, and
`require_internal_user` no longer issues a second `get_user_flags` per
DAV request: the flags pass that resolves the role resolves the kind.

This follows `token: PresentedToken` (`9a42e4299`) — the middleware holds
the fact once, rather than dropping it and leaving a consumer to
re-derive it or to be unable to ask.

### Per-route conditions to retire

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

The 30 s TTL carries a second consequence worth stating: a Gate-axis answer
can be up to 30 s stale. For `is_external` and role that is the intended
trade (§ What a scope is bound to), and every path that tightens already
calls `invalidate_user_flags` so the window closes immediately rather
than on expiry. A future condition whose staleness is NOT acceptable must
invalidate on change, not shorten the TTL for everyone.

This paragraph is about **Gate-axis conditions only**, and said "scope" before the two
axes were separated. Scopes resolve the other way round on purpose: they
ARE a claim on the token, because unlike `is_external` a scope cannot
change under its credential (§ Freezing at mint), so there is nothing to
invalidate and a hot-path check is free.

### Checking cost: role and scope are not alike

The two axes have opposite cost profiles, and representing them the same
way gets one of them wrong.

| | gate | scope |
|---|---|---|
| source | `auth.users`, re-read live | JWT claim, frozen at mint |
| per check | cached read — moka, 30 s TTL, single-flight; DB on a miss | pure CPU |
| staleness | up to the TTL, closed early by `invalidate_user_flags` | none — cannot change under its credential |
| paths | cold by design, EXCEPT the defaults — see below | **hot** — every `/webdav` request, every `/api/files` listing |

Neither axis is dominated by a query, so **representation is not a
performance question for either of them**, and the choice of one field per axis is
made on representability alone: it makes a contradictory pair impossible
to construct, where a slice makes it impossible to miss only if something
checks. § Reduction is per condition.

The scope masks are still the right call, for their own reason rather
than by contrast with a round trip: a scope check runs on paths AGENTS.md
names as hot, several times per DAV request, and `u8` masks make it one
`&` and a compare with `writable ⊆ readable` held as an invariant — so
`rw` implying `ro` is not a branch at every callsite.

### Per-request policy vs per-AuthN-phase check

Two decision points, and placing a check in the wrong one is the easiest
mistake this design can still make. Both are needed; neither substitutes
for the other.

| decided at | checks | the question |
|---|---|---|
| **AuthN phase** — login, and **re-run at every refresh** | account active · verified email · OPAQUE migration · credential-kind → scope ceiling · forced-reset narrowing | *may this account have a credential at all, and what is it issued for?* Failure means **no session**, not a refused request. |
| **Per request** | role · principal · scope · elevation | *may this caller reach this operation?* Failure is a refused request on a valid session. |

**Verified email is the worked example**, because it looks like it could
be an axis and is not. The question is whether the account may hold a
session at all — uniformly, for every operation — which is why the
exemption is by ROLE (`Owner` bypasses it, admins do not) rather than by
route. No operation has an opinion about it, so by § Not axes it is not
an axis.

"AuthN phase" does not mean "once, forever". Refresh re-runs admission
(`authn-pipeline.md` § Refresh is a re-verification ceremony), and an
active client refreshes at the access-token lifetime — so an authn-phase
check is re-evaluated roughly hourly, and the staleness it admits is
bounded by that rather than by the 7-day refresh window.

Which is what makes the split safe: a per-request check is for facts that
must bite *immediately*, an authn-phase check for facts that may bite *at
the next ceremony* — and `authn-pipeline.md`'s rule covers the gap
("every state change that **tightens** must either revoke sessions or be
in the per-call read").

### Which per-request axis is evaluated how

| axis | source | changes mid-credential? | evaluated |
|---|---|---|---|
| role (`Internal`/`Admin`/`Owner`) | `auth.users`, via `user_flags_cache` | **yes** — promote, demote, guest→internal | already resolved by `auth_middleware` into `CurrentCaller.role` |
| principal | the credential | no | field read |
| scope | the credential's `ScopeSet`, frozen at mint | no | mask `&` and compare |
| elevation | `auth.sessions.elevated_at` | yes — it expires | pending the column |

So a root gate layer adds **no lookup** to an authenticated request: the
one cached read already happens in `auth_middleware`, and the rest are
field reads — once `CurrentCaller` carries `is_external`.

Cost does not decide placement; staleness does:

1. **Does any operation have an opinion about it?**
   - **No** — it is an authn-phase check, uniform across the surface.
     Verified email, account active, the scope ceiling.
   - **Yes** — it is an axis; question 2 places it.
2. **Can the fact change while the credential is alive?**
   - **No** — read it off the credential per request: free, no
     staleness, nothing to invalidate. Principal and scope are here.
   - **Yes** — per-call read. `user_flags_cache` makes it free and
     `invalidate_user_flags` closes the TTL window. Role, `is_external`
     and elevation are here.

The test an implementor can actually run: *state what happens if this
fact changes one second after login.* If the answer is "the caller keeps
something they should have lost", it is per-call or the change revokes.

This belongs in `src/AGENTS.md` — § Required: amend.

### The defaults put a check on every request

"Cold by design" holds for the named gates, not for the defaults.
`MinPrincipal::User` as the default only bites if every
route is gated — the root choke point § Authentication by default argues
for — so the design does put a check on listing and thumbnails.

The cache makes that affordable, but **measure it before wiring a root
layer**, and measure the right thing: not steady-state hit cost but
single-flight at the 30 s boundary under K concurrent requests for one
user, which is a sync client's shape. `examples/bench_auth_herd.rs` is
that shape for the sibling cache. Benches run against 5433, not 5432.

(§ Rejected: default-deny across all of `/api` refuses a root
`MinRole::Internal` on correctness grounds — the external exemption list
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
`a compiled `Policy` requiring `MinRole::Internal`` on the whole protected `/api` router plus
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
| 4. **Fails closed at runtime** | the default answer is refusal | ceilings as allowlists; `CurrentCaller::role` resolving an unknown value to `Anonymous`, "the *least* privileged answer" |
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

### Reduction is per condition

**Reduction is per condition, never global.** The conditions are independent, so
a role conjunction collapses to its maximum while the app-password condition
stays untouched; reducing across conditions would silently drop a condition.

One field per condition makes both problems vanish instead of needing checks:
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

That makes the app-password CEILING a **deprecation instrument** rather
than a permanent allowance: as each sync surface moves to the device
flow, the ceiling narrows by one area, and when the last goes the
credential type can be retired. Each step is reviewable instead of a
flag day.

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
  away rather than becoming a condition. Until part B lands, PR 1 leaves the
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
> - **Gate** — which KIND of caller may reach this surface, as a slice of `Condition`s
>   value naming the conditions it needs (role, principal, scope, elevmity,
>   session-binding). Declared even when the answer is "any
>   authenticated caller", which is an empty slice and still written
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
>    (ceilings as allowlists;
>    `CurrentCaller::role` resolving an unknown value to `Anonymous`).
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
  (`["role:admin"]`)."** Gate requirements therefore declare as `role:` pseudo-scopes
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
`MinRole::Admin` **and** `Elevation`. OpenAPI puts conjunction inside
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
// elevation axis exists — not v1.
policy.require(compiled, caller).await?;   // [role(Admin)] + [elevated()]

// Two calls are EQUIVALENT here, and that is a property of the conditions
// being independent: `role` and `elevation` cannot overwrite one
// another, so sequencing two requires composes rather than conflicts.
// The single call is preferred for being one audit line, not because
// the pair is wrong.
// Declared separately, folded by the compile step into ONE `Policy`:
//   /admin [role(Admin)]  +  /admin/sessions [elevated()]
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
reason an admin does. Three consumer classes, so `Elevation` is not
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

**Scope closes it, and no credential-kind axis is needed.** Every route
in the hole above is in the `security` area — § Defaults classifies
`/app-passwords`, `/change-password`, `/oidc/link/start`,
`/oidc/unlink`, `/upgrade-to-internal`, `/opaque/register/*`,
`/device/devices` and `/device/verify` there — and the app-password
ceiling grants **no `security`**. So an app password minting another one
is refused by `Condition::scope(Scope::Security, …)`, not by anything
noticing it was an app password.

That makes credential kind a MINT-time input rather than a request-time
axis: `effective = (requested ?? type_default) ∩ type_maximum` already
decides, once, what each credential type can reach. A condition would be a
second implementation of the same decision, and the two would drift.

What scope genuinely cannot express is a PROTOCOL distinction —
`/api/files` and `/webdav` both need `files`, so no scope admits an app
password to the sync door while refusing it on the browser API. That is
accepted rather than worked around, for two reasons: the restriction
would add nothing (same bytes, same permission, different verb), and
protocol-shaped restriction is what § Areas, not protocols rejects for
the scope vocabulary in the first place. Wanting it here would be
wanting the shape the plan already argued against.

### Deny by default, permit explicitly

The ceiling is an **allowlist** per credential type, so the deny-by-
default property lives there: a new credential type whose author never
considers `security` gets it **withheld**. Forgetting is safe.

That is also better than the per-route opt-in this section used to
propose. The auditable artifact is **one table** — § Defaults — rather
than a set of call sites to grep for; "what can an app password reach?"
is answered by reading a row, not by trusting that every route that
should have declared an opt-in did.

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
`CurrentCaller` today cannot answer "was this a Basic app password or a
Bearer JWT", so the credential kind is threaded alongside `dpop_jkt` —
which is the precedent: that field exists precisely to answer a
session-shaped question without a DB round trip.

### The spec is rendered FROM the policy table, not read into it

`openapi.json` already carries the structure: 228 operations emit
`security: [{"bearerAuth": []}]` — an empty scope array — and 21 emit
`security: []` for public routes. So showing scopes needs no new
mechanism, only a non-empty array.

The direction matters, and it is engine → spec:

| | |
|---|---|
| spec → engine | declarations become stringly-typed; inheritance is lost (`role:admin` repeated per operation instead of once on the nest); a docs-only edit changes enforcement; and the DAV / WS surfaces carry no `#[utoipa::path]` at all |
| **engine → spec** | one declaration site, with inheritance; the spec is accurate **by construction** because it renders the compiled `Policy`; `generate-openapi` plus CI's existing drift check (`ci.yml:303`) turn a forgotten regeneration into a CI failure |

So the compiled table is the source and the spec is a projection of it.
A policy change then reaches Swagger and the generated TypeScript client
automatically, and cannot silently disagree with what is enforced —
which is stronger than the cross-check this plan proposed earlier, since
drift becomes impossible rather than merely detectable.

**Constraint this imposes:** `generate-openapi` runs with no database, so
the declarations must be constructible without a running app. The registry
therefore lives on a route registry built from `const` values, never
assembled inside `AppState`.

Gate conditions project as pseudo-scopes (`role:admin`, `elevated`)
because they are requirements, even though no client may request one. A
*relaxation* must never appear there — it would read as "this route
requires an app password" — which is moot now that credential kind is a
mint-time input (§ Credential-kind gating) rather than a condition.

### Ceiling: app passwords get no `account`

`/api/auth/me*` is in the `account` area, and an app password has no
business reading or writing a profile — the credential exists for sync
clients. The NC avatar path sits under `/remote.php/dav/avatars`, which
is `files`-shaped, so dropping `account` costs DAVx5 and rclone nothing.

Ceiling: `files,calendar,contacts`.

This used to be derived from "`/api` is closed to app passwords, so
`account` is unreachable anyway". That argument died with the
credential-kind condition — nothing closes `/api` by credential type any
more. The tightening stands on its own: the ceiling is an allowlist, and
`account` was never justified in it.

## Required: documentation

Not optional follow-up. Two of the three axes have no architecture doc
today, and the plan is not done while the only description of them is a
plan file — plans record *why a change was made*, architecture docs
record *how the system works now*, and a reader debugging a 403 needs
the second.

### `docs/architecture/authorization-axes.md` — WRITTEN

One page covering all three axes and how a request is decided, with
ReBAC remaining the deep dive for Grant.

Written ahead of the fold because the gate/grant distinction and the
request path are true TODAY; what is not yet enforced says so. Still
owed once PR 1 and PR 2 land: the `Condition` / `Policy` types, the scope vocabulary
table, and elevation.


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
| 1 — gates | a caller carrying what the conditions read; `Condition` / `Policy` + `MinRole` / `MinPrincipal`; the compile step, the boot refusals and the operation table; then fold three layers plus the `transfer_ownership` check | **behaviour-preserving**, and provable rather than hoped: the compiled table before and after the fold must be IDENTICAL, with `external_users.hurl` green *unchanged* |
| 2 — scopes | turns `device_codes.scopes` from an inert column into enforcement | **not** behaviour-preserving; carries every lockout risk below |

Landing them together means PR 2's risk rides in on PR 1's green suite.

#### PR 1 — status

| # | step | state |
|--:|---|---|
| 1 | the caller carries what the conditions read | **done** — `f481cfa8f` · `17f44e3b4` · `401c250a5` |
| 2 | `Condition` / `Policy` + `MinRole` / `MinPrincipal` (`application/ports/scope_ports.rs`) | **done** — `7ce329192`, inert |
| 3a | the compile step + the five chain-local boot refusals | **done** — `1167e58ba`, inert |
| 3b-i | the registry, the table, and the DUMP | **done** — `5b7ddb403`, inert |
| 3b-ii | `.policy()` + the orphan cross-check | **done** — `9f2525edd`, inert |
| 4a | the gate matrix — a behavioural baseline, before any layer moves | **done** — `4fc3fd79f`, green |
| 4-i | `evaluate` — one caller against one policy, audit-neutral | **done** — `479e5f38b`, inert |
| 4-ii | transcribe the four folded gates | **done** — `a037c5ab7`, inert |
| 4-iii | the policy gate layer | **done** — `1f521fbed` |
| 4-iv | SHADOW on the three DAV nests | **done** — `7eccaa28e`, needs a matrix run |
| 4-v | SHADOW on `/api/admin` (incl. the Owner declaration) | **done** — `bd9359030`, needs a matrix run |
| 4-vi | SHADOW on the whole protected `/api` subtree | **done** — `3dde235bf`, needs a broad matrix run |
| 4-vii | declare at the route (`.policy()`), not a central table | **done** — `6061aba6b` |
| 4-viii | REMOVE `require_internal_user_layer` from the DAV nests | **done** — `06a046593`, needs a matrix run |
| 4-ix | REMOVE `require_internal_user_layer` entirely (+ its fail-open) | **done** — `7a6f939ae`, needs a matrix run |
| 4-x | remove `require_admin`, then decide the allowlist | not started |

Step 1 landed as `CurrentCaller.kind: CallerKind`, not the `is_external`
bool this section first called for: the bool could express three states
that cannot exist, so `MinPrincipal` compares enum variants instead of a
bool plus a parsed role string. `SmolStr` roles and the stale role on
`CachedBasicAuthResult` went with it.

**The dump alone cannot prove the fold is faithful — hence step 4a.**
The acceptance below asks for the compiled table to be identical before
and after the fold, but that table is written BY HAND from reading the
layers. The diff therefore proves the fold matches the table, never that
the table matches the layers: a mis-transcribed gate makes both dumps
agree on a widened policy. `tests/api/gate_matrix.hurl` is the
behavioural half — it asks the running server what each principal kind
may reach, and is green BEFORE any layer moves. Its coverage map names
the files that already pin the other rows, so it only covers what was
missing: `/webdav` (nothing in `tests/api` ever PROPFINDed it),
anonymous against each folded surface, allowlisted vs non-allowlisted
under one subtree, and the NOT-folded DPoP layer.

**The third fail-open is GONE** (`7a6f939ae`), deleted with the layer that
held it. Recorded because the reasoning is reusable:
`require_internal_user_layer`'s else-branch passes the request through
when no `CurrentCaller` is in the extensions ("No auth populated, or auth
disabled globally"). It is distinct from the two `src/AGENTS.md`
documents (those are `get_user_flags` errors). It is unreachable today
only because `auth_middleware` is layered OUTSIDE it and ends in
`Err(AuthError::TokenNotProvided)` — so nothing uncredentialed ever
reaches the inner layer. The fold reshuffles exactly that stack, so the
ordering guarantee has to survive it, or the gate becomes
pass-through-on-missing-caller. A compiled policy has no such arm:
`MinPrincipal::satisfied_by(None)` is true only for `Public`.

**Step 3 must ship the table DUMP, not just the table.** This section's
acceptance — the compiled table identical before and after the fold — is
unprovable today, because nothing can render that table. The snapshot has
to be taken while the gates still live in middleware and handlers, so
step 3 lands the renderer and its snapshot test, and step 4 asserts the
snapshot byte-identical after folding. Reversing the two reduces PR 1 to
"the suite is still green", which is the hoped-for-not-proven outcome this
table exists to rule out.

§ 2b lists **eight** refusals, and they do not all need the same
machinery — which is why step 3 splits:

| refusal | needs | step |
|---|---|---|
| `public`/`visitor` + any role | the folded chain | **3a** |
| `public`/`visitor` + a scope | the folded chain | **3a** |
| a role declaration that widens | the folded chain | **3a** |
| a principal declaration that widens | the folded chain | **3a** |
| the same axis twice in ONE slice | the slice | **3a** |
| an operation with no policy | — | **rejected**, see below |
| a policy naming no live operation | the table × the spec's operation list | **3b-ii** |
| two `.policy()` on one site | nothing — it does not COMPILE | **3b-ii** |
| `public`/`visitor` + elevation | a `Condition::Elevated` variant | blocked |

**Row 5 is REJECTED — it contradicted § Authentication by default, and
that section is the better mechanism.** Row 5 refused "an operation with
no policy" at boot on deny-by-default grounds, but `Policy::default()`
IS an authenticated user, so an undeclared operation is already closed to
anonymous callers and the property falls out of the type instead of
needing a check. Refusing it would have meant declaring all 279 REST
operations and keeping them in sync, to catch an exposure that a nest
declaration already covers wherever a nest exists.

So a bare `Router` or handler needs no `.policy()` call, and an
undeclared operation compiles to the safe default. The residual risk is
named rather than hidden: an operation that *should* be admin-only and
sits under no admin nest gets `principal=User` — any authenticated user.
That is the fold's job to catch in step 4, by comparing the dump against
what the layers enforce today.

**Row 8 got stronger than specified.** It asked for a boot error on two
`.policy()` declarations for one operation. `.policy()` returns a
`Gated<T>` that does not itself implement `Policied`, so
`.policy(..).policy(..)` does not COMPILE — a contradiction the compiler
refuses beats one the server refuses at boot. The array literal is what
makes one slice the natural way to write several conditions.

**Row 6 is load-bearing, not belt-and-braces.** `axum::Router` cannot be
asked which paths it holds (`has_routes() -> bool` is the whole public
introspection surface), so a declaration for a renamed or typo'd path is
otherwise indistinguishable from one that applies, and it silently stops
gating anything. Nothing else in the design notices. It immediately
earned its place by catching two tests in `scope_table` that declared
prefixes covering no operation.

**ONE place to declare a route and its policy: `routes.rs`, per VERB.**
Route, layer and policy are written together — a reader sees all three
without cross-referencing, and the path appears exactly once.

```rust
router.nest("/admin", admin_router.policy([Condition::role(MinRole::Admin)]))
.route("/{id}", get(download).policy([Condition::visitor_allowed()]))
.route("/{id}", delete(remove))          // no policy: the safe default
```

`get`/`post`/`put`/`delete`/`patch` are OxiCloud's (`policy_router.rs`),
not axum's, because axum's return a `MethodRouter` that has forgotten its
verb — so `.policy()` on one could only declare for the whole PATH.
`.policy()` on a `MethodRouter` is therefore a COMPILE error. Not a
nicety: `ANONYMOUS_ALLOWLIST` is GET-only and three of its seven paths
also carry a write, so a path-level `visitor_allowed()` on
`/api/files/{id}` grants a share visitor `DELETE` — which it did, until
`c79955364`.

A declaration leaks in none of three directions, pinned end to end
through merge + nest + rebase, because `/api/files/{id}`'s GET and DELETE
live in SEPARATE routers that are merged before being nested:

| operation | policy | direction tested |
|---|---|---|
| `GET /api/files/{id}` | `Visitor` | declared |
| `DELETE /api/files/{id}` | `User` | same path, other verb |
| `GET /api/files` | `User` | upward, to the collection |
| `GET /api/files/{id}/versions` | `User` | downward, to a subpath |

**Nothing with a route may be declared centrally.**
`policy_declarations.rs` holds only operations that HAVE no route: DAV
(one `any` handler per surface → `(area, access)`), the WS `rt.*` methods,
and collab frame kinds. Never a copy of a list that exists elsewhere.

**`.policy()` cannot record its own path**, for the same reason: neither
a `Router` nor a `MethodRouter` knows where it will be mounted. It
attaches conditions, and the `Site` is learned at the `nest` / `route`
call — so the path is still written exactly once, which is the property
the syntax was reaching for.

Step 3a also found a gap in § 2. Compile: scope is specified to fold as
"union of what is required", but `Policy::scope` holds a SINGLE pair, so
two different areas have nowhere to live. The compiler refuses them
(`ConflictingScopes`) rather than dropping one, so a genuine two-area
operation forces `scope` to become a `ScopeSet` instead of quietly losing
a requirement. The same area twice tightens access, as intended.

Step 2 added `Axis` to the types above: the compile step has to tell a
repeated axis within ONE slice (a boot error — the declaration is
ambiguous) from a repeat across a nest CHAIN (legal inheritance), and it
needs the axis without the payload to do it. `public()` and
`visitor_allowed()` share the `Principal` axis, which is why declaring
both in one slice is ambiguous rather than additive.

Still open, and now blocking step 3: how a route records its policy — an
axum `.policy()` extension trait, or an explicit builder.

#### Not in PR 1 — the externals audit

`CallerKind::role()` reproduces the pre-`f481cfa8f` `role` field exactly:
`External` reports `User`. That was deliberate, because an external user's
role IS `user`, so every site reading the role alone matched externals
before and still does — rewriting them as `matches!(.., Internal(User))`
during the migration would have silently stopped matching them.

Which of those ~25 sites SHOULD exclude externals is therefore unexamined.
It is independent of the policy engine and belongs in its own diff: each
site is a judgement call that is either a fix or a regression, and folding
them into a rename or into step 4 hides them from review.

### PR 2 — a new route must not be silently scopeless

The two axes have opposite defaults, and only one of them is safe:

| axis | an undeclared operation gets | is that safe? |
|---|---|---|
| gate | `principal = User` | **yes** — a session is required, so a new route is gated by the type |
| scope | `scope = None`, i.e. no requirement | **NO** — reachable by ANY credential, including an app password that should never touch it |

So `Policy::default()` protects a forgotten route from anonymous callers
and not from a narrowly-scoped one. Add `/remote.php/new-feature` or
`/new-feature-dav/` and it inherits a session requirement and no area at
all, which is exactly the shape of the device-flow bug: a credential
restricted to `files` reaches a surface nobody classified.

`public()` turned out NOT to be a prerequisite for shadowing `/api`:
`main.rs:1049` warns that nesting two routers at one prefix composes
their layers, but that does not apply to `nest` plus an inner `.layer()`
— a layer on one nested router does not reach a sibling. Pinned by
`policy::nest_layer_semantics`, because the consequence of guessing wrong
is refusing every login. `public()` is still needed before the gate can
become the ONLY enforcement, since the public routers are outside
`auth_middleware` and the default policy demands a session.

**So the scope requirement is TOTAL and defaults to unsatisfiable.** Not
an `Option` that defaults to "no requirement":

```rust
pub enum ScopeReq {
    Undefined,            // the DEFAULT — unsatisfiable; a boot error
    None,                 // explicitly no scope required; greppable
    Area(Scope, Access),
}
```

`Policy::scope: ScopeReq`, `Default = Undefined`. Three named states, no
`Option`, and "no scope required" becomes a declaration an implementor
writes — exactly as `public()` is written on the principal axis rather
than fallen into. `/api/config` is the worked case: `ScopeReq::None` +
`MinPrincipal::Public`.

**NOT `Option<(Scope, Access)>` plus a `Scope::None` variant**, which is
the `CallerKind` mistake repeated: `Option::None` and `Scope::None` would
both be reachable and mean different things. Naming all three states is
what makes the wrong combination unrepresentable.

**`Undefined` is a BOOT error, not just a runtime denial.** A runtime
denial means the first request to an undeclared route fails, in whatever
environment reaches it first; a boot refusal means the deploy fails
(`feedback_fail_fast_config`: a boot panic beats a silent skip). The
sharper reason is that `Undefined` is unsatisfiable even by a FULL-scope
session cookie — so on enforcement day an undeclared operation breaks an
ordinary browser, not only an app password. Keep the runtime deny as
defence in depth; it should be unreachable.

The operation list for that check is § 2b's existing cross-check against
`openapi.json` and `asyncapi.json`.

**Declare by AREA PREFIX so the workload stays small.** § Areas, not
protocols already has the rule — "a new route under an existing area
inherits its scope. A route fitting no row means a scope is missing,
which is a decision rather than a lookup". About ten prefixes cover the
279 operations, so making the default unsatisfiable costs ten
declarations rather than 279. This is why the same move was REJECTED for
the gate axis (§ PR 1 — status, row 5): there the default is already
safe, so the work bought nothing.

**Pin the `ScopeReq::None` list.** It should be rare and auditable like
`public()`; § Deliberately scopeless already names the candidates. A test
over that list makes adding one visible in review instead of a quiet
one-word diff.

**Two opposite "absents", same word — do not merge them.** The CREDENTIAL
side grandfathers an absent claim to FULL (§ Absent means FULL,
everywhere), because refusing every token minted before the claim existed
would lock out the whole population. The OPERATION side defaults to
`Undefined` and denies. Opposite directions; a reader who conflates them
will "fix" one into the other.

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

`CurrentCaller` carries `token: PresentedToken`, whose `session_id` is the
JWT `sid` (merged with `authn-pipeline.md` part A). So `security` gets
the per-session form rather than ceiling-only, and `Elevation` reads
`caller.token.session_id` once `auth.sessions.elevated_at` exists.

It must live on the caller, not in request extensions: a gate is handed
`&CurrentCaller` and nothing else, so extensions can serve a handler but
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
- Unit: the app-password CEILING is exactly `files,calendar,contacts`,
  so no `security` or `account` operation is reachable with one. A
  ceiling widened without review fails here — the ceiling table is the
  one artifact to audit.
- Unit: a multi-axis `Policy` is **conjunction** — a caller holding
  elevation but not `Admin` is REFUSED by a policy requiring both, and so
  is one holding `Admin` but not elevation. Pin both halves: a disjunctive
  implementation passes the first silently and hands admin mutations to
  any user who re-authenticated.
- Unit: reduction — a chain declaring `MinRole::Internal`, then `Admin`,
  then `Owner` compiles to `Owner` and still refuses a plain user. A
  declaration that would WIDEN the role (an `Owner` nest, an `Internal`
  child) fails at boot rather than resolving silently.
- Unit: reduction is per condition. The role chain above compiles to
  `Owner` while the principal condition keeps its own value — a role
  conjunction must not reach across and alter another condition. Pin the
  independence, not just the role result: that is what a global reduction
  would silently break.
- Unit: the boot refusals in § 2b. `public()` beside any
  `MinRole` is REFUSED — structurally unsatisfiable, since
  `UserRole::Anonymous` is never stored in `auth.users`. Likewise
  `visitor_allowed()` with elevation, and with a scope.
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
