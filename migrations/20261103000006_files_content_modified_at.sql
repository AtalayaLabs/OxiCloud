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
--   * existing rows  → backfilled from `updated_at` below, which is exactly
--                      what every surface reports today, so the rollout is
--                      inert: no reported mtime and no ETag changes
--   * new rows       → NOW() automatically, without editing any of the ten-odd
--                      `INSERT INTO storage.files` sites
--   * overwrites     → set explicitly by `swap_blob_hash`, honouring
--                      `X-OC-Mtime` when the client sent it
--
-- ── Why the backfill is here and not a background job ───────────────────────
-- The plan originally specified a cursor-paged job to avoid delaying boot. That
-- trade is wrong for self-hosted software: a job's drain state cannot be
-- confirmed across deployments we do not operate, so §5 (`SET NOT NULL`, drop
-- the `COALESCE`) would never be safely reachable and the fallback expression
-- would live in the ETag and read paths indefinitely.
--
-- The `UPDATE` costs one full-table rewrite, once, on upgrade. This database is
-- metadata only — file content lives in blobs — so the row count is modest even
-- at the 10k-user target. Paying seconds of boot once buys a column that is
-- dense the moment this migration completes, which makes §5 a certainty rather
-- than an operational hope.
--
-- Operators with an unusually large `storage.files` should know this upgrade
-- step is proportional to row count; if a container healthcheck is tight enough
-- to crash-loop on it, raise the start-up grace period for this one deploy.
-- ════════════════════════════════════════════════════════════════════════════

ALTER TABLE storage.files
    ADD COLUMN IF NOT EXISTS content_modified_at TIMESTAMPTZ;

ALTER TABLE storage.files
    ALTER COLUMN content_modified_at SET DEFAULT NOW();

-- Backfill from `updated_at`, NOT `created_at`. `updated_at` is precisely what
-- `D:getlastmodified`, `FileDto.modified_at` and the ETag suffix resolve to
-- today, so after this every one of them is byte-identical to before. Using
-- `created_at` would report an OLDER mtime than today for any file touched
-- since upload — and a sync client seeing the server copy older than its local
-- one may answer with an upload or a conflict rather than a download.
--
-- `WHERE content_modified_at IS NULL` keeps this idempotent: a re-applied
-- migration (or one resumed after a failure) skips rows already written, and
-- never clobbers a value `swap_blob_hash` has since set.
UPDATE storage.files
   SET content_modified_at = updated_at
 WHERE content_modified_at IS NULL;

-- Dense as of the statement above, so tighten now rather than in a later
-- phase. sqlx runs a migration in one transaction, so the backfill and this
-- constraint apply together or not at all — there is no window in which the
-- column is declared NOT NULL while a row is still NULL.
--
-- This is what makes the whole plan simpler: §2's reads can name the column
-- directly instead of `COALESCE(content_modified_at, updated_at)`, so no
-- fallback expression ever enters the ETag or the hot read path, and the
-- separate "tighten later" phase is unnecessary.
--
-- The DEFAULT above stays deliberately. It is what lets the ten-odd
-- `INSERT INTO storage.files` sites keep working without naming the column,
-- and `NOW()` is the *correct* content-mtime for a newly created file — not a
-- silent stand-in for a value somebody forgot to pass.
ALTER TABLE storage.files
    ALTER COLUMN content_modified_at SET NOT NULL;

-- Defence in depth for the client-asserted lower bound. The application
-- validates `X-OC-Mtime` first and returns 400 on violation (never clamps), so
-- this should be unreachable; it exists so a future write path that forgets the
-- check cannot persist a 1970 timestamp. The "not more than one hour in the
-- future" half of the trust bounds cannot live here — it is not immutable.
ALTER TABLE storage.files
    DROP CONSTRAINT IF EXISTS files_content_modified_at_floor;

ALTER TABLE storage.files
    ADD CONSTRAINT files_content_modified_at_floor
    CHECK (content_modified_at > '2000-01-01Z');

COMMENT ON COLUMN storage.files.content_modified_at IS
    'POSIX mtime: when the BYTES last changed. Bumped only by content-mutation paths (PUT, chunked complete, delta commit, upload overwrite), never by rename/move/trash/grant/favourite — those bump updated_at alone. Client-assertable via X-OC-Mtime within the trust bounds. NULL means not yet backfilled; read as COALESCE(content_modified_at, updated_at) until docs/plan/normalize-file-dates.md §5.';
