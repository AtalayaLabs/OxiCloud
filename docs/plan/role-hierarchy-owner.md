# Role Hierarchy + Protected Server Owner

Issue: [AtalayaLabs/OxiCloud#690](https://github.com/AtalayaLabs/OxiCloud/issues/690)

**Status: IMPLEMENTED** on `feat/admin-vs-owner`. Every suite green:
unit (977), integration (1037), Hurl API, frontend (438), Playwright
(105).

What landed, in order: Step 0 (14 fail-open role parsers converted to
`from_stored`, the 3 external-identity guards restated as "not a plain
user"), the `Owner` variant with its two migrations, hierarchy
enforcement via `require_can_modify`, the transfer endpoint, the
`oxicloud user promote-to-owner` CLI, the frontend badge and transfer
flow, and docs.

Three things the plan did not predict, each found by running something
rather than by reading code — recorded because they are the parts a
re-implementation would get wrong again:

1. **Nine admin gates compared the role SPELLING** (`role == "admin"`)
   against a `String`, not a `UserRole`. The typed sweep could not see
   them, and every one denied the owner. `UserRole::str_at_least` now
   covers them. Found by the Hurl suite.
2. **`count_users_by_role("admin")` silently stopped counting
   administrators** once setup created an `Owner`, and it feeds
   `/api/auth/status`'s `initialized` and `registration_allowed`.
   Replaced by a typed `count_privileged_users`.
3. **Folding "self" into the rank comparison forbade every
   self-directed admin action**, because nobody outranks themselves —
   breaking an admin setting their own quota. The hierarchy governs
   acting on *others*; the three genuine self-refusals are per-method.

The `Anonymous` role work (`refactor/rationalize-publicshare`) landed
first. What that branch left behind, which this design predates and
must be read against:

```rust
pub enum UserRole { Admin, User, Anonymous }

/// Roles legitimately stored on `auth.users`. Returns None for
/// "anonymous" — it is a session-only role and can never be a row.
pub fn from_stored(raw: &str) -> Option<Self>
/// Roles as carried on a SESSION. Accepts "anonymous"; delegates the rest.
pub fn from_session(raw: &str) -> Option<Self>

pub fn rank(self) -> u8        // Anonymous 0, User 1, Admin 2
pub fn at_least(self, min: UserRole) -> bool   // rank() >= min.rank()
pub fn is_anonymous(self) -> bool
```

Three corrections to what follows:

1. **`rank()` and a rank-ordering already exist.** This plan proposes
   them as new. Slot `Owner` above `Admin` and keep the contiguous
   numbering (Anonymous 0, User 1, Admin 2, Owner 3) rather than the
   100/50/10 below — the integers are internal either way, and gaps
   "for future roles" buy nothing when adding a variant means editing
   every `match` regardless.

2. **There are TWO parsers, not one `parse()`.** `Owner` is a stored
   role, so it goes in `from_stored` and is inherited by
   `from_session`. Getting this backwards would let a session claim
   `role: "owner"` with no row behind it.

3. **`can_modify` should be a method, not an associated function.**
   `caller.outranks(target)` reads at the callsite and sits naturally
   beside the existing `at_least`. Same one-liner on `rank()`; the two
   differ only in `>` vs `>=`, which is exactly the distinction between
   "may act on them" and "meets this floor".

4. **Ownership is a `UserRole::Owner` variant, and exactly ONE owner
   exists today** (Ed, 2026-09-16). The design below is confirmed, with
   the costs and guard-rails in the next section. An intermediate draft
   of this plan proposed a settings row instead and was reversed — if
   you find `server.owner_user_id`, `resolve_owner()`, or a TTL cache
   for ownership on a branch or in an old revision, that is the
   discarded shape.

## Decision: `UserRole::Owner`, single owner today, multi-owner later

Ownership is a role: `auth.userrole` gains `'owner'`, ranked above
`'admin'`. **Exactly one owner exists today**, enforced by a partial
unique index — and that constraint is *policy*, not structure.

That distinction is the whole reason for this shape (Ed, 2026-09-16):
multiple owners are a plausible future — bus-factor alone argues for
it, since a sole owner who leaves takes ownership with them. An index
can be dropped when that day comes: one `DROP INDEX`, no data
migration, no re-modelling. The alternative considered — one
`auth.admin_settings['server.owner_user_id']` row — gets single-owner
*for free* because `key` is a PRIMARY KEY, but that is exactly the
problem: a key holds one value, so multi-owner would mean a new table
keyed by user, which is a role table with extra steps. It forecloses
permanently what the index merely postpones.

The other alternative, an `is_owner` boolean column, sits between the
two: it supports N owners and needs the same index, but adds a column
whose meaning duplicates the role it must agree with, plus a
`CHECK (NOT is_owner OR role = 'admin')` to keep them agreeing.

| | **`UserRole::Owner`** | `is_owner` column | settings row |
|---|---|---|---|
| **multiple owners later** | **DROP INDEX** | DROP INDEX | **impossible — needs a new table** |
| "exactly one" today | partial unique index | partial unique index | free — it is a PK |
| owner-is-an-admin | **inherent — it outranks admin** | CHECK constraint | validated on every read |
| read cost | **zero — already in the row** | zero | a cached lookup + its TTL |
| `auth.users` schema | enum widened | column added | untouched |
| sites asking `role == Admin` | 8 must change | none | none |
| sites PARSING the stored role | 14 must change | none | none |
| external-identity guard | **widen in 3 places, fails OPEN if missed** | already correct | already correct |
| migration | one `ALTER TYPE … ADD VALUE` | `ADD COLUMN` | one INSERT-able key |
| JWT `role` claim | new value, consumers must learn it | unchanged | unchanged |
| transfer | demote + promote, ordering matters | flip 2 booleans | one UPDATE |

The first row is the decision; the fourth is the bonus. The enum's
**read cost is zero** — ownership rides in `auth.users.role`, which
every user query already SELECTs and `FullUserDto` already serialises,
so there is no lookup to cache, no TTL, and no per-row cost on any
listing. That was the settings row's weakest point in practice:
`AdminSettingsService` has no cache at all (`get(key)` is a bare
SELECT), so it would have needed a 30s TTL cache purely to keep
`/api/auth/me` cheap.

The enum's costs are real but one-time, and they are all on the write
and parse side. They are listed next, because getting one of them
wrong is a security defect rather than an inconvenience.

### What the enum must pay for

**The migration is not the cost.** `auth.userrole` is a real PG enum —
`CREATE TYPE auth.userrole AS ENUM ('admin', 'user')`,
`initial_schema.sql:31` — so widening it is one
`ALTER TYPE auth.userrole ADD VALUE 'owner' BEFORE 'admin'` in its own
migration file. Declaration order *is* sort order and this roster is
strongest-first already (`'admin'` before `'user'`), so the positional
anchor keeps the ordinals meaningful; the house rules for enum rosters
(append-only, no reorder, no drop) are recorded in
`migrations/20260801000000_role_grants_enum.sql`, which uses the same
trick for `storage.grant_role`.

Use the anchor even though nothing reads the ordinal today — every
query selects `role::text` and parses in Rust, so the ordering has no
current consumer. It costs one keyword now and cannot be corrected
later: PG allows `ADD VALUE` but never a reorder.

The real cost is on the **read** side, in Rust.

*Comparisons (8).* Five fail **closed** — the owner silently loses
admin powers, baffling but safe: `authorization_ports.rs:42`,
`auth_application_service.rs:710`, `:1195`, `:3161`,
`middleware/user.rs:137`. Three fail **open**:
`domain/entities/user.rs:396`, `auth_application_service.rs:3497`, and
the SQL `CHECK (NOT (is_external AND role = 'admin'))`. That rule means
"an externally-federated identity may not hold privileged roles" —
`Admin` was simply the only one that existed. A role above it escapes
all three, leaving an IdP-provisioned account able to own the instance.

**Write these three as "not a plain user"** — `role <> 'user'`, or an
`is_privileged()` helper — never as a list of privileged roles. An
enumeration has to be revisited every time the roster grows, and the
failure when someone forgets is silent and total. A negative test is
correct by default for roles that do not exist yet.

*Parsers (14).* Fourteen sites parse the stored role with `_ =>
UserRole::User` (13 in `user_pg_repository.rs`, 1 in
`auth_application_service.rs`). **The codebase already documents this
shape as fail-open**: `UserRole::from_stored`'s own docstring says
"Callers reading the database previously used `_ => UserRole::User`,
which is fail-OPEN: an unrecognised value silently became a real
user." `from_stored` was introduced to retire it and reached only 6
callsites.

So a stored `'owner'` would be read back as four different answers
depending on the path: `User` at the 14 old sites, `None` via
`from_stored`, `Anonymous` in `user_dto.rs:509`, least-privileged in
`middleware/auth.rs:712`. Not one wrong answer — four inconsistent
ones, none of which raises anything.

The sharpest instance is `user_pg_repository.rs:1741`: `change_role`
takes `role: &str` and matches `"admin" => Admin, _ => User`. So
`change_role(id, "owner")` **writes `user`** — the promote path
silently no-ops through the very API that would implement it.

Note this pattern is **not a bug today**, which is why it has
survived: with exactly two values, "anything not admin is user" is
accurate. It is a trap that arms itself the moment a third value
exists — i.e. precisely on this feature.

**Convert all 14 to `from_stored`.** This is debt the tree already
wants cleared, not work this feature invents, and it is mechanical:
`from_stored` returns `Option`, so the compiler refuses to let a site
ignore the unknown case. Do it as its own commit, before `'owner'`
exists — then adding the variant cannot silently downgrade anyone,
because there is no silent path left.

*The session parser (1).* `'owner'` is a **stored** role, so it enters
`from_stored` and `from_session` inherits it. Getting that backwards
lets a session claim `role: "owner"` with no row behind it; skipping
it entirely means an owner's own JWT downgrades them on every request.

### What this costs at read time: nothing

Ownership is readable from the user row, because it *is* the user row:
`role == UserRole::Owner`. `auth.users.role` is already SELECTed by
every user query and already serialised on `FullUserDto`, so there is
no lookup, no cache, no TTL, and no per-row cost on any listing —
including `/api/admin/users`, whose single-query shape
(`list_users_with_derived_flags`) was deliberate, and `/api/auth/me`,
which the SPA calls on every page load.

Rank stays a function of role, and ownership needs no separate
predicate:

```rust
// caller may act on target iff it strictly outranks them
fn can_modify(caller: UserRole, target: UserRole) -> bool {
    caller.rank() > target.rank()
}
```

### No split state, by construction

Worth naming because the discarded settings-row design had to work for
it: there is no way for "who owns the instance" and "what role that
user holds" to disagree, because they are the same column. An owner is
an admin's superior by rank, so every existing `at_least(Admin)` gate
admits them without a second lookup to reconcile.

The settings row needed a `resolve_owner()` that re-validated the
stored id on every read — it named a user by bare UUID in a `TEXT`
column with no FK, so it could point at someone since demoted,
deactivated, or deleted. None of that applies here. `auth.users.role`
cannot name a user who does not exist, and a role cannot disagree with
itself.

### Multi-owner: what this design leaves open, and what it defers

Single-owner is **today's policy**, and the index is where it lives.
When multi-owner arrives the change is `DROP INDEX` plus one decision
this plan does not have to make now:

**Can owner A demote owner B?** The strict rule
(`caller.rank() > target.rank()`) says no — owners cannot touch each
other. That is good mutual protection, and it means a departed
co-owner can only be removed via the CLI. Relaxing to `>=` for owners
fixes removal but lets any single owner unilaterally strip the others,
which weakens the protection that motivated #690 in the first place.

Settle that before implementing multi-owner, not before implementing
this. With exactly one owner the question cannot arise: there is no
second owner to act on, and the strict rule is unambiguously right.

### How the UI learns who the owner is

**No DTO change and no new endpoint** — `role` already travels.

`FullUserDto` carries the role string, so an `'owner'` value answers
both questions the frontend has, from data it already receives:

- **`GET /api/admin/users`** returns `FullUserDto` rows → the badge,
  and the "hide destructive buttons on the owner's row" rule.
- **`GET /api/auth/me`** returns `SelfUserDto`, composed over
  `FullUserDto` → "am I the owner?" with no extra call, which drives
  the transfer-ownership button.

One consequence to accept deliberately: `role` is also on
`PublicUserDto`, which is visible to every authenticated caller about
every visible user (the share-picker vignette renders an admin badge
from it). So ownership becomes **visible to all authenticated users**,
where an `is_owner` field could have been admin-only.

That is a real disclosure and it is judged acceptable: it identifies
the one account nobody can demote, but only to users who already have
an account, who can already see who the admins are, and who cannot act
on the information — the hierarchy is enforced server-side regardless
of who knows about it. If that trade is ever unwanted, the fix is to
redact `role` in `PublicUserDto` for non-admin callers, not to move
ownership off the role.

The frontend `GrantRole`/`DriveRole` split (see
`project_grant_role_vs_drive_role`) is unaffected — those are
share-grant roles on a different axis and `'owner'` there already
means something else.

## Problem

Any administrator can currently remove another admin's role (including
the server owner's), reset another admin's password, or deactivate
their account. One rogue or compromised admin can lock out the server
owner. Today's authorization tree is flat: every user with
`role = 'admin'` is equally powerful; there is no notion of a
protected instance owner.

## Goal

Introduce a strict role hierarchy `Owner > Admin > User` where **a
caller can only mutate users of strictly lower rank**. The Owner role
is protected: nobody (not even other admins) can modify, deactivate,
delete, demote, or reset the Owner's credentials. Only the Owner
themselves can transfer ownership via a dedicated flow.

## Design decisions

1. **Three roles, ranked.** `UserRole` gains an `Owner` variant that
   ranks above `Admin`, which ranks above `User`. Ranking is by an
   integer `rank()` method (Owner=100, Admin=50, User=10). The exact
   integers are internal — external interfaces use the string form
   (`"owner"`, `"admin"`, `"user"`).

2. **Strict-hierarchy mutation rule.**
   `can_modify(caller, target) = caller.rank() > target.rank()`.
   Consequences:
   - Admin can modify User; cannot modify another Admin; cannot modify Owner.
   - Owner can modify Admin and User; cannot be modified by anyone
     else (self-mutation via `/me` endpoints is separate and always
     allowed for the acting user's own row).
   - This is a **real behavior change** for existing admin-vs-admin
     interactions today (two admins can currently reset each other's
     passwords). Post-change, only the Owner can do that.

3. **Exactly one Owner per instance.** Enforced at the schema level
   with a partial unique index or a CHECK-guarded pseudo-unique
   pattern. Zero Owners is allowed (existing installs pre-migration);
   two Owners is not.

4. **No auto-promotion on UPGRADE; automatic on FRESH INSTALL.** These
   are different situations and the plan originally conflated them
   (Ed, 2026-09-16).

   *Upgrading an existing install* does NOT auto-promote the
   earliest-created Admin. Ops run
   `oxicloud user promote-to-owner <username-or-email>` explicitly.
   Auto-promotion is unsafe here: the earliest admin may not be the
   current maintainer — someone took over, someone left.

   *A fresh install* has no such ambiguity. `POST /api/auth/setup`
   creates the first admin, and the person running it is by definition
   the one standing the instance up; there is exactly one candidate and
   no history to be wrong about. So setup creates that first user with
   `role = 'owner'` directly — not `'admin'` followed by a promotion,
   which would be two states where one will do.

   Without this, every new install starts in the zero-owner grace
   state — meaning admin-of-admin operations are refused until someone
   runs a CLI command they have no reason to know exists. That is a bad
   first-run experience for a protection they never asked to opt out
   of.

   Setup is already atomically once-only (`is_system_initialized`
   pre-check plus `try_claim_initialization`, which lets exactly one
   concurrent request win), so the owner write inherits that guarantee
   — no second caller can race in and claim ownership.

   Emit `server_owner.promoted_via_setup` to distinguish it in the
   audit trail from the CLI path.

5. **Zero-Owner grace state is quiet.** An install with no Owner
   still boots and serves. Every admin mutation of another admin —
   which is now the "requires Owner" tier — is REFUSED, with a
   structured audit line pointing at the CLI. There is no boot-time
   refusal for this feature (unlike `username-lowercase` where the
   invariant is load-bearing for lookups). Ops promote-to-owner when
   they need admin-of-admin operations again.

   After §4 this state has exactly one cause: an **upgraded** install
   before ops assign an owner. A **fresh** install is never in it, and
   there is no way to fall back into it — the role is the ownership, so
   it cannot go stale or point at a user who no longer qualifies.

6. **Owner cannot self-delete or self-deactivate.** Rationale:
   preserves the invariant "the Owner remains reachable via a
   password / OPAQUE reset from themselves". Self-delete would create
   a zero-Owner state that ops didn't consent to; strict rule is
   simpler than "delete allowed only if another Owner exists" (there
   are never two Owners).

7. **Ownership transfer is a single atomic API.**
   `POST /api/admin/transfer-ownership { new_owner_id }`, callable
   ONLY by the current Owner. Target must be an active internal user
   (not external, not soft-deleted). One SQL transaction swaps the
   two role columns; audit line
   (`server_owner.transferred`) records the swap with both user
   ids. No two-step confirmation — the endpoint is only reachable by
   the Owner's own session (or an admin-password-elevated session
   they hold), so accidental transfer requires the same access level
   as any other Owner action.

8. **AuthZ lives in the application service, not the handler.**
   Per [AGENTS.md § AuthZ](../../AGENTS.md), every service method
   that mutates a user row grows a `caller_id: Uuid` parameter and
   performs the hierarchy check internally. Handlers stop doing
   self-checks (`if admin_id == id { … }`) — those move into the
   service too. The handler is reduced to: parse input, call service
   with `caller_id`, map error to HTTP.

9. **Owner-only actions are a subset of "requires higher rank than target".**
   Concretely: `admin_reset_password`, `set_user_active`,
   `change_user_role`, delete, quota-change, and
   `admin_promote_external_to_internal` all use the shared
   `can_modify` gate. That gate naturally makes them Owner-only when
   the target is an Admin, and Admin-or-Owner when the target is a
   User. No separate "owner-only" branch — the rank comparison does
   the work.

## Not in scope

- Multiple Owners.
- Per-permission granularity (e.g. "sub-admin who can reset
  passwords but not change roles"). Three roles is the ceiling for
  this PR.
- OIDC group / role sync (there's no "owner" role in most IdPs).
- Ownership escrow / dead-Owner recovery — deferred to a follow-up.
  The mitigation for a lost Owner today is direct DB access, same
  as any lost-admin-password scenario.
- Delegation / temporary ownership handoff.
- Audit-log viewer for ownership transfers (the structured logs land
  in the operator's log pipeline; a UI viewer is a separate feature).

## Deliverables

### 1. Schema — widen `auth.userrole` enum

Add `'owner'` to the PG enum type. In sqlx, this is a bare `ALTER
TYPE auth.userrole ADD VALUE 'owner'` (idempotent via `IF NOT
EXISTS`). Confirm no existing rows use the string `'owner'` before
adding (they can't, since the enum forbade it, but the migration
should be defensive).

Add a partial unique index enforcing "at most one Owner":

```sql
CREATE UNIQUE INDEX IF NOT EXISTS auth_users_single_owner
    ON auth.users ((role = 'owner')) WHERE role = 'owner';
```

Note the index expression: `(role = 'owner')` is a boolean, unique
alongside the WHERE clause means at most one row with `role =
'owner'` can ever exist. Cheaper than a CHECK-with-subquery and
composes cleanly with `INSERT`/`UPDATE`.

### 2. Domain — `UserRole::Owner` + `rank` + `can_modify`

Updated for what the Anonymous work landed — there are four variants
and two parsers, not three and one:

```rust
pub enum UserRole { Owner, Admin, User, Anonymous }

impl UserRole {
    pub fn as_str(self) -> &'static str { … "owner" | "admin" | "user" | "anonymous" }

    /// Roles legitimately stored on `auth.users` — `'owner'` goes HERE.
    pub fn from_stored(raw: &str) -> Option<Self> { … }
    /// Session-carried roles; accepts "anonymous", delegates the rest.
    pub fn from_session(raw: &str) -> Option<Self> { … }

    /// Integer used only for rank comparisons; not part of any wire format.
    /// Contiguous — gaps "for future roles" buy nothing when adding a
    /// variant means editing every `match` anyway.
    fn rank(self) -> u8 {
        match self {
            UserRole::Anonymous => 0,
            UserRole::User => 1,
            UserRole::Admin => 2,
            UserRole::Owner => 3,
        }
    }

    /// Can `caller` mutate a user whose role is `target`?
    /// Strict hierarchy: caller must outrank target.
    pub fn can_modify(caller: UserRole, target: UserRole) -> bool {
        caller.rank() > target.rank()
    }
}
```

Update `Display`, `From<Row>`, DTO serialization, everywhere `Admin`
and `User` are matched exhaustively (compiler catches these via
`match` exhaustiveness).

### 3. Service — hierarchy check + `caller_id` in signatures

New shared method on `AuthApplicationService`:

```rust
/// Enforce `caller` outranks `target`. Emits an audit line on
/// denial. Returns `Ok(())` if allowed, `Err(Forbidden)` otherwise.
///
/// 403, NOT the 404 anti-enum shape used elsewhere — and the
/// "same shape as NotFound" note this plan originally carried here
/// was wrong, contradicting its own closing section. The anti-enum
/// rule exists to stop a caller probing for resources they cannot
/// SEE. An admin reached this method through `require_admin` and can
/// already list every user, so hiding existence protects nothing and
/// only makes a legitimate refusal unreadable.
async fn require_can_modify(
    &self,
    caller_id: Uuid,
    target_id: Uuid,
) -> Result<(), DomainError> { … }
```

Every admin mutation service method grows `caller_id: Uuid` and
calls this at the top. Methods touched:

- `change_user_role(caller_id, target_id, role)` — was `(target_id, role)`
- `admin_reset_password(caller_id, target_id, new_password)` — was `(target_id, new_password)`
- `set_user_active(caller_id, target_id, active)` — was `(target_id, active)`
- `update_user_quota(caller_id, target_id, quota)`
- `admin_promote_external_to_internal(caller_id, target_id)` — already takes admin id; verify it uses it for the check
- `admin_create_user(caller_id, dto)` — Owner creates any role; Admin creates only User

Also fold in the existing handler-side self-checks:
- "Cannot change own role" → part of the hierarchy check (self.role == target.role → strict-less-than false → refused)
- "Cannot deactivate own account" → same
- "Cannot delete own account" → same

### 4. Owner-specific protections

Beyond the shared `can_modify` gate, three Owner-only rules:

- **Owner cannot be demoted by anyone (including self)** via
  `change_user_role`. The only role change on the Owner row is via
  the transfer-ownership endpoint, which swaps atomically.

  > **This rule and §5 are in tension, deliberately.** The transfer's
  > first statement demotes the Owner — exactly what this forbids. It
  > works only because the transfer writes SQL directly instead of
  > going through `change_user_role`. State that in the code, or the
  > next person will "simplify" the transfer to reuse the service
  > method and find it refuses its own first step.
- **Owner cannot self-delete or self-deactivate.** `set_user_active`
  refuses when `caller_id == target_id && caller.role == Owner &&
  !active`. Same for the delete endpoint.
- **Owner is not deletable by anyone else** — covered by the
  strict-hierarchy rule (nobody outranks Owner).

### 5. Ownership transfer endpoint

`POST /api/admin/transfer-ownership`
```json
{ "new_owner_id": "<uuid>" }
```

- 200 on success (with the new state summary)
- 403 if caller is not the current Owner
- 400 if `new_owner_id == caller_id` (nothing to do — refuse rather than silent success)
- 400 if target is external, inactive, or NULL-username (OPAQUE-migrated with no handle)
- 404 if target doesn't exist (anti-enum: same-shape as 403 preferred; TBD)

Implementation:

```sql
BEGIN;
UPDATE auth.users SET role = 'admin' WHERE id = <current_owner>;
UPDATE auth.users SET role = 'owner' WHERE id = <new_owner>;
COMMIT;
```

Ordering matters because of the single-Owner partial unique index:
if we tried `<new_owner> := 'owner'` first, the index would fire on
the intermediate state where two Owners coexist. Demote first, then
promote.

Audit line:
```
event = "server_owner.transferred"
from_user_id = <caller_id>
to_user_id = <new_owner_id>
```

### 6. CLI — `oxicloud user promote-to-owner`

New subcommand under `oxicloud user` (new subcommand family since
we don't have one yet):
```
oxicloud user promote-to-owner <username-or-email> [--dry-run]
```

- Resolves the argument to a user_id (email by `@` presence, same
  dispatch rule as `POST /api/auth/login`).
- Refuses if the target is external, inactive, or NULL-username.
- Refuses if another Owner already exists (points ops at the
  transfer endpoint instead).
- On apply: single `UPDATE auth.users SET role = 'owner' WHERE id =
  <target>`. The partial unique index is the safety net — if a second
  owner somehow exists, this fails loudly rather than creating one.
- Emits `server_owner.promoted_via_cli` audit line.

Reusing an existing subcommand family (`opaque`, `migrate`, `storage`)
or adding a new `user` family is a tiny call. Recommend a new
`user` family — `opaque` and `storage` are specialty; `migrate` is
data-migration; user management deserves its own noun.

### 7. Handler cleanup

Every admin handler that today does
```rust
if admin_id == id { return Err(SelfXxx) }
```
loses the inline check. Pass `admin_id` into the service via a new
positional param and let the service do the guarding. Reduces
duplication and closes the AGENTS.md rule violation
(AuthZ-in-handler) for these paths.

### 8. Audit lines

New `event` names (stable enum-style keys):
- `authz.admin_hierarchy_denied` — caller lacks the rank to mutate target
- `server_owner.transferred` — successful transfer
- `server_owner.transfer_rejected` — refused transfer (with `reason` = "self_transfer" | "target_external" | "target_inactive" | "target_null_username" | "not_current_owner")
- `server_owner.promoted_via_cli` — CLI action succeeded
- `server_owner.promoted_via_setup` — first-run setup claimed ownership
  for the admin it created. Distinct from the CLI event so an operator
  reading the trail can tell "this instance was born with an owner"
  from "someone assigned one later"
- `server_owner.self_action_refused` — Owner attempted self-delete / self-deactivate

Every denial goes through `tracing::info!(target: "audit", event=…, reason=…, caller_id=…, target_id=…)` per the AGENTS.md audit convention.

### 9. Frontend

The admin panel currently shows a set of buttons per user. Add:

- **Hide destructive buttons on the Owner's row** when the viewer is
  not the Owner themselves (role-change, password-reset, deactivate,
  delete are all inert against Owner regardless — but showing them
  and 403-ing on click is worse UX than hiding).
- **Show an "Owner" badge** next to the Owner's username in the user
  list.
- **New "Transfer ownership" button** in the Owner's own admin
  profile view (or in a system-settings section). Two-step UI: pick
  target user → confirm dialog explaining the swap will occur atomically → POST.
- **Show an "Owner" chip** in the current-user header for the actual
  Owner. Not a security signal — a UX one, so the Owner knows they're
  the last line of defense.

### 10. Tests

- **Unit — `outranks` truth table.** 16 rows, not 9: `Anonymous` is a
  fourth role now. Every `Anonymous` cell is `false` in both
  directions — it is the floor, and it can never be a target either
  since it is not a stored role. Assert it anyway; that is the cheapest
  possible statement that a share visitor cannot administer anything.
- **Unit — the transfer-ownership service method.** Success case,
  self-transfer refused, external target refused, non-Owner caller
  refused, target-doesn't-exist refused.
- **Hurl — role hierarchy grid.** Set up three users (owner, admin,
  regular). For each admin-mutation endpoint × (owner→admin,
  owner→user, admin→owner, admin→admin, admin→user, user→anything)
  cell, assert the expected 200/403.
- **Hurl — ownership transfer flow.** Owner transfers to admin →
  200, DB roles swap. Admin tries to transfer → 403. Owner tries to
  transfer to external → 400.
- **Hurl — Owner self-actions.** Owner tries to change own password
  via /me → 200 (self-change is fine). Owner tries to deactivate
  own account via admin endpoint → 400. Owner tries to change own
  role → 400.
- **Query count on the listing.** Assert `/api/admin/users` issues the
- **Unit — every stored-role parser round-trips `'owner'`.** Table
  test over `from_stored` / `from_session`: `"owner"` must yield
  `Owner`, never `User` and never `None`. This is the regression
  guard for the 14-parser conversion, and it is the one failure mode
  that is silent in production — an owner read back as a plain user
  gets 403s nobody can explain.
- **Unit — `change_role(id, "owner")` actually writes `'owner'`.**
  Pins the `_ => User` bug at `user_pg_repository.rs:1741`, which
  today would make the promote path a no-op.
- **Hurl — an external user cannot become Owner.** The external-admin
  ban must widen with the roster; assert `is_external` + `'owner'` is
  refused by both the service and the DB CHECK.
- **Setup grants ownership.** Against a virgin DB, `POST
  /api/auth/setup` then assert the created user's role is `'owner'`.
  Worth pinning because it is the only path that confers ownership
  without an operator asking for it — and because a regression is
  silent: the instance works normally right up until the first
  admin-of-admin operation.
- **Manual smoke** — dev DB, promote self to Owner via CLI, try
  every admin action from a second admin against the Owner (should
  all 403), transfer ownership, verify roles swapped.

### 11. Documentation

- `docs/config/env.md` — no new env vars (this feature has no
  runtime toggle).
- `docs/install/binary.md` — Upgrading section gains a subsection:
  "After the upgrade, promote your server owner:
  `oxicloud user promote-to-owner <name>`." Say plainly that this is
  an **upgrade-only** step: a fresh install already has an owner (the
  account created at setup), and an operator who reads the command
  without that caveat will go looking for something to fix.
- `docs/guide/` — new end-user page for admins explaining the
  hierarchy (per Ed's docs style: prose only, no env vars mentioned
  since there aren't any).

## Delivery order

**Step 0 is not optional and must land first.** Everything in it is
safe on its own — no behaviour change while the roster still has two
values — and every item becomes a silent security defect the moment
`'owner'` exists. Landing it separately also keeps it reviewable: a
reviewer can check 14 mechanical conversions, or they can check them
buried in a feature diff, but not both well.

0. **Prerequisites, own commit(s), no behaviour change:**
   - Convert the 14 `_ => UserRole::User` parsers to `from_stored`
     (13 in `user_pg_repository.rs`, 1 in
     `auth_application_service.rs`), including `change_role`'s
     `role: &str` match at `user_pg_repository.rs:1741`.
   - Rewrite the 3 external-identity guards as "not a plain user"
     (`domain/entities/user.rs:396`,
     `auth_application_service.rs:3497`, and the SQL CHECK in
     `20260612000002_auth_users_is_external.sql:50`). **Never as a
     list of privileged roles** — see § *What the enum must pay for*.
   - Convert the 5 fail-closed `role == Admin` comparisons to
     `at_least(Admin)` so the owner inherits admin powers rather than
     silently losing them.

1. Schema migration: `ALTER TYPE auth.userrole ADD VALUE 'owner'
   BEFORE 'admin'`, plus the partial unique index enforcing at most one
   owner. Keep the `ADD VALUE` in its own migration file — sqlx wraps
   each migration in a transaction, and PG allows the add there only if
   nothing in the same transaction *uses* the new value.
2. Domain: `UserRole::Owner` variant, `rank()` extended (contiguous —
   Anonymous 0, User 1, Admin 2, Owner 3), `outranks` as a method, and
   `'owner'` added to `from_stored` (inherited by `from_session`).
   Extend `rank()`/`at_least`, do not add parallel machinery.
3. Service: `require_can_modify` helper + audit line.
4. Service signatures: add `caller_id` to every admin mutation
   method, port the existing self-checks into the service.
5. Owner-specific rules (self-delete / self-deactivate / self-demote refusals).
6. Handler cleanup: remove inline self-checks, wire `admin_id`
   through as `caller_id`.
7. Transfer-ownership endpoint + service method.
8. CLI `user promote-to-owner` (the upgrade path), and setup creating
   its first user as `'owner'` (§4's fresh-install path) — two callers
   conferring the same role, so land them together.
9. Frontend: badge, hidden buttons, transfer flow.
10. Hurl coverage (grid + transfer + self-actions).
11. Docs.
12. Manual smoke against dev DB.
13. PR.

## Total scope estimate

~10-14 hours across backend, frontend, tests, and docs. The signature
churn on admin service methods is the biggest sub-task
(caller_id propagation catches every write path via the compiler).
The frontend transfer-ownership dialog is another block of ~2 hours.
The migration + schema change is small (~30 min).

The parts that need care:
- Getting the partial-unique-index expression right.
- Making sure the transfer transaction demotes-then-promotes to
  avoid tripping the single-Owner constraint mid-way.
- Ensuring the anti-enumeration behavior on hierarchy denials
  matches how other authz denials in the codebase behave
  (`authz.denied` returns 404 rather than 403 when read visibility
  is absent — this is documented in AGENTS.md; the admin-mutation
  case is different because visibility is granted by the admin-role
  middleware, so 403 is honest and the audit line carries the
  specific reason).
