# ReBAC Authorization

OxiCloud uses **Relationship-Based Access Control** (ReBAC): access is
expressed as a typed triple

```
Subject  has  Role  on  Resource    (until ExpiresAt?)
```

stored as rows in a single table — `storage.role_grants` — and resolved at
request time by the **`AuthorizationEngine`** (concretely, `PgAclEngine`).

Each `Role` expands to a fixed set of atomic `Permission`s at engine read
time (Viewer → `{Read}`, Editor → `{Read, Comment, Create, Update}`, …).
The database stores the role name; permission expansion happens in Rust.

This document explains how subjects, roles, permissions, resources, groups
and two kinds of cascading fit together. For implementation details, follow
the links to the relevant Rust modules.

---

## Why ReBAC

A simpler RBAC ("Alice is an editor") is global. We need per-resource sharing:
"Alice can edit *this folder* but not that one"; "Bob can view *that file* until
March". ReBAC is the natural fit:

- **Grants are facts, not global attributes.** Each row is
  `(subject → role → resource)`, optionally with an expiration.
- **The same model covers users, anonymous share-links, groups, and federated
  identities** — they all share the `subject_type` discriminator.
- **The same model covers files, folders, drives, calendars, address books,
  and playlists** — every resource type routes through the same engine and
  the same `role_grants` table.
- **No global "admin of folder X" magic** — the engine answers a yes/no
  question by scanning `role_grants` plus the relationships (folder ancestry,
  drive membership, group membership) that connect a subject to a resource.

The owner short-circuit is the one bit of non-ReBAC logic: a resource's owner
always passes the check without needing a row in `role_grants`.

---

## The four entities

### Subject — *who is asking*

```rust
enum Subject {
    User(Uuid),      // auth.users
    Group(Uuid),     // auth.subject_groups
    Token(Uuid),     // storage.shares — anonymous share links
    External(Uuid),  // federated identity (Open Cloud Mesh, future)
}
```

Defined in `src/domain/services/authorization.rs`. Each variant carries the
UUID of the relevant row. The SQL discriminator (`subject_type` column) is
`'user' | 'group' | 'token' | 'external'`.

### Resource — *what is being acted on*

```rust
enum Resource {
    Folder(Uuid),
    File(Uuid),
    Drive(Uuid),        // top-level container (personal / shared)
    Calendar(Uuid),     // CalDAV
    AddressBook(Uuid),  // CardDAV
    Playlist(Uuid),     // music
}
```

`Folder`, `File`, and `Drive` participate in the folder-ancestry cascade
(a grant on a drive descends to every folder + file inside it — see below).
`Calendar`, `AddressBook`, and `Playlist` are top-level per user and don't
cascade — the engine resolves them directly against a single `role_grants`
row per (subject, resource).

The `Playlist`, `Calendar`, and `AddressBook` cases replaced the pre-2026
per-feature `*_shares` tables (`caldav.calendar_shares`,
`carddav.address_book_shares`, `music.playlist_shares`) with a single
uniform `role_grants` model + bespoke-helper-free code path.

### Role — *the primary sharing verb*

Since the D-Prep migration (2026-07), roles are the **primary sharing
unit**. Each `role_grants` row carries a role name; permissions are
computed by expanding it in Rust at read time.

| Role | Permissions expanded | Typical UX label |
|---|---|---|
| `Viewer` | `Read` | Can view |
| `Commenter` | `Read`, `Comment` | Can view & comment |
| `Contributor` | `Read`, `Create` | Can upload but not modify siblings |
| `Editor` | `Read`, `Comment`, `Create`, `Update` | Can edit |
| `Owner` | `Read`, `Comment`, `Create`, `Update`, `Delete`, `Share`, `Manage` | Can manage |

Defined in `src/domain/services/authorization.rs::Role::expand()` — the
single source of truth. The DB column is a Postgres ENUM
(`storage.grant_role`, migration
`20260801000000_role_grants_enum.sql`), so unknown values are refused at
the storage layer.

The REST API accepts the role name directly on grant endpoints
(`POST /api/grants { "role": "editor", … }`,
`PUT /api/grants/role`). Callers no longer manipulate permission sets
by hand.

### Permission — *the atomic verb the engine checks*

Seven atomic permissions. Handlers ask "does this subject have
`Permission::X` on `Resource::Y`?"; the engine translates that to
"…does any role granted to this subject include `X`?".

| `Read` | view the resource / list folder contents |
| `Create` | create a child resource (folders / drives only — meaningful as an inherited grant) |
| `Update` | rename, move, edit content |
| `Delete` | delete the resource |
| `Share` | grant roles to other subjects |
| `Comment` | add comments (reserved — comments feature not implemented yet) |
| `Manage` | change resource settings, membership, policies (Drive owners; future Group-as-Resource) |

---

## Storage shape

```
storage.role_grants
  id           UUID
  subject_type 'user' | 'group' | 'token'
  subject_id   UUID
  resource_type 'drive' | 'folder' | 'file' | 'calendar' | 'address_book' | 'playlist'
  resource_id  UUID
  role         storage.grant_role
                 -- ENUM: 'viewer' | 'commenter' | 'contributor' | 'editor' | 'owner'
  granted_by   UUID  (the user who issued the grant)
  granted_at   TIMESTAMPTZ
  expires_at   TIMESTAMPTZ NULL
```

**One row per role assignment.** A "viewer of folder X for user Y" is one
row; an "owner of drive Z" is one row. Permission expansion happens in
Rust at engine read time via `Role::expand()` — the DB never stores a
permission column.

### History

The pre-2026-07 model kept one row per `(subject, permission,
resource)` triple in `storage.access_grants` — an editor was 4 rows,
an owner was 6. The D-Prep migration
(`20260730000000_role_grants.sql` + follow-ups through
`20260801000002_drop_access_grants.sql`) collapsed that into one row
per assignment, added the DB-side `grant_role` ENUM, renamed the
former `admin` role bundle to `owner` (to disambiguate from
`UserRole::Admin`, the JWT-level user-account privilege), and dropped
`access_grants` entirely. Coverage extension migrations
(`20260906…_role_grants_calendar_address_book`,
`20260910…_role_grants_playlist`) folded the last three per-feature
share tables (CalDAV / CardDAV / Music) into the same `role_grants`
model.

Cleanup is trigger-driven (`trg_cleanup_role_grants_folder`, one per
resource type): when a resource or subject is deleted, all referencing
grants disappear in the same transaction.

---

## Subject groups — *bundling subjects*

Groups let you grant against many users at once, with two extra features:

1. **Nesting.** A group can contain users *and* other groups (up to depth 8).
   Cycles are rejected at write time by a recursive CTE in
   `subject_group_pg_repository::add_member`.

2. **Virtual groups.** Server-managed groups with a well-known UUID and
   immutable membership. Today: one entry, `Internal`
   (`00000000-…-000000000001`), implicitly containing every authenticated user.
   Future: `Everyone` (incl. externals).

The schema:

```
auth.subject_groups          (id, name, description, is_virtual, …)
auth.subject_group_members   (group_id, user_id XOR member_group_id, added_by, …)
```

Groups are addressed as a `Subject::Group(uuid)` and appear in `role_grants`
just like users. The Rust types live in
`src/domain/entities/subject_group.rs`.

---

## Two kinds of cascading

OxiCloud has **two independent cascades** that compose on every permission
check for the storage-tree resources (`Drive`, `Folder`, `File`). Standalone
resource types (`Calendar`, `AddressBook`, `Playlist`) skip cascade entirely
— the engine resolves them via a direct `role_grants` lookup keyed by
`(subject, resource)`.

### 1. Resource cascade — *down the drive → folder → file tree*

Every folder belongs to exactly one drive (the D0 refactor made
`storage.folders.drive_id` mandatory); the drive root is itself a folder
with `parent_id IS NULL`. Folder hierarchy uses PostgreSQL `ltree`. A
grant on a drive OR a folder implicitly applies to every descendant folder
and to every file inside any descendant folder. The check uses the GiST
index on `storage.folders.lpath` for an `O(log N)` ancestor lookup:

```
grant.lpath  @>  target.lpath
```

So one Owner grant on a drive permits reading any file within it; one
Editor grant on `/projects` permits editing `/projects/q4/report.pdf`.
Files are not part of the ltree — instead, a file inherits its containing
folder's position and the cascade query joins on `target.folder_id`.

The handler-layer `_cascade_grant_exists` functions in
`src/infrastructure/services/pg_acl_engine.rs` are the canonical
implementation. Drives cascade through the same code path — the drive's
root folder is what the ltree query anchors on.

### 2. Subject cascade — *up the group tree*

A `User` caller is automatically expanded to:

```
{ user_id }  ∪  groups_for_user(user_id)  ∪  { INTERNAL_GROUP_ID }
```

where `groups_for_user` is the recursive CTE that walks
`subject_group_members` to find every group the user belongs to transitively.
A grant on the top of a nesting chain `henry ∈ B ⊂ A` permits henry to act.

The expansion is computed by `PgAclEngine::expand_user(...)` and **cached in a
Moka cache** keyed by `user_id`:

- TTL: 30 s
- Capacity: 50 000 entries
- Invalidation: TTL-only today; explicit busts on group mutation are a
  follow-up.

The cache makes the listing + cascade hot path effectively free after the
first lookup per user per ~30 s window.

### Composition

The engine combines both cascades in a single SQL round-trip. The role
column carries the assignment; permission expansion happens by filtering
on the set of role names that include the requested permission
(computed once at process start via `Permission::roles_implying(...)`):

```
SELECT 1 FROM storage.role_grants g
  JOIN storage.folders gf ON gf.id = g.resource_id
 WHERE g.subject_type = ANY('{user,group}')          -- subject cascade
   AND g.subject_id   = ANY($expanded_set)           --   (user + groups + Internal)
   AND g.role         = ANY($roles_implying_perm)    -- role → permission
   AND g.resource_type IN ('drive','folder')         -- drive OR folder ancestry
   AND (g.expires_at IS NULL OR g.expires_at > NOW())
   AND gf.lpath @> (SELECT lpath FROM storage.folders  -- resource cascade
                     WHERE id = $target_folder_id)
 LIMIT 1
```

The file variant adds a `UNION ALL` branch for the direct-file-grant case
(where the grant is on the file itself, not a folder or drive above it).
The `Calendar` / `AddressBook` / `Playlist` variants skip the cascade join
entirely and check `(g.resource_type = <kind> AND g.resource_id = $target)`
directly.

---

## How a check is decided

`PgAclEngine::check(subject, permission, resource)` returns a `bool`:

```
                ┌─── owner short-circuit ───┐
                │                           │
   subject = user, owner ⇒ Ok(true)         │
                                            ▼
       otherwise:  expand_user(uid)  ⇒  (subject_types, subject_ids)
                                            │
                                            ▼
       resource = folder:  folder_cascade_grant_exists(...)
       resource = file:    file_cascade_grant_exists(...)   (direct OR ancestor)
                                            │
                                            ▼
                                       Ok(true / false)
```

Non-user subjects (Token / External / Group-as-caller) skip the expansion —
their cascade input is a single-element set.

The decision is made entirely in the application service layer
(`*_with_perms` methods). HTTP handlers authenticate the caller and pass
`caller_id` through; they never inspect ownership or grants directly. This is
enforced by convention — see `CLAUDE.md → "Authorization (AuthZ)"`.

---

## Listing endpoints — *symmetric expansion*

The "Shared with me" feed (`GET /api/grants/incoming`, paginated
`/api/grants/incoming/resources`) reuses the same subject expansion. A user
listing their incoming grants sees both:

- Direct grants where `subject_id = caller_id`.
- Group-mediated grants where `subject_id ∈ groups_for_user(caller) ∪ {Internal}`.

This guarantees that *anything the engine would allow* also surfaces in the
listing — no silent gap between "you have access" and "you see it". The
single chokepoint is `PgAclEngine::subject_match_set(...)`, shared by `check`
and the listing queries.

The reverse direction (`/api/grants/outgoing` — "what I've shared") filters
on `granted_by = caller`. Group membership has no role there.

---

## Lifecycle

Two state machines run alongside grants:

- **Resource deletion** — folder / file / drive / calendar / address book
  / playlist delete each fire a per-type trigger
  (`trg_cleanup_role_grants_folder`, `trg_cleanup_role_grants_file`,
  `trg_cleanup_role_grants_drive`, and the three for the standalone
  resource types) that nukes every grant whose `resource_id` matches.
  Same transaction; clients see grants vanish from incoming lists
  immediately.
- **Subject deletion** — deleting a user or group cascades to their
  outgoing/incoming grants via FK + matching triggers.

Expiry is enforced inline at read time: `expires_at IS NULL OR expires_at > NOW()`
is part of every cascade query, so an expired grant is invisible to the engine the
moment its timestamp passes. The AuthZ hot path never needs to consult a sweeper.

### Post-expiry cleanup

Dead rows are physically deleted by a background daemon, `GrantCleanupService`,
so `role_grants` doesn't accumulate lapsed rows indefinitely (each share with a
TTL would otherwise leave a permanent row unless someone manually revoked it).

| Env | Default | Meaning |
|---|---|---|
| `OXICLOUD_GRANT_CLEANUP_ENABLED` | `true` | Master switch. Default **on** — expired-grant purge is a security-hygiene default, not opt-in. |
| `OXICLOUD_GRANT_CLEANUP_GRACE_DAYS` | `15` | Days past `expires_at` before a row is eligible for deletion. |
| `OXICLOUD_GRANT_CLEANUP_INTERVAL_HOURS` | `24` | How often the daemon fires. |

The grace window (default 15 days) preserves the audit / support answer to
*"what happened to my access?"* for two weeks past expiration, then the row
goes. Because the AuthZ engine's `expires_at` filter is at read time, the
grace window has zero effect on live access decisions — an expired grant is
invisible to `check(...)` even during the grace period. Cleanup only affects
storage bloat and the `list_grants_*` history surface.

The daemon runs inside the same process, registered with the periodic-job
scheduler (`docs/plan/job-registry.md`) on a 24-hour tick. An admin-
triggered `POST /api/admin/jobs/grant_cleanup/trigger?force=true` lets
operators force a purge in test or incident scenarios — `force=true`
collapses the grace window to zero for that call only.

The [Share Integration](/architecture/share-integration) doc's reverse
trigger takes it from there: when the daemon deletes the last `role_grants`
row for a share-token subject, `trg_cleanup_share_on_grant_delete` fires and
deletes the paired `storage.shares` row in the same transaction. Expired
public shares vanish end-to-end after the grace window without any operator
intervention.

---

## What ReBAC does *not* cover (yet)

Extensions sketched in the design notes but not yet implemented:

- **`Resource::SubjectGroup(id)`** — per-group Manage / use-as-subject
  grants. Would let non-admins curate their own groups via the same
  engine path as files/folders/drives. `Permission::Manage` already
  exists in the enum for this reason; only the resource variant and
  the handler wiring are pending.
- **Global roles in the JWT** (`role = "admin"`) — today these gate a
  few admin-only management endpoints (user CRUD, group CRUD, admin
  settings). They live outside ReBAC because they're cross-cutting
  concerns, not per-resource permissions.
- **Materialised rights (v2)** — a future flattening of the cascade
  into an indexed materialised view for O(1) reads. Deferred; see
  `docs/plan/` for design.

---

## File map

| Concern | Module |
|---|---|
| Domain types (`Subject`, `Resource`, `Role`, `Permission`) + `Role::expand()` | `src/domain/services/authorization.rs` |
| Subject groups (entity + repo trait) | `src/domain/entities/subject_group.rs`, `src/domain/repositories/subject_group_repository.rs` |
| Engine — `check`, listing, expansion, cache | `src/infrastructure/services/pg_acl_engine.rs` |
| Group repo — recursive CTEs, cycle/depth | `src/infrastructure/repositories/pg/subject_group_pg_repository.rs` |
| Grant DTOs | `src/application/dtos/grant_dto.rs` |
| Schema — `role_grants` + ENUM + triggers, `subject_groups`, `subject_group_members` | `migrations/20260730000000_role_grants.sql` and follow-ups |
| REST handlers | `src/interfaces/api/handlers/grant_handler.rs`, `subject_group_handler.rs` |
| Hurl coverage | `tests/api/grants.hurl`, `subject_groups.hurl`, `grants_nested_groups.hurl`, `drives_membership.hurl` |
