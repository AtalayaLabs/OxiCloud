-- §3 — Repoint `media_sort_date` at the content clock.
--
-- `media_sort_date` is the denormalised sort key behind the Photos timeline.
-- It is deliberately a stored column rather than a query-time expression:
-- `idx_files_media_timeline_by_drive (drive_id, media_sort_date DESC)` lets
-- `LIMIT` stop the scan early, which re-deriving a COALESCE over a join to
-- `storage.file_metadata` could not do.
--
-- The target chain (see `docs/architecture/dates.md` § Sort axes) is:
--
--     media_sort_date = COALESCE(captured_at, source_created_at, content_modified_at)
--
-- `source_created_at` does not exist yet (plan §4 is blocked on confirming
-- NextCloud's `nc:creation_time` write semantics), so this migration lands the
-- two-arm interim form and §4 inserts the middle arm into the same two
-- functions below. `content_modified_at` terminates the chain because §1 made
-- it NOT NULL, and `media_sort_date` is itself NOT NULL — the chain has to end
-- on a column that always has a value.
--
-- ── What changes for users ─────────────────────────────────────────────────
-- The previous fallback was `created_at`, i.e. upload time:
--
--     SET media_sort_date = COALESCE(NEW.captured_at, created_at)
--
-- So an image with no EXIF `captured_at` sorted by when it was uploaded,
-- permanently, however often its content changed afterwards. Repointing to
-- `content_modified_at` moves exactly that set of files: EXIF-less media that
-- has been modified since upload. Files WITH `captured_at` do not move — the
-- first arm wins and is untouched.
--
-- This is a visible one-time reorder of the Photos timeline, which is why the
-- backfill runs here rather than being left to arrive lazily as files are
-- touched. Lazy arrival would be worse than either end state: the timeline
-- would mix untouched rows ordered on upload time with touched rows ordered on
-- content time, which is not an ordering at all.

-- ── 1. Existing trigger on file_metadata: swap the fallback ────────────────
-- Still fires on `captured_at` only, and still an AFTER trigger issuing its own
-- UPDATE — it has to, because it is reacting to a row in a different table.
-- That nested UPDATE is what `pg_trigger_depth() > 1` in
-- `bump_tree_from_files_stmt_upd()` exists to ignore
-- (`20260626000000_tree_etag_statement_triggers.sql`); unchanged here.
CREATE OR REPLACE FUNCTION storage.sync_media_sort_date()
RETURNS trigger AS $$
BEGIN
    UPDATE storage.files
       SET media_sort_date = COALESCE(NEW.captured_at, content_modified_at)
     WHERE id = NEW.file_id;
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

COMMENT ON FUNCTION storage.sync_media_sort_date() IS
    'Syncs storage.files.media_sort_date when EXIF captured_at is written. '
    'Falls back to content_modified_at (NOT the upload time) so an EXIF-less '
    'image sorts by when its content last changed. See docs/architecture/dates.md.';

-- ── 2. New trigger on storage.files: the second input to the chain ─────────
-- The chain now has an input on `storage.files` itself, so it needs a second
-- trigger rather than a widened first one.
--
-- BEFORE + assignment to NEW, not AFTER + UPDATE, and the distinction is
-- load-bearing:
--
--   * It writes no second statement, so it cannot re-enter the two
--     `FOR EACH STATEMENT` transition-table triggers on this table. The
--     `file_metadata` trigger above needs `pg_trigger_depth()` to be ignored;
--     this form never creates the depth to be ignored.
--   * An AFTER trigger updating the same row it fired for would recurse.
--
-- Cost is one index lookup into `file_metadata` per content mutation, to read
-- `captured_at`. Content mutations are rare next to reads, and the alternative
-- (recompute at query time) was rejected above for defeating the timeline
-- index.
--
-- No INSERT trigger is needed: on insert `content_modified_at` defaults to
-- NOW() and `media_sort_date` to CURRENT_TIMESTAMP, so a fresh row already
-- satisfies the chain. A later EXIF extraction fires trigger 1 and overwrites
-- with `captured_at`, which is the first arm and correctly wins.
CREATE OR REPLACE FUNCTION storage.sync_media_sort_date_from_content()
RETURNS trigger AS $$
BEGIN
    NEW.media_sort_date := COALESCE(
        (SELECT m.captured_at
           FROM storage.file_metadata m
          WHERE m.file_id = NEW.id),
        NEW.content_modified_at
    );
    RETURN NEW;
END;
$$ LANGUAGE plpgsql;

COMMENT ON FUNCTION storage.sync_media_sort_date_from_content() IS
    'Recomputes media_sort_date when a file content mutation moves '
    'content_modified_at. BEFORE-trigger assignment to NEW, so it adds no '
    'statement and cannot re-enter the tree-etag/search statement triggers.';

-- PG 13 compatibility: DROP-then-CREATE, no CREATE OR REPLACE TRIGGER.
DROP TRIGGER IF EXISTS trg_sync_media_sort_date_content ON storage.files;
CREATE TRIGGER trg_sync_media_sort_date_content
    BEFORE UPDATE OF content_modified_at ON storage.files
    FOR EACH ROW
    WHEN (NEW.content_modified_at IS DISTINCT FROM OLD.content_modified_at)
    EXECUTE FUNCTION storage.sync_media_sort_date_from_content();

-- ── 3. Backfill ───────────────────────────────────────────────────────────
-- Both `FOR EACH STATEMENT` triggers on `storage.files` filter in-function on
-- column tuples that exclude `media_sort_date`:
--
--   files_bump_tree_etag_upd  → (name, folder_id, blob_hash, size, mime_type,
--                                is_trashed, updated_at)
--   files_enqueue_search_upd  → (name, blob_hash, is_trashed)
--
-- so neither would take an action for this statement. But the filtering happens
-- only after Postgres has materialised the OLD and NEW transition tables over
-- every updated row, per trigger — the same per-statement blow-up §1's
-- migration documents. Suppress both for the duration.
--
-- DDL is transactional in Postgres, so a failure anywhere in this migration
-- rolls back the ENABLE/DISABLE along with everything else; there is no window
-- in which the triggers are left off.
ALTER TABLE storage.files DISABLE TRIGGER files_bump_tree_etag_upd;
ALTER TABLE storage.files DISABLE TRIGGER files_enqueue_search_upd;

-- Split by arm rather than one LEFT JOIN, because `UPDATE … FROM` cannot
-- outer-join its own target table without a self-join that reads worse than
-- two plain statements. `file_metadata.file_id` is that table's PRIMARY KEY, so
-- the join below is 1:1 and no row can be updated twice.
--
-- `IS DISTINCT FROM` on both keeps them idempotent and narrows the write set to
-- rows that actually move: a re-applied or resumed migration rewrites nothing,
-- and a file whose `media_sort_date` already equals the new chain value — every
-- EXIF photo, and every file never modified since upload — is never touched.

-- Arm 1: files with EXIF metadata. `captured_at` may still be NULL here (a
-- metadata row exists but carries no DateTimeOriginal), hence the COALESCE
-- rather than a straight assignment.
UPDATE storage.files f
   SET media_sort_date = COALESCE(m.captured_at, f.content_modified_at)
  FROM storage.file_metadata m
 WHERE m.file_id = f.id
   AND f.media_sort_date IS DISTINCT FROM COALESCE(m.captured_at, f.content_modified_at);

-- Arm 2: files with no metadata row at all — the set that actually moves,
-- since these were pinned to upload time.
UPDATE storage.files f
   SET media_sort_date = f.content_modified_at
 WHERE NOT EXISTS (SELECT 1 FROM storage.file_metadata m WHERE m.file_id = f.id)
   AND f.media_sort_date IS DISTINCT FROM f.content_modified_at;

ALTER TABLE storage.files ENABLE TRIGGER files_bump_tree_etag_upd;
ALTER TABLE storage.files ENABLE TRIGGER files_enqueue_search_upd;

COMMENT ON COLUMN storage.files.media_sort_date IS
    'Denormalised Photos-timeline sort key: COALESCE(captured_at, '
    'content_modified_at). Stored rather than derived so '
    'idx_files_media_timeline_by_drive can serve ORDER BY … LIMIT without a '
    'join. Maintained by trg_sync_media_sort_date (file_metadata) and '
    'trg_sync_media_sort_date_content (files). Plan §4 inserts a '
    'source_created_at arm between the two. See docs/architecture/dates.md.';
