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
| Is `content_modified_by` needed? | **Yes** — not derivable; see §1. Kept for per-row display and as the seed source, even though the feed no longer reads it |
| Does the column suffice for the feed? | **No** — last-writer-wins hides a coworker's edit the moment anyone else saves. Hence `storage.file_content_touches`, §1b |
| Backfill | `created_by` where `content_modified_at = created_at`, else `updated_by` |
| Which resources | **Files only.** `storage.folders` has `content_modified_at` too; out of scope |
| Access scope | Drive-accessible **and** folder/file grants — all three branches (§3) |
| "People" means | **Who changed it** (`content_modified_by`), not who owns it |
| Ops control | **Feature flag**, defaults on (§5) |
| Response shape | Flat `CursorListResponse<T>`, client draws the swimlanes (§4) |
| Per-person cap | **Dropped** — the time window bounds the data instead (§4) |
| Live updates | **No.** Polling on focus; a bus topic is overkill for this |
| Route name | `/changes`, NOT `/people` — that name is taken by photo face clusters |
| External users | **Denied** — router-level, `require_internal_user_layer` (§3) |
| Window | `days` default 30, **max 90** — the bound the whole design leans on |
| Other resources | **None for now.** Calendars/contacts have no provenance columns at all (§8) |

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

Both partial on `NOT is_trashed`, because a trashed file never appears here.

The first serves the feed: with `drive_id` leading, Postgres walks the index
once per accessible drive, already in clock order, and stops at the window
boundary — no sort. The second serves `?person=` and step 2 of the lane query
(§4), where `drive_id` is unconstrained and the feed index is therefore
useless.

**Ship the first; measure before adding the second.** Its value depends on data
we do not have — if most changes in a window come from a few people, a bitmap
scan over the feed index plus a filter may beat it, and a second index on this
table costs every INSERT and every `swap_blob_hash`. The decision is an
`EXPLAIN (ANALYZE, BUFFERS)` on a seeded database: an `Index Scan using
idx_files_content_modified_by` with `rows` ≈ actual means keep it; a bitmap heap
scan with a high "rows removed by filter" means it is not earning its write
cost.

---

## §1b — `storage.file_content_touches`: one row per (file, person)

### Why the column alone cannot answer the question

`content_modified_by` is last-writer-wins, and that breaks the view's actual
use case. Ed, 2026-11-10:

> I am looking for a file I know a coworker worked on yesterday but I don't
> remember the name nor its location. […] unfortunately someone else has just
> modified the file, I may have difficulties to find it.

Bob edits on Monday, Carol edits on Tuesday: the file now exists **only** in
Carol's lane, and Bob's work is unfindable. That is not an edge case — any file
worth hunting for is a file several people touch. Microsoft keeps this working
because it stores *changes*, not a current modifier.

### The table

```sql
CREATE TABLE storage.file_content_touches (
    file_id    UUID NOT NULL REFERENCES storage.files(id) ON DELETE CASCADE,
    user_id    UUID NOT NULL REFERENCES auth.users(id)    ON DELETE CASCADE,
    touched_at TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (file_id, user_id)
);

CREATE INDEX idx_touches_user_touched  ON storage.file_content_touches (user_id, touched_at DESC);
CREATE INDEX idx_touches_touched       ON storage.file_content_touches (touched_at DESC);
```

**Not an event log.** One row per pair, holding that person's *latest* change
to that file. Three consequences, all of them the reason to prefer this over
an append-only log:

- **Bounded by (files × distinct editors), not by edit count.** A WOPI or
  collab autosave storm writes the same row repeatedly instead of growing the
  table, so there is no coalescing window to tune and no retention cliff. At
  1M files and ~1.2 editors per file that is ~1.2M rows, stable under load —
  where an event log at 10k users × 20 changes/day would be ~6M rows per
  30 days and climbing.
- **The lane count becomes truthful for free.** `COUNT(*)` per `user_id` *is*
  the number of files that person changed — no `DISTINCT`, no double-counting
  a file someone saved forty times.
- **`user_id NOT NULL` with `ON DELETE CASCADE` removes the nullable-lane
  keyset trap** described in §4. A deleted user's lane disappears entirely,
  which is also the right answer: their account is gone, their lane should go
  with it. `content_modified_by` keeps its nullable "Unknown" semantics for
  per-row display, where NULL is informative rather than a sort key.

### Write

One upsert, in the same transaction as the content mutation (§2):

```sql
INSERT INTO storage.file_content_touches (file_id, user_id, touched_at)
VALUES ($file, $caller, $content_modified_at)
ON CONFLICT (file_id, user_id) DO UPDATE SET touched_at = EXCLUDED.touched_at
```

`touched_at` takes the same value as `content_modified_at` — including a
client-asserted `X-OC-Mtime` — so the feed and the file's own mtime never
disagree about when a change happened.

### Seeding

So the view is not empty on the first deploy, seed one row per file from the
`content_modified_at` / `content_modified_by` pair, restricted to the
retention window and to rows where the principal is known:

```sql
INSERT INTO storage.file_content_touches (file_id, user_id, touched_at)
SELECT id, content_modified_by, content_modified_at
  FROM storage.files
 WHERE content_modified_by IS NOT NULL
   AND NOT is_trashed
   AND content_modified_at > now() - interval '120 days'
ON CONFLICT DO NOTHING;
```

This inherits §1's backfill approximation — one seeded touch per file, crediting
the last known principal. It is the floor, not the truth: the table becomes
accurate as people edit. Note what the seed does *not* claim: it never invents
a second editor, so a pre-deploy file shows one lane until someone touches it
again.

### Retention

**A periodic sweep is mandatory, not optional** — without it the table grows
with every file anyone has ever touched, which is the one way this design
degrades over years rather than under load.

`file_touches_cleanup`, a daily job mirroring
`notifications_cleanup_service` exactly: a `JobHandler` with a single
`JobParam::number("retention_days", 120, …)` so an operator can override at
trigger time without a redeploy, an `OXICLOUD_FILE_TOUCHES_RETENTION_DAYS`
default read at boot, and the same self-registering
`.register(&app_state.core.job_registry)` call from `common/di.rs` that
`notifications_cleanup` and `ops_banner_expiry` use. It deletes rows with
`touched_at < now() - retention`.

Two properties to keep, both learned from the existing jobs:

- **Clamp the parameter to a minimum of 1 day.** `retention_days = 0` would
  purge every row on every tick — `NotificationsCleanupService`'s constructor
  clamps for exactly this reason.
- **A sweep that skips work must never report success.** If the delete is
  batched and a batch fails, pause at a cursor and report it
  (`docs/plan/jobs-handling-recoverable-error.md`). A silent partial sweep here
  is benign — stale rows, not lost data — but the convention is what keeps the
  loud-failure habit intact across every job.

Deleting a touch row is never data loss: the file, its `content_modified_at`
and its `content_modified_by` are untouched. The table is a cache of recency,
and the retention window is its ceiling.

**120 days, deliberately above the 90-day `days` maximum.** The window bounds
what the view can ask for; retention bounds what the table keeps. Setting them
equal would let a boundary request see a half-pruned window.

---

## §2 — Write path: two sites, three writes each

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
that writes `content_modified_at` writes `content_modified_by` **and** upserts
the §1b touch, in the same transaction. A future content path that does one and
not the others is the way this feature rots — and the touch is the one that
rots silently, because the file still looks right in every listing while
quietly never appearing in anyone's lane.

The upsert belongs in the same transaction as `swap_blob_hash`, not in a
follow-up call: a content change that commits without its touch is a change
the view can never recover, since there is no second source to reconcile
against.

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

### Write it as a UNION, not an OR

`branch1 OR branch2 OR branch3` across three different tables is the classic
planner killer: it typically collapses to a sequential scan on
`storage.files`. Postgres can sometimes build a `BitmapOr` of index scans, but
not with a join inside branch 3.

So: **three separately-indexable selects over file ids, unioned and deduped,
then joined to the payload.** Each branch gets its own index, and the dedupe is
on `id` rather than on the wide row.

Branch 3 must also not be a correlated subquery per candidate row. Resolve the
caller's granted folder subtree roots once into an `ltree[]`, then test with
the **indexed column as the left operand** so the GiST index on
`storage.folders.lpath` is usable:

```sql
-- $paths :: ltree[]  — the caller's granted folder subtree roots
JOIN storage.folders fo ON fo.id = f.folder_id
WHERE fo.lpath <@ ANY($paths::ltree[])
```

`ANY` expands to a ScalarArrayOp, which is fine here because the array holds
one entry per granted subtree — a handful, not a corpus. **Confirm with
`EXPLAIN` that the GiST index is chosen**; the mirror-image form with the
array on the left depends on operator commutators and may not be indexable.

`storage.caller_accessible_drives` is `LANGUAGE sql STABLE`, so it should
inline into a subquery the planner can hash-semi-join. Verify that too: if it
ever materialises instead, the planner falls back to a flat 1000-row estimate
and every join above it is costed on fiction. The photos index already depends
on this behaviour.

Rules that fall out, each of which is a test:

- **Expired grants are filtered on read.** The janitor is housekeeping, not
  enforcement (`feedback_trust_grant_janitor_no_expires_at_read`).
- **Trashed files are excluded** — `NOT is_trashed`, matched by the partial
  indexes.
- **Counts are computed inside the filtered query** (§4), so a lane header can
  never hint at a file the caller cannot see.

### External users: denied, in the router

Decided. An external is a guest invited to specific resources; a feed of
colleagues' activity has no meaning for them and aggregates more than the
invitation implied. They are already 403 on the system contacts listing, on
WebDAV/CalDAV/CardDAV and on REST address-book creation
(`tests/api/external_users.hurl` 11b, 11h–11k).

This is a **scope** question, not a grant — "may this kind of caller reach
this surface at all", with no resource to name — so it belongs in the router
as `require_internal_user_layer` on the route, never in the service. Pin it
both ways in `external_users.hurl`: the external 403, **and** an internal
caller 200 on the same path. A gate that refuses everyone satisfies a refusal
pin on its own.

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

### Two statements, not one — the performance decision

The obvious single query computes the lane ordering with window functions:

```sql
SELECT …, MAX(content_modified_at) OVER (PARTITION BY content_modified_by) AS lane_latest,
          COUNT(*)                 OVER (PARTITION BY content_modified_by) AS lane_total
  FROM visible
 ORDER BY lane_latest DESC, content_modified_by, content_modified_at DESC, id
 LIMIT $limit + 1
```

**Do not ship that.** Neither window function can be computed incrementally
under a `LIMIT`: Postgres must read every qualifying row in the window, sort by
the partition key, compute the aggregates, then sort again by the display order
before the limit applies. So **page 7 costs exactly what page 1 costs**, and
the cost scales with changes-in-window across all accessible drives rather than
with page size. At 1k active users × 5 files/day × 30 days that is ~150k wide
rows sorted per request; at the 10k target it is worse, and paging multiplies
it.

Two statements instead, both over `storage.file_content_touches` (§1b) rather
than over `storage.files`:

1. **Lanes, on a narrow projection:**
   ```sql
   SELECT t.user_id,
          MAX(t.touched_at) AS lane_latest,
          COUNT(*)          AS lane_total   -- = number of files, by construction
     FROM storage.file_content_touches t
    WHERE t.touched_at > now() - ($days || ' days')::interval
      AND t.file_id IN (SELECT id FROM visible_files)   -- §3
    GROUP BY 1
    ORDER BY lane_latest DESC, t.user_id
    LIMIT $lane_batch
   ```
   An aggregate over the window, but on three columns, returning a handful of
   rows. `COUNT(*)` is exact without `DISTINCT` — one row per (file, person) is
   the whole point of §1b.

2. **Rows for the lanes this page needs.** Walk the step-1 result accumulating
   `lane_total` until it covers `$limit`, then fetch
   `WHERE t.user_id = ANY($lanes) AND (keyset predicate)` — an index scan per
   person down `idx_touches_user_touched`, joined to `storage.files` by primary
   key for the payload.

Exact rather than approximate, because step 1 already computed the counts the
lane headers wanted anyway. This is approach B's *query* shape with approach
A's *response* shape: flat envelope, same cursor, no full-window sort.

`order_by=changed_at` needs none of this — a plain keyset walk down
`idx_touches_touched` joined to the visible-file set, which is the cheap path
and the default.

**Each row's `changed_at` is the TOUCH, not the file's mtime.** In Bob's lane
the row says when *Bob* changed it, even if Carol has changed it since; the
file's own `content_modified_at` rides along for a secondary label. Getting
this backwards would make every lane read as "Carol, Carol, Carol".

**A file may therefore appear more than once in one page** — once per lane that
touched it, and in chronological mode once per person's latest touch. That is
the feature, not a bug: "who worked on this lately" has several right answers.
The dedupe is per lane, and §1b's primary key enforces it.

Which also disposes of the **drive_id question**: the touch table deliberately
carries no `drive_id`, so a cross-drive move (D6) cannot leave it stale. The
cost is that the access filter reaches files by primary-key join rather than by
a leading drive index. If profiling ever says otherwise, denormalising
`drive_id` into the touch row is the trade — along with the invalidation that
comes with it.

### Cursor

`ChangedAt` is a plain `(changed_at, file_id)` keyset. **`Modifier` needs a
four-tuple**, because lanes are ordered by their own recency:

```
ORDER BY lane_latest DESC NULLS LAST, content_modified_by, content_modified_at DESC, id
```

where `lane_latest` comes from step 1 above. So the cursor carries `lane_ts`,
`lane_id` (`Option<Uuid>` — the Unknown lane has none), `item_ts` and
`resource_id`. The cursor stores its own axis, as `RecentCursor` does, so the
keyset predicate and the `ORDER BY` cannot drift.

**The nullable-lane keyset trap is designed out, and that was deliberate.**
A keyset predicate over a nullable sort column silently misbehaves —
`lane_id > $cursor_lane_id` is NULL, never true, so the lane either repeats
forever or vanishes at a page boundary, and `NULLS LAST` in the `ORDER BY`
does not fix the predicate. §1b avoids it by making the lane key
`user_id NOT NULL` with `ON DELETE CASCADE`, so there is no Unknown lane in
the feed at all. `content_modified_by` keeps its nullable "Unknown" for
per-row display, where it is a label and never a sort key.

Worth remembering if a future axis sorts on something nullable: the fix is a
sentinel via `COALESCE` on both sides, or an explicit `(key IS NULL)` branch —
and a fixture that straddles a page boundary, because every single-page test
passes regardless.

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
- **two people edited the same file, and it appears in BOTH lanes** — the
  regression this plan was amended for. Bob touches it on Monday, Carol on
  Tuesday; Bob's lane still carries it, dated Monday. A `content_modified_by`
  implementation passes every other pin in this list and fails this one
- the same person saving the same file repeatedly produces **one** row in their
  lane, carrying the latest timestamp (the §1b primary key doing its job)
- `lane_total` equals the number of visible rows for that lane, with a second
  user's invisible file present in the fixture
- `order_by=nonsense` → 400 naming the accepted set
- flag off → 404 on the prefix
- an external user → 403, **and** an internal caller → 200 on the same path
  (in `external_users.hurl`, beside 11b/11h–11k)
- a NULL-lane row straddling a page boundary appears exactly once — the keyset
  trap in §4, which every single-page test passes

Rust, inline where the code lives:

- `swap_blob_hash` writes `content_modified_by = caller_id` **and** upserts the
  touch, and a **failed CAS writes none of the three**
- a new file's `content_modified_by = created_by`, with its touch row
- the backfill's two arms, against a seeded row of each shape
- the retention job deletes beyond its window and **nothing inside it**, with
  `retention_days` honoured from the trigger parameter

Frontend:

- `endpoints/recentChanges.test.ts` — the query builder and the 400 path
- the page test: group-by wiring, and the lane-merge on append
- `AppShell.test.ts` — nav entry hidden when the flag is off, mirroring
  `hides /music when features.music is false`

---

## §8 — Deferred, with reasons

- **Every other resource kind.** Confirmed out of scope, and the reason is
  structural: the mechanism needs a content clock, a content principal and an
  access model to filter by. Files have all three bar the principal.
  `caldav.calendar_events` and `carddav.contacts` — the obvious analogue, since
  shared calendars exist — carry `created_at`, `updated_at`, `etag` and **no
  provenance columns at all**, so the same view there means adding
  `created_by` / `updated_by` *and* a content clock *and* wiring them through
  every iCal/vCard write path first. That is its own plan. Playlists are
  shareable but a playlist edit is low-value news, and a collab-document edit
  already lands as a file content change.

  This is why the endpoint is namespaced `/api/files/…`: a later
  `/api/calendars/recent-changes` is purely additive, where one polymorphic
  `/api/recent-changes` would force a `kind` discriminator from day one — the
  "activity stream" shape rejected above for promising audit semantics it
  cannot deliver.

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

1. §1 migration + §1b table and seed + §2 write path, with their pins. Both
   the column and the touch table are dense from the first deploy, before
   anything reads them — and the touch table only accumulates truth while it
   is unread, so there is no reason to wait.
2. §1b retention job, registered and scheduled. Before the read path, so a
   long-lived deployment never has an unbounded table even if §3–§6 stall.
3. §3 + §4 behind the flag, with the Hurl pins.
4. §5 flag wiring end to end, including `/api/config`.
5. §6 page and nav entry, with i18n.

Steps 1–2 are independently useful: a dense `content_modified_by` improves the
existing `/recent` owner column's honesty, and the touch table starts
accumulating the history this view needs, whether or not the view itself
ships. Shipping them early is also what makes the view useful on its first day
rather than thirty days later.
