# Dates and Timestamps

A single uploaded file carries up to four different notions of "when", and
every surface that touches it expects a different one. A NextCloud sync client
asks "did the bytes change?"; the Photos timeline asks "when was this taken?";
an audit query asks "when did this row last move?". Answering all of them from
one column is how a file that was merely renamed gets re-downloaded by every
client that had it.

This document defines which column means what, which wire slot each one feeds,
which of them a client actually *acts* on, and how far a client-supplied value
is trusted.

> **Status.** Columns marked **new** are agreed (2026-10-11) but not yet
> implemented; everything else is shipped. The current tree also has four
> mis-wirings that predate this model — see
> [§ Known divergences](#known-divergences). The migration path is
> [`docs/plan/normalize-file-dates.md`](/plan/normalize-file-dates).

---

## The four clocks

| Meaning | Column | Trust | Written by |
|---|---|---|---|
| Server received it (`btime`-ish) | `storage.files.created_at` | server-stamped, immutable | row insert |
| Bytes last changed (`mtime`) | `storage.files.content_modified_at` **new** | client-asserted, bounded | content-mutation paths only |
| Captured on the device | `storage.file_metadata.captured_at` | server-extracted | EXIF `DateTimeOriginal` |
| Created at the source (pre-upload `btime`) | `storage.files.source_created_at` **planned** | client-asserted, bounded | nothing yet — see below |
| Any row touch (`ctime`) | `storage.files.updated_at` | server-stamped, never client-assertable | every mutation through a repository write path: content **and** metadata |
| Entry list last changed (directory `mtime`) | `storage.folders.content_modified_at` **new** | server-stamped | direct-child add/remove/rename triggers |
| Subtree changed | `storage.folders.tree_modified_at` | server-stamped | `tree_etag_dirty` queue |

`content_modified_at` is `NOT NULL` and read directly — no fallback expression
in the ETag or the hot read path. Rows predating the column are backfilled from
`updated_at`, which is exactly what every surface reported before it existed, so
introducing it changes nothing a client can observe. Those historical values are
therefore ctime-flavoured — a file renamed in 2024 carries that rename time —
and that is accepted rather than fixable: the true value is unrecoverable, and
`updated_at` is wrong-but-stable where `created_at` would have been
wrong-and-disruptive. Correctness begins at the first content mutation after the
column ships. See `docs/plan/normalize-file-dates.md` §1.

The split between the two client-asserted columns is deliberate: `captured_at`
comes out of the bytes and is therefore trustworthy, while `source_created_at`
is whatever the client says. Keeping them apart means the trust bounds apply to
exactly one of them, and re-running EXIF extraction can never clobber a
client-supplied value or vice versa.

**`source_created_at` has no writer and does not exist yet.** The column is
unbuilt and the blocker is choosing who writes it, not a NextCloud answer:
`nc:creation_time` is only ever *emitted* today, there is no inbound path, and
**no browser can supply one** — the File API exposes `lastModified` (an mtime)
and no `btime` on any platform. The two candidates are a NextCloud client that
sends a creation time on upload, or server-side extraction from the bytes
(OOXML `docProps/core.xml` → `dcterms:created`, PDF `/CreationDate`). The
second is the stronger option: it makes the value server-extracted and so
removes it from the client-asserted class entirely. Until one lands, the column
is NULL and the COALESCE chains below simply fall through it. See
`docs/plan/normalize-file-dates.md` §4.

### What exactly bumps `updated_at`

"Every mutation" is imprecise in both directions, so to be exact.

**It is a strict superset of the content clock.** Every content mutation bumps
*both* columns; every metadata mutation bumps `updated_at` alone. That
asymmetry is the entire point of the split:

| Operation | `updated_at` | `content_modified_at` |
|---|---|---|
| PUT / chunked complete / delta commit / collab flush | ✅ | ✅ |
| rename, move, trash, restore, grant, favourite | ✅ | — |

**But it is not literally every DB write.** Three classes of write touch a row
and leave `updated_at` alone, by design:

- **Trigger-maintained denormalised columns** — `media_sort_date` is
  recomputed by `trg_sync_media_sort_date` and
  `trg_sync_media_sort_date_content` without bumping ctime, which is correct:
  nothing about the file changed, only a cached projection of it.
- **One-off backfills** — `drive_id`, `created_by`, `category_order` and the
  two `content_modified_at` backfills are migrations, deliberately invisible
  to ctime so a schema change does not look like user activity.
- **The `oxicloud migrate` CLI's NFC rename** (`cli/migrate.rs`) renames rows
  with a bare `SET name = $1`. Arguably an oversight rather than a decision,
  but harmless: it runs once, offline.

**A client cannot set ctime** (fixed 2026-10-18). `swap_blob_hash` writes the
two clocks from deliberately different expressions:

```sql
updated_at          = NOW(),                              -- always
content_modified_at = COALESCE(to_timestamp($4), NOW()),  -- $4 = X-OC-Mtime
```

Aligning them is a regression, not a tidy-up. Until this was fixed both read
`COALESCE(to_timestamp($4), NOW())`, so an upload carrying `X-OC-Mtime`
backdated ctime as well — a leftover from before §1, when the header had
nowhere to go *but* `updated_at`. §1 added the column it belongs in and set it
without reverting the old one.

The reason it matters: ctime is the one clock a client cannot influence, and
that is exactly what makes it usable for audit, consistency sweeps and
retention. POSIX draws the same line — `utimensat` sets atime and mtime, never
ctime.

### `content_modified_at` means the same thing on both tables

Files and folders each carry a `content_modified_at`, and the name is shared on
purpose: both answer *"when did this resource's own content last change"*. What
differs is what "content" **is**.

| | content is | moves when |
|---|---|---|
| file | its bytes | the bytes change |
| folder | its entry list | a **direct** child is added, removed, renamed, moved in or out, trashed or restored |

A folder's clock explicitly does **not** move when a child's bytes change, and
does not propagate from anything deeper in the subtree. Those are POSIX and
NTFS directory semantics — what `ls -l` shows — and they are the reason this is
a separate column from `tree_modified_at` rather than a reuse of it.

Two consequences worth stating, because both were learned the hard way:

- **`modified_at` on the wire is now symmetric.** Before, `FileDto.modified_at`
  was a content clock while `FolderDto.modified_at` was the row clock, so a
  mixed listing sorted its two halves on different meanings and renaming a
  folder looked like a content change while renaming a file did not.
- **Every read names `content_modified_at` and never aliases it.** When only
  files had the column, the correct column depended on which table an alias
  happened to bind to — and `f.` means *files* in five repositories and
  *folders* in one. That ambiguity shipped a broken folder-move query. Giving
  both tables the same column name removes the choice; not renaming it in an
  alias means `grep content_modified_at` finds every read site, and that a
  query's producer and its consumers cannot drift apart.

### Why `tree_modified_at` is not that column

`tree_modified_at` is a **full recursive** rollup: it walks `lpath @>` to bump
every ancestor to the root, and its change tuple includes `blob_hash` and
`size`, so a content edit five levels down moves the top-level folder. It is
maintained out-of-band through the `tree_etag_dirty` queue rather than
synchronously.

That is right for its one job — invalidating a DAV collection ETag, where
missing a deep change serves stale data — and wrong for a human-readable
column, because it **saturates**. Every top-level folder would read "just now"
every day, and a date sort would stop discriminating, which is the only reason
to offer one. The effect worsens the more collaborative the instance, and
collaboration is this product's main axis.

So the two coexist: `content_modified_at` is what a person reads,
`tree_modified_at` is what a cache validates.

## Roles — who actually reads them

Only two of the six are **behavioural**. Sorting the rest by who reads them is
what pushes both server-stamped columns almost entirely out of the protocol.

| Role | Columns | Consumer, and consequence of being wrong |
|---|---|---|
| **Behavioural** | `files.content_modified_at`, `folders.tree_modified_at` | Sync clients. Decides whether a file re-downloads and whether a folder is descended into — the only timestamps where being wrong changes what transfers. |
| **Presentational** | `folders.content_modified_at` | The "Modified" column for a folder, and its `order_by=modified_at` sort. Deliberately *not* behavioural: no sync client reads it, because a collection's cache validity rides on `tree_modified_at` instead. |
| **Presentational** | `captured_at`, `source_created_at` | Ordering and display: the Photos timeline, `nc:creation_time`. Wrong values look wrong and cost no bandwidth. |
| **Informational** | `created_at` | Emitted as `nc:upload_time` and `D:creationdate`; nothing acts on it. Immutable, so it stays trustworthy for audit. |
| **Internal** | `updated_at` (with `updated_by`) | Audit queries, consistency sweeps, retention. No surface asks for `ctime` — WebDAV excludes property changes, NextCloud has no equivalent — so it is emitted nowhere. |

`updated_at` keeps its broad "any row touch" meaning precisely *because* it is
internal. Narrowing its bumps to content changes would silently change the
semantics of every audit and retention query already reading it.

`updated_by` belongs in the same bucket, and the pair is why. `FileDto` and
`FolderDto` do expose `updated_by` — "the principal that bumped `updated_at`" —
so after this change the wire names *who* last touched a row while the nearest
date means something different (content mtime, which a rename does not move). A
client could therefore render "last changed by Alice" beside a timestamp from
before Alice's rename.

That is accepted rather than fixed: both halves are provenance, not product
surface, and `updated_by` is consumed as an authorship signal rather than
paired with a timestamp in any view. Adding a third date to `FileDto` to restore
the pairing would reintroduce exactly the "which date do I show?" ambiguity this
document exists to remove. If an audit view ever needs the pair, it should take
an explicitly-named field — never one called `updated_at`.

## Wire mapping

| Slot | Direction | Source / destination | Notes |
|---|---|---|---|
| `D:getlastmodified` | out | `content_modified_at` | RFC 4918 §15.7 — content changes only, property changes excluded |
| `D:creationdate` | out | `created_at` | RFC 4918 §15.1 — the *resource* on this server |
| `D:getetag` (folder) | out | `tree_modified_at` | `{id[..16]}-{tree_modified_at}` |
| `D:getetag` (file) | out | `blob_hash` + `content_modified_at` | see [§ ETag coupling](#etag-coupling) |
| `X-OC-Mtime` | in | `content_modified_at` | epoch seconds, on `PUT` |
| `nc:creation_time` | out | `COALESCE(captured_at, source_created_at, content_modified_at)` | see [§ When creation date is unknown](#when-creation-date-is-unknown) |
| `nc:creation_time` | in | `source_created_at` | see [§ Open questions](#open-questions) |
| `nc:upload_time` | out | `created_at` | server receipt |

### REST DTO fields and where they come from

The table above covers the DAV and NextCloud slots. The REST DTOs the SPA reads
are a separate surface, and this is their mapping. All are RFC 3339 UTC strings
unless noted — see § Representations.

**`FileDto`**

| Field | Column | Notes |
|---|---|---|
| `created_at` | `files.created_at` | server receipt; immutable |
| `modified_at` | `files.content_modified_at` | the content clock. A rename, move, trash, grant or favourite does **not** move it |
| `etag` | `blob_hash` + `content_modified_at` | derived, not stored — see § ETag coupling |
| *(none)* | `files.updated_at` | **deliberately absent.** ctime is internal; no REST field exposes it |

**`FolderDto`**

| Field | Column | Notes |
|---|---|---|
| `created_at` | `folders.created_at` | |
| `modified_at` | `folders.content_modified_at` | the **entry-list** clock, POSIX directory semantics — not the byte semantics `FileDto.modified_at` has. See § Collections |
| `tree_modified_at` | `folders.tree_modified_at` | the recursive rollup. Exposed because DAV sources a collection's `getlastmodified` and `getetag` from it; the SPA does not render it |
| `etag` | `id` + `tree_modified_at` | derived |
| *(none)* | `folders.updated_at` | absent, as on files |

**Where the two DTOs deliberately differ:** `modified_at` means "the bytes
changed" on a file and "the entry list changed" on a folder. Both are "this
resource's own content changed" for the respective type — that symmetry is the
point — but a listing that interleaves the two is interleaving two different
*kinds* of event, which is worth knowing when reading a date sort.

**Other DTOs carrying dates**

| DTO | Field | Column |
|---|---|---|
| `PhotoResourceItemDto` | `sort_date` | `media_sort_date` (or `created_at` / `content_modified_at` when the caller picks another axis) |
| `PhotoResourceItemDto` | `captured_at` | `file_metadata.captured_at`, null when no EXIF |
| `TrashItemDto` | `trashed_at` | when it entered the trash |
| `TrashItemDto` | `deletion_date` | when it will be **purged** — a future instant, not a past one. §7 renames it `expires_at` for exactly that reason |

`PhotoResourceItemDto`'s `sort_date` and `captured_at` are RFC 3339 strings
like every field above — **normalised 2026-10-19**, having previously been
epoch-second integers.

That inconsistency was not cosmetic; it caused a crash. `sort_date` is the
value of the active sort axis, so a client must group on it, and the integer
form made it look like a pre-pivot leftover beside the RFC-string `created_at`.
The SPA's `photoTimestamp` therefore preferred `created_at`, grouped the
timeline on an axis the server had not sorted by, and `/photos` died with
`each_key_duplicate` the moment §3 let the two diverge. One representation
removes the tell that invited the wrong guess.

### API names are not column names

One rule, because the alternative is a token that means two things:

> **The wire never says `updated_at`.**

`updated_at` is a real column with a real meaning (ctime) that no surface
exposes. If an API value were also spelled `updated_at` but resolved to
`content_modified_at`, the same identifier would mean ctime in SQL and mtime on
the wire. So the wire uses `modified_at`, which exists only above the repository
layer and always has.

| API / sort value | Column |
|---|---|
| `modified_at` | `content_modified_at` |
| `created_at` | `created_at` |
| `captured_at` | `media_sort_date` |
| *(nothing)* | `updated_at` |

The domain entity and DTO field are `modified_at` for the same reason — the
translation happens once, in the repository SQL, and no layer above it knows
`updated_at` exists.

### `D:creationdate` is the resource, not the file

RFC 4918 §15.1 defines `creationdate` as the moment **the resource** came into
existence — this URL, on this server. WebDAV has no concept of the file's life
before it was uploaded, and no property for it. That gap is exactly why
NextCloud introduced `nc:creation_time` in its own namespace, and why
`D:creationdate` stays wired to `created_at`.

The MOVE/COPY semantics then fall out of the row model without special-casing:
a MOVE keeps the same row, so `created_at` persists (the same resource,
relocated), while a COPY inserts a new row with a new `created_at` (a new
resource).

### When creation date is unknown

Most files have neither EXIF nor a client-supplied creation claim. The rule
there is that **storage and presentation differ**:

- **The column stays NULL.** Never write a fallback into `source_created_at`.
  The trust bounds would then be applied to a value the server invented; the
  distinction between "the client told us" and "we guessed" is lost permanently,
  so a future backfill has nothing to target; and a stored fallback *freezes* at
  upload time, so if the file is later overwritten and mtime legitimately moves,
  the stored "creation" date silently drifts behind with nothing to correct it.

- **The emitted property falls back**, using the same chain as the sort axis:
  `COALESCE(captured_at, source_created_at, content_modified_at)`.
  If the Photos timeline ordered by one date and displayed another, the grid
  would look wrong in a way nobody could diagnose — so the displayed creation
  date and `media_sort_date` are the same expression.

The chain always terminates on `content_modified_at`, which is `NOT NULL` — so
there is no empty-property case and no `404` propstat block to emit.

For the common file this means `nc:creation_time` and `D:getlastmodified` carry
the same instant. That is honest rather than a defect: creation and modification
genuinely coincide for a file nobody edited after making it.

## The browser edge

The File API exposes exactly one timestamp, and it is in the wrong unit for
everything on the other side.

| Browser has | Maps to | Notes |
|---|---|---|
| `File.lastModified` | `content_modified_at` | epoch **milliseconds** — the only timestamp the File API gives |
| — | `source_created_at` | no `btime` is exposed to web content on any platform |
| — | `captured_at` | the bytes carry EXIF; the server extracts it with `nom-exif` |

So a drag-and-drop upload can preserve mtime and nothing else, while a native
sync client has filesystem access and could supply both. `source_created_at`
will therefore be NULL for most SPA uploads — correct, not a gap, provided NULL
keeps meaning "unknown".

**None of this is wired yet** (checked 2026-10-18), and the gap is easy to
miss because the two ends exist separately:

- `X-OC-Mtime` is accepted, validated and stored — but **only** on the two
  NextCloud surfaces (`nextcloud/webdav_handler::handle_put` and
  `uploads_handler::handle_assemble`, both via `parse_client_mtime`).
- The REST upload endpoints accept no mtime in any form. The multipart field
  `source_modified_at` named below is a **planned** name, not an implemented
  one — it appears nowhere in the tree.
- The SPA never reads `File.lastModified` at all, so a drag-and-drop upload
  currently discards the source mtime and the file is stamped `NOW()`.

So today a NextCloud desktop client preserves mtime and a browser does not.
Closing that needs a mtime slot on the REST upload path plus a frontend that
fills it — at which point the rule below applies.

**Round, don't truncate** on the way in: discarding up to 999 ms reports a file
a second older than the client believes, which is enough to make the next mtime
comparison flap.

## Representations

| Where | Representation | Example |
|---|---|---|
| Postgres columns | `TIMESTAMPTZ` | `2026-10-11 09:14:22+00` |
| **REST wire** | **RFC 3339 string**, nullable | `"2026-10-11T09:14:22Z"` |
| Domain entities `File` / `Folder` | `u64` epoch seconds | `1792055662` |
| Other domain entities | `DateTime<Utc>` | — |
| Browser File API | integer epoch **milliseconds** | `1792055662417` |
| `X-OC-Mtime` | integer epoch **seconds** | `1792055662` |
| `nc:creation_time`, `nc:upload_time` | integer epoch seconds | `1792055662` |
| `D:getlastmodified` | HTTP-date (rfc1123) | `Sat, 11 Oct 2026 09:14:22 GMT` |
| `D:creationdate` | ISO-8601 / RFC 3339 | `2026-10-11T09:14:22Z` |

The REST wire carries **strings, not integers** — `FileDto.created_at` and
`.modified_at` are `DateTime<Utc>` serialising to RFC 3339 by serde's default,
and `resources/gen/openapi.json` declares them `"type": ["string", "null"]`.

This is the deliberate outcome of **PR #804** (`pivot/dto-timestamps-iso`,
merged `fc3f27a5`, 2026-10-12): RFC 3339 everywhere via chrono's default serde,
chosen over "integer everywhere" because it is OpenAPI-standard and because
making timestamps `string` in TypeScript surfaces every silent-arithmetic site.
Inbound request DTOs stay lenient — `deserialize_flex_datetime_option` accepts
integer epoch, RFC 3339, `null` or a missing key — so pre-pivot clients keep
working.

> That PR shipped the convention in code and tests but left **no design doc**,
> so this section is its record. The commits worth reading if the reasoning
> matters: `94edba1c` (ship RFC 3339), `6af90407` (lenient inbound),
> `a4689620` (wire snapshot tests), `77773efe` (e2e smoke across list surfaces),
> `dca28bad` (ETag convergence via `etag_for_listing`), and `95961545` (the
> precision pin, reverted).

**Sub-second precision is load-bearing; do not pin it.** A seconds pin was
attempted and reverted (`95961545`): two grants created within the same
wall-clock second collapsed to identical ISO strings, and `/api/notifications`
cursor pagination could not advance past `?after=<ts>`. Chrono's default
precision is what the cursor flow depends on.

So the REST response is **not** a lossy hop. The lossy hops are `X-OC-Mtime`
(integer seconds) and the `u64` on the `File` / `Folder` entities.

### `File` and `Folder` keep `u64` by design

`DateTime<Utc>` lives **only at the serde boundary**. Domain entities hold epoch
seconds, because `File::compute_etag(hash, modified_at: u64)` embeds the value
verbatim in the ETag string — so an integer-typed entity makes "ETag input is
integer seconds" a type-level guarantee rather than a convention, with no
rounding step for a callsite to get wrong.

The pre-conversion layer is wider than those two structs: it also covers
`virtual_file_etag(size, modified_at: u64)`
(`domain/services/external_mount_id.rs:154`) and the virtual file/folder
descriptors in `application/ports/external_mount_ports.rs:50-68`.

`src/application/dtos/timestamp_helpers.rs` is the single conversion point —
`epoch_to_datetime` at the DTO mappers, `datetime_to_epoch` in reverse,
`epoch_zero` for stub builders. The listing path additionally needs
`datetime_to_etag_seconds` (see § ETag coupling) precisely because it starts
from a `DateTime` and must round to match `EXTRACT(EPOCH FROM x)::bigint`; the
entity path needs no such care.

## Trust bounds

Applies to the two client-asserted columns only — never to the server-stamped
pair:

- Reject a value more than **one hour in the future**.
- Reject anything earlier than **2000-01-01**.
- Rejection returns **400**. Never silently clamp: a wrong client clock should
  be visible, not absorbed.

Unknown stays NULL. The fallback chain lives in the sort expression, never
written into the column — otherwise the trust bounds would be applied to a
value the server fabricated.

## What bumps `content_modified_at`

Content-mutation paths only, taking `X-OC-Mtime` when the client sent it and
`NOW()` otherwise:

- WebDAV `PUT` replacing the blob
- chunked upload completion
- delta commit
- `/api/files/upload` overwrite

**Not** rename, move, trash, grant or favourite. Those bump `updated_at` alone.

**COPY sets it to `NOW()`.** A copy is a new file whose content came into
existence here, at this moment — the same answer `cp` without `-p` gives, and it
needs no preservation plumbing on the copy path. The source is untouched.

**Trash restore leaves it alone**, which needs no code: restore flips a
soft-delete flag rather than re-inserting a row, so the value survives while
`updated_at` bumps. Content did not change, so mtime must not move.

## Collections

A folder's `D:getlastmodified` is sourced from `tree_modified_at`, not from the
folder row's `updated_at`. For a collection, "content" *is* its children, so
this is both what RFC 4918 §15.7 asks for and consistent with the folder's own
`D:getetag`, which already derives from `tree_modified_at`. A folder rename
bumps `updated_at` and leaves `getlastmodified` alone — correct, because
renaming a directory does not change what it contains.

**Folders also carry their own `content_modified_at`** — POSIX directory
semantics, covering the entry list only. The two folder clocks answer different
questions and have different consumers:

| | question | consumer |
|---|---|---|
| `content_modified_at` | did this folder's **entry list** change? | the "Modified" column, `order_by=modified_at` |
| `tree_modified_at` | did **anything beneath** this change, recursively? | `D:getetag`, `D:getlastmodified` |

An earlier revision of this document asserted that folders needed no content
clock because `tree_modified_at` already was one. That conflated two things:
`tree_modified_at` is a recursive rollup that includes descendants' *byte*
changes, which is correct for cache invalidation and saturating as a displayed
date — every top-level folder reads "just now" every day. See § The four clocks.

**DAV stays on `tree_modified_at`.** `getlastmodified` and `getetag` keep
deriving from the recursive rollup, so a collection's two properties stay
consistent with each other and a sync client cannot miss a deep change. Pointing
`getlastmodified` at the entry-list clock would arguably describe a
`Depth: 1` response more precisely, but clients act on the ETag rather than on
that property, so it would buy conformance tidiness and nothing functional. The
divergence this creates — REST reports the entry clock, DAV the subtree clock —
is deliberate and recorded under § Known divergences.

## Sort axes

`storage.files.media_sort_date` is `NOT NULL`
(`migrations/20260307000000_initial_schema.sql:561`), so the chain must end on a
column that always has one. `content_modified_at` is that terminator — EXIF
beats a client's creation claim beats content mtime:

```sql
media_sort_date = COALESCE(captured_at, source_created_at, content_modified_at)
```

| Query | Resolves to |
|---|---|
| `?order_by=captured_at` | `media_sort_date` |
| `?order_by=updated_at` | `content_modified_at` — the UI's "Last modified" axis means POSIX mtime, which is what users expect |
| `?order_by=created_at` | `created_at` — server arrival, the admin-facing axis |

## ETag coupling

`File::compute_etag` is `{blob_hash[..16]}-{modified_at}`
(`src/domain/entities/file.rs:388`). The suffix exists specifically to catch the
`X-OC-Mtime` touch-then-resync case: identical bytes, new mtime, and the ETag
must still change or clients serve stale metadata.

When mtime moves to `content_modified_at`, **the ETag suffix must move with
it**. Leaving it on `updated_at` would make the ETag change on a rename (it
shouldn't) and not change on a pure mtime touch (it must).

**Two entry points, one rule.** Entity-path callers use `compute_etag` with
`u64`; listing-path callers use

```rust
File::etag_for_listing(blob_hash: Option<&str>, modified_at: DateTime<Utc>)  // file.rs:423
```

which rounds through `timestamp_helpers::datetime_to_etag_seconds` so the
`DateTime` path matches `EXTRACT(EPOCH FROM x)::bigint` exactly, and returns an
empty string for folder rows (where `blob_hash` is `None` in the `UNION ALL`).
It exists because the rounding was previously inlined per listing site and
drifted — see the Cyberduck regression captured in
`tests/api/webdav_etag_cross_check.hurl`. **Never inline the dance; always route
through one of these two.**

That helper also settles a question that recurs: a `DateTime<Utc>` field *can*
feed the ETag with identical output, so the epoch-seconds **format** is the
constraint, not the entity's field type.

The folder formula is separate — `Folder::compute_etag(id, tree_modified_at)` —
and unaffected.

## Known divergences

### Resolved

| What | Resolved by |
|---|---|
| `D:getlastmodified` fed from `updated_at`, so it bumped on rename, move, trash, grant and favourite — non-conformant with RFC 4918 §15.7, and clients re-downloaded files whose bytes never moved | plan §2: all file-side reads now resolve `modified_at` from `content_modified_at` |
| `X-OC-Mtime` parsed and validated but dropped on the CREATE path, so a client's mtime survived only when overwriting an existing file — and new-file upload is the common NextCloud-desktop case | plan §1 completion: `save_file_with_blob` now takes the asserted mtime and writes it in the INSERT. Covered by `nc_webdav_client_mtime.hurl`, which is what found it |

### Deliberate

| What | Why |
|---|---|
| REST reports a folder's entry-list clock (`folders.content_modified_at`) while DAV reports its subtree clock (`tree_modified_at`) | The two have different consumers — a person reading a "Modified" column, versus a sync client deciding whether to descend. See § Collections |
| A malformed (non-integer) `X-OC-Mtime` is ignored with a `WARN`, while an implausible-but-parseable one is a 400 | Decided 2026-10-18. A parseable-but-wrong value is the client asserting something specific and wrong, and clamping it would bake a bad timestamp into stored data. An unparseable one carries no information, so on a compatibility surface the lenient reading wins — refusing the request over a header we can disregard would break clients that otherwise work. The `WARN` (`<surface>.mtime_ignored`) keeps the broken client visible |

### Open

| What | Where | Should be |
|---|---|---|
| `nc:creation_time` reads `created_at` | `nextcloud/webdav_handler.rs:2268` | `COALESCE(captured_at, source_created_at, content_modified_at)` |
| `nc:upload_time` reads `modified_at` | `nextcloud/webdav_handler.rs:2273` | `created_at` |
| NC REPORT (search) renders a folder's `getlastmodified` from a projection carrying no subtree clock, so a REPORT hit and a PROPFIND on the same folder disagree | `nextcloud/report_handler.rs` | widen the search projection |

## Rejected alternatives

- **`created_at` = client `lastModified`.** Turns an audit-relevant, immutable
  column into client-controllable data.
- **`updated_at` = client `lastModified` on upload.** The source mtime is lost
  the first time anything else touches the row; the axis loses its meaning
  immediately.
- **Two columns, `content_modified_at` + `source_modified_at`.** One column
  covers both the initial upload and later overwrites: a re-upload with a newer
  local mtime updates `content_modified_at` from the new `X-OC-Mtime`, which is
  the correct answer.
- **`source_created_at` into `D:creationdate`** so generic DAV clients (Finder,
  a mapped drive, Cyberduck) show the real date. It makes a standard live
  property client-asserted and therefore forgeable, goes non-monotonic against
  the server's own history, and contradicts `nc:upload_time`. Nothing
  behaviourally depends on `creationdate`, so the upside is cosmetic while the
  downside is a lost guarantee. Revisitable as an explicit opt-in if the
  generic-client case ever becomes a real complaint.
- **Naming the column `origin_at`.** `origin` already denotes provenance in this
  codebase (sessions carry `"origin":"opaque"`) and reads as *backend/upstream*
  in a storage product. `source_created_at` also advertises that the value is
  client-asserted, which is where the trust bounds apply.

## Scope — files only

This model governs `storage.files`. Two neighbours deliberately keep their own
semantics:

- **Folders** carry a `content_modified_at` of their own, but with POSIX
  directory semantics rather than this document's byte semantics: it tracks the
  entry list, not content, and nothing client-asserted ever writes it — so the
  trust bounds, `X-OC-Mtime` and the rounding rules below do not apply to it.
  Their subtree clock remains `tree_modified_at`, which feeds both `D:getetag`
  and `D:getlastmodified`. See § Collections.
- **CalDAV and CardDAV** feed `getlastmodified` from the calendar or contact
  row's own `updated_at` (`caldav_adapter.rs:201`), and that is correct: an
  event or a contact has no separate blob, so its row change *is* its content
  change. The ctime/mtime split this document draws does not apply there.

## Open questions

**Does any client send creation time?** `nc:creation_time` is only ever emitted
today — no read path exists anywhere in the tree. If no NextCloud client
supplies it on upload, `source_created_at` has no writer except EXIF extraction
and the SPA uploader, and the column should wait for a confirmed writer rather
than ship empty.

This does not block `content_modified_at`, which has both a confirmed writer
(`X-OC-Mtime`, already parsed) and a confirmed reader (`D:getlastmodified`,
which drives actual sync behaviour). The two can ship separately.

**NextCloud's own semantics for the two `nc:` properties are unverified.** The
claim that `nc:creation_time` carries the original creation date and
`nc:upload_time` the server receipt comes from familiarity with the protocol,
not from reading NextCloud's source, and their property set has shifted across
releases. Everything stated about *this* codebase is read from the current tree,
but the re-sourcing of those two slots rests on that assumption — so confirm it
against the NextCloud version being targeted before changing them, or the fix
could make them less correct rather than more.
