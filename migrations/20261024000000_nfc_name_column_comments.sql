-- COMMENT ON COLUMN for every user-visible-name column whose invariant
-- ("stored bytes are NFC") is enforced by the write repository, not by
-- the DB itself.
--
-- Why this migration exists
-- ─────────────────────────
-- Before 2026-09-04 the NFC invariant lived at `File::new` /
-- `Folder::new_folder` entity constructors — plausible-looking but
-- DEAD CODE for the create path, because every real caller went
-- straight from a DTO string to `sqlx::bind()` inside the repos
-- without ever constructing the entity first. Result: 22 audited
-- entry points, every single one shipped raw client input to the DB.
-- macOS Finder / DAVX5 / NC-desktop uploads landed NFD; NFC-
-- normalizing clients then failed to find their own content by URL
-- (AtalayaLabs/OxiCloud#706).
--
-- The fix moved normalization to the repository methods that own the
-- INSERT / UPDATE. The next contributor writing a new write surface
-- may reasonably wonder where to enforce the invariant — this comment
-- puts the answer next to the column so grep-hunting the codebase is
-- not required. Purely documentation; no runtime effect. A stronger
-- form (CHECK CONSTRAINT `name = normalize(name, NFC)`) was
-- considered and rejected for now — that would rely on every
-- historical row already being NFC (which we deliberately do NOT
-- migrate on read, so pre-fix rows stay in place until an operator
-- runs `oxicloud migrate nfc-filenames`), and would fail-boot any
-- upgrade path where the migrate has not yet been applied.
--
-- Idempotent. COMMENT ON COLUMN replaces any prior comment on the
-- same target, so re-running has no effect.

COMMENT ON COLUMN storage.files.name IS
    'User-visible file name. MUST be NFC (Unicode Normalization Form C). '
    'Invariant enforced at write time by '
    'src/infrastructure/repositories/pg/file_blob_write_repository.rs — the '
    '`save_file_with_blob_impl`, `copy_file`, `rename_file`, '
    '`register_file_deferred`, and `copy_folder_tree` methods each call '
    '`normalize_storage_name(_owned)` before binding. No DB-level CHECK '
    'constraint (historical NFD rows may still exist on pre-2026-09-04 '
    'databases until `oxicloud migrate nfc-filenames` is run). New write '
    'surfaces MUST land in one of those repo methods; direct INSERT '
    'bypasses the invariant.';

COMMENT ON COLUMN storage.folders.name IS
    'User-visible folder name. MUST be NFC (Unicode Normalization Form C). '
    'Invariant enforced at write time by '
    'src/infrastructure/repositories/pg/folder_db_repository.rs — the '
    '`create_folder` and `rename_folder` methods each call '
    '`normalize_storage_name_owned` before binding. See also '
    'storage.files.name — identical contract, different table. No DB-level '
    'CHECK (see that column comment). New write surfaces MUST land in one of '
    'those repo methods; direct INSERT bypasses the invariant.';
