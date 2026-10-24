-- Re-key EXIF / capture metadata from the FILE to the CONTENT.
--
-- Sibling of `20261104000000_audio_metadata_content_keyed.sql`, same
-- rationale: every column here is a deterministic pure function of the
-- source bytes, which by the keying rule in docs/plan/derived-blobs.md
-- makes them content-addressable. The relation map in that doc has carried
-- a ⚠ on this table for exactly this reason since it was drawn.
--
-- Two things make this bigger than the audio half, both handled below:
-- the `media_sort_date` trigger chain, and the Places viewport query.
--
-- ── Naming: `created_at` is the CONTENT's claimed date ───────────────────
--
-- `captured_at` becomes `created_at`, per docs/plan/normalize-file-dates.md,
-- which already specifies `storage.content_metadata.created_at` as the value
-- `nc:creation_time` reads. The rename is not cosmetic: the column is about
-- to stop being photo-specific. A Word or PDF creation date is the same
-- fact — "when the content says it was created" — and lands in this column
-- when the extractor widens, at which point `captured_at` would be a lie.
--
-- Row bookkeeping is therefore `extracted_at`, not `created_at`, matching
-- `audio.content_metadata`. In a content-keyed table "created" means the
-- content, not the row.

CREATE TABLE IF NOT EXISTS storage.content_metadata (
    source_hash  VARCHAR(64) PRIMARY KEY,

    -- The content's OWN claimed creation date: EXIF DateTimeOriginal today;
    -- a document's creation date once the extractor widens. NULL when the
    -- bytes make no claim, which is the common case.
    created_at   TIMESTAMPTZ,

    latitude     DOUBLE PRECISION,
    longitude    DOUBLE PRECISION,
    camera_make  TEXT,
    camera_model TEXT,
    orientation  SMALLINT,
    width        INTEGER,
    height       INTEGER,

    -- Bookkeeping. See the naming note above for why this is not
    -- `created_at`.
    extracted_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at   TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP
);

COMMENT ON TABLE storage.content_metadata IS
    'Facts parsed out of file content — capture date, GPS, camera, dimensions — keyed by the BLAKE3 of the content. Content-keyed on purpose: identical bytes share one extraction, so N copies of a photo cost one parse and a copy costs nothing. Dependent reference: holds no ref_count and keeps no blob alive; reaped with its source. Not EXIF-specific by design — document creation dates land here too.';

COMMENT ON COLUMN storage.content_metadata.created_at IS
    'The content''s OWN claimed creation date (EXIF DateTimeOriginal; a document creation date once the extractor widens), NOT when the row was written — that is extracted_at. Read by nc:creation_time. See docs/plan/normalize-file-dates.md.';

COMMENT ON COLUMN storage.content_metadata.extracted_at IS
    'When WE parsed these facts — row bookkeeping, not a property of the content.';

-- Places: same partial index, on the new key. Only rows carrying GPS are
-- indexed, so it stays small.
CREATE INDEX IF NOT EXISTS idx_content_metadata_geo
    ON storage.content_metadata (longitude, latitude)
    WHERE latitude IS NOT NULL AND longitude IS NOT NULL;

-- ── Backfill ────────────────────────────────────────────────────────────
--
-- N file-keyed rows collapse to one per distinct content, with a total-order
-- tie-break so the survivor is reproducible rather than whatever the planner
-- emitted. There is no `updated_at` on the old table, so freshness is proxied
-- by `created_at` (its row-insert time) and then by `file_id`.
--
-- Rows whose file has vanished cannot be mapped to content; the join drops
-- them. They were already unreachable — every reader resolves file → hash.
--
-- Recoverable if it goes wrong: these are derived facts, so the table rebuilds
-- from the blobs via `POST /api/admin/photos/metadata/reextract` (later
-- `content_rescan --rederive=always`). The cost of an error is an I/O pass,
-- not lost data.
--
-- ⚠ The timeout lift is not optional. Migrations run on the PRIMARY pool,
-- which sets `statement_timeout = 30s` by default
-- (`OXICLOUD_DB_STATEMENT_TIMEOUT_SECS`, `infrastructure/db.rs`), and this
-- INSERT sorts the whole metadata table — unbounded in corpus size. Exceeding
-- it rolls the migration back and the server never finishes booting, on every
-- restart. That is not hypothetical: `20261017000002_repair_existing_refcount_drift`
-- carries the same lift for the same reason, after it happened to an operator.
-- `SET LOCAL` is transaction-scoped and resets at COMMIT, so later migrations
-- and the running app keep the safety net.
SET LOCAL statement_timeout = 0;

INSERT INTO storage.content_metadata
    (source_hash, created_at, latitude, longitude, camera_make, camera_model,
     orientation, width, height, extracted_at)
SELECT DISTINCT ON (f.blob_hash)
       f.blob_hash, m.captured_at, m.latitude, m.longitude, m.camera_make,
       m.camera_model, m.orientation, m.width, m.height, m.created_at
  FROM storage.file_metadata m
  JOIN storage.files f ON f.id = m.file_id
 ORDER BY f.blob_hash, m.created_at DESC, m.file_id
ON CONFLICT (source_hash) DO NOTHING;

DO $$
DECLARE
    old_rows BIGINT;
    new_rows BIGINT;
BEGIN
    SELECT count(*) INTO old_rows FROM storage.file_metadata;
    SELECT count(*) INTO new_rows FROM storage.content_metadata;

    IF old_rows > 0 THEN
        RAISE NOTICE
            'EXIF metadata re-keyed to content: % file-keyed row(s) collapsed to % content row(s).',
            old_rows, new_rows;
    END IF;
END
$$;

-- ── Trigger 1: repoint onto the content-keyed table ─────────────────────
--
-- Was `AFTER INSERT OR UPDATE OF captured_at ON storage.file_metadata`,
-- updating the single file it belonged to. Now one content row backs N
-- files, so the UPDATE fans out on `blob_hash`.
--
-- Still an AFTER trigger issuing its own UPDATE — it reacts to a row in a
-- different table and has no NEW of `storage.files` to assign to. That
-- nested UPDATE remains what `pg_trigger_depth() > 1` in
-- `bump_tree_from_files_stmt_upd()` exists to ignore
-- (`20260626000000_tree_etag_statement_triggers.sql`); unchanged.
--
-- Write amplification is bounded by the dedup factor, and is paid once per
-- EXTRACTION rather than once per file — strictly less total work than
-- before, where every copy re-extracted and re-fired this trigger.
--
-- The media gate is not decoration. Content is shared across files
-- regardless of their name or type, so an unfiltered fan-out would also
-- rewrite `media_sort_date` on, say, a `.txt` holding the same bytes —
-- a column no reader consults for it (every timeline index is partial on
-- the same two prefixes) at the cost of a row write and a tree-ETag
-- enqueue. Same predicate as the INSERT arm below, so the two agree on
-- which files the column is meaningful for.
CREATE OR REPLACE FUNCTION storage.sync_media_sort_date()
RETURNS trigger AS $$
BEGIN
    UPDATE storage.files
       SET media_sort_date = COALESCE(NEW.created_at, content_modified_at)
     WHERE blob_hash = NEW.source_hash
       AND (mime_type LIKE 'image/%' OR mime_type LIKE 'video/%');
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

COMMENT ON FUNCTION storage.sync_media_sort_date() IS
    'Syncs storage.files.media_sort_date when content metadata is written. '
    'Fans out over every image/video file sharing that content. Falls back to '
    'content_modified_at (NOT upload time) so content with no claimed date '
    'sorts by when its bytes last changed. See docs/architecture/dates.md.';

-- ⚠ ORDERING IS LOAD-BEARING: this trigger is created AFTER the backfill,
-- never before. Created first, every backfilled row would fire a fan-out
-- UPDATE over `storage.files` — a corpus-wide write, inside the boot
-- transaction, each one also enqueueing an ancestor tree-ETag bump. It would
-- also be pure waste: those `media_sort_date` values are already correct,
-- written by the old file-keyed trigger when the metadata was first
-- extracted. As written the migration fires no trigger at all.
--
-- The one case it leaves behind is pre-existing: files created by the
-- recursive-copy SQL path, which ran no lifecycle hook and so never got a
-- metadata row, still carry `media_sort_date = copy time`. Content-keying
-- fixes that going forward (the INSERT trigger below), but correcting the
-- existing rows is a corpus-wide UPDATE and belongs to `content_rescan`
-- (docs/plan/blob-resniff.md §B), not to boot.
DROP TRIGGER IF EXISTS trg_sync_media_sort_date ON storage.file_metadata;
DROP TRIGGER IF EXISTS trg_sync_media_sort_date ON storage.content_metadata;
CREATE TRIGGER trg_sync_media_sort_date
    AFTER INSERT OR UPDATE OF created_at ON storage.content_metadata
    FOR EACH ROW EXECUTE FUNCTION storage.sync_media_sort_date();

-- ── Trigger 2: same shape, reading the new table ────────────────────────
CREATE OR REPLACE FUNCTION storage.sync_media_sort_date_from_content()
RETURNS trigger AS $$
BEGIN
    NEW.media_sort_date := COALESCE(
        (SELECT m.created_at
           FROM storage.content_metadata m
          WHERE m.source_hash = NEW.blob_hash),
        NEW.content_modified_at
    );
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

-- ── Trigger 2 now fires on INSERT too, and that is NOT cosmetic ─────────
--
-- `20261103000007` argued no INSERT trigger was needed:
--
--     "on insert content_modified_at defaults to NOW() and media_sort_date
--      to CURRENT_TIMESTAMP, so a fresh row already satisfies the chain.
--      A later EXIF extraction fires trigger 1 and overwrites with
--      captured_at."
--
-- That reasoning depended on file-keying, and content-keying removes its
-- second half. A new file on ALREADY-KNOWN content produces no metadata
-- upsert at all — the row is already there — so trigger 1 never fires and
-- the file keeps `media_sort_date = NOW()`. Every copy of a photo, and
-- every re-upload of one already in the system, would land in the timeline
-- at upload date instead of its capture date, silently and permanently.
--
-- Extending trigger 2 to INSERT closes it with no new function: column
-- defaults are applied before BEFORE-INSERT triggers run, so the COALESCE
-- sees a populated `content_modified_at`, and `blob_hash` is NOT NULL.
--
-- TWO triggers, one function, and it has to be two: a WHEN clause cannot
-- test `TG_OP` (that is a PL/pgSQL variable, not available to the planner
-- here), and `OLD` does not exist for the INSERT case, so the two
-- conditions cannot be folded into one combined `INSERT OR UPDATE` trigger.
--
-- The INSERT arm's WHEN keeps the probe off the non-media upload path. It
-- costs one PK lookup into content_metadata per media INSERT, which matters
-- because folder copy inserts in bulk. The prefix test duplicates no mapping
-- table — it is the same two prefixes `MediaMetadataService::handles` uses,
-- and a false negative only means the file sorts by content_modified_at,
-- which is the fallback it would have had anyway.
DROP TRIGGER IF EXISTS trg_sync_media_sort_date_content ON storage.files;
CREATE TRIGGER trg_sync_media_sort_date_content
    BEFORE UPDATE OF content_modified_at ON storage.files
    FOR EACH ROW
    WHEN (NEW.content_modified_at IS DISTINCT FROM OLD.content_modified_at)
    EXECUTE FUNCTION storage.sync_media_sort_date_from_content();

DROP TRIGGER IF EXISTS trg_sync_media_sort_date_insert ON storage.files;
CREATE TRIGGER trg_sync_media_sort_date_insert
    BEFORE INSERT ON storage.files
    FOR EACH ROW
    WHEN (NEW.mime_type LIKE 'image/%' OR NEW.mime_type LIKE 'video/%')
    EXECUTE FUNCTION storage.sync_media_sort_date_from_content();

DROP TABLE storage.file_metadata;
