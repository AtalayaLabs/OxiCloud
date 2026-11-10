-- ════════════════════════════════════════════════════════════════════════════
-- Recently modified — §1 `content_modified_by` + §1b `file_content_touches`
-- ════════════════════════════════════════════════════════════════════════════
-- Design: `docs/plan/recently-modified.md` §1 / §1b. Semantics of the two file
-- clocks: `docs/architecture/dates.md`.
--
-- Two things land together because they are written together (one transaction
-- per content mutation) and because neither is useful alone:
--
--   1. `storage.files.content_modified_by` — WHO last changed the bytes.
--      `updated_by` cannot answer this: it means "who last touched the row",
--      which includes rename, move and restore. Alice edits a file, Bob
--      renames it, and `updated_by` credits Bob for Alice's edit.
--
--   2. `storage.file_content_touches` — one row per (file, person), holding
--      that person's LATEST content change to that file. The column above is
--      last-writer-wins, so on its own it hides a coworker's edit the moment
--      anyone else saves; this table is what makes "the file Bob worked on
--      yesterday" findable after Carol has saved it today.
--
-- Nothing READS either yet. The read path (`/api/files/recent-changes`) lands
-- behind `OXICLOUD_ENABLE_RECENT_CHANGES` in a later step; these are written
-- from the first deploy so the view has history on the day it is switched on.
-- ════════════════════════════════════════════════════════════════════════════


-- ── §1 — storage.files.content_modified_by ─────────────────────────────────
-- Nullable with `ON DELETE SET NULL`, exactly like `created_by` / `updated_by`
-- (`20260802100000_drives_schema_additive.sql` §3): deleting a user nulls the
-- provenance rather than cascading the file away. NULL therefore carries two
-- meanings — "that account is gone" and "unknown, pre-backfill" — and both
-- render as Unknown rather than being guessed at.
--
-- Deliberately NOT `NOT NULL`. Unlike `content_modified_at`, there is no
-- truthful value for every historical row, and a column that lies is worse
-- than one that admits what it does not know.
ALTER TABLE storage.files
    ADD COLUMN IF NOT EXISTS content_modified_by UUID
        REFERENCES auth.users(id) ON DELETE SET NULL;

COMMENT ON COLUMN storage.files.content_modified_by IS
    'WHO last changed the bytes — the principal half of content_modified_at. '
    'Written by exactly the paths that write that clock (PUT, chunked '
    'complete, delta commit, upload overwrite, create, copy) and by no other; '
    'rename/move/trash/restore/grant/favourite leave it alone, as they leave '
    'the clock alone. NOT updated_by, which means "who last touched the row" '
    'and would credit a renamer for someone else''s edit. NULL means the '
    'account was deleted, or the row predates the backfill. Every path that '
    'writes this must also upsert storage.file_content_touches in the same '
    'transaction — see docs/plan/recently-modified.md §2.';


-- Backfill. Two arms, and only the first is truthful:
--
--   * `content_modified_at = created_at` — the file has never been overwritten
--     since upload, so the uploader IS the content author. After
--     `20261103000006` backfilled `content_modified_at := updated_at`, this
--     equality holds for every row nobody has touched since creation.
--
--   * otherwise `updated_by` — a documented APPROXIMATION. It is wrong exactly
--     for rows last renamed or moved by someone other than the last content
--     author, and both of those stamp `updated_by` per file row. Accepted as a
--     starting point: it self-limits because the view's window excludes most
--     historical rows, and any row that gets a real content change from then
--     on corrects itself.
--
-- `WHERE content_modified_by IS NULL` keeps it idempotent — a re-applied or
-- resumed migration skips rows already written and never clobbers a value a
-- write path has since set.
--
-- ── Why the statement triggers are suppressed around it ────────────────────
-- `storage.files` carries two `AFTER UPDATE … REFERENCING OLD TABLE … NEW
-- TABLE … FOR EACH STATEMENT` triggers:
--
--   files_bump_tree_etag_upd    → bump_tree_from_files_stmt_upd()
--   files_enqueue_search_upd    → enqueue_search_from_files_stmt_upd()
--
-- Neither watches `content_modified_by`, so neither would enqueue anything.
-- But Postgres materialises the OLD and NEW transition tables for the WHOLE
-- statement, per trigger, BEFORE that filtering happens — so one bulk UPDATE
-- builds two full copies of the table twice over, spilling to temp files, to
-- conclude there is nothing to do. `20261103000006` hit exactly this.
--
-- Disabling both for the duration is precise rather than a shortcut: we know
-- from the column tuples above that neither trigger had an action to take. DDL
-- is transactional in Postgres, so a failure anywhere in this migration
-- restores the triggers along with everything else — there is no window in
-- which they are left disabled.
ALTER TABLE storage.files DISABLE TRIGGER files_bump_tree_etag_upd;
ALTER TABLE storage.files DISABLE TRIGGER files_enqueue_search_upd;

UPDATE storage.files
   SET content_modified_by = CASE
         WHEN content_modified_at = created_at THEN created_by
         ELSE updated_by
       END
 WHERE content_modified_by IS NULL;

ALTER TABLE storage.files ENABLE TRIGGER files_bump_tree_etag_upd;
ALTER TABLE storage.files ENABLE TRIGGER files_enqueue_search_upd;


-- ── §1b — storage.file_content_touches ─────────────────────────────────────
-- NOT an event log. One row per (file, person) holding that person's latest
-- content change, which is what makes the table's size a function of
-- files × distinct editors rather than of edit count:
--
--   * a WOPI or collab autosave storm rewrites one row instead of appending
--     thousands, so there is no coalescing window to tune;
--   * `COUNT(*)` per `user_id` IS the number of files that person changed — no
--     DISTINCT, and no file counted forty times because someone kept saving;
--   * `user_id NOT NULL` + `ON DELETE CASCADE` means there is no "Unknown"
--     lane, which removes a keyset-pagination trap: a nullable sort key makes
--     `lane_id > $cursor` evaluate to NULL, so that lane either repeats
--     forever or vanishes at a page boundary.
--
-- Deleting a touch row is never data loss: the file, `content_modified_at` and
-- `content_modified_by` are all untouched. This table is a cache of recency,
-- and the retention sweep is its ceiling.
--
-- No `drive_id` here, deliberately. Denormalising it would give the access
-- filter a leading index key, at the cost of a value that goes stale the first
-- time a file moves between drives (D6). The access filter reaches files by
-- primary key instead.
CREATE TABLE IF NOT EXISTS storage.file_content_touches (
    file_id    UUID        NOT NULL REFERENCES storage.files(id) ON DELETE CASCADE,
    user_id    UUID        NOT NULL REFERENCES auth.users(id)    ON DELETE CASCADE,
    touched_at TIMESTAMPTZ NOT NULL,
    PRIMARY KEY (file_id, user_id)
);

COMMENT ON TABLE storage.file_content_touches IS
    'One row per (file, person): that person''s LATEST content change to that '
    'file. Upserted in the same transaction as every content mutation, which '
    'is what keeps it in step with storage.files.content_modified_by. Powers '
    'GET /api/files/recent-changes — "who worked on what lately" — which the '
    'last-writer-wins column cannot answer once a second person saves. Swept '
    'by the file_touches_cleanup job; a deleted row costs recency, never data.';

COMMENT ON COLUMN storage.file_content_touches.touched_at IS
    'Same value as the file''s content_modified_at at that moment, including a '
    'client-asserted X-OC-Mtime, so the feed and the file''s own mtime never '
    'disagree about when a change happened. NOT a monotonic sequence and not '
    'usable as a sync cursor — see docs/plan/recently-modified.md §8.';

-- Lanes and the `?person=` drill-in: "this person's changes, newest first".
CREATE INDEX IF NOT EXISTS idx_touches_user_touched
    ON storage.file_content_touches (user_id, touched_at DESC);

-- The chronological axis and the retention sweep both walk the clock alone.
CREATE INDEX IF NOT EXISTS idx_touches_touched
    ON storage.file_content_touches (touched_at DESC);


-- Seed, so the view is not empty on the first deploy: one touch per file from
-- the pair backfilled above, bounded by the retention window the sweep will
-- enforce (120 days, deliberately wider than the 90-day query maximum).
--
-- This inherits the backfill's approximation and claims no more than it can:
-- one seeded touch per file, crediting the last known principal. It never
-- invents a second editor, so a pre-deploy file shows a single lane until
-- someone touches it again — the table becomes accurate as people work.
--
-- `ON CONFLICT DO NOTHING` for idempotence on a resumed migration. No trigger
-- suppression needed: the target is a brand-new table with no triggers, and
-- reading `storage.files` fires none.
INSERT INTO storage.file_content_touches (file_id, user_id, touched_at)
SELECT id, content_modified_by, content_modified_at
  FROM storage.files
 WHERE content_modified_by IS NOT NULL
   AND NOT is_trashed
   AND content_modified_at > NOW() - INTERVAL '120 days'
ON CONFLICT DO NOTHING;
