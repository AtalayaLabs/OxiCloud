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
| Created at the source (pre-upload `btime`) | `storage.files.source_created_at` **new** | client-asserted, bounded | `nc:creation_time` on upload |
| Any row touch (`ctime`) | `storage.files.updated_at` | server-stamped | every mutation |
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

## Roles — who actually reads them

Only two of the six are **behavioural**. Sorting the rest by who reads them is
what pushes both server-stamped columns almost entirely out of the protocol.

| Role | Columns | Consumer, and consequence of being wrong |
|---|---|---|
| **Behavioural** | `content_modified_at`, `folders.tree_modified_at` | Sync clients. Decides whether a file re-downloads and whether a folder is descended into — the only timestamps where being wrong changes what transfers. |
| **Presentational** | `captured_at`, `source_created_at` | Ordering and display: the Photos timeline, `nc:creation_time`. Wrong values look wrong and cost no bandwidth. |
| **Informational** | `created_at` | Emitted as `nc:upload_time` and `D:creationdate`; nothing acts on it. Immutable, so it stays trustworthy for audit. |
| **Internal** | `updated_at` | Audit queries, consistency sweeps, retention. No surface asks for `ctime` — WebDAV excludes property changes, NextCloud has no equivalent — so it is emitted nowhere. |

`updated_at` keeps its broad "any row touch" meaning precisely *because* it is
internal. Narrowing its bumps to content changes would silently change the
semantics of every audit and retention query already reading it.

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

Uploads send the value as `X-OC-Mtime` (protocol-aligned) or as the multipart
field `source_modified_at`. **Round, don't truncate** on the way in: discarding
up to 999 ms reports a file a second older than the client believes, which is
enough to make the next mtime comparison flap.

## Representations

| Where | Representation | Example |
|---|---|---|
| Postgres columns | `TIMESTAMPTZ` | `2026-10-11 09:14:22+00` |
| REST wire | integer epoch **seconds** (`ts_seconds`) | `1792055662` |
| Browser File API | integer epoch **milliseconds** | `1792055662417` |
| `X-OC-Mtime` | integer epoch seconds | `1792055662` |
| `nc:creation_time`, `nc:upload_time` | integer epoch seconds | `1792055662` |
| `D:getlastmodified` | HTTP-date (rfc1123) | `Sat, 11 Oct 2026 09:14:22 GMT` |
| `D:creationdate` | ISO-8601 / RFC 3339 | `2026-10-11T09:14:22Z` |

Wire-facing `DateTime<Utc>` on any DTO deriving `Serialize + ToSchema` must ship
as an integer via `#[serde(with = "chrono::serde::ts_seconds")]`, so the
generated TypeScript SDK sees `number` on both sides.

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

Folders get no `content_modified_at`: `tree_modified_at` already is the
subtree-content clock.

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
(`src/domain/entities/file.rs:371`). The suffix exists specifically to catch the
`X-OC-Mtime` touch-then-resync case: identical bytes, new mtime, and the ETag
must still change or clients serve stale metadata.

When mtime moves to `content_modified_at`, **the ETag suffix must move with
it**. Leaving it on `updated_at` would make the ETag change on a rename (it
shouldn't) and not change on a pure mtime touch (it must).

The folder formula is separate — `Folder::compute_etag(id, tree_modified_at)` —
and unaffected.

## Known divergences

Present in the tree today, all predating this model:

| What | Where | Should be |
|---|---|---|
| `D:getlastmodified` fed from `updated_at`, so it bumps on rename, move, trash, grant and favourite — non-conformant with RFC 4918 §15.7, and clients re-download files whose bytes never moved | `webdav_adapter.rs`, `nextcloud/webdav_handler.rs` | `content_modified_at` |
| `nc:creation_time` reads `created_at` | `nextcloud/webdav_handler.rs:2268` | `COALESCE(captured_at, source_created_at, content_modified_at)` |
| `nc:upload_time` reads `modified_at` | `nextcloud/webdav_handler.rs:2273` | `created_at` |
| `X-OC-Mtime` is parsed and threaded into the upload path, but lands in `modified_at` — so the first rename overwrites the client's mtime | `nextcloud/webdav_handler.rs:868` → `:1019`, `nextcloud/uploads_handler.rs:395` | `content_modified_at` |

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

- **Folders** get no `content_modified_at` — see § Collections above; their
  subtree clock is `tree_modified_at`, which now feeds both `D:getetag` and
  `D:getlastmodified`.
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
