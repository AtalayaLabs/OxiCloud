# Plan — Normalize file dates: one column per meaning, one wire slot each

**Status:** design agreed 2026-10-11, **not started**. The end state is specified
in [`docs/architecture/dates.md`](../architecture/dates.md) — this plan is only
how to get there. Triggered by a review of the four date surfaces (OxiCloud
columns, the browser File API, NextCloud DAV, RFC 4918) which found that
`storage.files.updated_at` is doing the work of three different timestamps, and
that two NextCloud properties already on the wire read the wrong columns.

Progress, kept current as work lands — each phase carries its own marker at its
heading:

| phase | state |
|---|---|
| §0 fix the two `nc:` slots | **NOT STARTED** — 2 lines, ships alone, no behaviour change |
| §1 add `content_modified_at`, write it, read nothing | **IN PROGRESS** — migration + writers + trust bounds done (`625f089d`); integration tests remain |
| §2 repoint the `modified_at` alias + ETag | **NOT STARTED** — one commit; no resync, see §2 |
| §3 sort axes | **NOT STARTED** — couples to §2 |
| §4 add `source_created_at` | **BLOCKED** — on choosing a writer, see §4. Not a pre-start blocker; §4 is last and independent |
| §5 tighten to `NOT NULL` | **FOLDED INTO §1** 2026-10-17 — the migration lands it in-transaction |
| §6 UI axes | **NOT STARTED** — one relabel; no new sort options |
| §7 naming normalisation | **NOT STARTED** — `order_by` everywhere, typed per-endpoint enums, `deletion_date` → `expires_at`; ships independently. Compile-time protection waits on the generated-SDK redirects |

> **Reassessed 2026-10-17** against the tree, after the `pivot/dto-timestamps-iso`
> work landed. Three corrections:
>
> - The REST wire carries **RFC 3339 strings**, deliberately — see
>   `docs/architecture/dates.md § Representations`. So the response is not a
>   lossy hop; `X-OC-Mtime` and the entity `u64` are. And **sub-second precision
>   must not be pinned**: a seconds pin was reverted because it broke
>   `/api/notifications` cursor pagination.
> - §7's compile-time protection is a **precondition, not a bonus** — the two
>   sort types the frontend imports are still hand-written, so the
>   OpenAPI→SDK→`npm run check` chain does not close yet.
> - `File` and `Folder` keep `u64` **by design** (confirmed 2026-10-17):
>   `DateTime<Utc>` lives only at the serde boundary, and an integer-typed entity
>   makes "ETag input is integer seconds" a type-level guarantee. §2 therefore
>   changes only the column inside each `EXTRACT(EPOCH FROM …)`, with no type
>   churn riding along.
>
> The three-pattern survey is unchanged — 10 aliases, 32 `EXTRACT(EPOCH FROM …)`
> selects, 1 cursor predicate — so §2's sizing still holds.

---

## Where the work actually is — `modified_at` is a name, not a column

Nothing above the repository layer knows `updated_at` exists. The domain entity,
the DTO, the ETag formula and both DAV surfaces all speak `modified_at`, which
the SQL produces from `updated_at` — sometimes by a named alias, more often
positionally. Five files carry every one of those reads:

```
f.updated_at   AS modified_at     ← files
fld.updated_at AS modified_at     ← folders
fm.updated_at  AS modified_at     ← file_metadata join
```

- `infrastructure/repositories/pg/folder_db_repository.rs`
- `infrastructure/repositories/pg/favorites_pg_repository.rs`
- `infrastructure/repositories/pg/recent_items_pg_repository.rs`
- `infrastructure/repositories/pg/trash_db_repository.rs`
- `infrastructure/services/path_resolver_service.rs`

Everything downstream — `File.modified_at`, `FileDto.modified_at`,
`File::compute_etag`, `D:getlastmodified`, `nc:upload_time`, and **WOPI's
`LastModifiedTime` and `Version`** — reads `modified_at`, never `updated_at`.
So no handler, service or DTO changes: the repointing happens entirely in the
repository SQL.

That is convenient and also the main hazard. Several protocol fields change
meaning **without anyone editing the code that emits them**, so every one has to
be enumerated deliberately rather than discovered afterwards. WOPI is the one
that is easiest to miss — see §2.

**But the named alias is only one of three read patterns, and it is the smallest.**
A survey of the tree:

| pattern | file-side sites | note |
|---|---|---|
| `f.updated_at AS modified_at` | 5 | named alias; the `ORDER BY` follows it for free |
| `EXTRACT(EPOCH FROM fi.updated_at)::bigint` | 21 (`fi.` 17, `f.` 4) | **positional** — feeds `modified_at` by column order, no alias to change |
| keyset cursor predicate | 1 | `folder_db_repository.rs:1880` — see the hazard below |

So §2 is a **~27-site sweep of the file-side reads**, not a ten-line alias
change — and the folder-side reads (`fo.` 10, `fld.` 3) are in scope too, since
folder `D:getlastmodified` now repoints to `tree_modified_at` (decided
2026-10-11). Folders get no new column; the work is pointing the property at the
subtree clock they already have.

### The cursor hazard

Under `order_by=modified_at` the sort and the keyset cursor read *different*
things today:

```rust
// folder_db_repository.rs:1874  — sorts on the ALIAS
("<", "ORDER BY modified_at DESC, id DESC")
// folder_db_repository.rs:1880  — pages on the RAW COLUMN
Pred(format!("({col}.updated_at, {col}.id) {op} ($4, $5::uuid)"))
```

Repointing the alias without the predicate would leave the query **ordering by
`content_modified_at` while paginating against `updated_at`** — which silently
skips and duplicates rows across page boundaries. Page one looks perfect. This
must move in the same commit as the alias, and it deserves a dedicated
pagination test: list a folder whose files have divergent `updated_at` /
`content_modified_at`, page through it, and assert the full set is returned
exactly once.

The other consequence of all reads going through `modified_at`, and the reason
§2 and §3 are coupled: REST, both DAV surfaces and the ETag formula all move
together. There is no way to repoint DAV only.

---

## §0 — Fix the two `nc:` slots **NOT STARTED**

Independent of everything else. Two lines at
`src/interfaces/nextcloud/webdav_handler.rs:2268` and `:2273`:

| property | today | after |
|---|---|---|
| `nc:upload_time` | `file.modified_at` | `file.created_at` |
| `nc:creation_time` | `file.created_at` | `COALESCE(captured_at, created_at)` |

Purely presentational — no client acts on either property, so there is no
resync. Strictly an improvement on today: EXIF capture dates start reaching
NextCloud clients, which they currently never do.

The fallback is `created_at` only until §3, which swaps it for
`content_modified_at`; §4 then inserts `source_created_at` between the two.

**Change the source, never the format.** `nc:creation_time` and
`nc:upload_time` are **integer Unix epoch seconds**, emitted via
`common::fmt::u64_str` on `.timestamp().max(0) as u64` — not RFC 3339, unlike
the REST wire. NextCloud desktop and mobile parse them with `(int)$value`, so an
ISO string silently coerces to `0` and the client treats the file as never
uploaded. This phase repoints which column feeds them and nothing else.

**Verification:** a PROPFIND assertion in the NC Hurl suite that the two
properties carry different values for a file whose `created_at` and
`updated_at` differ — and that both are still bare integers. Existing format
guards live in `nc_webdav_patch.hurl` and `webdav_patch_consistency.hurl`.

---

## §1 — Add `content_modified_at`, write it, read nothing **NOT STARTED**

### Schema

Shipped as `migrations/20261103000006_files_content_modified_at.sql`. Four
statements, and the order is load-bearing:

```sql
ALTER TABLE storage.files ADD COLUMN IF NOT EXISTS content_modified_at TIMESTAMPTZ;
ALTER TABLE storage.files ALTER COLUMN content_modified_at SET DEFAULT NOW();

UPDATE storage.files SET content_modified_at = updated_at
 WHERE content_modified_at IS NULL;

ALTER TABLE storage.files ALTER COLUMN content_modified_at SET NOT NULL;
```

**`ADD COLUMN` and `SET DEFAULT` must be separate statements.** Since PG 11 an
added column *with* a default makes EXISTING rows read as that default without a
rewrite — so `ADD COLUMN … DEFAULT NOW()` would have every pre-existing file
report "modified at upgrade time", changing every ETag at once: a fleet-wide
re-download. Applying `SET DEFAULT` afterwards is metadata for subsequent
inserts only. That also means **none of the ten-odd `INSERT INTO storage.files`
sites needed touching** — new rows self-populate, and `NOW()` is the correct
content-mtime for a newly created file rather than a stand-in for a forgotten
value.

**Backfill from `updated_at`, not `created_at`** (decided 2026-10-12). This is
what makes the rollout inert: `updated_at` is exactly what
`D:getlastmodified`, `FileDto.modified_at` and the ETag suffix resolve to today,
so afterwards every one of them is byte-identical. `created_at` would report an
*older* mtime than today for any file touched since upload, and a sync client
seeing the server copy older than its local one may answer with an upload or a
conflict rather than a download — worse than a re-download.
`WHERE content_modified_at IS NULL` keeps it idempotent and prevents clobbering
a value `swap_blob_hash` has since written.

**`NOT NULL` lands here, not in a later phase** (decided 2026-10-17, revising
the original plan). The backfill was going to be a cursor-paged job to avoid
delaying boot — but that trade is wrong for self-hosted software: a job's drain
state cannot be confirmed across deployments we do not operate, so the
`COALESCE(content_modified_at, updated_at)` fallback would have lived in the
ETag and read paths **indefinitely** and the tightening phase would never have
been safely reachable.

The `UPDATE` costs one full-table rewrite, once, on upgrade. This database is
metadata only — content lives in blobs — so the row count is modest even at the
10k-user target. Seconds of boot, once, buys a column that is dense the moment
the migration completes. sqlx runs a migration in one transaction, so the
backfill and the constraint apply together or not at all; there is no window
where the column is declared `NOT NULL` while a row is still NULL.

Consequences, both simplifications:

- **§2's reads name the column directly** — no fallback expression ever enters
  the ETag or the hot read path.
- **§5 is unnecessary** and has been folded in here.

Operators with an unusually large `storage.files` should know this upgrade step
is proportional to row count; a tight container healthcheck may need a raised
start-up grace period for that one deploy.

**Accept that backfilled values are ctime-flavoured.** A file renamed in 2024
gets that rename time as its "content modified". The true value is
unrecoverable — `created_at` is a lower bound, `updated_at` an upper one, neither
correct — so this is wrong-but-stable rather than wrong-and-disruptive. Stated
explicitly so nobody later tries to "fix" the historical data. Correctness
begins at the first real content mutation after this migration.

Backstop: `CHECK (content_modified_at > '2000-01-01Z')`. The "not more than an
hour in the future" bound cannot be a `CHECK` — it isn't immutable — so the
application validates first and returns 400 either way; the constraint exists so
a future write path that forgets the check cannot persist a 1970 timestamp.

### Writers

Four content-mutation paths set it, taking `X-OC-Mtime` when the client sent it
and `NOW()` otherwise:

- WebDAV `PUT` replacing the blob
- chunked upload completion
- delta commit
- `/api/files/upload` overwrite

`X-OC-Mtime` is **already parsed** — `nextcloud/webdav_handler.rs:868` → `:1019`
and `nextcloud/uploads_handler.rs:395` — it just currently lands in
`updated_at`. Same header, new destination.

Trust bounds per `docs/architecture/dates.md § Trust bounds`: reject more than
one hour in the future, reject earlier than 2000-01-01, 400 and never clamp.

Nothing reads the column in this phase, so a bug writes a wrong value somewhere
invisible.

**Verification — this is the phase where the tests earn their keep**, because
they are what makes §2 safe to ship:

- after each of the four paths, the column holds the expected instant
- `X-OC-Mtime` is honoured when present, ignored when out of bounds (400)
- rename, move, trash, grant and favourite leave the column **untouched** while
  still bumping `updated_at`

---

## §2 — Repoint the alias and the ETag **NOT STARTED**

**Classify, then repoint — this is not a find-replace.** The ~27 file-side reads
surveyed at the top of this plan are not all mtime-intent. At least one aliases
to `updated_at` rather than `modified_at`:

```sql
-- file_blob_read_repository.rs:695
EXTRACT(EPOCH FROM top.updated_at)::bigint AS updated_at
```

which may legitimately want ctime. Every site needs a per-site verdict — *wants
mtime* (repoint to `content_modified_at`) or *wants ctime* (leave alone) —
because a blind sweep would quietly convert audit-shaped reads into mtime reads.
Budget the classification, not just the edit.

The column is `NOT NULL` from §1, so these are bare column swaps — no
`COALESCE`, no fallback expression anywhere in the ETag or the read path.

`File::compute_etag` follows for free once the mtime sites move, since it reads
`modified_at`. **But it is not the only ETag entry point**, and the other one
does not ride the SQL alias.

### The four `etag_for_listing` sites are part of this sweep

```rust
// file.rs:423
pub fn etag_for_listing(blob_hash: Option<&str>, modified_at: DateTime<Utc>) -> String
```

It takes a `DateTime<Utc>` straight off a listing row, rounds via
`timestamp_helpers::datetime_to_etag_seconds` to match
`EXTRACT(EPOCH FROM x)::bigint`, and delegates to `compute_etag`. Four callsites
pass `row.modified_at` directly:

```
src/application/services/trash_service.rs:998
src/interfaces/nextcloud/report_handler.rs:439
src/interfaces/api/handlers/recent_handler.rs:269
src/interfaces/api/handlers/favorites_handler.rs:255
```

Each needs the same per-site verdict as the SQL reads: does this listing's
`modified_at` come from a repointed column or not? Trash, REPORT/SEARCH, Recent
and Favorites all show file ETags, so a miss here means one surface's ETag
disagrees with another's for the same file.

**That exact failure already happened once.** `etag_for_listing` exists because
the three-line `compute_etag` dance was inlined per site and drifted; its
docstring points at the captured Cyberduck regression in
`tests/api/webdav_etag_cross_check.hurl`. Same shape as the keyset hazard above:
one rule, computed in several places, silently diverging. That Hurl file is the
right regression gate for this phase too.

Search needs no reindex: `sort_enriched_files`
(`src/application/services/search_service.rs:220`) sorts an in-memory
`Vec<SearchFileResultDto>` in Rust, reading the DTO field — Tantivy holds no
sort field for this axis, so the search sort follows automatically.

### `File` and `Folder` keep `u64` — a design decision, not drift

Confirmed by the maintainer 2026-10-17. From the `pivot/dto-timestamps-iso`
record:

> **Domain entities keep `u64` internally.** `File::compute_etag(hash,
> modified_at: u64)` embeds the value verbatim in the ETag string — changing
> that would invalidate every cached ETag on upgrade.

`DateTime<Utc>` lives **only at the serde boundary**. The conversion is
centralised in `src/application/dtos/timestamp_helpers.rs`
(`epoch_to_datetime`, `datetime_to_epoch`, `epoch_zero`), applied at the DTO
mappers. The `u64` layer extends beyond these two entities — it also covers
`virtual_file_etag(size, modified_at: u64)`
(`domain/services/external_mount_id.rs:154`) and the virtual file/folder
descriptors in `application/ports/external_mount_ports.rs:50-68`. That is the
deliberate pre-conversion layer, not a set of holdouts.

Keeping the entity integer-typed also makes "ETag input is integer seconds" a
**type-level guarantee** rather than a convention: there is no rounding step for
a callsite to get wrong. The listing path needs
`timestamp_helpers::datetime_to_etag_seconds` precisely because it starts from a
`DateTime`; the entity path needs no such care.

What this means for the sweep:

- The sites keep their `EXTRACT(EPOCH FROM …)` shape — **only the column inside
  changes.** No type churn, so §2 stays a behaviour-only commit whose inertness
  is easy to verify.
- Nothing to coordinate with, and nothing to decide about merging.
- `content_modified_at` is `TIMESTAMPTZ` in the schema and reaches the entity as
  `u64` through the same `EXTRACT`, exactly as `updated_at` does today.

Worth doing while in these files: add doc comments to
`file.rs:22-23` and `folder.rs:23-24` recording this. Every other non-obvious
field in both structs is documented; these are not, which is why the question
gets re-asked.

### WOPI rides the alias too

Two WOPI fields are sourced from `modified_at` and therefore change with no edit
to the handler:

```rust
// src/interfaces/api/handlers/wopi_handler.rs
:179   last_modified_time: file.modified_at.to_rfc3339()   // WOPI LastModifiedTime
:192   version:            file.modified_at.to_string()    // WOPI Version
```

`Version` is not cosmetic. Office editors use it to detect "this document
changed underneath me", so it carries conflict semantics and belongs in this
phase's reasoning rather than inherited by accident.

**The change is an improvement.** Today a rename bumps `updated_at`, which bumps
`Version`, and Collabora/OnlyOffice can warn about an external edit that never
happened. Afterwards only a real content save moves it. And because this phase
is inert at deploy (§ No resync), a document open in an editor across the
rollout sees no `Version` change at all.

**Verification** belongs with the §2 manual pass: open a document in the office
editor, rename it in the SPA, and confirm the editor does *not* report an
external change — the behaviour that is wrong today.

#### The two fields want different things

| | WOPI requires | `updated_at` (today) | `content_modified_at` (after §2) | `blob_hash` |
|---|---|---|---|---|
| `LastModifiedTime` | last time the file was modified | ctime ✗ | **correct** ✓ | n/a |
| `Version` | changes iff the file changed | bumps on rename ✗ | no rename bump ✓ | **exact** ✓ |

`LastModifiedTime` lands correct for free. `Version` improves — renames stop
moving it — but `content_modified_at` is still a second-granularity timestamp,
so two saves inside one second collide and lost-update detection fails. That is
true today and stays true after this phase.

`blob_hash` is the strictly better source: content-addressing *is* "the same if
the file has not changed", with no granularity problem.

**Note this is the opposite requirement from the DAV ETag.** The ETag's mtime
suffix exists deliberately so a same-bytes-new-mtime touch invalidates, because
DAV clients track mtime. WOPI wants the reverse — identical bytes, identical
`Version`. The two protocols genuinely disagree, and today both derive from
mtime.

**Keep `Version` on `content_modified_at` for this plan** — but only to keep §2
single-purpose, not because switching it is risky. It is not:

`Version` is compared **only within a live editing session**. The editor calls
`CheckFileInfo` on open, takes whatever it gets as the baseline, and re-checks
during the session; nothing caches it across sessions. So a format change is
invisible to every closed document — the next open just establishes a new
baseline. The only exposure is a session spanning the restart, and those lose
their lock to the restart anyway:

```rust
// wopi_lock_service.rs — process-local, not persisted
:15   expires_at: Instant,                             // LockEntry: monotonic TTL
:28   locks: Arc<RwLock<HashMap<String, LockEntry>>>,  // in-memory map
```

Locks therefore cannot be inspected to schedule the migration — a deploy
restarts the process and empties the table regardless — but they also show that
a cross-restart session is already disrupted by the deploy itself, not by the
`Version` format. **No interlock or window is needed.**

> ⚠️ **That free pass depends on a defect.** Lock ephemerality is itself a
> lost-update hole, filed as [`durable-locks.md`](durable-locks.md): a session
> can hold a lock, lose it to a restart, and have a second session legitimately
> acquire the same lock — after which whoever saves second silently overwrites.
> If that plan lands first, the `Version` → `blob_hash` change *does* need the
> interlock dismissed above. Cheapest ordering is to do the `Version` change
> while locks are still ephemeral.

**Follow-up (not this plan): derive WOPI `Version` from `blob_hash`.** Fixes
same-second lost-update detection and matches the protocol's requirement that
identical content yields an identical version. Cheap, given the above.

**With a known successor.** If true file versioning lands, `Version` should
become the version identifier — stable, meaningful, and exactly what the
protocol means. A content hash is the right answer only until something better
exists to point at, so do the simple fix and do not engineer around it.

**This must be a single commit.** The two halves are not independently correct:

- `D:getlastmodified` repointed without the ETag → a mtime-only touch stops
  invalidating, and clients serve stale metadata. This is exactly the case
  `File::compute_etag`'s own comment at `src/domain/entities/file.rs:371` says
  the suffix exists to catch.
- ETag repointed without `getlastmodified` → the two properties disagree about
  when the file last changed.

### No resync — the deploy is inert

Because §1 backfills from `updated_at` and the transitional read falls back to
`updated_at`, every file's reported mtime and ETag are **byte-identical before
and after this commit**. Nothing re-downloads, nothing conflicts, and no release
note is needed.

The behaviour change arrives gradually instead: from this point on, a rename,
move, trash, grant or favourite no longer moves the ETag, while a content
mutation still does. Divergence between `updated_at` and `content_modified_at`
only begins with the first real content change after §1 shipped.

That is the whole reason to backfill from `updated_at` rather than `created_at`.
The `created_at` variant would have changed the ETag of every file ever touched
since upload — and reported an *older* mtime than before, which a sync client
can read as "my local copy is newer" and answer with an upload or a conflict
rather than a download.

**Verification:** the WebDAV + NC api-test suites, plus one manual NextCloud
desktop resync against a file that has been renamed since upload (the case that
is wrong today and right afterwards).

---

## §3 — Sort axes **NOT STARTED**

Couples to §2 — the REST `modified_at` field moves in the same commit, so the
UI's "Last modified" column repoints at the same moment.

The end state, from `docs/architecture/dates.md § Sort axes`:

```sql
media_sort_date = COALESCE(captured_at, source_created_at, content_modified_at)
```

Three arms. `content_modified_at` terminates the chain because §1 made it
`NOT NULL` — and `media_sort_date` is itself `NOT NULL`, so the chain must end on
a column that always has a value.

One transitional wrinkle: while §4 is blocked the `source_created_at` arm does
not exist, so this phase lands the two-arm form and §4 inserts the middle arm:

```sql
media_sort_date = COALESCE(captured_at, content_modified_at)   -- interim
```

Photos ordering does **not** shift at deploy, because §1 backfilled
`content_modified_at` from `updated_at` — which is what `media_sort_date`
already resolves to for a file without EXIF. The improvement arrives as files
are modified afterwards: a non-EXIF image picks up a real content mtime instead
of staying pinned to its upload time.

Either way the **UI axis binds to the `media_sort_date` column, never to the
expression.** It is denormalised precisely so the Photos timeline can seek:
`idx_files_media_timeline_by_drive (drive_id, media_sort_date DESC)` lets
`LIMIT` stop the scan early. Re-deriving the COALESCE at query time would need a
join to `file_metadata` for `captured_at`, could not use that index, and would
turn the timeline into a scan.

The trigger needs a **second firing condition**. Today
`trg_sync_media_sort_date` fires only on `file_metadata.captured_at`
(`migrations/20260307000000_initial_schema.sql:697`); it now also has to fire
when `storage.files.content_modified_at` changes.

| query | resolves to |
|---|---|
| `?order_by=captured_at` | `media_sort_date` |
| `?order_by=updated_at` | `content_modified_at` |
| `?order_by=created_at` | `created_at` — unchanged, admin-facing |

`nc:creation_time` moves to the same chain as `media_sort_date` —
`COALESCE(captured_at, content_modified_at)` — so the displayed creation date
and the timeline order stay the same expression at every phase. §4 adds the
`source_created_at` arm to both together.

Photos ordering does **not** shift at deploy, because §1 backfilled
`content_modified_at` from `updated_at`, which is what `media_sort_date` already
resolves to for a file without EXIF. The improvement arrives as files are
modified afterwards — a
non-EXIF image picks up a real content mtime instead of staying pinned to its
upload time.

---

## §4 — Add `source_created_at` **BLOCKED**

```sql
ALTER TABLE storage.files
    ADD COLUMN source_created_at TIMESTAMPTZ;
```

Nullable, no default, **no backfill ever** — NULL is the meaningful value, and
`docs/architecture/dates.md § When creation date is unknown` explains why a
stored fallback is worse than none.

On `storage.files` rather than `storage.file_metadata` where `captured_at`
lives: `captured_at` is extracted *media* metadata, while a source creation date
applies to any file. Keeping it on `storage.files` also leaves two of the three
`media_sort_date` arms local to one table.

**The blocker is choosing a writer, not a NextCloud answer.** `nc:creation_time`
is only ever *emitted* today — no read path exists. Two candidate writers:

1. **A NextCloud client**, if any sends creation time on upload. Resolve this
   *empirically rather than by reading their source*: one `tracing::debug!` on
   the NC upload path logging unrecognised PROPPATCH properties and unhandled
   `X-OC-*` headers, left running for a week, answers it for the clients your
   users actually run — and catches the header variant (`X-OC-CTime` in some
   ownCloud/NextCloud versions) that a property-name grep would miss.
2. **Server-side content metadata** — OOXML `docProps/core.xml` →
   `dcterms:created`, PDF `/CreationDate`. `lopdf` and `pdf_extract` are already
   in the tree (`search_index/text_extractor.rs` uses them for search text), so
   the parsing capability exists even though no date extraction does.

Candidate 2 is the better feature and entirely in your control: it makes the
column **server-extracted and trustworthy**, dropping it out of the
client-asserted class — no trust bounds, no 400s — and gives documents a real
creation date instead of a fallback to mtime.

Note this cannot reuse `captured_at`: `file_metadata` is media-shaped
(`camera_make`, `orientation`, `latitude`, `longitude`, `width`, `height`), so a
`.docx` creation date does not belong beside it. `storage.files` is the right
home either way.

**Not a pre-start blocker.** §4 is last and independent of §0–§3 and §5–§7, and
the behaviour when nothing writes the column is already specified: NULL forever,
`nc:creation_time` falling through the COALESCE.

If it does land, the sort chain gains its middle arm:
`COALESCE(captured_at, source_created_at, content_modified_at)`.

---

## §5 — Tighten to `NOT NULL` **FOLDED INTO §1** (2026-10-17)

No separate phase. §1's migration backfills in-transaction and lands `NOT NULL`
in the same statement sequence, so the column is dense the moment it completes
and the end-state shapes apply immediately:

```
D:getlastmodified = content_modified_at
ETag              = {blob_hash[..16]}-{content_modified_at}
media_sort_date   = COALESCE(captured_at, source_created_at, content_modified_at)
```

No fallback expression ever enters the ETag or the hot read path.

> **History, because this flip-flopped three times and the reasoning matters.**
> Dropped 2026-10-11 (column nullable forever with a `created_at` fallback);
> restored 2026-10-12 when the backfill source changed to `updated_at`; folded
> into §1 on 2026-10-17 during implementation.
>
> The last move came from questioning why the backfill needed to be a job at
> all. It was specified that way to avoid delaying boot — but a job's drain
> state cannot be confirmed across self-hosted deployments we do not operate,
> so the transitional `COALESCE` would have lived in the ETag path indefinitely
> and this phase would never have been safely reachable. One full-table
> `UPDATE` on a metadata-only table, once, on upgrade, buys certainty instead.
>
> The `NOT NULL` constraint was never the point; removing a permanent fallback
> expression from the hot path is.

Section kept rather than renumbered so §6 and §7 keep their identifiers.

---

## §6 — UI axes **NOT STARTED**

The user-visible half of §3. The axes are already the right ones — they were
pointed at the wrong columns — so this is one repoint and one relabel, with no
new sort options anywhere.

### `/files`

`SortField = 'name' | 'type' | 'size' | 'modified_at' | 'created_at'`
(`routes/files/[...path]/+page.svelte:1803`).

| Option | Column | Change |
|---|---|---|
| Modified | `updated_at` → `content_modified_at` | **none in the FE** — follows §2 automatically. The label finally means what every file manager's "Date modified" means. |
| Created | `created_at`, unchanged | **relabel → "Added"** |

The relabel is the only FE edit. The column is right — arrival is a genuinely
useful axis — but the word is wrong: a user who uploads a 2008 photo library and
reads "Created: today" concludes the app is broken. Finder draws exactly this
distinction with *Date Added* beside *Date Created*, and Drive uses *Date
added*. Same column, honest name, no backend change.

**Do not add a third date axis here.** `source_created_at` is NULL for nearly
every row and `captured_at` is media-only; an axis that is empty for most files
sorts arbitrarily and reads as a bug.

The group-by swimlanes share these dimensions (Type / Size / modifiedAt /
createdAt), so the relabel has to land there too — easy to miss.

#### The relabel is an i18n change, not a string edit

There are **16 locales** (`frontend/static/locales/*.json`), reached through
`t(key, [params], fallback)`. The exact labels to change:

```ts
// routes/files/[...path]/+page.svelte
:1897   label: t('groupby.modifiedAt', 'Modified date'),   orderBy: 'modified_at'
:1905   label: t('groupby.createdAt',  'Created date'),    orderBy: 'created_at'
```

So it is `groupby.createdAt` whose **value** becomes "Added date". **Keep the
key**: it is named after the dimension, not the wording, so renaming it would
mean touching all sixteen files to avoid sixteen missing-key fallbacks for no
gain.

The work is therefore:

- change the value in **`en.json` only**, and the inline fallback beside it
- leave the other fifteen to the existing translation backlog — the English
  fallback carries them meanwhile
- **do not** run `scripts/bulk_translate.mjs`: it translates without context

Same treatment for Photos' three axis labels if any are reworded.

**One surface needs no change at all.** The `/files` date *column* header is
`t('files.col_modified', 'Date')` (`ResourceList.svelte:1594`, overridable via a
`dateLabel` prop) — generic enough that it does not lie today and does not lie
afterwards. Only the sort/group-by dimension labels carry the misleading word.

### Photos

`PhotosOrderBy = 'captured_at' | 'created_at' | 'updated_at'`
(`lib/api/endpoints/photos.ts:118`), already labelled "Captured date" /
"Upload date" / "Last modified".

**No FE change at all.** All three improve underneath:

| Option | Resolves to | Effect of this plan |
|---|---|---|
| Captured date | `media_sort_date` column | the column's definition improves in §3 and §4; the axis never changes |
| Upload date | `created_at` | already correct |
| Last modified | `updated_at` → `content_modified_at` | follows §2 |

The notable gain is "Captured date" for images *without* EXIF: today they fall
back to upload time, so a camera roll imported in one session collapses onto a
single day. After §3 they fall back to content mtime, which usually survives a
copy — materially better timeline ordering, for free.

### Retire `updated_at` from the wire — not optional

Today the two endpoints spell the same axis differently:

```
folder_dto.rs:360    "modified_at", "created_at", "size"
search_dto.rs:358    "updated_at",  "created_at", "size"
```

A full audit of the sort surface — only two endpoints are affected, and trash
and grants are correctly out of scope because they sort on domain events rather
than file clocks:

| Endpoint | Param | Date axes | Spelling |
|---|---|---|---|
| `folders/{id}/resources` | `order_by` | `modified_at`, `created_at` | ✅ |
| `recent` | `order_by` | `modified_at` | ✅ |
| `favorites` | `order_by` | `favorited_at`, `modified_at` | ✅ |
| `photos/resources` | `order_by` (typed `PhotoOrderBy`) | `captured_at`, `created_at`, **`updated_at`** | ❌ rename |
| `search` | `order_by` / `sort_by` | **`updated_at`**, `created_at` | ❌ rename |
| `trash` | `order_by` | `deletion_date`, `trashed_at` | n/a |
| `grants/{incoming,outgoing}/resources` | `sort_by` | `granted_at`, `granted_by` | n/a |

Two further inconsistencies, worth settling while the surface is open but
neither blocking:

- **`order_by` vs `sort_by`.** Grants uses `sort_by`, everything else
  `order_by`, and search uses both (`search_dto.rs:73`/`:279` vs `:363`/`:467`).
  One concept, two names, one API.
- **Typed axes everywhere** (decided 2026-10-12). `PhotoOrderBy` is a serde
  enum; folders, recent, favorites, trash and search all take `Option<String>`
  and validate at runtime, so a bad value silently falls back to the default.
  See § Typed sort axes below.

Also rewrite the two `PhotoOrderBy` doc comments, which currently document the
old behaviour accurately: `UpdatedAt` describes ctime ("uploads, renames,
overwrites, trash transitions"), and `CapturedAt` says it falls back to
`created_at`, which §3 changes to `content_modified_at`.

That is cosmetic today. After §2 it is not: an API value spelled `updated_at`
would resolve to `content_modified_at`, while the column `updated_at` keeps
meaning ctime. The same token would mean mtime on the wire and ctime in SQL —
which is how someone eventually sorts a listing by the wrong clock and cannot
see why. The rule, stated in
`docs/architecture/dates.md § API names are not column names`:

> **The wire never says `updated_at`.**

So this is a rename, not an alias-forever. Live senders:

| Site | Value | Becomes |
|---|---|---|
| `lib/api/types.ts:335` | `SortBy = … \| 'updated_at'` | `'modified_at'` |
| `routes/search/+page.svelte:262` | `orderBy: 'updated_at'` | `'modified_at'` |
| `lib/api/endpoints/photos.ts:118` | `PhotosOrderBy = … \| 'updated_at'` | `'modified_at'` |
| `lib/api/generated/types.gen.ts` | documented enum | regenerates |
| `search_dto.rs:358` | accepted value | accept `modified_at`; `updated_at` warns |

Server side: accept `modified_at` everywhere, keep `updated_at` accepted with a
deprecation warning for one release, then remove — the same warn-now/remove-next-major
pattern used for contradictory auth config.

**Easy to miss: the Photos axis is persisted.**
`routes/photos/+page.svelte:560` compares against a *saved* `savedOrderBy`, so
stored `'updated_at'` values outlive the deploy. The preference loader must map
the legacy value to `'modified_at'` on read, or users who had picked "Last
modified" silently fall back to the default axis.

**Do not rename the column.** `updated_at` also exists on folders, contacts and
calendars; renaming it is a tree-wide sweep that buys nothing once the wire
stops using the token. One rule beats one rename.

**Guard it.** After the deprecation window, a CI grep is enough: no `order_by`
handler accepts `"updated_at"`, and no wire DTO selects `storage.files.updated_at`.

---

## §7 — Naming normalisation **NOT STARTED**

Adjacent to the date work rather than part of it — ships independently, no
column changes. Two conventions, applied everywhere.

### `order_by`, not `sort_by`

Five of seven paged endpoints already say `order_by`; grants and search's
internal pair say `sort_by`. Normalise to `order_by`.

The deviation has a source, and it is not grants:

```rust
// cursor.rs:95 — the shared base every paged query struct builds on
pub struct CursorQuery {
    pub limit: u32,
    pub cursor: Option<String>,
    pub sort_by: Option<String>,
}
```

`cursor.rs:78-84` is a doc example instructing implementers to write
`pub sort_by: Option<String>` in their own struct. **Fixing that example is what
makes the convention stick** — otherwise the next endpoint re-seeds `sort_by`.

`CursorQuery` derives `IntoParams, ToSchema`, so the field is in
`resources/gen/openapi.json` and the generated TS SDK. Wire-visible: accept
`order_by`, keep `sort_by` with a deprecation warning for one release, remove
after.

### Typed sort axes

`PhotoOrderBy` (`photos_dto.rs:50`) is the shape to converge on — every other
paged endpoint takes `Option<String>`.

**Per-endpoint enums, not one shared union.** `Name`, `Type` and `Size` recur,
but the date axes do not: trash has `trashed_at`/`expires_at`, favorites has
`favorited_at`, recent has `accessed_at`, search has `relevance`. A single union
would accept nonsense on every endpoint but its own.

Seven enums, variants as the accepted values stand today with this plan's
renames applied. Defaults differ per endpoint and must be preserved — mark the
right variant `#[default]`:

| Enum | Variants | Default |
|---|---|---|
| `FolderOrderBy` | `Name` `Type` `ModifiedAt` `CreatedAt` `Size` | `Name` |
| `PhotoOrderBy` *(exists)* | `CapturedAt` `CreatedAt` `ModifiedAt` | `CapturedAt` |
| `RecentOrderBy` | `AccessedAt` `Name` `Type` `ModifiedAt` `Size` `Owner` | `AccessedAt` |
| `FavoritesOrderBy` | `Name` `Type` `FavoritedAt` `ModifiedAt` `Size` `Owner` | `Name` |
| `TrashOrderBy` | `ExpiresAt` `TrashedAt` `Name` `Type` `Size` | `ExpiresAt` |
| `SearchOrderBy` | `Relevance` `Name` `Size` `ModifiedAt` `CreatedAt` | `Relevance` |
| `GrantOrderBy` | `GrantedAt` `GrantedBy` | `GrantedAt` |

Copy `PhotoOrderBy`'s derives verbatim — `ToSchema` is the one that matters,
since it is what makes OpenAPI emit a real `enum` rather than `string`:

```rust
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
```

**Type the cursor axis too.** Each endpoint carries its axis in cursor state
(`folder_dto.rs:403`, `search_dto.rs:467`, `trash_dto.rs:103`,
`recent_dto.rs:151`, `favorites_dto.rs:163`) as a `String` or `&str`. Typing
those is what makes the mid-scroll-flip rejection uniform — `PhotoOrderBy`
already does this, and its doc comment explains why:

> the cursor carries the axis so a mid-scroll flip is rejected by the handler
> with 400 rather than silently drifting pagination

Worth confirming each endpoint actually rejects a flip rather than assuming it;
that is the same sort-key-versus-cursor disagreement as §2's keyset hazard.

**Why it belongs in this plan rather than after it.** Typed axes make the §6/§7
renames safe end-to-end:

1. Rust: every internal use of a renamed variant becomes a compile error, not a
   grep.
2. OpenAPI: a serde enum emits a real `enum` schema instead of `string`.
3. Generated TS SDK: that becomes a union type, so the frontend cannot send a
   stale value — it fails at `npm run check` rather than silently sorting by the
   default.

**Step 3 is a precondition, not a bonus — it does not hold today.** The two sort
types the frontend actually imports are still hand-written, and the generated
SDK carries neither:

```ts
endpoints/photos.ts:118   export type PhotosOrderBy = 'captured_at' | 'created_at' | 'updated_at'
types.ts:223              export type SortBy = 'relevance' | 'name' | 'size' | 'updated_at' | 'created_at'
generated/types.gen.ts    — no SortBy, no OrderBy
```

TypeScript types are now generated from OpenAPI, but the migration off
`types.ts` is incomplete. Until those two are redirected to the generated SDK,
the chain breaks at the last link and the frontend can still send a retired
value with no compile error.

So §7 either waits on those redirects, or ships with steps 1–2 only — in which
case the protection against stale values is a server-side 400 rather than a
compile error, and the persisted-Photos-preference migration below becomes the
*only* thing preventing a silent revert rather than a belt beside braces.

Without this, a stale `order_by=updated_at` from any client is indistinguishable
from "no preference" and sorts by the default. With it, the deprecation window is
an explicit variant rather than a string comparison someone forgets to delete.

**The compatibility cost, stated plainly.** Typed extraction is strict: a value
that is tolerated today starts returning 400 from the extractor. That is the
same unforgiving behaviour as `Query<bool>` only accepting literal
`true`/`false`. Two requirements follow:

- Map the extractor rejection to a useful error that names the accepted values,
  rather than Axum's default "Failed to deserialize query string". The SPA
  switches on `error_type`, not on message text, so the mapping needs an
  `error_type` as well as readable prose.
- Keep the deprecated variants (`UpdatedAt`, `DeletionDate`) in the enums for
  the deprecation window so the rename stays a warning rather than a 400, then
  remove them with the window.

### `deletion_date` → `expires_at`, not `deleted_at`

The `*_at` convention should apply, but the tense has to match the value.
`deletion_date` holds a **future** instant — `trash_dto.rs:170` says it is "the
real timestamp at which the item will be permanently deleted", and
`trash_service_test.rs:421` compares it forward (`deletion_date() > now`).

`deleted_at` would therefore read as a past event while holding a future one —
the same class of confusion this plan exists to remove. The trash endpoint
already has the genuine past-tense field, so the correct pair is:

| Axis | Meaning | Tense |
|---|---|---|
| `trashed_at` | when it went to the bin | past |
| `expires_at` | when it will be purged | future |

`expires_at` is also already the codebase's name for a scheduled expiry
(`role_grants.expires_at`), so this converges rather than inventing.

Affects the `order_by` value, `TrashItemDto.deletion_date`
(`trash_dto.rs:88`, `:181`) and `default_order_by` (`:123`) — all wire-visible,
same deprecation window.

---

## Tests

Per-phase verification notes live with their phases. Three things need writing
from scratch, and the first is the only one that exercises the browser edge at
all.

### E2E — drag-and-drop preserves the source mtime

New spec `tests/e2e/spa/dates.spec.ts`, following the house conventions
(`import { test, expect } from './coverage-helpers'`, `apiLogin` in a
`beforeEach`, `getByTestId` selectors). Nothing in the suite currently drags a
file; `apiUploadFile` goes through the API and so never produces a
`File.lastModified` at all.

**Build the `File` in-page, not with `setInputFiles`.** Playwright's
`setInputFiles` from a buffer gives no control over `lastModified`, which is the
whole point of the test. A drag needs a `DataTransfer` anyway, so construct both
inside `page.evaluate`:

```ts
const MTIME_MS = 1_792_055_662_700;   // see below — the .7 matters
await page.evaluate((ms) => {
  const file = new File(['hello'], 'dragged.txt', {
    type: 'text/plain',
    lastModified: ms,
  });
  const dt = new DataTransfer();
  dt.items.add(file);
  document.querySelector('[data-testid="…dropzone…"]')!
    .dispatchEvent(new DragEvent('drop', { dataTransfer: dt, bubbles: true }));
}, MTIME_MS);
```

(Grep `data-testid` in the files route for the actual drop target.)

**Choose the millisecond value deliberately.** The sub-second remainder must be
**≥ 500** so truncation and rounding disagree:

| | value |
|---|---|
| `lastModified` | `1792055662700` ms |
| truncated | `1792055662` ← wrong |
| rounded | `1792055663` ← the assertion |

With a remainder below 500 the test passes under either implementation and
proves nothing about the round-don't-truncate rule.

**Where the precision is actually lost** — not on the REST wire, which carries
RFC 3339 strings and so is sub-second capable. The lossy hops are
`X-OC-Mtime` (integer seconds) and the entity-level `u64` on `File`. So this
test exercises the *upload* path's rounding, and reading the value back is not
itself lossy.

That also means the assertion stays valid at **second granularity** regardless of
whether the `u64` → `DateTime` entity conversion has landed: truncation yields
`…662` and rounding `…663`, a whole-second difference either way. Do not assert
sub-second precision — it is undefined until that conversion settles, and a test
that pins it will break when the conversion lands.

Then three assertions:

1. The stored `modified_at` is `1792055663` — the source mtime, **not** the
   upload moment.
2. The listing's "Modified" cell renders that instant, so the ms→s→display path
   is covered end to end.
3. **Rename the file and re-read.** `modified_at` must be unchanged. This is the
   regression the whole plan exists to fix, and it is one extra step.

### Pagination across divergent clocks

The §2 keyset hazard needs its own test: seed a folder whose files have
deliberately divergent `updated_at` and `content_modified_at`, list it with
`order_by=modified_at` at a page size smaller than the set, page to the end, and
assert every file appears exactly once. A sort key and cursor predicate reading
different columns passes a single-page test and fails only here.

### Stale sort values are rejected, not defaulted

Once §7's enums land, assert that `order_by=updated_at` and
`order_by=deletion_date` produce a 400 naming the accepted values (or a
deprecation warning during the window) rather than silently falling back to the
default axis. Same for the persisted Photos preference: a stored legacy value
must map forward, not reset the user's choice.

---

## Performance

Surveyed against the current schema. One trap, one real operational cost, and
otherwise nothing measurable.

**No read shim to pay for.** §1 lands the column `NOT NULL`, so §2's reads name
it directly — there is no `COALESCE` in any query plan at any point.

Worth recording that it would have been free anyway: the only date indexes on
`storage.files` are the three on `media_sort_date` (`initial_schema.sql:582`,
`files_media_timeline_by_drive_index.sql:23`,
`files_media_dedup_by_drive_blob_index.sql:29`). Nothing indexes `updated_at` or
`created_at`, so `order_by=modified_at` and `order_by=created_at` are *already*
unindexed sorts — and bounded ones, since the listing filters by parent and
drive before sorting. Photos is untouched either way: it keeps seeking the
indexed `media_sort_date` column.

**The one-time cost is the migration's `UPDATE`.** One full-table rewrite on
upgrade, proportional to row count, on a metadata-only table. Plus autovacuum
afterwards to reclaim the dead tuples.

**The trigger trap: two triggers, two shapes.** The existing
`sync_media_sort_date` is `AFTER INSERT OR UPDATE OF captured_at ON
storage.file_metadata` and its body issues `UPDATE storage.files … WHERE id =
NEW.file_id`. A cross-table AFTER trigger is correct *there*. Extending that same
function to also fire on `storage.files.content_modified_at` would make every
upload issue a **second UPDATE of the row it just wrote** — on the hottest write
path in the product. The files-side firing must instead be a
`BEFORE INSERT OR UPDATE OF content_modified_at ON storage.files` trigger
setting `NEW.media_sort_date` in place: no extra write, no re-entrancy.

**Row width:** two `TIMESTAMPTZ` columns add 16 bytes per row. Not TOAST-able,
so no wide-read penalty of the kind `hidden-system.md` documents for the avatar
column.

**There is no resync, and that is the point of backfilling from `updated_at`.**
Every reported mtime and every ETag is byte-identical across the whole rollout,
so no client transfers anything it would not have transferred anyway. The
`created_at` variant of this plan would have changed the ETag of every file ever
touched since upload; see § No resync in §2.

**The §1 backfill is deliberately a job, not a migration**, so its cost is
steady background load that can pause and resume rather than a boot stall
proportional to table size.

---

## Open decisions

### Decided 2026-10-11

**COPY sets `content_modified_at` to `NOW()`.** A copy is a new file whose
content came into existence here and now — the `cp`-without-`-p` answer, and it
needs no preservation plumbing on the copy path. Trash restore needs no code at
all: it flips a soft-delete flag, so the value survives while `updated_at` bumps.

**Folder `D:getlastmodified` repoints to `tree_modified_at`.** Collections
currently emit the folder row's `updated_at` while their ETag derives from
`tree_modified_at`, so the two disagree — the ETag bumps when a child changes,
`getlastmodified` when the folder is renamed. For a collection "content" is its
children, so this is what RFC 4918 §15.7 asks for and it makes the two
properties consistent.

This brings the **folder-side reads into §2's scope** after all: `fo.` (10
sites) and `fld.` (3 aliases) repoint to `tree_modified_at` for the
`getlastmodified` path. Note the asymmetry — folders get no new column, because
`tree_modified_at` already *is* the subtree-content clock; the work is pointing
the property at it.

---

## Out of scope

**CalDAV and CardDAV.** `caldav_adapter.rs:201` feeds `getlastmodified` from the
calendar row's own `updated_at`, and that is right: an event or contact has no
separate blob, so its row change *is* its content change. Do not apply this plan
there — stated explicitly so a later reader does not "fix" it.

**External mounts.** There is a third ETag producer,
`virtual_file_etag(size, modified_at) -> "ext-{size:x}-{modified_at}"`
(`domain/services/external_mount_id.rs:154`, plus `virtual_folder_etag`), used
from `mount_dto.rs:122`/`:149`, `file_handler.rs:1161` and
`folder_handler.rs:858`. Its `modified_at` comes from the provider's `stat` or
directory entry, **not** from `storage.files` — so there is no column to
repoint and nothing here applies. Listed because it is easy to find while
grepping for ETag code and mistake for an omission.

---

## Sequencing

§0, §1 and §4 are independently shippable. §2 and §3 are coupled tightly enough
to go back to back. §5 no longer exists — §1's migration lands `NOT NULL`
in-transaction, so nothing waits on operational signal. §7 is independent of all
of them.

Because §1 backfills from `updated_at`, **no phase here carries a client-visible
discontinuity.** The order matters for internal consistency, not for blast
radius.

Expect test-fixture churn in §2: anything in the Hurl suites or integration
tests pinning a specific `modified_at` value shifts when the aliases move.
