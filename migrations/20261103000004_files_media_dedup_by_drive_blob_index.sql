-- ════════════════════════════════════════════════════════════════════════════
-- §9 (photos-resources-migration) — composite partial index for the
-- within-drive DISTINCT ON (blob_hash) dedup
-- ════════════════════════════════════════════════════════════════════════════
-- The `list_media_resources` lateral probe now wraps its scan in
-- `SELECT DISTINCT ON (fi.blob_hash) …
--     ORDER BY fi.blob_hash, fi.media_sort_date DESC, fi.id DESC`
-- so a user who copied the same photo across folders sees exactly one
-- representative tile per unique blob within the drive. The existing
-- partial covering index on `(drive_id, media_sort_date DESC)` already
-- carries every row the query needs, but its sort axis doesn't match
-- the DISTINCT ON's leading key, so without this index Postgres falls
-- back to an in-memory sort of every matching row before it can emit
-- the first-row-per-group.
--
-- The leading `(drive_id, blob_hash)` lets the planner do a loose
-- index scan — one seek per unique (drive, blob) pair — and the
-- trailing `(media_sort_date DESC, id DESC)` aligns with the DISTINCT
-- ON's per-group ORDER BY so the newest row per blob is at the head
-- of each group with no extra sorting. Partial WHERE mirrors
-- `idx_files_media_timeline_by_drive` exactly: only non-trashed
-- image/video rows, keeping the index narrow.
--
-- Shipped alongside §9 on 2026-10-08 so the dedup query is
-- well-planned from day one rather than earned later via an EXPLAIN
-- regression.

CREATE INDEX IF NOT EXISTS idx_files_media_dedup_by_drive_blob
    ON storage.files (drive_id, blob_hash, media_sort_date DESC, id DESC)
    WHERE NOT is_trashed
      AND (mime_type LIKE 'image/%' OR mime_type LIKE 'video/%');
