# Plan — Normalize file dates: one column per meaning, one wire slot each

**Status:** design agreed 2026-10-11; §1–§3 landed 2026-10-17. The end state is specified
in [`docs/architecture/dates.md`](../architecture/dates.md) — this plan is only
how to get there. Triggered by a review of the four date surfaces (OxiCloud
columns, the browser File API, NextCloud DAV, RFC 4918) which found that
`storage.files.updated_at` is doing the work of three different timestamps, and
that two NextCloud properties already on the wire read the wrong columns.

Progress, kept current as work lands — each phase carries its own marker at its
heading:

| phase | state |
|---|---|
| §0 fix the two `nc:` slots | **DONE** 2026-10-20 — both slots repointed. `nc:upload_time` → `created_at`; `nc:creation_time` → the content's own claimed date (`captured_at` today, `content_metadata.created_at` once the extractor widens) with **`0` for "unknown"**, which is NC's own value for this property. The planned `COALESCE(captured_at, content_modified_at)` form was REJECTED mid-implementation — see the correction below |
| §1 add `content_modified_at`, write it, read nothing | **DONE** 2026-10-20 — migration, writers and trust bounds (`625f089d`), CREATE-path gap closed, and the tests landed green. Two documented non-gaps: delta commit is not coverable in Hurl (CDC hashes — `rt_bus_check.sh` S21 covers it), and `/api/files/upload` **overwrite** was misdescribed — that endpoint only creates. See § Coverage as landed |
| §2 repoint the `modified_at` alias + ETag | **DONE** — file-side (27 sites + cursor predicate, `8d0631be`) and folder-side DAV-only (`2def1acb`). One gap left open: NC REPORT search, see Open decisions |
| §3 sort axes | **DONE (interim two-arm form)** — `media_sort_date` on the content clock + both search axes repointed. §4 inserts the `source_created_at` arm |
| §4 add `source_created_at` | **UNBLOCKED, and now clearly necessary** — repurposed 2026-10-20: it is the store for the client-asserted creation time, NextCloud's `X-OC-CTime` (the sibling of §9's `X-OC-Mtime`/`source_modified_at`). That is a DIFFERENT value from the metadata date §0 now emits — client filesystem birthtime vs. what the bytes claim — so the two coexist rather than one replacing the other. Final chain: `COALESCE(client ctime, content metadata date, 0)`. Still gated on the inbound test, which now answers one concrete question: does the NC client actually send `X-OC-CTime`? |
| §5 tighten to `NOT NULL` | **FOLDED INTO §1** 2026-10-17 — the migration lands it in-transaction |
| §6 UI axes | **DONE** 2026-10-19 — `updated_at` retired from the wire outright (no deprecation window: API too recent, SPA is the only consumer), `Created date` → `Added date`, persisted Photos preference migrated on read |
| §7 naming normalisation | **NOT STARTED** — `order_by` everywhere, typed per-endpoint enums, `deletion_date` → `expires_at`; ships independently. Compile-time protection waits on the generated-SDK redirects |
| §9 preserve the browser's mtime on upload | **DONE** 2026-10-20 — `source_modified_at` (epoch ms) on ALL THREE upload paths: multipart `/api/files/upload`, `/api/files/by-hash` (the instant-upload short-circuit) and delta commit (both create and update modes). Both UI entry points — picker and drag, single and multi — send `File.lastModified`. Original: — repository layer and helpers are ready. THREE upload paths to cover (multipart, delta, chunked); the SPA picks by file size, so covering only `/api/files/upload` would drop mtime on every large upload |
| §8 folders get a content clock | **DONE** 2026-10-18 — `storage.folders.content_modified_at` with POSIX entry-list semantics (migration `20261103000008`), 14 folder-side reads repointed, keyset predicate back to one column |

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
change. The folder side turned out *not* to be a SQL sweep at all: folders get
no new column, and the `fo.`/`fld.` reads were left alone, because repointing
them would have moved REST `FolderDto.modified_at` along with DAV. The subtree
clock reaches `D:getlastmodified` through a new `FolderDto.tree_modified_at`
field read at the three DAV emission sites instead — see § Folder side.

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
§2 and §3 are coupled: for **files**, REST, both DAV surfaces and the ETag
formula move together — there is no way to repoint DAV only, because they share
one SQL alias.

For **folders** there turned out to be, and §2 used it: add a second field to
the DTO and read it only where DAV is emitted. That option exists for folders
precisely because the subtree clock is a real column (`tree_modified_at`) rather
than an alias, so surfacing it costs a field rather than a meaning change.

---

## §0 — Fix the two `nc:` slots **DONE** (2026-10-20)

### Correction: `COALESCE(captured_at, content_modified_at)` was wrong

Everything below this box describing `nc:creation_time` as
`COALESCE(captured_at, content_modified_at)` — matching `media_sort_date`'s
chain — was **rejected during implementation**. It is kept for the reasoning
trail; the shipped behaviour is the table at the end of this box.

The defect: **that chain drifts.** `media_sort_date`'s two triggers recompute
it on every content mutation
(`20261103000007_media_sort_date_content_clock.sql`), so for any file without a
metadata date it tracks `content_modified_at` permanently. A *creation* date
that advances every time the file is saved is the same lie this phase removed
from `nc:upload_time`, which used to read the content clock. Making the two
expressions "match exactly" would have propagated the drift into a second slot
rather than fixing anything.

Both file clocks fail, in opposite directions, and neither failure is fixable
by choosing the other:

| fallback | 2019 doc uploaded today | after a later server-side edit |
|---|---|---|
| `content_modified_at` | 2019 ✅ (via §9) | moves to today ❌ |
| `created_at` | today ❌ — and identical to `nc:upload_time`, the very bug §0 exists to fix | today ✅ |

**What shipped instead: `0` is a real answer.** `nc:creation_time` is NOT a
server-side clock at all. In NextCloud it is the *client's* local filesystem
birthtime, sent on upload via `X-OC-CTime`, and it stays at its `0` default
when the client's OS or filesystem cannot supply one. So `0` is NC's own
representation of "unknown", and every NC client already handles it. Reporting
`0` for a `.txt` is therefore well-formed, where guessing a file clock would
turn "unknown" into a confident wrong answer.

That also removes the fallback decision entirely, which is why §0 shipped
without waiting on §4:

| property | reads | when unknown |
|---|---|---|
| `nc:upload_time` | `created_at` — when WE received the bytes | n/a, always known |
| `nc:creation_time` | the content's own claim: `captured_at` today, `content_metadata.created_at` once the extractor widens | `0` |

Three consequences worth carrying forward:

- **`media_sort_date` is NOT renamed.** A name like `content_created_at` was
  considered and rejected: the column legitimately drifts, which is correct for
  a sort axis and disqualifying for a creation date, so any `*_created_at` name
  would lie. It keeps its name because `nc:creation_time` no longer reads it.
- **§4 is unblocked and repurposed** — it is the store for `X-OC-CTime`. See
  its row in the status table.
- **Forward-compatible for free.** The emit site reads one value, so when
  `captured_at` becomes `storage.content_metadata.created_at` that is a
  one-line repoint, and Word/PDF/Excel creation dates reach NC clients the
  moment the extractor widens — §0 is never revisited.

**Implementation note.** The value is a per-row side channel, not a `FileDto`
field: it lives in a different table and is sparse (media only, today). All
four emit sites take it from `FileMetadataRepository::get_batch` — which was
dead `pub` code until now — via `interfaces::nextcloud::nc_creation_time`. In
the streaming PROPFIND the read joined the existing `tokio::join!`, so it adds
no wall-clock alongside the three batched reads already in flight. A read
failure degrades the property to `0` rather than failing the listing.

**Coverage.** Four Rust wire-shape guards in `webdav_handler.rs`
(`wire_shape_tests`), whose fixture gives `created_at`, `modified_at` and the
metadata date three mutually distinct values so a regression to either file
clock fails; plus Part A3 of `tests/api/nc_webdav_client_mtime.hurl`, which
pins both slots end-to-end on a `.txt` whose content clock is three years from
its receipt instant. **Known gap:** no end-to-end assertion that a *present*
metadata date reaches the wire — no test fixture carries EXIF, and extraction
is spawned off the upload (`on_file_created` is sync), so such a test would
race. The emitter half is covered in Rust; the missing link is repository →
wire for a non-zero value.

---

### Original phase text (superseded above for `creation_time`)

Independent of everything else. Two lines at
`src/interfaces/nextcloud/webdav_handler.rs:2268` and `:2273`:

| property | today | after |
|---|---|---|
| `nc:upload_time` | `file.modified_at` | `file.created_at` |
| `nc:creation_time` | `file.created_at` | `COALESCE(captured_at, content_modified_at)` |

Purely presentational — no client acts on either property, so there is no
resync. Strictly an improvement on today: EXIF capture dates start reaching
NextCloud clients, which they currently never do.

### Sizing correction — it is not two lines (2026-10-20)

The heading above claims two lines. That holds for one slot only:

- **`nc:upload_time` → `created_at`** is genuinely one line. `FileDto` already
  carries `created_at`, so this half can ship on its own, today, and is a
  strict improvement: the slot currently reports the content clock.
- **`nc:creation_time` → `COALESCE(captured_at, content_modified_at)`** cannot.
  `captured_at` lives in `storage.file_metadata` and is **not available at the
  emission site** — `FileDto` has only `created_at` and `modified_at`, and the
  NC PROPFIND path never joins that table. It needs either a join in the NC
  query or a new field threaded onto the DTO.

That plumbing is independent of the metadata re-keying: it is needed whatever
the table is named or keyed by. So the re-keying work neither blocks nor
unblocks §0 — and conversely, waiting for it buys §0 nothing.

**§0 is also not blocked by the NextCloud inbound test.** That test gates §4,
which is about whether a *client-asserted* column must exist. §0 emits
outbound and reads columns that already exist today.

§3 has already landed, so write the `content_modified_at` form directly — the
`created_at` fallback this phase originally called for no longer has a reason to
exist. It must also match `media_sort_date`'s chain exactly
(`20261103000007_media_sort_date_content_clock.sql`), so the displayed creation
date and the timeline order stay the same expression; §4 inserts
`source_created_at` into both together.

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

## §1 — Add `content_modified_at`, write it, read nothing **IN PROGRESS** (`625f089d`)

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

#### Coverage as landed, 2026-10-20 — and one path that does not exist

| path | status |
|---|---|
| WebDAV `PUT` replacing the blob | ✅ `nc_webdav_client_mtime` Part B2 |
| chunked upload completion | ✅ `nc_webdav_client_mtime` Part E (the NC assemble surface) |
| delta commit | ⚠️ **not coverable in Hurl** — needs real CDC chunk hashes. Exercised by `rt_bus_check.sh` S21 only |
| `/api/files/upload` **overwrite** | ❌ **misdescribed in this plan.** That endpoint does not overwrite: `upload_file_streaming` calls `save_file_with_blob` directly — a plain INSERT, no existence check. It is a CREATE path, and it stamps `NOW()` by passing `None` |
| the five metadata mutations | ✅ rename in `nc_webdav_client_mtime` Part B; favourite / grant / move / trash+restore in `content_clock_metadata_mutations` |

Two honest caveats on the ticks. The REST chunked endpoint
(`/api/uploads/{id}/complete`) cannot carry a mtime at all — it routes through
`upload_file_streaming_with_perms`, which takes no such parameter — so "chunked
upload completion" means the NC assemble path specifically. And the second half
of the metadata-mutation requirement, *"while still bumping `updated_at`"*, is
**not observable over HTTP**: `updated_at` is absent from every DTO by design,
so the tests prove the content clock held and cannot prove ctime moved.

**Verification — this is the phase where the tests earn their keep**, because
they are what makes §2 safe to ship:

- after each of the four paths, the column holds the expected instant
- `X-OC-Mtime` is honoured when present, ignored when out of bounds (400)
- rename, move, trash, grant and favourite leave the column **untouched** while
  still bumping `updated_at`

---

## §2 — Repoint the alias and the ETag **DONE** (`8d0631be`, `2def1acb`)

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

### Folder side — DAV-only, done (`2def1acb`)

Two mechanisms were on the table, with very different blast radius:

- **Repoint the `fld.`/`fo.` aliases.** One-line change, but `modified_at` is
  shared, so REST `FolderDto.modified_at` moves too — and a folder's "Modified"
  column in the SPA would then change whenever *any descendant* changes. Folders
  would bubble to the top of a date sort on every edit inside them. Arguably
  Finder-like, arguably surprising; either way a visible product change.
- **Change the DAV emission sites** to read `tree_modified_at` directly.
  DAV-only, REST untouched — but needs `tree_modified_at` reachable at those
  callsites, which it is on the entity and was *not* on `FolderDto`.

**Decided 2026-10-17: the second.** The decision was about protocol conformance,
not about changing what the SPA shows, so REST keeps reporting the folder row's
own mtime and the two clocks coexist on the wire with doc comments saying which
is which.

Shipped as:

- `FolderDto` gains `tree_modified_at: DateTime<Utc>` — a wire-visible addition,
  so `resources/gen/openapi.json` and the TS SDK were regenerated in the same
  commit (additive: one new required field on a response type, nothing the FE
  constructs).
- Three emission sites repointed: `webdav_adapter.rs` (both folder sites) and
  `nextcloud/webdav_handler.rs`.
- Folder ETag was *already* derived from the rollup, so ETag and
  `getlastmodified` now agree by construction rather than by accident.

Twelve hand-built `FolderDto` sites have no rollup in scope and fall back to the
row's own mtime, each commented. Nine are REST-only listings (favorites,
recents, search, trash, share-recipient, plus a test stub) where nothing emits
DAV, so the fallback is invisible. Three are genuinely rollup-less:

| Site | Why no rollup | Consequence |
| --- | --- | --- |
| Synthetic DAV root (`api/handlers/webdav_handler.rs`) | No row exists; all three clocks are `now()` | None — unchanged from today |
| External mounts (`mount_dto.rs`, `folder_handler.rs`) | Backend gives one directory mtime, no subtree view | Out of scope per § Out of scope; DAV reports what it always did |
| NC REPORT search (`nextcloud/report_handler.rs`) | Search projection doesn't select it | **Load-bearing fallback** — a REPORT hit still reports the folder's own mtime. Fixing it means widening the search projection; see Open decisions |

`path_resolver_service.rs` already selected `tree_modified_at` for the folder
ETag, so it passes the real value.

The folder wire-shape test (`webdav_adapter::wire_shape_tests`) now sets the two
clocks to *different* values, so repointing the emitter back to `modified_at`
fails the test instead of silently regressing conformance.

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

## §3 — Sort axes **DONE** (interim two-arm form)

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

#### Photos ordering *does* shift at deploy — corrected 2026-10-17

An earlier draft of this section claimed the deploy was inert, by analogy with
§2. **That was wrong**, and the error is worth recording because it inverts the
rollout story.

The live fallback is `created_at`, not `updated_at`:

```sql
-- migrations/20260307000000_initial_schema.sql:689
SET media_sort_date = COALESCE(NEW.captured_at, created_at)
```

`media_sort_date` has exactly two writers — that trigger, and the column's own
`DEFAULT CURRENT_TIMESTAMP` at insert. Nothing in `src/` writes it; the only
Rust reference is `PhotoOrderBy::CapturedAt => "media_sort_date"`. So today a
non-EXIF image sorts by **upload time**, permanently, no matter how often its
content changes afterwards.

Repointing the chain at `content_modified_at` therefore moves every EXIF-less
media file that has been modified since upload. That is the intended
improvement, not a regression — but it is a visible one-time reorder of the
Photos timeline, so it needs a release note and the backfill has to run in the
migration rather than being left to arrive lazily. Leaving it lazy would be
worse than either end state: the timeline would mix old rows on upload time with
touched rows on content time, which is not an ordering at all.

Files *with* EXIF `captured_at` do not move — the first arm wins and is
unchanged.

**Measured 2026-10-17** by running §1's and §3's migrations against the dev
database inside a transaction and rolling back (4,916 files):

| statement | rows written |
| --- | --- |
| §1 backfill `content_modified_at = updated_at` | 4,916 (all) |
| §1 `SET NOT NULL` + `CHECK (> 2000-01-01Z)` | passed — no pre-2000 `updated_at` |
| §3 arm 1 (files with a `file_metadata` row) | 15 |
| §3 arm 2 (files with none) | 4,665 |

So **95% of rows move** on §3's backfill. That is not a sign of trouble — it is
the measure of how far `media_sort_date` had drifted from the content clock
while pinned to upload time — but it does confirm the reorder is large enough to
need a release note rather than a changelog line. Both migrations are otherwise
clean against the real schema and triggers.

#### The backfill and the statement triggers

Same trap as §1, with a better outcome. `storage.files` carries two
`FOR EACH STATEMENT` triggers with transition tables, and both filter
*in-function* on a column tuple:

| Trigger | Compares | Contains `media_sort_date`? |
| --- | --- | --- |
| `files_bump_tree_etag_upd` | `name, folder_id, blob_hash, size, mime_type, is_trashed, updated_at` | no |
| `files_enqueue_search_upd` | `name, blob_hash, is_trashed` | no |

So the backfill is **semantically** inert — neither trigger's `changed` CTE
yields a row, no tree ETag bumps, nothing is enqueued for reindex. But the
filtering happens *after* PostgreSQL has materialised both transition tables
over every updated row, which is the actual cost. Suppress both around the
`UPDATE`, exactly as §1's migration does, and for the same reason.

Either way the **UI axis binds to the `media_sort_date` column, never to the
expression.** It is denormalised precisely so the Photos timeline can seek:
`idx_files_media_timeline_by_drive (drive_id, media_sort_date DESC)` lets
`LIMIT` stop the scan early. Re-deriving the COALESCE at query time would need a
join to `file_metadata` for `captured_at`, could not use that index, and would
turn the timeline into a scan.

#### Two triggers, not one trigger with two conditions

Today `trg_sync_media_sort_date` fires only on `file_metadata.captured_at`
(`migrations/20260307000000_initial_schema.sql:697`). The chain now has a second
input on a *different table*, so this is a second trigger rather than a widened
one:

- **Existing, on `file_metadata`** — keep as `AFTER INSERT OR UPDATE OF
  captured_at`, but change its body's fallback from `created_at` to
  `content_modified_at`.
- **New, on `storage.files`** — `BEFORE UPDATE OF content_modified_at`,
  `FOR EACH ROW`, with `WHEN (NEW.content_modified_at IS DISTINCT FROM
  OLD.content_modified_at)`, assigning `NEW.media_sort_date` in place.

`BEFORE` + assign-to-`NEW` matters, and is not a style preference:

- It writes no second statement, so it cannot re-enter the statement triggers.
  The existing `file_metadata` trigger relies on `pg_trigger_depth() > 1` to
  dodge the tree-ETag bump (`20260626000000:93`); the `BEFORE` form never
  creates the depth to dodge.
- It costs one index lookup into `file_metadata` per content mutation to read
  `captured_at`. Content mutations are rare next to reads, and the alternative —
  recomputing the COALESCE at query time — was already rejected above because it
  defeats `idx_files_media_timeline_by_drive`.

`INSERT` needs no trigger at all: `content_modified_at` defaults to `NOW()` and
`media_sort_date` defaults to `CURRENT_TIMESTAMP`, so a fresh row already
satisfies the chain. A later EXIF extraction fires the `file_metadata` trigger
and overwrites with `captured_at`, which is the first arm and correctly wins.

| query | resolves to |
|---|---|
| `?order_by=captured_at` | `media_sort_date` |
| `?order_by=updated_at` | `content_modified_at` |
| `?order_by=created_at` | `created_at` — unchanged, admin-facing |

`nc:creation_time` moves to the same chain as `media_sort_date` —
`COALESCE(captured_at, content_modified_at)` — so the displayed creation date
and the timeline order stay the same expression at every phase. §4 adds the
`source_created_at` arm to both together.

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

### Reframed 2026-10-19 — `captured_at` may already BE this column

Ed's observation, reviewing the `nc:creation_time` outbound chain: `captured_at`
need not mean "EXIF capture date". It can mean **the creation date the bytes
themselves claim** — EXIF `DateTimeOriginal` for images, `dcterms:created` in
OOXML `docProps/core.xml`, `/CreationDate` in a PDF. One column, one meaning:
*server-extracted creation date, from the file's own metadata, whatever the
format*.

**This is the logical conclusion of §4's own preference.** The section above
already argued candidate 2 (server-side extraction) was the stronger option
because "it makes the value server-extracted and so removes it from the
client-asserted class entirely". If the writer is server-side extraction, the
value belongs in the column that already holds server-extracted dates. A second
column only earns its place if a *client-asserted* writer exists.

So §4 stops being "add a column" and becomes "widen the extractor":

- `source_created_at` is **not created**. The four-clocks table loses a row.
- §3's chain `COALESCE(captured_at, content_modified_at)` stops being the
  "interim two-arm form" and becomes **final** — there is no middle arm to
  insert later.
- `nc:creation_time` (§0) emits that same two-arm chain and needs no revisit
  when extraction widens; new formats simply start populating the first arm.
- The dates plan loses its only BLOCKED phase.

**Audio is explicitly out of scope** (Ed, 2026-10-19). `AudioMetadataService`
already extracts a `year` — an integer, into its own table, not a date and not
`captured_at` — and audio tagging is a different problem from a file's creation
date. Do not fold it into this chain; synthesising a date from a year would
invent a January 1st that no file claims.

Mechanically this is an extension, not a rewrite:
`MediaMetadataService::handles(mime)` gates on `image/*` | `video/*` and
`extract_blocking` dispatches from one place, so a `.docx`/PDF branch slots in
beside the existing two. `lopdf` and `pdf_extract` are already in the tree
(`search_index/text_extractor.rs` uses them for search text), so the parsing
capability exists — only date extraction is missing.

**It needs no re-key.** `file_metadata` stays `file_id`-keyed; only the
*meaning* of one column widens. (Re-keying that table to the content hash is
owned by separate work — do not pick it up here.)

#### The three honest costs

1. **The name becomes a misnomer.** A `.docx` is not "captured". The plan's own
   rule for `updated_at` applies — *do not rename the column; one rule beats
   one rename* — so document the broadened meaning instead. `file_metadata`
   and `MediaMetadataService` acquire the same mild drift: both say "media"
   while holding a little more.
2. **The trust-class separation is lost.** `docs/architecture/dates.md` calls
   the split deliberate: `captured_at` comes out of the bytes and is
   trustworthy, `source_created_at` is whatever the client says, so the trust
   bounds apply to exactly one of them and EXIF re-extraction can never
   clobber a client-supplied value. Folding them is only safe while **no
   client-asserted path exists** — which is true today, and no browser can
   ever supply one.
3. **Media-shaped neighbours.** A `.docx` row would populate `captured_at` and
   leave `camera_make`, `orientation`, `latitude`, `width`, `height` NULL.
   Mechanically fine (all nullable), but the table's shape stops matching its
   name.

#### The decision is conditional on Ed's NextCloud test

The same test that gates §0 now decides §4's shape:

| finding | §4 becomes |
|---|---|
| **no** client sends `nc:creation_time` inbound | adopt this reframing — widen the extractor, drop `source_created_at`, §3's chain is final |
| some client **does** send it | keep both columns — the trust classes genuinely differ, and cost 2 above is a real loss rather than a theoretical one |

Recommendation: adopt the reframing unless the test finds an inbound writer.
It removes a column, a COALESCE arm, and the plan's only blocked phase, and it
leaves one concept — "the date the bytes claim" — instead of two that differ
only by provenance.

#### The keying asymmetry, and what it implies for the name

Worked out 2026-10-19/20, and it is the input that settles the naming — so
it is recorded before the candidates below, which were argued without it.

**The two clocks are not keyed the same way:**

| | keyed by | because |
|---|---|---|
| `files.content_modified_at` | the **file row** | a copy gets `NOW()`; two files sharing identical bytes legitimately differ |
| a byte-derived creation date | the **content** | the same bytes always claim the same creation date, whoever copied them |

So they are not the symmetric pair `content_created_at` / `content_modified_at`
suggests, and a name implying that symmetry would advertise a guarantee that
does not hold.

**This also tells us where the column belongs.** Separate work is moving
`file_metadata` to blob/content keying (Ed, 2026-10-19). That makes it exactly
the right home for a byte-derived date — not merely a convenient one — and the
copy gap recorded above closes for free, since a copy shares the blob and
therefore shares the metadata row.

**And once the table is content-keyed, it should say so** (Ed's point): with
the table named `storage.content_metadata`, the qualifier is carried by the
table and the column needs no prefix at all:

```
storage.content_metadata            -- keyed by blob_hash
  created_at    -- the date the bytes claim      (was captured_at)
  extracted_at  -- when extraction last ran      (was created_at)
  camera_make, orientation, latitude, width, …
```

That also fixes a misnomer already present: `file_metadata.created_at` means
*when the extraction row was inserted*, which is why `created_at` looked
unavailable for the real creation date. `extracted_at` is what it is — and it
becomes load-bearing once extraction is re-runnable, because "when did we last
extract this" is a question a recoverable job has to answer.

**None of this is this plan's work.** It is input for whoever owns the
re-keying. The dates plan's only stake is that it stops needing a naming
decision at all: if the column ends up `content_metadata.created_at`, §4 reads
"widen the extractor", and nothing in §0–§8 changes.

#### Then `captured_at` is the wrong name

Asked directly, 2026-10-19: no. A `.docx` is not *captured*; the word is photo
vocabulary and the column would outlive its origin. Three candidates:

| name | for | against |
|---|---|---|
| **`content_created_at`** | symmetric with `content_modified_at` — "the content's own two clocks", one created, one modified. Reads correctly for every format | lives on `file_metadata` while its twin lives on `storage.files`, so the symmetry is in the name but not the location |
| `source_created_at` | semantically exact, and the name is **free** once §4 declines to create it | carries the plan's history as the *client-asserted* column; reusing it for a server-extracted one invites a wrong reading of older commits |
| `captured_at` (keep) | zero churn; the `updated_at` precedent says document the meaning rather than rename | the misnomer compounds — the column is young, and this is the cheapest moment it will ever be to rename |

`content_created_at` is the better pair, and the location asymmetry is a real
argument for putting it on `storage.files` instead — which is where §4
originally wanted `source_created_at`, for exactly this reason ("a source
creation date applies to any file"). That decision is **not** taken here:
`file_metadata` is owned by separate work, and moving a column out of it is
that work's business, not this plan's.

Note the rename is also a **wire** rename — `captured_at` is a field on
`PhotoResourceItemDto`, so it belongs with §7's naming normalisation rather
than being smuggled in.

#### Yes — and per-format extraction is where the cost actually is

Generalising the *meaning* is a doc change. Generalising the *extractor* is the
work, and it is not small: each format needs its own parser reading its own
metadata container — EXIF for images, `nom-exif` for video, zip + XML for
OOXML `docProps/core.xml`, the trailer dictionary for PDF `/CreationDate`,
`meta.xml` for ODF.

**The dispatch already exists and must be reused, not rebuilt.**
`search_index::text_extractor::classify(name, mime)` already resolves exactly
these formats — `Pdf`, `Docx`, `Xlsx`, `Pptx`, `Odf`, `Plain` — by mime with an
extension fallback, and `supports()` lets the worker skip I/O for the rest. A
second format registry beside it would be the duplication AGENTS.md forbids;
the date extractor should share that classifier.

**And it inherits that module's hard-won resilience discipline.** Parsing
untrusted documents to read one date is an attack surface, and this codebase has
already been bitten: `extract_pdf` wraps `pdf_extract::extract_text_from_mem` in
`catch_unwind` because a malformed PDF *panics*, pinned by both a unit test
(`corrupt_pdf_fails_terminally_instead_of_panicking`) and an integration test
(`broken_pdf_resilience.hurl`). Any date extractor needs the same treatment plus
size and time bounds — a zip bomb in `docProps` must not become a worker stall.
(Related: `panic = "abort"` must stay off release builds, or `catch_unwind`
stops working at all.)

So the honest sizing: the semantic reframing is nearly free and strictly
simplifying; the extraction work is a separate, security-sensitive piece that
should be scoped on its own rather than folded into a dates phase.

#### Where it should live — not inside `text_extractor`

Asked 2026-10-19 whether `text_extractor` should become
`metadata_extractor`. **No, on both counts.**

The name is accurate today: it extracts *text*, for the search index, and it
sits in `search_index/` alongside `content_index_worker` and
`tantivy_content_index` — placement that correctly says "this is a search
concern". Renaming it fails either way: if it keeps extracting only text the
name lies, and if it grows date extraction it conflates two concerns with
different cost profiles and different callers.

| | text | creation date |
|---|---|---|
| reads | the whole blob, document body | a PDF trailer dict, or OOXML `docProps/core.xml` — one small zip entry |
| consumer | Tantivy index | `media_sort_date`, `nc:creation_time` |
| timing | background queue (`content_index_worker`) | at or near upload |

Only the **format question** is shared, and today it is unreachable:
`enum Kind` and `fn classify` are private; just `supports` and `extract` are
`pub`.

So the move is to hoist the classifier out — out of `text_extractor` *and* out
of `search_index/`, since a creation date is not a search concern. Something
like `infrastructure/services/document_format.rs` owning `Kind`, `classify`
and `supports`, with `text_extractor` and a new date extractor both depending
on it. One format registry, two consumers, neither pretending to be the other.

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

## §6 — UI axes **DONE** (2026-10-19)

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

Server side: accept `modified_at` everywhere.

**No deprecation window — decided 2026-10-19.** The original plan called for
warn-now/remove-next-major, matching the contradictory-auth-config pattern. Ed
overruled it: the API is recent enough that no third-party client plausibly
sends `order_by`, and the only consumer is the SPA, which ships *with* the
server. A window exists to protect clients that upgrade independently; with
none, it is pure cost — and removing it deleted code rather than adding a
normaliser plus a warn plus a removal task.

Both acceptance points reject the old token outright now:

- `PhotoOrderBy` keeps its derived `Deserialize`; the variant is `ModifiedAt`
  and `rename_all = "snake_case"` makes the wire value `modified_at`. serde's
  own error names the accepted values, so a stale client gets a 400 that says
  what to send — which is what §7 wants from every sort axis anyway.
- Search normalises nothing: `to_criteria` passes the token through and the
  four downstream matches (two SQL `order_column`s, `sort_enriched_files`, the
  folder in-memory sort) all read `"modified_at"`.

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

## §8 — Give folders a content clock **DONE** (2026-10-18)

### Why this exists

§2 left the two resource types reporting `modified_at` from different kinds of
clock:

| | `modified_at` means | moves on a rename of itself? |
|---|---|---|
| file | content clock (`content_modified_at`) | no |
| folder | row clock (`updated_at`) | **yes** |

So a mixed `/files` listing sorted by "Modified" sorts the two halves on
different semantics. That is not a data bug — every alias reads the correct
column for its own table — but it is an inconsistency a user notices, and it
reads as a defect.

### Why not the obvious fix

The tempting answer is to point folder `modified_at` at `tree_modified_at`,
which already exists. **Rejected.** `tree_modified_at` is a *full recursive*
rollup: `bump_tree_from_files_stmt_upd` walks `lpath @>` to bump every ancestor
to the root, and its change tuple includes `blob_hash` and `size`, so a content
edit five levels down moves the top-level folder.

That is correct for its actual job — invalidating a DAV collection ETag — and
wrong for a human-readable column, because it **saturates**. Every top-level
folder shows "just now" as soon as anyone touches anything beneath it. In a
listing of `Documents` / `Photos` / `Projects` all three show today's date,
every day, and the column stops discriminating — which is the entire reason to
sort by it. The failure gets worse the more collaborative the instance, and
collaboration is this product's main axis.

No reference implementation does it either. POSIX and NTFS both change a
directory's timestamp only when its **direct** entries change — editing a file's
contents does not touch the containing directory, and nothing propagates upward.

### The design

Adopt POSIX semantics, as a real column.

```sql
ALTER TABLE storage.folders
    ADD COLUMN content_modified_at TIMESTAMPTZ;
```

**Name it `content_modified_at`, matching `storage.files`.** This is the part
worth getting right. A folder's *content* is its entry list — that is exactly
what POSIX means by a directory mtime — so the two tables are expressing the
same concept, each realised for its own type. Three consequences:

1. The DTO asymmetry disappears. `modified_at` means "when did this resource's
   own content last change" for both types, with no per-type caveat.
2. Every read becomes `<alias>.content_modified_at AS modified_at`, uniformly.
   That kills the bug class that shipped in `8d0631be` and was caught by
   `webdav_permissions.hurl`: the column to select depended on which table the
   alias happened to bind to, so pattern-matching on the alias silently
   produced a column that did not exist.
3. The `modified_at` keyset predicate goes back to one column name for both
   arms, undoing the per-arm split that fix required.

**What bumps it** — direct entries only, no recursion, not content:

| event | bumps |
|---|---|
| file or folder created in F | F |
| file or folder deleted from F | F |
| direct child renamed | F |
| direct child moved F → G | **both** F and G |
| direct child's bytes change | **nothing** — POSIX does not bump a directory for a content edit |
| grandchild anything | **nothing** |

Statement-level triggers with transition tables, mirroring the existing
`files_bump_tree_etag_*` set, but bumping `folder_id` / `parent_id` directly
instead of walking `lpath`. Strictly cheaper than the recursive trigger: one
join to the parent, no ancestor set.

**Backfill from `updated_at`**, so the deploy is inert exactly as §1's was:
`updated_at` is what the wire reports for folders today, so every folder's
reported `modified_at` is byte-identical before and after. Divergence begins
with the first real entry change afterwards. Then `SET NOT NULL` in the same
transaction, as §1 does, so no read ever needs a `COALESCE`.

**Reads to repoint** — the folder-side aliases §2 deliberately left alone:
`fld.updated_at AS modified_at` (`trash_db_repository.rs:369`,
`favorites_pg_repository.rs:376`, `recent_items_pg_repository.rs:255`,
`favorites_pg_repository.rs:42`'s second COALESCE arm),
`f.updated_at AS modified_at` (`folder_db_repository.rs:1751`),
`fo.updated_at` (`path_resolver_service.rs:104`), the folder move RETURNING
(`folder_db_repository.rs:824`), and the keyset `mk("f", "updated_at")`.

### Open sub-decision — does DAV follow?

§2 shipped folder `D:getlastmodified` reading `tree_modified_at`, pinned by a
test. §8 does **not** silently change that.

There is a real argument that it should follow: a `PROPFIND Depth: 1` lists
exactly the direct children, so a direct-children clock describes what was
returned, and POSIX agreement is worth something. The argument against is that
sync clients use the collection ETag — not `getlastmodified` — to decide whether
to descend, and that ETag stays on the recursive rollup regardless, so changing
`getlastmodified` buys conformance tidiness and nothing functional.

Recommend **leaving DAV on `tree_modified_at`** and revisiting only if a client
is observed acting on a collection's `getlastmodified`. Flagged rather than
decided.

### Not in scope

`tree_modified_at` is untouched — it keeps feeding the collection ETag, which is
the job it was built for. §8 adds a clock, it does not replace one.

### What shipped, and what the triggers were verified to do

Migration `20261103000008_folders_content_modified_at.sql`, plus 14 folder-side
reads repointed (9 positional `EXTRACT(EPOCH FROM fo.updated_at)` in
`folder_db_repository`, the folder-move `RETURNING`, three `fld.… AS
modified_at` aliases, the favorites COALESCE's folder arm, and
`path_resolver_service`). The keyset predicate went back to a single
parameterised column, undoing the per-arm split §2's bugfix had needed.

Two implementation facts worth keeping, both found by running the migration
against the dev database inside a transaction and rolling back:

- **`NOT NULL` must be declared at `ADD COLUMN`, not tightened afterwards.**
  The files migration tightens after its backfill; the same shape fails here
  with `cannot ALTER TABLE "folders" because it has pending trigger events`,
  because `storage.folders` carries AFTER **row** triggers that queue events
  for the rest of the transaction. `storage.files` has only statement
  triggers.
- **The `pg_trigger_depth()` guard is mandatory, and the reasoning that said
  otherwise was wrong.** The first draft omitted it, arguing the self-fire
  would find an empty change set and stop. It does not: a `FOR EACH STATEMENT`
  trigger fires for a statement matching **zero rows**, so the empty UPDATE
  re-fires the trigger forever — `stack depth limit exceeded` on the first
  run.

Behaviour confirmed in that same rolled-back transaction:

| event | parent | grandparent |
|---|---|---|
| child's `size` changes (content edit) | not bumped | not bumped |
| child renamed | **bumped** | not bumped |
| child moved parent → grandparent | **bumped** (source) | **bumped** (destination) |

---

## §9 — Preserve the browser's mtime on upload **NOT STARTED**

The plan already contains a Playwright spec asserting that a dragged file keeps
its source mtime (§ E2E — drag-and-drop preserves the source mtime). That spec
assumes a capability **that does not exist**, which is why it was never
runnable: no section ever specified building it. This is that section.

Today a NextCloud desktop client preserves mtime and a browser does not — a
drag-and-drop upload silently stamps `NOW()`.

### What is already done

- `content_modified_at` accepts a client value on **both** the create and
  overwrite paths (`save_file_with_blob` takes it, `swap_blob_hash` already
  did), so the repository layer needs nothing.
- `validate_client_mtime` enforces the trust bounds and returns a clean 400.
- `millis_to_seconds_rounded` converts the browser's milliseconds half-up.

### The upload surface is three paths, not one

An earlier draft of this section said "add a field to `/api/files/upload`".
That is **wrong and would have shipped a half-working feature**: the SPA picks
its upload path by file size, so mtime would have survived for small files and
been silently dropped for large ones — the exact path-dependent capability this
section warns about two paragraphs later.

| SPA path | chosen when | server entry point |
|---|---|---|
| multipart `POST /api/files/upload` | below `DELTA_UPLOAD_MIN_SIZE` | `upload_file_streaming` |
| **delta** `/api/files/delta/{negotiate,chunks,commit}` | **at or above `DELTA_UPLOAD_MIN_SIZE`** | create → `upload_file_streaming`<br>update → `update_file_content_by_id_with_perms` |
| chunked `/api/uploads/{id}` + `/complete` | chunked clients | `upload_file_streaming_with_perms` |

One piece of luck: **delta's create branch calls the same
`upload_file_streaming`** as the multipart endpoint
(`delta_upload_service::register_row`), so a single parameter there covers both
of the common cases. Genuinely separate are delta's *update* branch and the
chunked path.

Four service entry points exist in total; the two NC ones
(`update_file_streaming_with_perms`) already thread a client mtime, which is
why the NextCloud surfaces work today.

### What is missing

1. **A mtime slot on each REST upload path.** Multipart field
   `source_modified_at` on `/api/files/upload`, and an equivalent field on the
   **delta commit** body — carrying **epoch milliseconds** (`File.lastModified`
   verbatim; convert server-side so only one place knows the rounding rule).
   On the multipart path it must arrive **before** the `file` part, which
   consumes the stream — the same ordering caveat `folder_id` already carries.
   Round with `millis_to_seconds_rounded`, then `validate_client_mtime`, then
   pass down. `upload_file_streaming` currently hard-codes `None`.

2. **The frontend sending it on both paths.** The two UI entry points converge
   before the size decision, which helps:

   ```
   file input (modal) ─┐
   drag & drop        ─┴→ uploadBatch → uploadOneFile ─┬→ uploadFileWithProgress  (small)
                                                       └→ deltaUpload            (large)
   ```

   `input.files` and `dataTransfer.files` both yield real `File` objects, so
   `lastModified` is available on either UI path — the drag path loses nothing.
   But the branch below means **two** request shapes to change:
   `lib/api/endpoints/files.ts` (which itself has two builders, `uploadFile`
   and `uploadFileWithProgress`) and the delta commit body in
   `lib/api/endpoints/deltaUpload.ts`.

   Miss any one of these and the capability becomes size-dependent, which no
   single-file test would notice. A test that uploads one small file passes
   while every large upload silently loses its mtime.

### Why milliseconds on the wire, not seconds

`X-OC-Mtime` is seconds because ownCloud defined it that way; our own field has
no such constraint. Sending milliseconds keeps the rounding rule in exactly one
place (the server) instead of asking every client to implement half-up
correctly — and truncation on the client is invisible to us.

### Tests

The Playwright spec already written up above becomes runnable. Note it has
**never run** — it was written against a capability that did not exist, so
treat it as unvalidated until it passes once.

Add api-tests too, and **one per path is not optional** given the size branch:

- a multipart upload with `source_modified_at` set to a past instant,
- a delta commit carrying the same field,

each asserting the stored `modified_at` is that instant and not the upload
moment. Same shape as `nc_webdav_client_mtime.hurl`, which is the NC-surface
equivalent and already green.

The Playwright side needs **two** files for the same reason — one below
`DELTA_UPLOAD_MIN_SIZE` and one above it. A single small fixture would pass
against an implementation that forgot delta entirely.

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

**And the trigger trap on that `UPDATE`, which is where a bulk migration
actually blows up.** `storage.files` carries two statement-level `AFTER UPDATE`
triggers with transition tables:

| trigger | compares |
|---|---|
| `files_bump_tree_etag_upd` | `(name, folder_id, blob_hash, size, mime_type, is_trashed, updated_at)` |
| `files_enqueue_search_upd` | `(name, blob_hash, is_trashed)` |

`content_modified_at` is in neither tuple, so neither would enqueue anything —
no tree-etag storm, no full Tantivy reindex. That is lucky rather than designed:
had either keyed on "any update", this migration would have queued the entire
table for reindexing.

But both declare `REFERENCING OLD TABLE … NEW TABLE … FOR EACH STATEMENT`, and
Postgres materialises those transition tables for the **whole statement before
the function filters anything** — so one bulk `UPDATE` builds two full copies of
the table, twice over, spilling to temp files, to conclude there is nothing to
do. That is a per-statement space cost scaling with row count, not an
algorithmic one, which is exactly how an `O(n)` migration becomes an OOM.

The migration therefore disables both triggers around the `UPDATE` and
re-enables them after. Precise rather than a shortcut — the column tuples above
prove neither trigger had an action to take — and DDL is transactional in
Postgres, so a failed migration rolls the triggers back with everything else.

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

### Open: NC REPORT search emits the wrong folder `getlastmodified`

§2's folder side left one real gap. `nextcloud/report_handler.rs` renders DAV
responses from search results, and the search projection doesn't select
`tree_modified_at`, so a folder hit in a NC REPORT still reports the folder
row's own mtime while the same folder reached by PROPFIND reports the rollup.
The same URL, two answers, depending on how the client asked.

Nothing is *more* wrong than before this work — the fallback preserves today's
behaviour exactly — but the inconsistency is new and worth closing. Fixing it
means widening `SearchFolderResultDto` and the query behind it to carry the
rollup, which touches the Tantivy-backed search path rather than the dates work
proper. Deliberately not bundled into §2.

Severity is low: NC desktop clients use REPORT for search results they display,
not for sync decisions, and the ETag — which *is* load-bearing — is correct on
both paths.

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

This brings the **folder side into §2's scope** after all. Note the asymmetry —
folders get no new column, because `tree_modified_at` already *is* the
subtree-content clock; the work is pointing the property at it.

> **Superseded 2026-10-17 on mechanism.** This entry assumed the `fo.`/`fld.`
> SQL aliases would be repointed. They were not: that would have moved REST
> `FolderDto.modified_at` too, which is a product change nobody asked for. The
> DAV emission sites read a new `FolderDto.tree_modified_at` instead. The
> *decision* above stands; see § Folder side for what shipped.

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
