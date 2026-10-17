-- ════════════════════════════════════════════════════════════════════════════
-- §1 (normalize-file-dates) — `storage.files.content_modified_at`: POSIX mtime
-- ════════════════════════════════════════════════════════════════════════════
-- `updated_at` currently does the work of three timestamps: it is ctime (any
-- row touch), it feeds `D:getlastmodified`, and it is the suffix in the file
-- ETag. So renaming a file re-downloads it on every client that had it. This
-- column separates "the bytes last changed" from "the row last changed".
--
-- Design and rollout: `docs/plan/normalize-file-dates.md` §1, semantics in
-- `docs/architecture/dates.md`. Nothing READS this column yet — §2 repoints the
-- reads, and until §5 they go through `COALESCE(content_modified_at,
-- updated_at)` so the rollout is invisible to clients.
--
-- ── Why two statements, and why in this order ───────────────────────────────
-- `ADD COLUMN … DEFAULT NOW()` in one statement would be wrong: since PG 11 an
-- added column with a default makes EXISTING rows read as that default without
-- a rewrite. Every pre-existing file would then report "modified at upgrade
-- time", which under §2 changes every ETag simultaneously and triggers a
-- fleet-wide re-download — the exact outcome this plan is built to avoid.
--
-- Applying `SET DEFAULT` afterwards is metadata for SUBSEQUENT inserts only and
-- does not touch existing rows. So:
--
--   * existing rows  → NULL, backfilled from `updated_at` by a cursor-paged
--                      job, reading as `updated_at` in the meantime (identical
--                      to today's value, hence inert)
--   * new rows       → NOW() automatically, without editing any of the ten-odd
--                      `INSERT INTO storage.files` sites
--   * overwrites     → set explicitly by `swap_blob_hash`, honouring
--                      `X-OC-Mtime` when the client sent it
--
-- Deliberately NOT NULL-able yet: §5 tightens it once the backfill has drained
-- fleet-wide, and only then does the `COALESCE` come out of the read paths.
-- ════════════════════════════════════════════════════════════════════════════

ALTER TABLE storage.files
    ADD COLUMN IF NOT EXISTS content_modified_at TIMESTAMPTZ;

ALTER TABLE storage.files
    ALTER COLUMN content_modified_at SET DEFAULT NOW();

-- Defence in depth for the client-asserted lower bound. The application
-- validates `X-OC-Mtime` first and returns 400 on violation (never clamps), so
-- this should be unreachable; it exists so a future write path that forgets the
-- check cannot persist a 1970 timestamp. The "not more than one hour in the
-- future" half of the trust bounds cannot live here — it is not immutable.
ALTER TABLE storage.files
    DROP CONSTRAINT IF EXISTS files_content_modified_at_floor;

ALTER TABLE storage.files
    ADD CONSTRAINT files_content_modified_at_floor
    CHECK (content_modified_at IS NULL OR content_modified_at > '2000-01-01Z');

COMMENT ON COLUMN storage.files.content_modified_at IS
    'POSIX mtime: when the BYTES last changed. Bumped only by content-mutation paths (PUT, chunked complete, delta commit, upload overwrite), never by rename/move/trash/grant/favourite — those bump updated_at alone. Client-assertable via X-OC-Mtime within the trust bounds. NULL means not yet backfilled; read as COALESCE(content_modified_at, updated_at) until docs/plan/normalize-file-dates.md §5.';
