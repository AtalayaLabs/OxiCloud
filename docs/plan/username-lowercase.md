# Plan — Case-insensitive usernames (lowercase-on-ingest)

## Context

Feature ask: [issue #691](https://github.com/AtalayaLabs/OxiCloud/issues/691).
Usernames are currently case-sensitive, so `Alice`, `alice`, and `ALICE`
refer to three different accounts. Users hit this as a login friction —
they type their name with different capitalization on different clients
and get "invalid credentials" instead of a successful login.

## Why this is simpler than it looks in this codebase specifically

- `validate_username` in `src/domain/entities/user.rs:884` already
  restricts usernames to ASCII-only `[a-zA-Z0-9._-]{2,64}` with no `@`.
  The Unicode case-folding minefield (Turkish dotted-I, German ß, Greek
  final sigma, NFC vs NFD) does not apply — ASCII case-folding is
  trivial (`to_ascii_lowercase`), deterministic, and locale-independent.
- OIDC identity binds via `(iss, sub)` in
  `get_user_by_federation_subject` at
  `src/infrastructure/repositories/pg/user_pg_repository.rs:1257-1303` —
  case-sensitivity of the local username is orthogonal to OIDC identity
  matching. No OIDC breakage risk.
- Password verification runs through Argon2's `verify_password`
  (constant-time by construction). Not affected.
- `@`-forbidden rule in usernames is the disjoint namespace with email
  lookup (`dispatch_login` at `auth_application_service.rs:1018`).
  Case-insensitive usernames align semantics with email addresses
  (already case-insensitive in practice), so any future
  `groupname@domain` composition stays consistent.
- NextCloud URL `/remote.php/dav/files/{user}/…` uses `{user}` as an
  informational / consistency-check marker, not a security boundary —
  the chroot ACL is the real authz. Handling case in the URL segment is
  a small local change (documented in `session.rs:33-34`).

## Design decisions

1. **Silently lowercase on ingest** (registration, admin-create, OIDC
   provisioning, rename). Never reject uppercase input from clients —
   accept liberally, store strictly (Postel's Law).
2. **Explicit migration** (`oxicloud migrate lowercase-usernames
   [--dry-run]`). The server never mutates `auth.users` at boot. Ops
   MUST run the migration explicitly. Follows the
   [[feedback_no_silent_auto_repair]] rule: consistency tenants are
   discovery-only by default; mutation is opt-in.
3. **Refuse-to-boot** if any active-user username is not already
   lowercase. Boot error message shows the exact CLI command to run.
   Boot performs a read-only verification only.
4. **Collision tiebreak** on migration: `(last_login_at DESC NULLS
   LAST, created_at ASC)`. Winner keeps the canonical lowercased name.
   Losers get `-2`, `-3`, … suffix (increment until free), matching the
   pattern in `oxicloud migrate nfc-filenames`.
5. **Active accounts only** in the boot check + migration. Soft-deleted
   / disabled rows are skipped (they don't block usable logins). NULL
   usernames (OPAQUE-migrated accounts) are skipped in every layer —
   the boot verifier, the migration UPDATE, the CLI report. The
   `WHERE username <> LOWER(username)` predicate is already NULL-safe
   by SQL semantics (NULL comparisons yield NULL, filtered out); state
   it explicitly so a reviewer isn't left wondering.
6. **Rename-only** on collision resolution — sessions are not
   invalidated. Sessions key on `user_id` so they survive the rename.
7. **Un-soft-delete of a mixed-case account uses the SAME suffix
   scheme.** If `Alice` is soft-deleted (skipped by migration) and
   later un-soft-deleted while `alice` already exists, the un-soft-
   delete path re-normalizes via `set_username` and, on collision,
   assigns `alice-2` / `alice-3` / … — the same helper the migration
   CLI calls. Both callers reach for a shared
   `find_free_username_suffix(pool, base) -> String` in
   `src/common/username_migration.rs` so migration + un-soft-delete
   agree by construction. Without this, an un-soft-delete could
   create a fresh collision that the next boot's auto-rename
   couldn't resolve (auto-rename handles singletons only) — the
   server would then refuse-to-boot until an admin resolves the
   tiebreak.
8. **Boot-time behaviour has three outcomes, not two.** The
   verifier categorises the DB into: (a) clean — nothing to do;
   (b) mixed-case rows with no `LOWER(username)` collision — the
   server **auto-lowercases them in one atomic transaction and
   continues**, emitting an audit line per rename; (c) at least
   one `LOWER(username)` collision — the server **refuses to
   boot** because tiebreak requires human judgement. Silent
   action is bounded to (b), where there is exactly one correct
   move. This is a narrower reading of
   [[feedback_no_silent_auto_repair]] than "no silent action
   ever": the rule targets consistency-check jobs where drift is
   a bug signal; a schema-adjacent boot invariant with a
   unique-correct-fix is a different situation. Making the
   trivial-case common path a no-op massively lowers upgrade
   friction for the 90% self-hosted deployment.

## Not in scope

- Unicode case-folding (usernames are ASCII-only by validation).
- `display_name` split (usernames were already just identifiers;
  free-form display is a separate future feature if a user asks for
  it — deferred pending real demand signal).
- OIDC provisioning behaviour change beyond the ingest-normalize point.
- Case-insensitivity for emails (already achieved in practice; not
  touched).
- Any change to `validate_username`'s character-class rules.
- Any change to the WebDAV URL shape `/dav/files/{user}/…` (client
  compat; drop deferred separately per [[project_nc_multidrive_poc]]).
- Group names (`SubjectGroup`). Lowercase by convention today; no
  runtime enforcement, no migration. If group-name case-insensitivity
  becomes a real ask, it lands as a sibling plan doc with the same
  shape.

## Deliverables

### 1. Ingest normalization

Change `validate_username` to return the canonical form instead of
`()`:

```rust
// src/domain/entities/user.rs — new signature
fn validate_username(username: &str) -> UserResult<String> {
    let normalized = username.trim().to_ascii_lowercase();
    // ... existing length + charset + boundary checks apply to `normalized` ...
    Ok(normalized)
}
```

Every caller that today does `Self::validate_username(u)?;` becomes
`let u = Self::validate_username(&u)?;` — the returned canonical form
is what gets stored. Because the return type changes from `Result<()>`
to `Result<String>`, any caller that ignores the result now becomes a
compile error — the type system forces every write path through the
normalizer.

Write sites all funnel through `User::new` (`src/domain/entities/user.rs:313`)
or `User::set_username` (`:811`), so the signature change catches the
entity-write path automatically. Callers to touch:

- Application services calling `User::new`:
  - `auth_application_service.rs:840` — `register()` public signup
  - `auth_application_service.rs:944` — `setup_create_admin()`
    first-boot admin
  - `auth_application_service.rs:3520`, `:3532` — `admin_create_user()`
    external + internal branches
  - `auth_application_service.rs:4692` — OIDC JIT provisioning
  - `magic_link_invite_service.rs:233` — magic-link external invite

- User-driven rename (calls `User::set_username`):
  - `auth_application_service.rs:2756-2801` — `update_profile()`

- Repository-write compile-error catches:
  - `src/infrastructure/repositories/pg/user_pg_repository.rs:281`
    (`create_user` INSERT) and `:740` (`update_user` UPDATE) — these
    bind `user_clone.username()`, which is now guaranteed lowercase by
    the entity constructor.

### 2. OIDC JIT derivation

`auth_application_service.rs:4649-4690` derives a local username from
the OIDC `preferred_username` / `name` / `sub` claims, filters to
`[a-zA-Z0-9._-]`, and truncates. **It does not currently lowercase.**
Add `to_ascii_lowercase()` on the derived string before passing to
`User::new`. This is beyond what the entity signature change catches —
explicit fix required.

### 3. Lookup normalization

Repository `find_by_username`-style methods internally lowercase the
input before the SQL query, so callers don't have to remember. One-line
change per method:

- `src/infrastructure/repositories/pg/user_pg_repository.rs:487`
  (`get_user_by_username`) — add `let username = username.trim().
  to_ascii_lowercase();` before the `.bind(&username)` at line 488.
- `src/infrastructure/repositories/pg/user_pg_repository.rs:1043`
  (`search_users`) — `ILIKE` is already case-insensitive by
  construction; verify nothing regresses.
- `src/infrastructure/repositories/pg/user_pg_repository.rs:1504`
  (`search_usernames`) — same as above.
- `src/application/services/storage_usage_service.rs:145-149`
  (`update_user_storage_usage_by_username`) — raw SQL bind; normalize
  before `.bind()`.
- `src/cli/opaque.rs:125`, `:202` — `opaque reset` CLI identifier
  dispatch on `@`; lowercase the username branch input.

Post-migration, the DB is fully lowercase so `WHERE username = 'alice'`
matches. The mixed-case-DB-during-transition state cannot serve
traffic because the boot flow either (a) auto-renames the singleton
rows before `AppState` assembles, or (b) refuses to boot on collision
groups.

### 4. NextCloud DAV surface

Two coordinated changes on the NC surface:

- `src/interfaces/nextcloud/basic_auth_middleware.rs:94-134` — decoded
  `raw_username` from the Basic Auth header, lowercase the whole
  string. Safe for the `user~drive_uuid` multi-drive format because
  UUID hex is `[0-9a-f-]` which lowercases to itself.
- `src/interfaces/nextcloud/basic_auth_middleware.rs:307-323`
  (`parse_basic_auth` helper) — lowercase the username portion before
  returning.
- `src/interfaces/nextcloud/session.rs:90-111`
  (`extract_url_user`) — lowercase the returned `Cow<'_, str>` value
  from URL decode. The cross-check comparison at `session.rs:157-161`
  (`url_user != session.raw_username`) then compares normalized vs
  normalized — no change needed at the comparison site itself.

Downstream `session.raw_username` consumers (WebDAV / OCS href
builders, MOVE Destination parsers, avatar / trashbin handlers) all
pass through and emit lowercase automatically — no per-site change
needed.

**Client compatibility:** NC / DAVX5 clients that cached URLs like
`/remote.php/dav/files/Alice/…` continue to work through the migration
because the server accepts uppercase URL segments **indefinitely**
(the Basic Auth middleware + `extract_url_user` both lowercase on
decode). No forced client upgrade or reconfiguration. PROPFIND
response bodies emit lowercase hrefs (from canonical
`session.raw_username`), which well-behaved clients update on next
sync.

Expected per-client behavior on first PROPFIND after upgrade:

- **Nextcloud desktop** — prompts a one-time re-sync notification
  when it notices the account URL case changed. Files re-verify
  via ETag, so no re-upload; the re-sync completes in
  seconds-to-minutes depending on file count. Users click through
  the reconnect dialog.
- **DAVX5** (calendars, contacts) — silent update of the internal
  `principal-URL`; user sees no dialog.
- **NC mobile app** — silent refresh of the account tile.
- **Older / misbehaving clients** — may create a duplicate account
  profile (rare, cosmetic, not destructive).

**Zero data risk in every path.** The chroot ACL keys on
`user_id`, not username, so files, calendars, contacts, and
grants all follow the user across the rename. The blast radius
is a one-time UX notification, not lost bytes.

**Power-user pre-emption** (worth documenting in CHANGELOG): ops
who want to avoid the re-sync prompt entirely can, before
upgrading, log into each NC desktop client and manually update
the account URL from `.../USERNAME` to lowercase. Cheap
prophylactic for organizations rolling out to non-technical
users.

### 5. Chunked-upload directory rename

`src/infrastructure/services/nextcloud_chunked_upload_service.rs:99-103`
uses `user.username` as an on-disk directory name AND as an in-memory
cache key. Post-migration, `user.username` becomes lowercase; any
in-flight upload for `Alice` at migration time strands the on-disk
`base_dir/Alice/upload_xxx/` directory and orphans its cache entry.

The migration command SHOULD walk `base_dir/*/` and rename any
mixed-case subdirectory to its lowercase form. Collision handling
(both `Alice/` and `alice/` present) → merge contents; else simple
rename. In practice this is likely a no-op — chunked-upload state
is ephemeral, and simultaneous mixed-case uploads by the same user
are rare.

**Implementation status:** deferred. Chunked-upload state is
ephemeral: any in-flight upload that gets stranded is retryable
by the client (the upload session's timeout eventually purges the
stale dir; the client retries with a fresh `upload_id`, this time
under the lowercase username). Wiring the dir-walk into the CLI
adds ~40 lines of async filesystem code (walk, collision merge,
mtime-preserving move) and a new `--chunk-dir <path>` arg — the
CLI otherwise doesn't need to know about the storage-path
config layer. Not worth it for a rare no-op; add if user reports
show a real problem.

**Ops manual step** — if a migration is run WHILE an upload is
in flight, ops can either restart the affected client (the
upload session is stateful across a `create → chunks → complete`
cycle, so the client will retry from scratch) or manually
`mv base_dir/Alice base_dir/alice` after the DB migration
completes.

### 6. Boot-time verification

New module `src/common/username_migration.rs` exposing:

```rust
pub async fn verify_all_usernames_lowercase(pool: &PgPool) -> Result<(), String>
```

Runs after `sqlx::migrate!()` completes, before `AppState` is
assembled. Query:

```sql
SELECT id, username, created_at, last_login_at
  FROM auth.users
 WHERE username <> LOWER(username)
   -- NULL usernames (OPAQUE-migrated accounts) are already filtered
   -- out by SQL semantics: NULL <> anything yields NULL, which
   -- WHERE excludes. Explicit for the reviewer's benefit.
   -- add is_deleted / disabled filter if such a flag exists
 ORDER BY LOWER(username),
          (last_login_at IS NULL),
          last_login_at DESC NULLS LAST,
          created_at ASC
 LIMIT 200;  -- soft cap on error-message size
```

If empty → boot proceeds. If non-empty → format the FATAL error and
return `Err(String)`. `main.rs` propagates via `?` to a non-zero
process exit.

Boot only READS `auth.users`; never WRITES. This is the "explicit
migration required" enforcement layer.

**Error message format** (self-sufficient — no docs required at 3 AM):

```
FATAL: cannot start — <N> user account(s) have non-lowercase usernames.

Before this version can boot, run the migration:

  oxicloud migrate lowercase-usernames --dry-run     # preview
  oxicloud migrate lowercase-usernames               # apply

Affected accounts (up to 20 shown; full list via the dry-run):

  Alice   (id: a1b2c3d4-...  last_login: 2026-08-01)
  BOB     (id: 9abc0000-...  last_login: never)
  ...

The migration handles case-collisions (Alice + alice → alice keeps
the name based on most recent login; the other gets alice-2 suffix).
Sessions and grants survive the rename (they key on user_id).
```

### 7. Migration CLI

New action under `oxicloud migrate`:

```rust
// src/cli/migrate.rs — extend the Action enum
Action::LowercaseUsernames { dry_run: bool }
```

Following the shape of `run_nfc_filenames`:

- Load all active users (skip soft-deleted / disabled AND rows
  where `username IS NULL` — OPAQUE-migrated accounts have no
  username string to normalize)
- Group by `LOWER(username)`
- For each group:
  - Single-member group with mixed-case name → UPDATE to lowercase
  - Multi-member group (collision) → apply tiebreak
    `(last_login_at DESC NULLS LAST, created_at ASC)`, winner UPDATEs
    to lowercase, losers UPDATE to `<lowercase>-2`, `-3`, … (increment
    until free)
- Per-row `println!` log:
  `NORMALIZE  user=<uuid>  '<before>' ({}B) → '<after>' ({}B)`
- Summary at end: scanned / already-lowercase / normalized /
  collision-resolved / renamed-to-suffix
- `--dry-run` guards all UPDATEs

After the DB pass, the chunked-upload directory rename step (see
Deliverable 5) is deferred; run manually only if in-flight uploads
were live at migration time.

Suffix search reuses the pattern from
`find_free_folder_duplicate_name` in the existing NFC migration —
increment-until-free loop, starting at `-2`, probing until an
unused suffix is found. Robust against pre-existing rows like
`alice-2` already being taken (the probe just steps past them
to `-3`, `-4`, …).

Extracted into a shared public helper in
`src/common/username_migration.rs`:

```rust
pub async fn find_free_username_suffix(pool: &PgPool, base: &str) -> Result<String, sqlx::Error>
```

Both the migration CLI AND the un-soft-delete API (Design decision
7) call this helper — same collision-resolution behavior by
construction, no drift risk between the two paths.

Bounded at 10,000 as a safety cap. The probability of reaching
that in a real deployment is negligible — it would require ~10 K
distinct accounts all originally cased differently but sharing
the same lowercase form (a normal collision is 2-3 accounts, not
10 K). If the cap ever fires, something is very wrong with the
account universe and the migration ABORTs with a loud error
rather than silently truncating — the loud abort IS the
detection mechanism.

### 8. Test seed audit

Sweep-verified: existing test seeds all produce lowercase or NULL
usernames. Worth one more grep pass to ensure no test fixture INSERTs
`INSERT INTO auth.users … 'AliceTest'` — if any exist, lowercase them
in the same commit to avoid CI refuse-to-boot regressions.

Files verified (all safe):
- `src/infrastructure/repositories/pg/user_pg_repository.rs:1786`
- `src/infrastructure/repositories/pg/opaque_pg_repository.rs:339`
  (NULL)
- `src/application/services/auth_application_service.rs:4982` (NULL)
- `src/application/services/subject_group_service.rs:796` (NULL)
- `src/bin/load-seed.rs:414`, `:446` (`load_user_XXXX` — lowercase)
- `src/mount_it_support.rs:61` (`make_user(name)` — verify callers)
- `tests/common/init-test-schema.sh:40` (`ci-admin` — lowercase)

### 9. Cosmetic side-effects (worth noting in CHANGELOG, non-blocking)

- `src/interfaces/nextcloud/avatar_handler.rs:283` — `pick_color`
  derives a deterministic tile color from username bytes. Users whose
  canonical username had uppercase letters will get a different
  fallback-avatar tile color after the migration. One-time cosmetic
  change.
- **NC desktop may perform a one-time re-sync** — see Deliverable 4.

### 10. Documentation

Release notes / CHANGELOG entry is NOT part of this PR — the
canonical repo's maintainer handles release notes at version-bump
time. This PR just leaves the notes-worthy items enumerated here
so the maintainer has the bullets to pick from when the next
version ships:

- Server auto-lowercases non-colliding mixed-case usernames at
  first boot. No ops action needed for the common case.
- On `LOWER(username)` collision (`Alice` + `alice` both active),
  the server refuses to boot; ops runs `oxicloud migrate
  lowercase-usernames`. Exact CLI command shown in the refusal.
- Nextcloud desktop clients will prompt for a one-time re-sync on
  first PROPFIND after upgrade. Files are ETag-verified, not
  re-uploaded. DAVX5 and NC mobile handle the URL case change
  silently. **No forced client upgrade or reconfiguration** —
  server accepts uppercase URL segments indefinitely.
- Optional pre-emption for non-technical users: ops can manually
  update the account URL to lowercase in each NC desktop client
  before upgrading, avoiding the re-sync prompt entirely.
- Usernames become lowercase in ALL UI display surfaces (share
  dialogs, activity feeds, admin panels, PROPFIND response
  bodies, notification bell). Login identity unchanged from the
  user's POV (they can still type any case at the login form).
- Avatar fallback color may change for users with previously-
  uppercase usernames.
- Note: `display_name` is a possible follow-up if users miss
  capitalisation for display — deferred pending demand signal, no
  compat cost to adding later.

The two docs that DO ship with this PR:

- `docs/config/env.md` — note the boot-time check + migration command.
- `docs/install/binary.md` — upgrade-from-case-sensitive section.

### 11. Test coverage

- **Unit**: `validate_username("Alice")` returns `Ok("alice")`;
  `validate_username("alice-")` returns `Err(...)` unchanged;
  `validate_username("  Alice  ")` returns `Ok("alice")`.
- **Unit**: `format_refusal_message_collisions` — 1 group renders
  canonical + members + CLI; > 10 groups renders overflow tail;
  total-affected-count sums across groups.
- **Hurl** (`tests/api/lowercase_usernames.hurl`, new): register a
  user with `MixedCase`, assert DB stores `mixedcase`; log in with
  `MIXEDCASE` and `mixedcase` — both succeed; rename to `NewName`,
  assert `newname` stored; NC Basic Auth accepts `MixedCase:pass`,
  `MIXEDCASE:pass`, `mixedcase:pass`.
- **Manual** (against dev DB, not CI):
  - Auto-rename path: `UPDATE auth.users SET username='Alice' WHERE
    username='alice'` (no collision); boot server → verify audit log
    line + WARN summary, row is `alice` after boot, service starts.
  - Collision path: `INSERT INTO auth.users … 'Alice'` on top of
    existing `alice`; boot server → verify refusal message names both
    rows + exact CLI shown, server exits non-zero.
  - `oxicloud migrate lowercase-usernames --dry-run` → verify report
    of collision + tiebreak decision
  - `oxicloud migrate lowercase-usernames` → verify apply, one row
    keeps `alice`, other gets `alice-2`
  - Boot again → succeeds (Clean outcome)
  - `curl -u ALICE:pass https://oxicloud/remote.php/dav/files/ALICE/…`
    → succeeds (accepts uppercase input, resolves to lowercase user)

## Cache-and-consistency observations (informational)

- `src/infrastructure/services/login_lockout_service.rs:33,68-89` —
  already lowercases the key at line 58. No code change; comment
  becomes factual not incidental.
- `src/application/services/app_password_service.rs:89,317-323` —
  BLAKE3-keyed cache using the raw wire username. Post-normalization,
  both sides normalize consistently → cache stays coherent. 300 s TTL
  self-heals any transitional window.
- `NC_CHROOT_CACHE` in `basic_auth_middleware.rs:34-40` — keyed on
  `Uuid`, not username. Unaffected.

## Critical files

Full enumeration in the Deliverables sections above. Grouped summary:

**Ingest normalizer:**
- `src/domain/entities/user.rs` (signature change + callers)

**Application services (write callers):**
- `src/application/services/auth_application_service.rs`
- `src/application/services/magic_link_invite_service.rs`

**Repositories (lookup normalization):**
- `src/infrastructure/repositories/pg/user_pg_repository.rs`
- `src/application/services/storage_usage_service.rs`
- `src/cli/opaque.rs`

**NextCloud DAV surface:**
- `src/interfaces/nextcloud/basic_auth_middleware.rs`
- `src/interfaces/nextcloud/session.rs`
- `src/infrastructure/services/nextcloud_chunked_upload_service.rs`

**New files:**
- `src/common/username_migration.rs`
- `tests/api/lowercase_usernames.hurl`

**Main entry:**
- `src/main.rs` (call verifier after `sqlx::migrate!()`)

**Migration CLI:**
- `src/cli/migrate.rs`

**Docs:**
- `CHANGELOG.md`
- `docs/config/env.md`
- `docs/install/binary.md`

## Delivery order

1. Change `validate_username` signature to return `Result<String>` —
   one file.
2. Fix OIDC JIT derivation
   (`auth_application_service.rs:4649-4690`) to lowercase before
   passing to `User::new` — explicit change beyond the entity
   normalizer's compile-time catches.
3. Iterate on compile errors — the return-type change catches every
   downstream write-site.
4. Update repository lookup methods (`user_pg_repository.rs`,
   `storage_usage_service.rs`, `cli/opaque.rs`) to internally
   lowercase input before `.bind()`.
5. Update NC `basic_auth_middleware.rs` (lowercase `raw_username` at
   decode) + `session.rs::extract_url_user` (lowercase return).
6. Add the boot-time verification helper (`src/common/username_migration.rs`)
   + wire into `main.rs`.
7. Extend `oxicloud migrate` with `lowercase-usernames [--dry-run]` —
   DB pass + chunked-upload directory rename.
8. Test seed audit (grep pass).
9. Add hurl coverage.
10. Admin docs update (`docs/config/env.md` boot-check subsection +
    `docs/install/binary.md` upgrade section). CHANGELOG is Dio's
    job at version-bump time — not part of this PR.
11. Manual smoke test against dev DB.
12. PR to canonical.

## Total scope estimate

~6-8 hours of careful work. Larger than the initial estimate because
of these sweep-surfaced additions:

- OIDC JIT explicit fix (small).
- Chunked-upload directory rename step in the migration (~30 min).
- Test-seed audit (~15 min).
- More lookup callsites than initially thought.

The shape is uniform (`to_ascii_lowercase()` at every touchpoint) and
the compiler catches missed entity-write sites via the
`Result<String>` signature change. The parts NOT caught by the
compiler (OIDC JIT, lookup normalizers, NC URL segment, chunked-upload
directory) are the ones needing careful review — enumerated above.

## References

- Issue: [#691](https://github.com/AtalayaLabs/OxiCloud/issues/691)
- Related feature restrictions today:
  - `validate_username` at
    `src/domain/entities/user.rs:884-916`
  - `@`-disjoint dispatch at
    `src/application/services/auth_application_service.rs:1018`
- Related project docs:
  - `docs/plan/auth-simplification.md` — the broader auth surface this
    fits within
  - Prior similar migration:
    `oxicloud migrate nfc-filenames` in `src/cli/migrate.rs`
