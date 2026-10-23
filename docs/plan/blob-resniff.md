# Plan — Re-sniffing content: content-keyed metadata and the `content_rescan` job

**Status:** design captured 2026-10-07; **§0, Part A and Part B all shipped**
(2026-10-24). Part A verified green across unit + integration + api-test;
`content_rescan` has unit, DB-integration and real-ffprobe coverage, and the
two endpoints it replaces are gone. Remaining: an api-test scenario for the
job's wire surface, and the mime-only `category_order` rework — deliberately
later, since the extension fallback is what classifies the generic-mime rows
this job exists to drain. Triggered by
`d5f37514` (`fix(security): sniff the content type before trusting the
uploader's claim`), which fixed MIME detection on the *inflow* and surfaced
three things it cannot fix: rows already stored, rows invalidated by content
overwrite, and the fact that both existing re-extract endpoints iterate files
rather than content.

Progress, kept current as work lands — each item carries its own marker at its
heading:

| item | state |
|---|---|
| §0 envelope narrowing in `refine_content_type` | ✅ **DONE 2026-10-07** — `1e41f1b9` (carve-out + `upload_mime_sniffing.hurl`), `a7caafab` (widened to the vendor tree, measured on Apache Tika's 826-file corpus: 541 → 576 correctly typed), `ef6a3243` + `e40dcc38` (everyday + phone-camera type coverage). api-test green |
| §A.1 migration — **audio** keyed by content | ✅ **DONE 2026-10-22** — `audio.content_metadata`, backfilled `DISTINCT ON (blob_hash)`, old table + its 4 dead indexes dropped |
| §A.1 migration — **capture facts** keyed by content | ✅ **DONE 2026-10-23** — `storage.content_metadata`, same `DISTINCT ON (blob_hash)` backfill, old table + geo index moved. `captured_at` → `created_at` per `normalize-file-dates.md`; bookkeeping is `extracted_at` |
| §A.2 the `media_sort_date` triggers | ✅ **DONE 2026-10-23** — **three**, not two. Trigger 1 fans out on `blob_hash`; the dates plan's `BEFORE UPDATE`/assign-`NEW` function is reused by a **new `BEFORE INSERT`** arm, because content-keying falsifies that plan's "`INSERT` needs no trigger": a copy of known content produces no upsert, so nothing would ever overwrite `media_sort_date = NOW()`. Two triggers share one function — a `WHEN` clause cannot test `TG_OP` |
| §A.3 writers, readers, dead-code deletions | ✅ **DONE** — audio 2026-10-22, capture facts 2026-10-23: both clone paths and `on_file_copied` gone on each, repository re-keyed, `reextract_all` walks `DISTINCT blob_hash`, Places viewport + media listing joins repointed (sample id now from the drive-filtered side) |
| §A.4 reap hook + orphan sweep | ✅ **DONE 2026-10-23** — `BlobHandler::purge_content_facts` covers **both** tables, called from `purge_derived_blobs` so there is one place to forget; every reap path reaches it (single-blob via `reap_blob`, and the bulk manifest sweep, which calls it directly). The backstop is `satellites_consistency` phase 3, `content_facts_orphan` (`anomaly`), discovery-only with repair behind `?repair=true`. **§B is no longer gated** |
| §A coverage — extraction, end to end | ✅ **DONE 2026-10-23** — the re-key shipped against tests that asserted no VALUE: no fixture carried EXIF or ID3, so nothing pinned that a capture date or an artist survives the trip out of a blob. `exif-sample.jpg` + `id3-sample.mp3` (with generators in `scripts/helpers/`), unit tests on both extractors, and `photos_exif_extraction.hurl` / `audio_id3_extraction.hurl` pinning the content-keying property itself: same bytes twice → one extraction serves both files, and deleting one copy does not strip the other. The MP3 found a live bug on its first run — `id3::Tag::year()` reads only TYER, so the release year had been silently NULL for every ID3v2.4 file |
| §B `content_rescan` job | ✅ **DONE 2026-10-24** — walk, head sniff, `decide()`, fan-out UPDATE carrying `category_order`, re-derive (`changed`/`always`/`never`), stale-metadata drop, 3 params, 5 counters, 3 finding kinds, DI + detached trigger. Two things the plan did not specify, both found while building: transient errors pause at the last *settled* content instead of becoming findings (a backend outage would otherwise emit N findings and report Completed), and `is_backend_writer` is **true** despite no backend writes — it is the only gate on a job that reads the whole corpus, and with no interval the admin trigger is its only path. Original scope note follows:|
| | scope grew 2026-10-22: the fan-out UPDATE must also move `storage.files.category_order`, in the SAME statement and **per row** (the classifier takes `(name, mime)`, so it is not computable per hash). It is the column the search type filter now reads, so retyping mime without it makes a row invisible under its own category. Supersedes the detector proposed in `search-type-filter-by-category.md` |
| §B ambiguous containers (ffprobe) | ✅ **DONE 2026-10-23** — `container_probe_service`, bidirectional (Ogg promotes, the other three demote), optional like video thumbnails, path derived from `OXICLOUD_FFMPEG_PATH`. The correction is NOT driven by a detected-vs-stored mismatch: an audio-only MP4 is stored AND detected `video/mp4`, so it runs on content the rest of the job considers settled. Cover art is a real video stream — only `disposition.attached_pic` separates it, and getting that wrong would promote a whole music library out of audio extraction, so it is pinned twice (captured JSON + two committed fixtures through the real binary, with ffmpeg added to the CI integration job) |
| §B.7 delete the two reextract endpoints | ✅ **DONE 2026-10-24** — routes, handlers, both service methods, `MetadataExtractionResult` ×2, `MediaFileRow`, the FE wrappers/buttons/state, and `admin.reextract_*` across 16 locales (translations of `maintenance_hint` kept — en.json only, per the house rule). The `files_consistency` `mime_type_reconciliation` idea is marked settled-elsewhere in the same change. **Changelog must say `rederive=always`**: these were the only repair path for the ID3v2.4 year bug and the default `changed` skips every affected row |
| §C.1/C.2 the two diagrams | ✅ **DONE 2026-10-23** — `derived-blobs.md` moves both metadata tables into the content layer with dashed dependent edges; `derived-and-attached-blobs.md` gains `CONTENT_FACTS`, the third satellite shape (per-content, **no** artifact, **no** reference) |
| §C.3 smaller doc edits | ✅ **DONE 2026-10-24** — keying tables + content-keying TODO in `derived-blobs.md`, stale `storage.file_metadata` references in code comments, `job-registry.md` (both endpoints marked replaced), and `jobs.md` (which turned out to list only the plain-`JobHandler` tenants — now says so, since every long-running job including this one is a `RecoverableJobHandler` and none of them appear in that table). plus the `files_consistency_service` `mime_type_reconciliation` pointer, marked settled-elsewhere with §B.7 |

Related: `infer` was bumped 0.19 → 0.22 alongside this design. It gains exactly
two formats (`application/x-qemu-disk`, `image/vnd.dwg`), so it is hygiene
rather than a driver — see §B.6b on why that measurement killed a proposed
"rescan recommended" marker.

## Context

`storage.files.mime_type` is now sniffed content-first on every upload path.
That fixes the inflow. It does nothing for:

- rows written before the change, which hold whatever the client claimed;
- rows invalidated by `swap_blob_hash`
  (`src/infrastructure/repositories/pg/file_blob_write_repository.rs:189`),
  which replaces a file's content on overwrite and leaves `mime_type`
  describing the **previous** bytes — an ongoing staleness source, not a
  legacy one;
- content whose format `infer` did not recognise at upload time.

MIME is not a display hint here. It is a **gate**:
`FileLifecycleHook::on_file_created` refuses to extract audio tags unless
`is_audio_file(content_type)`, and thumbnails key off it too. A file mistyped
as `application/octet-stream` is permanently excluded from every derived
feature. Re-identification unlocks content, it does not merely relabel it.

Two existing admin endpoints already do "re-derive everything"
(`POST /api/admin/{audio,photos}/metadata/reextract`). Both iterate **files**,
not content — so N copies of one song cost N downloads and N parses — and both
are awaited inline in the HTTP handler with no batching, cursor, or cancel.
`docs/plan/job-registry.md` already flags them for replacement.

**Outcome:** one recoverable job that walks distinct content once, corrects the
type from magic bytes, and re-derives metadata for what changed — replacing
both endpoints. Prerequisite: both metadata tables move from file-keyed to
content-keyed, so "once per content" is expressible at all.

## Decisions taken

| | |
|---|---|
| Scope | Resniff **and** re-derive in one job |
| Mutation | `Mutates::Always` (routine maintenance, like `uploads_cleanup`) |
| Write policy | Take `infer`'s answer whenever it is confident and differs — including specific→specific. Silent `infer` → never touch |
| Re-key scope | **Both** `audio.file_metadata` and `storage.file_metadata` |
| Old endpoints | Delete outright (nothing in the frontend calls them) |

---

# §0 — the container regression in the shipped upload path — ✅ **DONE 2026-10-07**

Shipped in `1e41f1b9` and widened in `a7caafab`. Kept here because the
reasoning is what §B.2 has to agree with, and because the final rule is
broader than this section originally proposed:

- The carve-out is **not** container-specific. It covers any detection that
  describes the *envelope*: `application/zip`, `application/x-tar`,
  `application/gzip`, `application/x-ole-storage` and **`text/xml`** — the
  last because `infer` has no SVG matcher at all, so every SVG degraded to
  `text/xml`, which was the costliest case (an SVG stored as XML is not an
  image to anything downstream).
- The accepted narrowing is the **shape** of the set, not a list of formats:
  `application/vnd.*` plus the `+zip` / `+xml` structured suffixes. An
  enumerated version was measured against Apache Tika's 826-file corpus and
  missed ~25 real cases — `.msg`, `.vsd`, `.mpp`, `.kmz`, `.xps`, `.ggb`,
  `.thmx`, and every password-protected OOXML document (an OLE2 wrapper
  around the real zip).
- **Not** narrowed, deliberately: `text/x-shellscript` (so `.py` with a
  shebang reads as a shell script — same category, presentational, and the
  frontend owns highlighter choice) and `text/html` (the detection is
  security-relevant on the download path).
- Residual, measured and accepted: ~22 corpus files where `infer` gives the
  correct *parent* type — OOXML base vs `.docm`/`.xlsm`/`.xlsb`,
  `image/heif` vs `heic`, `image/tiff` vs camera RAW. Those need a type
  hierarchy (Tika's `<sub-class-of>`), not another whitelist entry.

Original statement of the defect follows, for the record.

**Was a live defect in `d5f37514`, not future work.**

Making `refine_content_type` magic-first (`src/common/mime_detect.rs:49`) means
`infer` now outranks a specific claim. For container formats where `infer`
cannot see the discriminating entry inside `MAGIC_BYTES_LEN`, that is a
*downgrade*:

| scenario | before `d5f37514` | after |
|---|---|---|
| `.docx`, browser sends the real OOXML type, `infer` detects docx | correct | correct |
| `.docx`, browser sends the real OOXML type, `infer` falls through to `is_zip` | **correct** (claim won) | **`application/zip`** ← regression |

Browsers do send the real OOXML type for `.docx`, so the claim used to carry
this and no longer does.

**Fix — the same rule as §B.2, one layer down.** In `refine_content_type`, when
`infer` returns a **container generic** (`application/zip`,
`application/x-tar`, `application/gzip`, `application/x-ole-storage`), do not
return it immediately. Prefer a known *specialisation* of that container if
either the claim or the extension supplies one; fall back to the container type
otherwise. Everything non-container keeps today's magic-first ordering
untouched.

Unlike the job (§B.2) there is no per-file/per-content tension here — this
function already receives `filename` and `claimed`, so the signature does not
change.

### Tests — regression locks, in `src/common/mime_detect.rs`

The existing seven precedence tests stay. Add, as a named group:

| case | expected | guards against |
|---|---|---|
| zip bytes + `report.docx` + OOXML claim | the OOXML type | the `d5f37514` regression itself |
| zip bytes + `report.docx` + `application/octet-stream` | the OOXML type | extension rescues when the client says nothing |
| zip bytes + `sheet.xlsx` / `deck.pptx` | the matching OOXML type | one-format-only fixes |
| zip bytes + `book.epub` | `application/epub+zip` | `infer` has an epub matcher; the rule must not fight it |
| **zip bytes + `backup.zip` + any claim** | **`application/zip`** | the inverse — the rule must not stop an archive being an archive |
| **zip bytes + `archive` (no extension)** | **`application/zip`** | same, with nothing to fall back to |
| **real PNG bytes + `evil.docx`** | **`image/png`** | the rule applies **only** to container detections; a non-container must never defer to the extension, or the security fix is undone |
| gzip bytes + `logs.tar.gz` | `application/gzip` | multi-suffix names resolve sanely |

The two inverse cases are the point of the group. The obvious bad "fix" for
this regression is to let the extension win generally, which would hand the
filename back the authority the security commit just took away from the claim —
so `real PNG bytes + evil.docx → image/png` is the test that keeps both
properties true at once, and it should say so in a comment.

Mirror the same table in the job's `decide()` tests (§B.8) so the two layers
cannot drift.

---

# §A — re-key both metadata tables to content

Rationale and the security analysis are written up at
`docs/plan/derived-blobs.md` § "TODO — `audio.file_metadata` is keyed to the
file, not the content". Align with it rather than inventing a second shape.

## A.1 Migrations

**`audio.file_metadata` → `audio.content_metadata`**, PK
`source_hash VARCHAR(64)`. Backfill
`INSERT … SELECT DISTINCT ON (f.blob_hash) … FROM audio.file_metadata m JOIN storage.files f ON f.id = m.file_id`
with a deterministic `ORDER BY m.updated_at DESC` so the N→1 collapse is
reproducible. Uses `idx_files_blob_hash`. Drop the old table and all four
`idx_audio_metadata_{artist,album,genre,year}` indexes — they serve only dead
code (see A.3).

**`storage.file_metadata` → keyed `source_hash`**, same shape otherwise
(`captured_at`, lat/lon, camera make/model, orientation, width, height — every
column is `f(bytes)`).

No FK on either: `source_hash` is a content address, matching
`storage.content_derived_blobs.source_hash` and `storage.blob_extracted_text`.

## A.2 The `media_sort_date` triggers — two, not one

Today `trg_sync_media_sort_date` on `storage.file_metadata` runs
`UPDATE storage.files SET media_sort_date = COALESCE(NEW.captured_at, created_at) WHERE id = NEW.file_id`
(`migrations/20260307000000_initial_schema.sql:685-698`).

Content-keying needs **both** of these:

1. **Fan-out on the upsert** — `WHERE blob_hash = NEW.source_hash`. Write
   amplification bounded by the dedup factor, but extraction now happens once
   per content instead of once per file, so total work drops.
2. **A new trigger on `storage.files` INSERT** that seeds `media_sort_date`
   from any pre-existing metadata row for that `blob_hash`. **This does not
   exist today and its absence would be a silent correctness bug:** under
   content-keying a new file on already-known content produces *no* metadata
   upsert, so trigger (1) never fires and the photo lands in the timeline at
   upload date forever.

Dropping the denormalisation instead is not an option —
`idx_files_media_timeline` and
`migrations/20260901000001_files_media_timeline_by_drive_index.sql` exist so the
Photos `ORDER BY media_sort_date DESC LIMIT n` stops early.

## A.3 Code changes

| | |
|---|---|
| `AudioMetadataService::extract_and_save(file_id, blob_hash)` → `(source_hash)` | `audio_metadata_service.rs:110`; `ON CONFLICT (file_id)` → `(source_hash)` |
| `MediaMetadataService::extract_and_save` → split into `extract(hash, mime) -> Option<Meta>` + `save(source_hash, &meta)` | `media_metadata_service.rs:148`; keep the composition for the live path |
| **Delete** `clone_from_source_background`, `clone_or_extract_background`, and the `is_new_blob` branch in `on_file_created` | `audio_metadata_service.rs:265,308,~390` and the media twins at `:241,:540` — all exist *only* to compensate for file-keying |
| **Delete** both `on_file_copied` bodies | a copy shares the blob; the row already exists. Mirror `ThumbnailRefreshHook`, already a no-op here for exactly this reason |
| **Add** `delete_metadata_for_content(source_hash)` on both | the job needs it when a type changes *away from* media |
| Readers | `MusicStorageAdapter::get_audio_metadata` → `playlist_pg_repository.rs:496`, and the enriched listing's `LEFT JOIN` at `:227` — both join through `storage.files.blob_hash` |
| **Delete** dead repo methods | `playlist_pg_repository.rs:431,526,541,578,615` — no callers |

## A.4 Lifecycle — must not be skipped

Content-keyed rows hold **no `ref_count`** and have **no `ON DELETE CASCADE`**,
so they leak when the source blob is reaped. Required before §B ships, because
the job *creates* rows corpus-wide and would turn a slow leak into a fast one:

1. **Reap-time purge** inside `BlobHandler::purge_derived_blobs`
   (`blob_handler.rs:1310`, called from `:1298` and `:3720`) — one place that
   can be forgotten rather than two.
2. **Orphan sweep** for rows whose source vanished by a path that ran no reap.
   Prefer a finding + repair arm in `satellites_consistency_service.rs`, which
   exists for this failure class, over a silent `DELETE` (precedent:
   `ContentIndexWorker::sweep_orphaned_text`, `content_index_worker.rs:419`).

Update both `on_file_deleted` comments: the row is content-keyed and
deliberately outlives the file.

## A.5 Copy, recursive copy, deletion — the reference model

**The metadata row must never become a `BlobReferenceSource`.** It holds no
`ref_count` and participates in no refcounting. This is not an omission: if it
registered as a reference holder, deleting every file on a blob would leave the
blob pinned alive by its own metadata, forever. The precedent states both roles
explicitly (`migrations/20261018000000_content_derived_blobs.sql:62-66`):
`source_hash` is a *"dependent reference — holds no ref_count; rows are deleted
when the source Blob is reaped"*, while only `blob_hash` is a *"reference
HOLDER"*. These tables have no `blob_hash` — they are columns, not artifacts —
so they are purely dependent, the simpler half of that pair.

| event | today (file-keyed) | after (content-keyed) |
|---|---|---|
| File copy | `clone_from_source_background` duplicates the row per copy | **no-op** — the row is already there |
| Recursive folder copy | **no row at all** (see below) | **correct for free** |
| Delete 1 of N files on a blob | that file's row cascades away | row survives — other files still reference the content |
| Delete the last file | row cascades away immediately | row survives until the blob is reaped |
| Trash | row cascades only on purge | trashed rows still carry `blob_hash`, so blob and metadata are retained |

**The recursive folder copy is a latent bug that content-keying fixes.** The
WebDAV `COPY Depth: infinity` path is pure SQL
(`migrations/20260307000000_initial_schema.sql:784-786`) and fires **no Rust
lifecycle hooks**. Its `INSERT … SELECT` carries `media_sort_date` across but
creates no `storage.file_metadata` row — so a recursively-copied photo today
has a sort date and no EXIF detail. Content-keyed, the copy resolves to the
existing row and the gap closes with no code. Worth confirming against
`BatchOperations::copy_folders` too, which may take a different path.

**On the delete window:** the last-file-deleted case means metadata now
outlives the final file reference, until `dedup_gc` reaps. That is not a
privacy regression — the blob *bytes* survive exactly the same window, so EXIF
GPS is no more exposed than the photo it came from. The invariant to hold onto
is **metadata lifetime == blob lifetime**, which is what the A.4 reap hook
enforces and what the A.4 orphan sweep catches when a reap path misses.

**Minor consequence for §B:** the job's walk excludes `is_trashed`, so content
referenced only by trashed files is never re-sniffed and a restored file keeps
its stale type until the next run. Acceptable; worth a line in the module doc.

---

# §B — the `content_rescan` job

`src/infrastructure/services/content_rescan_service.rs`, type `ContentRescan`,
`CONTENT_RESCAN_JOB_NAME = "content_rescan"`. Copy the structure of
`drive_policies_consistency_service.rs` (the canonical complete example).

**Must not end in `_consistency`** — `consistency_batch_service.rs:76`
auto-enrols any such name into the weekly batch, and this job can cost a
full-corpus read on a metered backend.

## B.1 One walk, not two

Page over `storage.files` **grouped by `blob_hash`**, keyset on `blob_hash`:

```sql
SELECT f.blob_hash AS content_hash,
       array_agg(f.id ORDER BY f.id)   AS file_ids,
       array_agg(DISTINCT f.mime_type) AS stored_types,
       max(f.size)                     AS size
  FROM storage.files f
 WHERE ($1::text IS NULL OR f.blob_hash > $1)
   AND ($3::text IS NULL OR f.mime_type LIKE $3 || '%')
   AND NOT f.is_trashed
 GROUP BY f.blob_hash ORDER BY f.blob_hash LIMIT $2
```

This beats a CDC-then-legacy two-phase walk on every axis: the `FROM` clause
**is** the "referenced by a file" predicate (so thumbnails and previews in
`chunk_manifests` are excluded without a subquery), it covers CDC and legacy
content in one pass with one cursor, and `array_agg` hands over the fan-out set
for free. Runs on `idx_files_blob_hash`.

Cursor = last `content_hash` as UTF-8 bytes; decode exactly as
`manifests_consistency_service.rs:243-256`.

`count_total` = `SELECT COUNT(DISTINCT blob_hash) FROM storage.files WHERE NOT is_trashed`,
with `ProgressKind::Approximate` — `count_total()` takes no args, so it cannot
see `mime_prefix` and a scoped run would otherwise show a bar stuck at 3 %.

Resolve CDC vs legacy **per page**, one query, on the maintenance pool:
`SELECT file_hash, chunk_hashes[1], total_size FROM storage.chunk_manifests WHERE file_hash = ANY($1)`.
Present → read the head from the first chunk (`CDC_MIN_CHUNK = 65_536`, so it
always covers 8 KiB); absent → legacy, read from the content hash. Do **not**
route this through `manifest_cached` — it is a 32 MiB / 60 s moka shared with
the live read path.

## B.2 Per-blob step

Add to `BlobHandler` (not `Arc<dyn BlobStorageBackend>` in the job —
`src/AGENTS.md:29` forbids that, and CDC-awareness belongs in one place):

- `read_object_head_uncached(object_hash, len) -> Bytes` — uses
  `backend.uncached()` then `get_blob_range_stream(hash, 0, Some(len))`. The
  pattern and the `cached_read_bypassed` log field already exist at
  `backend_consistency_service.rs:459,471`.
- `first_chunk_hashes(&[String]) -> HashMap<String,(String,i64)>` on the
  maintenance pool.

Call **`infer::get(&head)` directly**, not `refine_content_type` — steps 2–4 of
that function need a filename and a claim, and a blob has neither. Say so in
the module doc so nobody "fixes" it.

### Decision table

| detected | stored | action |
|---|---|---|
| `None` | any | leave; **no finding** (silence is normal for text/CSV/SVG) |
| `Some(d)` | all `== d` | leave |
| `Some(d)` | any `!= d` | retype — per the decided write policy |

### Archives defer to the extension

A `.docx` **is** a ZIP, so "take `infer` whenever confident" needs a tiebreaker
for container formats — `infer` is confident that a docx is a zip, which is
correct and useless, and `mime_type` governs what browsers do on download.

How much of this `infer` already solves, measured rather than assumed:

- **ODF is safe.** The spec mandates `mimetype` as the first, *stored*
  (uncompressed) entry precisely so sniffers can read it; `matchers/odf.rs` has
  `is_odt` / `is_ods` / `is_odp` exploiting that. Always within the head.
- **OOXML is usually safe.** `matchers/doc.rs` has `is_docx` / `is_xlsx` /
  `is_pptx`, and `check_msooml` (`:136`) walks forward through successive
  `PK\x03\x04` local headers via `search` (`:148`) rather than testing only the
  first entry — so for a typical document the `word/` header lands well inside
  8 KiB. It fails only on unusual archives (an oversized `[Content_Types].xml`,
  heavy padding).

So the residual is narrow, and the right handling is **fall back to the
extension**, not refuse:

> When the detected type is a **container generic** (`application/zip`,
> `application/x-tar`, `application/gzip`, `application/x-ole-storage`) and
> `mime_guess` on the file's own name yields a known specialisation of that
> container, take the extension's answer for that file. Otherwise take the
> detected type.

**This is the one case where the answer is per-file, not per-content** — a blob
shared by `report.docx` and `backup.zip` legitimately resolves differently for
each, because the extension is the only signal distinguishing them. The fan-out
UPDATE therefore degrades to one statement per file *for container detections
only*; every other detection stays one statement per blob. Keep that narrow and
say so in the module doc, or it will get "simplified" back.

Record the finding either way with `detail.reason = "container_extension"` so
the disambiguation is visible rather than silent.

### Fan-out UPDATE

```sql
UPDATE storage.files SET mime_type = $1
 WHERE blob_hash = $2 AND mime_type IS DISTINCT FROM $1
```

Bind the **hash**, not `file_ids`: re-reading under the UPDATE's own snapshot
picks up files inserted since the page was fetched, and `IS DISTINCT FROM`
makes it idempotent. Same trick as `manifests_consistency_service.rs:110-116`.

#### `category_order` must move in the SAME statement

`storage.files.category_order` is a denormalised `smallint` derived from
`display_helpers::category_order_for(name, mime)` and indexed as
`idx_files_category_order`. **It is no longer optional bookkeeping.** Since the
search type filter landed it is the column the filter *reads*
(`fi.category_order = ANY($n)`), not merely the `type` sort key — so a row whose
mime is retyped while its category is not becomes invisible under its own
category. A `.bin` resniffed to `audio/mpeg` keeps `9999` (Other) and never
again appears under "Audio".

Three constraints, in order of how easily they are got wrong:

1. **Same `UPDATE`, not a follow-up statement.** A separate write leaves a
   window — and, if the job is cancelled between the two, a permanent state —
   where `mime_type` and `category_order` disagree, so the filter contradicts
   the mime the API reports for that row.
2. **`category_order_for` takes `(name, mime)`, not just mime — for now.** The
   resniffer learns the new mime per *content*, but the category also depends on
   the file *name*: the classifier falls back to the extension when the mime is
   generic. So one value per hash is not computable — it is **per row**. Compute
   it in Rust per file id and fan out with a `VALUES` join. This is the one place
   the "bind the hash, not `file_ids`" rule above has to bend, and the statement
   needs each row's `name` regardless.

   **This constraint is TRANSITIONAL and this job is what retires it.** Ed's
   direction (2026-10-22) is that mime becomes the source of truth once
   normalised, at which point `category_for` drops its `name` parameter and the
   extension arms go. A mime-only classifier is one value per *content*, so the
   fan-out reverts to the plain per-hash shape and this bullet disappears.
   Sequencing: ship the per-row form here, because the extension fallback is the
   only thing classifying legacy rows whose mime is still generic — exactly the
   population this job exists to drain. Revisit once a count of generic-mime
   rows reads zero. Same reasoning applies to §A.1's exclusion of
   `category_order` from content-keying: name-dependence is the stated
   objection, and it expires with the fallback.
3. **Do not re-encode the mapping in SQL.** `category_order_for` is the single
   source of truth; the only other copy is the backfill in
   `20260527000001_files_category_order.sql`, and a third will drift. Precedent:
   `20261103000009`, where `copy_folder_tree` needed the value and derived it
   from a sibling row with the same mime rather than duplicating the table.
   **`0` and `1`–`99` are reserved** — folder sentinel plus headroom for future
   non-file resource kinds; see `docs/architecture/resource-listing.md`.

This also settles a question §A.1 would otherwise raise: `category_order` can
**not** move to a content-keyed table despite being mime-derived, because it
also depends on the per-file name. It stays on `storage.files`, refreshed per
row when the content's mime changes — the same asymmetry `name` itself has.

**This supersedes the `category_order` consistency detector** proposed in
`docs/plan/search-type-filter-by-category.md`. That plan wanted a
discovery-only detector for rows whose stored value disagrees with
`category_order_for(name, mime)`. This job recomputes that same expression and
can *correct* it, so a detector would be a second reader of one denormalised
column — which is how two readers drift. The audit behind that plan found the
column clean on a live instance (4093 rows, 0 NULL, 0 defaulted), so the
detector guarded a hypothesis; **this** job creates the real staleness and
therefore owns the fix.

#### Ambiguous containers: correct the mime, do not add a flag

Decided 2026-10-23. A container signature says what the wrapper is, not what
is inside it, and `infer` matches signatures — so for four shapes the stored
type is a coin flip, **in both directions**:

| container | stored today | should be |
|---|---|---|
| audio-only MP4 (brand not `M4A`) | `video/mp4` | `audio/mp4` |
| audio-only WebM | `video/webm` | `audio/webm` |
| audio-only Matroska | `video/x-matroska` | `audio/x-matroska` |
| Theora in Ogg | **`audio/ogg`** | `video/ogg` |

`infer` cannot fix this and should not be asked to: it distinguishes only
`.m4a`, by `ftyp` brand, and has no `video/ogg` entry at all. Knowing the
answer requires reading the stream table.

**The fix is to correct `mime_type`, NOT to store an audio-vs-video flag
beside it.** A flag was considered and rejected for the reason the
`category_order` section above already establishes: a denormalised value needs
exactly one writer. Every gate in the system is a mime-prefix test —
`AudioMetadataService::is_audio_file`,
`FfmpegVideoFrameService::is_supported_video`,
`MediaMetadataService::handles` — so a flag would force each to choose which
source to believe, and `Content-Type` on download, WebDAV `getcontenttype`
and the future `category_for(mime)` would *still* be wrong. Correcting the
mime fixes all five at once and keeps "mime is the source of truth" literally
true. All four target values are registered; nothing is invented.

**When it can be known splits the work:**

- **At upload** `refine_content_type` sees only the first 8 KiB, and an MP4's
  `moov` atom is routinely past that. The container default is often the best
  answer available, so this is not reliably fixable at ingest.
- **At rescan** the blob is already open, so the probe is nearly free. This is
  where the backlog gets drained, and it rides along with the fan-out UPDATE
  above rather than being a separate pass.
- **The gap between them** is newly-uploaded audio-only MP4s, which would sit
  as `video/mp4` and miss audio extraction until a rescan. Worth closing in
  the lifecycle hook, which already opens the blob asynchronously — probe for
  the four ambiguous mimes only, correct, then dispatch.

⚠ **The ordering trap.** The gates run BEFORE the extractor
(`on_file_created → is_audio_file(mime)? → audio extractor`), so an audio-only
MP4 stored as `video/mp4` never reaches the code that could discover it is
audio. The information is locked behind the decision it would correct. Any
implementation has to probe *outside* the gate, not inside an extractor the
gate may never call.

**Probe with `ffprobe`.** ffmpeg is already an optional configured dependency
(`OXICLOUD_FFMPEG_PATH`) and video thumbnails already degrade gracefully when
it is absent; the same posture applies — probe when available, otherwise keep
the container default and leave it for a later run. The alternative is
hand-parsing `moov`/`trak` handler types, Ogg codec headers and EBML
`TrackType`: three parsers for four formats, when the dependency is already
present and already optional.

**Do not touch `updated_at`** — the content did not change, only our
description of it. Bumping it would make every WebDAV and sync client re-fetch
the corpus.

**Known side effect for the module doc:** `mime_type` is in the tree-ETag
observable column list
(`migrations/20260626000000_tree_etag_statement_triggers.sql:98`), so each
retype enqueues an ancestor `tree_modified_at` bump. Correct, but a
thundering-herd risk at scale. Mitigated by cancel-and-resume, not by a knob.

`chunk_manifests.content_type` is **never written**: it is write-once today
(`attach_manifest` uses `ON CONFLICT DO NOTHING`) and its only reader is a
diagnostics DTO, so correcting it buys nothing operationally. Report the drift
as a finding and stop. `storage.blobs.content_type` is out of scope too — for
CDC it describes a chunk.

This job settles the open question at `files_consistency_service.rs:47-49`
("requires deciding which is authoritative first"): **magic bytes when `infer`
speaks, otherwise what is stored.** Update that comment to point here.

## B.3 Re-derive step

Dispatch on the **new** type; post-§A both arms are one call per content:

| new type | action |
|---|---|
| `audio/*` | `audio.extract_and_save(content_hash)` |
| `image/*` or `video/*` | `media.extract(content_hash, mime)` then `media.save(content_hash, &meta)` |
| other, and it changed *away from* media | delete the stale audio/EXIF row |

The job carries **zero extraction logic** — it takes
`Option<Arc<AudioMetadataService>>` (it is `Option` on `AppState`) and
`Arc<MediaMetadataService>` and calls their public methods.

`rederive` has three values: `changed` (**default** — only when the sniff
actually changed the type), `always` (the faithful replacement of the old
endpoints), `never`.

## B.4 Cost control

1. **`rederive=changed` by default.** On a healthy corpus the job costs
   `N_content × 8 KiB` and nothing else. The difference between minutes and
   days.
2. **Bypass the disk cache.** `CachedBlobBackend` on a miss downloads the whole
   object into the LRU and serves the range (`cached_blob_backend.rs:363-405`);
   a full-corpus sweep is 100 % miss, so reading through it would pull
   everything **and evict the entire hot working set**. The cache sits outside
   encryption, so peeling it still yields plaintext.
3. **Read the first *chunk*, not the blob.**
   `EncryptedBlobBackend::get_blob_range_stream`
   (`encrypted_blob_backend.rs:832`) decrypts the **entire object** before
   slicing. Targeting `chunk_hashes[1]` caps that at 1 MiB.
4. **`const MAX_DERIVE_BYTES` (512 MiB).** Re-deriving EXIF from a 4 GiB video
   means fetching and decrypting 4 GiB to read a header. A constant, not a knob.
5. **Maintenance pool** throughout, including the manifest probe.
6. **`dry_run`**, and `mime_prefix` which narrows the walk in SQL so
   out-of-scope content is never read.

Legacy whole-file blobs are included and pay a full fetch + decrypt for an
8 KiB sniff. Not worth a flag: once `backend_rechunk` has converged the
population is zero, and if it has not, that is the bigger problem.

`BATCH_SIZE = 100` (a "row" here is a network round-trip). Checkpoint cursor
**and** `checkpoint_counters` every batch — a batch takes seconds and a cancel
should stop now.

**Override `unattended_resume_allowed` to `false`.** The default keys on
`repair`/`deep` (`recoverable.rs:546`); this job declares neither, so with
`Mutates::Always` it would inherit `true` and a paused full-corpus sweep could
be resumed by a scheduled tick with nobody watching. Mandatory, not optional.

## B.5 Parameters — three, not eight

`dry_run` (bool, false) · `rederive` (string, `changed`) ·
`mime_prefix` (string).

An earlier draft had eight. Each was defensible alone; together they were a
configuration surface larger than the job, and every one is a branch to test.
**Cut, with what they protected kept as constants where it mattered:**

| cut | why |
|---|---|
| `max_derive_mb` | becomes `const MAX_DERIVE_BYTES` — still skips oversized content and records a finding, just not tunable |
| `max_blobs` | cancel already works and the cursor already survives a pause; this was a third way to stop |
| `include_legacy` | legacy content is simply included. If `backend_rechunk` has not converged, the operator has a bigger problem than this job |
| `fix_manifest_type` | **never** write `chunk_manifests.content_type`. Its only reader is a diagnostics DTO, so correcting it buys nothing. Report the drift and stop |
| `drive` | `mime_prefix` is the scoping knob that earns its keep — `application/octet-stream` is exactly the set worth re-sniffing |

Reject an unrecognised `rederive` with `RunOutcome::Failed` **before any work** —
silently defaulting a typo'd `alwyas` to `changed` would make a requested full
backfill quietly do nothing.

## B.6 Counters and findings

Counters via `checkpoint_counters` **every batch** (absolute, not delta —
`extra_stats` on `Completed` only covers the last segment after a pause). Five:

`head_reads` · `mime_changed` · `files_retyped` · `rederived` · `failures`

`files_retyped` counts rows whose `mime_type` **and** `category_order` moved
together — see the fan-out UPDATE above for why the two cannot be counted
separately. A sixth counter for the category alone would only ever equal this
one, or indicate a bug that belongs in `failures`.

An earlier draft had thirteen. `infer_silent` / `mime_agreed` are the
uninteresting majority and are implied by `head_reads - mime_changed`;
`bytes_fetched`, the per-extractor split and the legacy/skip counters were
telemetry nobody asked for.

Findings via `record_or_log`, **one per content, not per file** — a song with
400 copies must not produce 400 findings, so `detail` carries `file_count` and
a capped `file_ids`. Three kinds:

| kind | severity |
|---|---|
| `mime_type_mismatch` | **`anomaly`** — informational |
| `blob_head_unreadable` | `anomaly` |
| `rederive_failed` | `anomaly` |

⚠ **A changed mime type is INFORMATIONAL, not an inconsistency.** All three
kinds are `anomaly`, which is what "informational" means in this codebase —
`record_finding`'s own doc says `anomaly` is *"surprising state worth
surfacing, no known impact… the level the admin panel labels 'notices'; there
is no separate `notice` severity"*, and `finding_severity_counts` renders
`data_loss + inconsistent > 0` as an actionable amber/red pill versus
`anomaly`-only as a neutral notice.

An earlier draft had `mime_type_mismatch` at `inconsistent`. That would be
wrong twice:

1. **It is not an inconsistency.** Nothing is unreachable and no counter
   disagrees with reality — the stored type was simply derived under an older
   rule or from a client's claim, and the job has now derived a better one.
   The *content* was never in doubt.
2. **It would page people.** `FindingNotifier` alerts on per-kind
   transitions gated by `NotifyThreshold::admits(severity)`, so at
   `inconsistent` the first run over a real corpus — which is expected to
   correct a great many types at once, that being the point — would fire
   alerts at every configured sink. A job whose normal successful outcome
   generates alerts trains operators to ignore the channel.

Corrections are reported in `detail.corrected` rather than by promoting the
severity, which is the convention the trait doc states explicitly: *"a job
that acted on what it found says so in `detail` rather than in a fourth
severity"*.

If a genuine `Severity::Info` is ever added below `anomaly`, this kind is the
first candidate to move — it is the clearest case of "worth reading, never
worth waking anyone".

One kind covers found-and-fixed and found-and-not; `detail.corrected`
distinguishes them (precedent: `detail.removed` in drive_policies).
`blob_head_unreadable` stays `anomaly` deliberately — `files_consistency`'s
`missing_blob` is the authority for `data_loss`, and two jobs disagreeing about
the same condition is worse than one under-claiming. Transient errors go to
`RunOutcome::from_domain_error` instead.

## B.6b Deliberately not built

Recorded so it is a decision rather than an omission:

- **No "rescan recommended" marker.** Storing the detection epoch (or `infer`'s
  version) in `auth.admin_settings`, checking it at boot and surfacing a hint
  would mean a `build.rs` change, a DB key, a staleness check and an admin
  surface — to advise running a job over a crate bump that added two niche
  formats. The changelog plus an on-demand job is enough. Revisit only if a
  future `infer` release changes detection broadly.
  *Counter-argument on file, for whoever revisits:* `job_runs_cleanup` deletes
  terminal runs after a retention window, so run history cannot answer "has a
  full sweep ever completed" — it degrades to "within the last N days". That is
  a real gap; it is just not worth four moving parts today.
- **No per-row sniffer stamp** on `storage.files` (the
  `blob_extracted_text.extractor` pattern, `content_index_worker.rs:191-211`).
  It would make the job self-scoping — `WHERE sniffed_by <> current` instead of
  a global flag — but costs a column on the largest table to save a full scan
  nobody runs often.

## B.7 Removal of the old endpoints

Delete: `reextract_all_audio_metadata` (`audio_metadata_service.rs:208`),
`reextract_all_image_metadata` (`media_metadata_service.rs:343`), their
`*FileRow` and `MetadataExtractionResult` types, handlers
(`admin_handler.rs:1839,1866`), routes (`:150,:152`), the two frontend API
wrappers (`frontend/src/lib/api/endpoints/admin.ts:59-69`), the two buttons and
their state, and the `admin.reextract_*` locale keys across all 16 locales.

⚠ The buttons moved after this plan was written: maintenance actions now live
under the **`maintenance` tab** of `frontend/src/routes/admin/[[tab]]/+page.svelte`
(tab in the union at `:232`, rendered from `:3483`, and the file's own comment
at `:2662` records the move). Still the same file — `/admin/maintenance` is a
tab value, not a separate route — but grep for `reextract` rather than trusting
line numbers.

No shims: both are admin-only, synchronous, and their only callers are the
buttons being deleted in the same change.

Extend `is_detached_job` (`admin_handler.rs:3301`) to
`matches!(name, "backend_migration" | "content_rescan")` — its comment already
anticipates this. Without it the HTTP trigger blocks on the whole sweep, which
is the bug being fixed.

DI (`src/common/di.rs`, template `:1550-1557`): `maintenance_pool`,
`blob_handler`, both metadata services; interval **`None`** — on demand only.

**Changelog note:** `rederive=always` is the faithful replacement of the two
buttons; the default `changed` deliberately does far less.

⚠ **Say that loudly, because there is already a case where the default does
nothing at all.** The ID3v2.4 year fix (`5466a9f7`) left every existing
`audio.content_metadata` row with `year = NULL`: `id3::Tag::year()` reads
only the v2.3 TYER, so the value was silently dropped for anything a modern
encoder wrote. Those rows need re-deriving — but the MIME type of an MP3
was always `audio/mpeg` and does not change, so `rederive=changed` skips
every one of them. Only `rederive=always` repairs it.

Nor does the corpus self-heal: `on_file_created` early-returns on
`!is_new_blob`, so re-uploading the same song is a dedup hit that never
re-extracts. Until this job ships the repair path is
`POST /api/admin/audio/metadata/reextract`, which already walks
`DISTINCT blob_hash` and whose upsert overwrites `year` — it just blocks
the HTTP request while it runs.

The general lesson for the changelog: a fix to an EXTRACTOR is invisible to
`changed`, which keys on the sniffed type. Every such fix has to tell
operators to run `always`.

## B.8 Verification

**Unit (no DB)** — these catch the real bugs:

- cursor round-trip; empty cursor starts from the beginning.
- `decide(detected, stored, filename) -> Decision` table-tested, extracted as a
  free function. **`image/png` vs `image/jpeg` → Retype** is the spec for the
  specific→specific rule. Plus the container cases: detected `application/zip`
  + name `report.docx` → **`…wordprocessingml.document`**; detected
  `application/zip` + name `backup.zip` → **`application/zip`**; detected
  `application/zip` + name with no extension → **`application/zip`**.
- `rederive` parsing accepts the three values and rejects a typo by name.

**Integration** (`just test-integration content_rescan`), following
`satellites_consistency_service.rs:808-1000`: drive through real
`run_or_resume`, **one test function with sequential phases** (parallel
`#[tokio::test]`s on one job name silently attach instead of dispatching —
assert `extra["completed"] == true`).

Seed a real PNG via `BlobHandler::new_for_test`, three `storage.files` rows on
that one hash all claiming `image/jpeg`, plus a `content_derived_blobs`
thumbnail whose hash has no file row.

- Phase 1 `dry_run` → one finding, `corrected: false`, nothing written.
- Phase 2 default → all three become `image/png`, `files_retyped == 3`,
  **`head_reads == 1`** (the dedup guarantee the old file-iterating code failed).
- Phase 3 → the thumbnail hash was never visited.
- Phase 4 → re-run is a no-op (idempotence).
- Separately: cancel mid-run pauses with a cursor, and resuming finishes the
  remainder rather than restarting.

**Manual:** upload a file, then
`UPDATE storage.files SET mime_type = 'text/plain'` by hand to simulate
`swap_blob_hash` staleness; `dry_run` then real; confirm the row flips and an
`audio.content_metadata` row appears for the content hash. With
`CachedBlobBackend` enabled, confirm cache stats show **no growth** during a
sweep — that is the `uncached()` bypass, and nothing else will catch its
regression.

## B.9 Risks

1. **`infer` false positives** — mitigated by the container/extension rule
   (§B.2). `infer` already handles ODF reliably and OOXML usually, so the
   residual is unusual archives plus non-container formats where the first
   8 KiB is genuinely ambiguous.
2. **ETag storm** on mass retype — mitigated by cancel-and-resume.
3. **`rederive=always`** streams every in-scope blob. `MAX_DERIVE_BYTES` caps
   each one, not the total; on a large corpus this is a long job by design.
4. **`Mutates::Always` inverts the house polarity** — every sibling that writes
   does so behind `repair`, so the *safe* mode here is the one you must remember
   to ask for, on a job whose principal failure mode is writing a wrong value to
   a download-governing column. Accepted per decision; the
   `unattended_resume_allowed = false` override is what makes it safe.

---

# §C — documentation

Not optional: both metadata tables appear in diagrams that become **wrong**
with §A, and one of them already carries a ⚠ flagging the very defect §A fixes.

## C.1 `docs/plan/derived-blobs.md` — the relation map (`:105-140`)

The `flowchart` already places EXIF in the wrong layer *and says so*:

```
FMD["<b>storage.file_metadata</b> (EXIF)<br/>file_id PK<br/><i>⚠ content-derived, file-keyed</i>"]
```

Changes:

- **Move `FMD` from `RESOURCE LAYER` to `CONTENT LAYER`**, re-key to
  `source_hash PK`, drop the ⚠ annotation.
- **Replace the edge** `FILES -->|"FK file_id · CASCADE"| FMD` (a solid
  reference-holding arrow) with a **dashed dependent** arrow to the blob layer,
  matching `BET` and `FACES`: `FMD -.->|"dependent cache"| BLOB`.
- **Add `audio.content_metadata`** to the content layer — it is missing from
  this diagram entirely today, which is part of why the mis-keying went
  unnoticed. Same dashed dependent edge.
- The three "things to read off it" below the diagram (`:142-157`) still hold
  verbatim; item 3 ("the two new tables meet the rest of the graph only at the
  Blob layer") now covers four tables.

## C.2 `docs/architecture/derived-and-attached-blobs.md` — the erDiagram (`:22-46`)

Currently models two satellite shapes: `ATTACHED` (per FILE) and `DERIVED` (per
CONTENT), both pointing at an `ARTIFACT` via `blob_hash`.

These tables are a **third shape** and need their own node: per-CONTENT, but
*columns rather than an artifact* — no `blob_hash`, no `ARTIFACT` edge, nothing
to reap but the row itself. Add e.g. `CONTENT_FACTS` with
`BLOBS ||--o{ CONTENT_FACTS : "source_hash — per CONTENT"` and no outgoing
edge, listing `audio.content_metadata` and `storage.file_metadata` (and
arguably `blob_extracted_text` / `faces.faces`, which are the same shape and
equally absent). The prose should state plainly that this shape holds **no**
reference and dies with its source.

## C.3 Smaller doc edits

| file | change |
|---|---|
| `docs/plan/derived-blobs.md` § Keying table (`:191`) | the audio row reads "✅ in principle — but `audio.file_metadata` is **file**-keyed today. See the TODO below". Becomes a plain ✅; add a row for EXIF/image metadata |
| `docs/plan/derived-blobs.md` § "TODO — `audio.file_metadata` is keyed to the file" | mark **DONE** with a date, following the `— **DONE 2026-10-04**` / `— **CLOSED**` convention already used at `:1423` and `:1807`. Keep the security analysis — the forward constraint about user-editable tags outlives the migration |
| `docs/plan/job-registry.md` (`:18-19`, `:103-104`) | both reextract endpoints are listed as "Long-running, blocks HTTP request today", slated for Part 2. Mark replaced by `content_rescan` |
| `docs/architecture/jobs.md` | add `content_rescan` wherever jobs are enumerated. Its "Adding a new job" recipe (`:51-163`) documents only the plain `JobHandler` path and is stale — out of scope here, but worth a pointer to `recoverable.rs:28-38` |
| `docs/architecture/file-and-blob-lifecycle.md:12` | literally says `TODO: draw a mermaid`. Copy/delete semantics change materially (§A.5) — either draw it or leave the TODO; do not leave a *wrong* diagram |
| `src/infrastructure/services/files_consistency_service.rs:47-49` | code comment listing `mime_type_reconciliation` as planned with "requires deciding which is authoritative first" — point it at `content_rescan`, which makes that decision |

## Sequencing

**§0 is done** (`1e41f1b9`, `a7caafab`, `ef6a3243`, `e40dcc38`) — shipped on its
own, as intended, so the regression and its fix stay legible in history rather
than buried in a feature branch.

Next: §A ships independently — A.1/A.2 (migrations + both triggers), A.3
(writers, readers, deletions), A.4 (reap hook) — then §B. A.4 is a hard
prerequisite: the job creates rows corpus-wide.

§C splits across both: C.1/C.2 (the two diagrams) ship **with §A**, since that
is what makes them wrong; the job-registry and `jobs.md` entries ship with §B.
