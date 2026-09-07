-- Release the blob reference when an attachment row goes away.
--
-- `storage.file_attached_blobs.file_id` is `ON DELETE CASCADE`, so deleting a
-- file removes its attachment rows inside the database — invisible to Rust.
-- The lifecycle hook cannot cover this: `on_file_deleted` fires AFTER
-- `delete_file`, by which point the cascade has already run and there is
-- nothing left to read. The references would survive with no row behind them,
-- and `dedup_gc` would see a positive count forever — bytes pinned for good.
--
-- `storage.decrement_blob_ref()` already exists for exactly this, on
-- `storage.files`. It keys off `OLD.blob_hash` and is otherwise
-- table-agnostic, so it applies verbatim — and reusing it keeps the
-- manifest-first decrement contract defined in one place rather than
-- transcribed into a second trigger that can drift.
--
-- Only DELETE. Replacing a preview updates `blob_hash` in place
-- (`store_attached_blob` is ON CONFLICT DO UPDATE), and the reference to the
-- superseded blob is released there, in Rust. Adding UPDATE here would
-- double-decrement it.

CREATE OR REPLACE TRIGGER trg_file_attached_blobs_decrement_blob_ref
    AFTER DELETE ON storage.file_attached_blobs
    FOR EACH ROW
    EXECUTE FUNCTION storage.decrement_blob_ref();

COMMENT ON TRIGGER trg_file_attached_blobs_decrement_blob_ref
    ON storage.file_attached_blobs IS
    'Releases the blob reference held by an attachment row. Needed because file_id is ON DELETE CASCADE, so rows vanish inside the DB where the Rust lifecycle hooks cannot see them.';
