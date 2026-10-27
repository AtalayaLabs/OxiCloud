# Photos — personal archive + library-folder scope

> **Status**: design proposal, locked after design discussion with Ed
> (Oct 2026). Two features, two migrations, two PRs. This document
> covers both because they share a query path.

## Context

OxiCloud's photos view today is a flat time-ordered feed of every
image/video resource visible to the caller. Two user problems surface
from that:

1. **No way to hide a specific photo from the feed** without deleting it.
   An embarrassing-screenshot / receipt-photo / expired-coupon piles up
   in Memories forever. Every modern photos app (Google Photos, Apple
   Photos, iCloud) solves this with a per-user **archive** gesture —
   the photo stays in storage, but it's hidden from the main grid and
   from derivatives (Memories, People, Places).

2. **No way to scope the library to the folders the user cares about.**
   Users importing from Android devices land `/DCIM/Camera` and
   `/WhatsApp/Media/WhatsApp Images` as separate directory trees;
   a Nextcloud desktop sync drops photos from the whole user home.
   Everything shows up in the main grid together: holiday photos next
   to meme forwards next to screenshots of QR codes. Google Photos
   solves this with **Backup folders** — a per-user allowlist of
   directory trees that participate in the library.

Both features are user-view preferences. Neither touches the actual
file rows or their intrinsic metadata. They compose cleanly at query
time.

## Scope

- **F1 — Per-user archive.** Explicit "hide this photo" action, scoped
  to the acting user. One row per `(user_id, file_id)`.
- **F2 — Library folder scope.** Per-user allowlist of folders whose
  descendants populate the photos view. Empty allowlist = "all photos
  visible" (friendly default for new installs and users who never
  configure anything).

## Non-goals

- **Drive-level archive.** Discussed and rejected: it collides with
  the ReBAC permission model (who among 50 shared-drive viewers can
  hide content from the other 49?) and doesn't exist in the reference
  UX (Google Photos). If retire-this-content ever becomes a demand,
  it ships as a *different concept* with a *different table* so the
  two can compose without one erasing the other.
- **Automatic archive by folder rule.** Discussed and rejected: the
  move/rename synchronization problem is nontrivial, provenance in the
  `file_archives` row would conflate "user said hide" with "rule
  fired", and the user intent ("library scope") is already covered
  by F2 as a cleaner feature.
- **Albums.** A different model (user-named containers with ordinals,
  covers, sharing semantics). Deferred to its own plan.
- **Folder archive.** Not planned — Google Photos doesn't offer it,
  OxiCloud has trash for the "retire a folder" case. If raised later,
  it's one new table (`folder_archives`) mirroring F1's shape.

## Design

### Why per-user, not per-drive

The semantic that decides: *who owns the hide-this decision?*

- **Per-user**: the viewer. Each user carries their own library
  hygiene. Alice archives, only Alice's grid changes.
- **Per-drive**: the curator. The hide propagates to every member.
  Opens an unresolvable permission question (if Viewers can archive,
  one disgruntled Viewer wipes the Owner's grid; if Viewers can't,
  Viewers have no library-hygiene recourse on shared drives).

OxiCloud photos aim for Google-Workspace feature ambition
(`AGENTS.md`). The per-user model matches that UX convention and
sidesteps the permission tangle. On a personal drive (single-user)
the two models are indistinguishable, so no regression.

### Why no auto-archive materialization

Pre-computing archive rows at ingest time based on folder rules would
save ~10–30 ms per listing (one fewer JOIN + CTE) but costs:

- Ingest + move + rename + folder-delete hooks that re-evaluate rules
- Provenance ambiguity (`file_archives` row can no longer tell "user
  action" from "rule fired")
- Sync-drift bug class (race between rule mutation and in-flight
  uploads)

At the 10k-user target, 10–30 ms per paginated page is not where to
spend complexity. Keep the filter lazy and composable at query time.

## Data model

### F1 — `storage.file_archives`

```sql
CREATE TABLE storage.file_archives (
    user_id      UUID        NOT NULL,
    file_id      UUID        NOT NULL REFERENCES storage.files(id) ON DELETE CASCADE,
    archived_at  TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (user_id, file_id)
);

-- Covers the admin-side "which users archived this file" query (rare,
-- but cheap to carry; the PK covers the hot per-user case already).
CREATE INDEX idx_file_archives_file ON storage.file_archives(file_id);
```

**Design notes**

- **Composite PK on `(user_id, file_id)`** rather than `SERIAL id +
  UNIQUE` (the shape `auth.user_favorites` uses). One b-tree instead
  of two; cheaper, and the tuple is also the natural lookup key —
  there's no admin flow that needs a surrogate `id`.
- **No FK on `user_id`.** Matches the `storage.*` schema's provenance
  convention (`project_provenance_column_convention`): `user_id`
  columns are `NOT NULL` with no FK to `auth.users`, keeping the
  storage schema independent of auth-schema ownership for cross-schema
  maintenance.
- **`REFERENCES storage.files(id) ON DELETE CASCADE`** cleans the row
  automatically on file deletion. Avoids the orphan accumulation that
  `auth.user_favorites` suffers (its `item_id TEXT` can't FK to one of
  two tables, so orphans survive forever).
- **File-only, not polymorphic.** Folders don't need archive today
  (there's no "folder in a photos grid" case). If folders ever gain
  archive, add `folder_archives` as a sibling — don't retrofit
  polymorphism into this table.

### F2 — `storage.photos_folder_scope`

```sql
CREATE TABLE storage.photos_folder_scope (
    user_id   UUID        NOT NULL,
    folder_id UUID        NOT NULL REFERENCES storage.folders(id) ON DELETE CASCADE,
    recursive BOOLEAN     NOT NULL DEFAULT TRUE,
    added_at  TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (user_id, folder_id)
);
```

**Design notes**

- **`recursive BOOLEAN`** lets a user say either "DCIM and everything
  under it" (default) or "exactly this folder, not its children"
  (unusual but easy to carry).
- **No separate blocklist**. Allowlist-only in Phase A. If the demand
  for "see everything *except* X" surfaces, ship a sibling table
  `photos_folder_exclude` rather than inverting allowlist semantics.
- **Empty table for a user = "see everything"** (friendly new-user
  default). Non-empty = allowlist mode. The query shape below carries
  this.
- **ON DELETE CASCADE on `folder_id`**: when a folder is deleted, the
  rule referencing it drops automatically. On rename or move, nothing
  changes (the rule is by `folder_id`, not by path).
- **Validation cap: ~50 scope entries per user** at the service layer.
  More is a pathological case; the GIST `ANY(array)` lookup stays
  log-time but the array copy is bounded.

### Load-bearing index

Already shipped: `idx_folders_lpath` (GIST on `storage.folders.lpath`,
migration `20260307000000`). The `<@` subtree operator uses it. No new
GIST index needed.

## Query integration

### The default photos list predicate

```sql
WITH scope AS (
    SELECT fo.lpath, s.recursive
      FROM storage.photos_folder_scope s
      JOIN storage.folders fo ON fo.id = s.folder_id
     WHERE s.user_id = $caller
),
scope_recursive AS (
    SELECT array_agg(lpath) AS paths FROM scope WHERE recursive
),
scope_leaf AS (
    SELECT array_agg(lpath) AS paths FROM scope WHERE NOT recursive
)
SELECT f.*
  FROM storage.files f
  JOIN storage.folders ff ON ff.id = f.folder_id
 WHERE
    -- F2: library-folder scope. Empty allowlist = include everything.
    (
        NOT EXISTS (SELECT 1 FROM storage.photos_folder_scope WHERE user_id = $caller)
        OR ff.lpath <@ ANY(SELECT paths FROM scope_recursive)
        OR ff.lpath =  ANY(SELECT paths FROM scope_leaf)
    )
    -- F1: per-user archive.
    AND NOT EXISTS (
        SELECT 1 FROM storage.file_archives
         WHERE user_id = $caller AND file_id = f.id
    )
    -- …plus the existing predicates (drive scope, trash, mime, grants).
;
```

**Three things keep this fast and must survive code review**

1. **CTE materializes the scope paths once.** A correlated subquery
   would re-evaluate the ltree lookup per row and tank the GIST index
   path.
2. **`<@ ANY(array)` + GIST** is the index-accelerated shape. Not
   `path <@ (SELECT path FROM scope WHERE …)`.
3. **Fast path on empty scope** short-circuits with
   `NOT EXISTS (SELECT 1 FROM photos_folder_scope WHERE user_id = …)`
   — one PK-covered existence check, hoisted out of the per-row
   predicate.

### Pipeline-wide integration

The same two predicates apply to every photos-adjacent surface by
default, with a documented `include_archived=true` escape hatch:

- **`GET /api/photos`** — default feed.
- **`GET /api/photos/geo`** (map view / Places). Hide archived from
  the pins by default.
- **`GET /api/people`** (face clustering / People). Archived photos
  stay in the face DB (so re-un-archiving a photo doesn't re-trigger
  recognition), but their thumbnails and tiles don't surface in People
  cards or "photos of X" listings.
- **Search** (`GET /api/search?...` with mime filter): default
  excludes archived. Explicit toggle in advanced search.
- **Memories / Rediscover** (if ever): never surface archived.
- **Albums** (future): album membership overrides scope — a photo in
  an album the user is viewing is visible even if its folder is out of
  scope. Archive is NOT overridden by album membership — if the user
  archived it, it stays hidden everywhere, including in albums (match
  Google Photos).

### Query locations touched

Approximate list of repo files whose predicates need the composed
`archived + scope` filter added:

- `src/infrastructure/repositories/pg/photos_pg_repository.rs`
  (list / count / geo variants)
- `src/application/services/people_service.rs` (face-tile
  enumeration)
- `src/application/services/search_service.rs` (image/video filter
  branch)

Each gets the same CTE prefix. The two filter predicates belong on a
shared helper (`photos_visibility_filter()` or similar) so a future
third predicate — "exclude this content type" — can land in one place.

## API surface

### F1 — archive endpoints

```
POST   /api/photos/{file_id}/archive                 → 204
DELETE /api/photos/{file_id}/archive                 → 204 (unarchive)

POST   /api/photos/archive/batch                     → 200
       body: { "file_ids": ["uuid", ...], "action": "archive" | "unarchive" }
       returns: { "archived": N, "skipped": [...] }  # skipped = permission denials

GET    /api/photos/archived                          → paginated feed, mirror of /api/photos
GET    /api/photos?include_archived=true             → opt-in override for the default feed
```

- **Permission check**: the caller must have Read on the file (not
  Owner, not Edit — just read). Rationale: the archive signal says "I
  don't want to see this in my feed." Anyone who can see the photo
  can hide it from their own feed. Enforced via
  `AuthorizationEngine::require(caller, Read, Resource::File(id))`.
  No new permission variant.
- **Idempotency**: POST on already-archived returns 204 (upsert
  semantics — the row exists after either path, that's all the
  endpoint promises).
- **Audit log**: no per-row audit; archive volume is high and the row
  itself IS the audit (user_id + timestamp). Admin surfaces can query
  the table directly.

### F2 — folder-scope endpoints

```
GET    /api/photos/library/folders                   → list the caller's scope rules
PUT    /api/photos/library/folders                   → replace the whole set
       body: { "folders": [{ "folder_id": "uuid", "recursive": true }, ...] }

POST   /api/photos/library/folders/{folder_id}       → add one rule
       body: { "recursive": true }
DELETE /api/photos/library/folders/{folder_id}       → remove one rule
```

- **Permission check**: the caller must have Read on each folder in
  the request. Scope-of-nonexistent-folder returns 404 (anti-enum, per
  the AGENTS.md audit log pattern).
- **PUT vs POST/DELETE**: PUT is the single-shot "my library is these
  folders" write; POST/DELETE are the incremental tweak endpoints.
  Both exist because the UX will want both (one for a settings page,
  one for a context-menu "add this folder to my library").
- **Validation**: at most 50 entries per user. Service layer rejects
  with 422 and a stable `reason` field.

### DTOs

All request/response types in `src/application/dtos/photos_dto.rs`
with `#[utoipa::path]` on each handler so the generated OpenAPI picks
them up. Both features ship behind the `photos` tag.

## Permission model summary

| Action | Required perm | Resource |
|---|---|---|
| Archive a file | Read | the file |
| Unarchive a file | Read | the file |
| Set library scope on folder | Read | the folder |
| List own archives | — | self-scoped |
| List own scope | — | self-scoped |

Admin-side listings (if ever added) require admin role, routed through
the standard admin middleware.

## Edge cases

| Case | Behavior |
|---|---|
| File moved to another folder in-scope | Still visible. No row changes. |
| File moved out of scope | Hidden on next query. No row changes. |
| File deleted | `file_archives` row CASCADE's. |
| Folder in scope deleted | Rule row CASCADE's. |
| Folder in scope renamed | No change — rule is by `folder_id`. |
| Grant revoked on a file the user had archived | Archive row persists. Harmless: no listing can reach the file for that user anyway. A background janitor could prune these; not worth the complexity in Phase A. |
| File is in trash AND archived | Trash wins (trash is universal; archive is just a feed hint). Standard trash filters still apply. |
| File is in an album AND archived | Album wins IF the user is viewing the album's listing. The archive filter is scoped to the main feed and derivatives; album views are explicit opt-in to see every photo in the album. Mirrors Google Photos. |
| User sets scope to an empty list | Treated as "see everything" (fast path), same as no rules. The settings page should surface this explicitly ("No folders selected = all photos visible"). |

## Migration strategy

Three PRs, in order:

### PR A — F1 (archive)

1. SQL migration: `storage.file_archives` table + index.
2. Rust port + PG repo (`FileArchiveRepository`) with `set`, `unset`,
   `is_archived`, `list_archived_file_ids_for_user`, `exists_batch`.
3. Service layer (`PhotosArchiveService`) with the standard
   `_with_perms` suffix pattern and `AuthorizationEngine::require`
   call sites.
4. HTTP handlers in `src/interfaces/api/handlers/photos_handler.rs`
   (extend; don't create a new file) + utoipa annotations.
5. Repository-level predicate injection: `photos_pg_repository.rs`
   adds the `NOT EXISTS (...)` filter.
6. Hurl scenarios:
   - archive one → appears in `/archived`, disappears from `/`
   - unarchive → reverse
   - batch of 10 → outcome shape
   - permission denial on a file the caller can't read → 404
   - idempotent re-archive → 204
7. FE: new "Archive" button in the lightbox + the folder-grid
   contextual actions; new `/photos/archive` route showing the
   archived view.

### PR B — F2 (folder scope)

1. SQL migration: `storage.photos_folder_scope` table.
2. `PhotosFolderScopeRepository` port + PG impl.
3. `PhotosFolderScopeService` with the 50-entry cap, Read-on-folder
   permission checks.
4. HTTP handlers (settings-page endpoints + context-menu endpoints).
5. Query integration: extend `photos_pg_repository.rs` with the CTE
   above. The `photos_visibility_filter()` helper lives in that repo
   module.
6. Hurl scenarios:
   - no scope → all photos visible
   - add one recursive scope → photos outside its subtree hidden
   - add a non-recursive scope → only direct children visible
   - toggle recursive → listing shifts
   - 51st entry → 422 with stable `reason`
7. FE: new "Library folders" settings tab under the Photos route +
   per-folder context-menu toggle "Include in Photos library."

### PR C — pipeline sweeps

A sweep PR that extends the filter to:

- People / face tiles
- Places / geo pins
- Search image/video branch

Done in its own PR because it touches several services and the test
surface is big. Keeping it off the PR-A/PR-B critical path avoids a
mega-review.

### Non-destructive, reversible

Neither migration touches existing data. The default behavior with
empty tables for every user is identical to today. Rollback is a
`DROP TABLE` with no downstream impact.

## Testing

### Hurl

- `tests/api/photos_archive.hurl` (PR A) — the five scenarios above.
- `tests/api/photos_folder_scope.hurl` (PR B) — the six scenarios above.
- `tests/api/dto_shape_regression_guards.hurl` additions — the
  batch/action enum tightening gets its own 422 pin.

### Unit (`#[cfg(test)]` in-module)

- `photos_archive_service.rs` — permission check firing on
  non-visible file (anti-enum returns 404).
- `photos_folder_scope_service.rs` — 51st entry rejected; permission
  check on each folder; empty-list behavior.
- `photos_pg_repository.rs` — the CTE produces the right rows under:
  - empty scope / empty archive
  - archive only
  - scope only
  - both
  - non-recursive scope entry
  - folder deletion (CASCADE verified)

### Performance

- `tests/api/photos_archive.hurl` leaves an archive set of ~500
  entries; the listing query response time must stay under 100ms at
  p95. Enforced via Hurl's `[Options] duration` on the main list
  response.
- Add one Criterion bench (`benches/photos_visibility_filter.rs`) over
  a 50k-file fixture with scope + archive populated, so later changes
  that regress the ltree index path show up on PR CI.

## Performance budget (reference)

At the 10k-user target, with GIST index on `folders.lpath` already
shipped:

| Scenario | Est. page latency |
|---|---|
| Empty scope, empty archive | 2–5 ms |
| 10 recursive scope entries, 500 archive entries, 50k photos total | 10–25 ms |
| 10 recursive scope entries, 2k archive entries, 500k photos total | 30–60 ms |
| Pathological: 50 scope entries, 500k photos | 40–80 ms |

Fits comfortably in the hot-path budget. If later measurements demand
sub-10ms even at the pathological upper edge, a moka-cached
"resolved scope paths" per user drops the CTE cost. 20 MB envelope at
10k users; implement if measurements say so, not before.

## Open questions

Marked as resolved during the design discussion unless flagged.

- ~~Per-user vs per-drive scope?~~ → **Per-user**, decided.
- ~~Auto-archive by folder rule?~~ → **No**, replaced by F2.
- ~~Allowlist vs blocklist for scope?~~ → **Allowlist** in Phase A;
  blocklist as a sibling table later if demand surfaces.
- ~~Does archive apply to folders too?~~ → **No** in Phase A; mirror
  Google Photos.
- **Pending**: how do shared-drive viewers discover the "archive"
  gesture? UI question for the FE PR, not a backend decision.
- **Pending**: should archived photos count against user quota the
  same way as visible ones? **Yes** — archive is a view preference,
  not storage lifecycle. Trash is the lifecycle gesture; archive
  isn't.
- **Pending**: do pipeline sweeps re-run when a file is archived? E.g.
  face clustering reclusters without it? **No** — archive is a feed
  hint. Face DB keeps the detection; People surfaces just filter.
  Re-clustering on every archive would be wasteful and incorrect
  (users un-archive).

## Future — Albums (separate plan)

Albums land under their own PR with their own two tables:

```sql
storage.photo_albums(id, user_id, name, cover_file_id, …)
storage.photo_album_items(album_id, file_id, ordinal, added_at)
```

Album semantics compose with F1 and F2 as follows:

- An album view (`GET /api/photos/albums/{id}`) IGNORES F2 (scope) —
  the album membership IS the scope for that view.
- An album view RESPECTS F1 (archive) — Google Photos semantic: a
  photo you archived is hidden even in albums you're viewing.
- The default photos feed does NOT auto-include album photos that
  are outside F2 scope — a photo in an album but outside the library
  folders stays hidden from the main feed. Album is a lens; it
  doesn't rewrite the library definition.

Design these when the album PR lands. Noting here so the two features
don't get retrofitted with surprising interactions.
