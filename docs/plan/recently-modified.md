# Plan — Recently modified: what changed lately, and who changed it

**Status: DESIGN AGREED 2026-11-09, NOT STARTED.** Design settled in
discussion; nothing in this document is built yet.

OxiCloud has `/recent`, which answers *what did I open*
(`auth.user_recent_files`, recorded per viewer on access, private to the
viewer). It has no answer for *what changed, and who changed it* across the
drives a user can reach — the view OneDrive puts behind "People". This plan
adds it.

The two are not projections of one dataset and must not be confused: `/recent`
is a per-viewer access log, this is shared content provenance. See §6 for why
that argues for a separate nav entry rather than a tab.

---

## §0 — Decisions already taken

Recorded so implementation does not re-open them.

| Question | Decision |
| --- | --- |
| Is `content_modified_by` needed? | **Yes** — not derivable; see §1 |
| Backfill | `created_by` where `content_modified_at = created_at`, else `updated_by` |
| Which resources | **Files only.** `storage.folders` has `content_modified_at` too; out of scope |
| Access scope | Drive-accessible **and** folder/file grants — all three branches (§3) |
| "People" means | **Who changed it** (`content_modified_by`), not who owns it |
| Ops control | **Feature flag**, defaults on (§5) |
| Response shape | Flat `CursorListResponse<T>`, client draws the swimlanes (§4) |
| Per-person cap | **Dropped** — the time window bounds the data instead (§4) |
| Live updates | **No.** Polling on focus; a bus topic is overkill for this |
| Route name | `/changes`, NOT `/people` — that name is taken by photo face clusters |

---

## §1 — `storage.files.content_modified_by`

### Why it cannot be derived

The existing columns have deliberately asymmetric write discipline, each
stated in its own `COMMENT ON COLUMN`:

- `updated_by` — *"Who last touched the file (rename, move, overwrite,
  restore)"*
- `content_modified_at` — *"Bumped only by content-mutation paths (PUT,
  chunked complete, delta commit, upload overwrite), never by
  rename/move/trash/grant/favourite"*

So: Alice edits a file at T1, Bob renames it at T2. The row now reads
`updated_by = Bob`, `content_modified_at = T1`. A "modified by" view keyed on
`updated_by` files **Alice's edit under Bob's name, carrying Alice's
timestamp**. It gets worse with volume — a batch move of 200 files stamps 200
rows with the mover, because move is per-file (`file_blob_write_repository.rs`
`SET folder_id … updated_by = $3`). A folder move does *not* touch child file
rows, so the blast radius is per-file operations only.

The content clock needs its own principal, written by the same paths that
write the clock, or the two drift exactly as `updated_at` and
`content_modified_at` did before `normalize-file-dates`.

### Migration

```sql
ALTER TABLE storage.files
    ADD COLUMN IF NOT EXISTS content_modified_by UUID
        REFERENCES auth.users(id) ON DELETE SET NULL;
```

Nullable with `ON DELETE SET NULL`, matching `created_by` / `updated_by`
exactly (`20260802100000_drives_schema_additive.sql` §3): deleting a user
nulls provenance rather than cascading the file away. NULL therefore has two
meanings — "the principal was deleted" and "unknown, pre-backfill" — and the
UI renders both as *Unknown* rather than guessing.

**No `NOT NULL`.** Unlike `content_modified_at`, there is no truthful value
for every historical row, and a column that lies is worse than one that admits
it.

### Backfill

```sql
UPDATE storage.files
   SET content_modified_by = CASE
         WHEN content_modified_at = created_at THEN created_by
         ELSE updated_by
       END
 WHERE content_modified_by IS NULL;
```

The first arm is *truthful*: a file never overwritten since upload has its
uploader as content author, and after `normalize-file-dates` §1 backfilled
`content_modified_at := updated_at`, that equality holds for every untouched
row. The second arm is a **documented approximation** — it is wrong exactly
for rows last renamed or moved by someone other than the last content author.
Accepted deliberately (few deployments use collaborative editing yet, and this
is a starting point), and it self-limits two ways: the view's time window
excludes most historical rows, and any row that gets a real content change
from then on corrects itself.

**Suppress the statement triggers around the UPDATE**, exactly as
`20261103000006_files_content_modified_at.sql` does and for the same reason:

```sql
ALTER TABLE storage.files DISABLE TRIGGER files_bump_tree_etag_upd;
ALTER TABLE storage.files DISABLE TRIGGER files_enqueue_search_upd;
-- … the UPDATE …
ALTER TABLE storage.files ENABLE TRIGGER files_bump_tree_etag_upd;
ALTER TABLE storage.files ENABLE TRIGGER files_enqueue_search_upd;
```

Both are `AFTER UPDATE … REFERENCING OLD TABLE … NEW TABLE … FOR EACH
STATEMENT`. Neither watches `content_modified_by`, so neither would enqueue
anything — but Postgres materialises both transition tables for the whole
statement *before* deciding that, so one bulk UPDATE builds two full copies of
the table twice over, spilling to temp files, to conclude there is nothing to
do. DDL is transactional, so a failure restores the triggers with everything
else.

### Indexes

```sql
CREATE INDEX idx_files_drive_content_modified
    ON storage.files (drive_id, content_modified_at DESC)
    WHERE NOT is_trashed;

CREATE INDEX idx_files_content_modified_by
    ON storage.files (content_modified_by, content_modified_at DESC)
    WHERE NOT is_trashed;
```

The first serves the feed (few accessible drives, window on the clock); the
second serves the `?person=` drill-in. Both partial on `NOT is_trashed`,
because a trashed file never appears in this view. **Confirm with `EXPLAIN` on
a seeded database before committing to both** — the second may prove
redundant if the planner prefers a bitmap scan.

---

## §2 — Write path: two sites, and that is all

`swap_blob_hash` (`file_blob_write_repository.rs`) is the single content
mutation site. PUT, WOPI, chunked-upload finalize and delta commit all funnel
through it — its own doc comment says so. It already sets
`content_modified_at`, `updated_at` and `updated_by` in one CASE-guarded
UPDATE and already takes `caller_id` as `$5`, so this is one line inside the
existing compare-and-swap guard:

```sql
content_modified_by = CASE WHEN $6::text IS NULL OR old.blob_hash = $6
                           THEN $5 ELSE f.content_modified_by END
```

Inside the guard, not beside it: a failed CAS must write neither clock nor
principal.

The second site is the INSERT for a new file, where the honest value is
`created_by` — the uploader is the content author of a file that has only ever
been uploaded once.

**Invariant to state in the column comment and pin in a test:** every path
that writes `content_modified_at` writes `content_modified_by` in the same
statement. A future content path that sets one and not the other is the way
this feature rots.

---

## §3 — Access filter: three branches

Every row must be one the caller could already open. The predicate is a union,
because OxiCloud's access model has three doors into a file and `require()`
honours all three:

1. **Drive-accessible** — `storage.caller_accessible_drives($1)`
   (`20261103000005`), a STABLE SQL function resolving drive `role_grants`
   including group membership via `storage.caller_group_ids` and `expires_at`.
   The photos index already scopes exactly this way
   (`file_blob_read_repository.rs`).
2. **Direct file grant** — `role_grants` with `resource_type = 'file'` on the
   file, for the caller or one of their groups.
3. **Folder-cascade grant** — a grant on any ancestor folder. The point-check
   engine does this per resource with `gf.lpath @> (SELECT lpath …)`
   (`pg_acl_engine.rs`).

Branch 3 must **not** be written as a correlated subquery per candidate row.
Resolve the caller's granted folder subtrees once into an `ltree[]`, then
invert the test so it is a GiST index hit on `storage.folders.lpath`:

```sql
-- $paths :: ltree[]  — the caller's granted folder subtree roots
JOIN storage.folders fo ON fo.id = f.folder_id
WHERE $paths @> fo.lpath      -- ltree[] @> ltree : "some element is an ancestor of"
```

Rules that fall out, each of which is a test:

- **Expired grants are filtered on read.** The janitor is housekeeping, not
  enforcement (`feedback_trust_grant_janitor_no_expires_at_read`).
- **Trashed files are excluded** — `NOT is_trashed`, matched by the partial
  indexes.
- **Counts are computed inside the filtered query** (§4), so a lane header can
  never hint at a file the caller cannot see.

**Open question for the AuthZ pass:** should external users
(`is_external`) reach this view at all? It aggregates colleagues' activity,
which is a different proposition for a guest than for a staff member. The
scope-vs-grant rule puts that in the router, not the service — the
`require_internal_user_layer` already applied to the DAV nests is the shape to
copy if the answer is no.

---

## §4 — API

```
GET /api/files/recent-changes
    ?days=30            # window, default 30, max 90
    &order_by=modifier  # swimlane axis; default changed_at
    &person=<uuid>      # drill-in: one person, window still applies
    &limit=50&cursor=…  # standard keyset pagination
```

Not `/api/files/activities`: *activity* promises an audit stream — created,
renamed, moved, deleted — and this is content changes only. Not a sub-resource
of `/api/recent`, which is a different table with a different meaning.

**Response: `CursorListResponse<FileChangeDto>`** — the flat
`{ items, next_cursor }` envelope used everywhere else. The client draws a
swimlane header when `content_modified_by` changes, which is what `/favorites`
(`bucketOf: item.created_by`) and `/shared-with-me` (`bucketOf: ctx.ownerId`)
already do with people lanes.

Why flat rather than people-as-items: grouping is a *view option*, not the
data's shape. A chronological "what changed in the last hour" is then the same
endpoint with `order_by=changed_at`, no second shape, and the whole response
renders in `ResourceList` with selection, context menu, batch ops and
virtualisation inherited rather than rebuilt.

`FileChangeDto` mirrors `RecentResourceRow`'s field discipline — including
`is_favorite`, `is_shared`, `drive_id`, `blob_hash` for a matching `etag`, and
`path` cleared when the caller is not the owner — plus:

- `content_modified_at`, `content_modified_by`
- `modifier`: the **public tier** `UserDto`, never the self tier
  (`project_userdto_three_layer_split`) — this view names third parties to
  anyone sharing a drive with them
- `lane_total`: `COUNT(*) OVER (PARTITION BY content_modified_by)` from inside
  the access-filtered query, so a header can read "Bob · 14 changes" without a
  second round trip and without leaking

### Order-by enum

A per-endpoint enum, typed rather than `Option<String>`, so an unknown axis is
a 400 naming the accepted set rather than a silent fallback — same discipline
as `RecentOrderBy` / `TrashOrderBy`:

```rust
pub enum RecentChangesOrderBy {
    #[default] ChangedAt,   // chronological, newest first
    Modifier,               // the people swimlane
    Name, Size, Type,
}
```

Register it in `src/application/dtos/mod.rs` or the OpenAPI components list —
an unregistered enum silently drops from the spec
(`project_openapi_enum_gotchas`) — and regenerate
`resources/gen/openapi.json`.

### Cursor

`ChangedAt` is a plain `(changed_at, file_id)` keyset. **`Modifier` needs a
four-tuple**, because lanes are ordered by their own recency:

```
ORDER BY lane_latest DESC NULLS LAST, content_modified_by, content_modified_at DESC, id
```

where `lane_latest = MAX(content_modified_at) OVER (PARTITION BY
content_modified_by)`. So the cursor carries `lane_ts`, `lane_id`
(`Option<Uuid>` — the Unknown lane has none), `item_ts` and `resource_id`. The
cursor stores its own axis, as `RecentCursor` does, so the keyset predicate and
the `ORDER BY` cannot drift.

Ordering lanes alphabetically by display name would avoid the window function
(a plain join on `auth.users`), but "most recently active first" is the right
default for a recency view. The window function runs over the
**window-bounded** set, which is why `days` has a maximum: the sort cost is
proportional to the window, not the page. If a deployment ever needs a
year-long window, that is the point at which the people-paged response shape
becomes the better trade — revisit then, not now.

---

## §5 — Feature flag

Five wiring points, following `enable_photos` / `enable_music`:

1. `FeaturesConfig::enable_recent_changes` + `OXICLOUD_ENABLE_RECENT_CHANGES`
   (`src/common/config.rs`), default **true**.
2. The service built as `Option<Arc<…>>` in `common/di.rs`, logging the
   disable at startup like `create_audio_metadata_service`.
3. The route **not mounted** when off (`routes.rs`, the `enable_photos`
   shape), so a non-SPA client gets a 404 rather than a surface the SPA merely
   hides.
4. A `FeaturesDto` field (`config_handler.rs`) so `/api/config` advertises it.
5. The SPA nav gated on `serverConfig.features.recent_changes` — sidebar
   **and**, if it ever gets a footer entry, the footer. Those two have already
   drifted once (`035cddfa`).

The flag earns its place beyond symmetry: this view aggregates *who changed
what* across a shared drive. Every row is already accessible to the viewer, so
it is not a privacy leak — but aggregated activity is a soft surveillance
surface, and whether a given organisation wants it is an operator's decision,
not ours.

---

## §6 — Frontend

A **new Library entry**, not a tab on `/recent`:

```ts
library.push({ href: '/changes', label: t('nav.recent_changes', 'Recent changes'),
               icon: 'pen', section: 'changes' });
```

gated on the flag, placed immediately after Recent. `pen` and `users` are the
only fitting icons in the registry, and `users` collides with the People/faces
sense.

Why not a tab: `/recent` is the viewer's private access log and this is shared
provenance — tabs would imply two filters over one dataset, the way
`/photos` → People/Places genuinely are three projections of one corpus. The
view also carries its own URL state (`days`, `person`, cursor, grouping), and
`/recent` has no tab bar today, so a tab means building one to host a second
thing when the sidebar already *is* the navigation. And when ops disables the
feature a nav entry simply vanishes, where a tab bar collapses to a single
orphan tab.

The page is `ResourceList` with `groupBys`:

- `{ key: 'modifier', orderBy: 'modifier', bucketOf: (_i, ctx) => ctx?.extras?.modifier_id, labelOf: … }`
- `{ key: 'changed_at', orderBy: 'changed_at', bucketOf: (_i, ctx) => dateBucket(ctx?.date) }`

typed as `GroupByDef<RecentChangesOrderBy>[]` so a retired axis fails
`npm run check` instead of becoming a runtime 400 — `/recent` already does
this. The modifier rides in `ItemContext` (`ownerId` override for the vignette,
`extras` for the lane key), so `ResourceList` needs no new props.

One detail the page owns: a lane can straddle a page boundary, so appending a
page must merge into the open lane rather than draw a second header for the
same person.

i18n: `nav.recent_changes`, the page title, the lane header for the Unknown
bucket, and the window selector labels — English in `en.json`, then all 15
locales (insert **before** an anchor key, per `project_i18n`).

---

## §7 — Pins

Backend, `tests/api/recent_changes.hurl`:

- a file in a drive the caller cannot read is **absent**
- a file reachable only through a folder-cascade grant is **present** (branch 3
  is the one most likely to be dropped as "an optimisation")
- a grant with `expires_at` in the past does **not** admit its file
- a trashed file is absent
- `lane_total` equals the number of visible rows for that lane, with a second
  user's invisible file present in the fixture
- `order_by=nonsense` → 400 naming the accepted set
- flag off → 404 on the prefix

Rust, inline where the code lives:

- `swap_blob_hash` writes `content_modified_by = caller_id`, and a **failed
  CAS writes neither clock nor principal**
- a new file's `content_modified_by = created_by`
- the backfill's two arms, against a seeded row of each shape

Frontend:

- `endpoints/recentChanges.test.ts` — the query builder and the 400 path
- the page test: group-by wiring, and the lane-merge on append
- `AppShell.test.ts` — nav entry hidden when the flag is off, mirroring
  `hides /music when features.music is false`

---

## §8 — Deferred, with reasons

- **Folders.** They have `content_modified_at`; the view is files-only because
  a folder's "content change" is a different event (a child moved) and would
  need its own semantics.
- **Live updates.** The bus would need a topic scoped to "drives I can read",
  which is a new subscription shape. Polling on focus until someone misses it.
- **Per-person cap.** Dropped in favour of the window; revisit if a lane with
  thousands of rows proves awkward in the UI rather than in the query.
- **`content_modified_by` on folders**, and a non-spoofable ordering. Note
  `content_modified_at` is client-assertable via `X-OC-Mtime` within trust
  bounds, so feed position is partly client-controlled. That is correct POSIX
  behaviour; `updated_at` is the clock a client cannot influence, but it
  includes renames. If spoofing ever matters, clamp the display when the two
  disagree wildly rather than changing the sort.

---

## §9 — Rollout order

1. §1 migration + §2 write path, with their pins. The column is dense from the
   first deploy, before anything reads it.
2. §3 + §4 behind the flag, with the Hurl pins.
3. §5 flag wiring end to end, including `/api/config`.
4. §6 page and nav entry, with i18n.

Steps 1–2 are independently useful: a dense `content_modified_by` improves the
existing `/recent` owner column's honesty whether or not this view ships.
