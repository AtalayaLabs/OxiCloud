-- §8 — Give folders a content clock with POSIX directory semantics.
--
-- A folder's "content" is its entry list. This column moves when a DIRECT
-- child is added, removed or renamed — and NOT when a child's bytes change,
-- and NOT for anything deeper in the subtree. That is exactly what POSIX and
-- NTFS mean by a directory mtime, and it is what `ls -l` shows.
--
-- ── Why not reuse tree_modified_at ─────────────────────────────────────────
-- `storage.folders.tree_modified_at` already exists and already answers "did
-- anything beneath this change". It is the wrong column for a human-readable
-- "Modified" date, for one reason: it is a FULL RECURSIVE rollup whose change
-- tuple includes `blob_hash` and `size`, so a content edit five levels down
-- bumps every ancestor to the root. As a sort key it saturates — every
-- top-level folder reads "just now" every day, and the column stops
-- discriminating, which is the whole reason to sort by it.
--
-- That behaviour is correct for its actual job (invalidating a DAV collection
-- ETag, where missing a deep change would serve stale data) and only that.
-- §8 adds a clock; it does not replace one. `tree_modified_at` is untouched
-- and keeps feeding the ETag.
--
-- ── Why the name matches storage.files ─────────────────────────────────────
-- Same concept, realised per type: "when did this resource's own content last
-- change". Naming both `content_modified_at` means every read becomes
-- `<alias>.content_modified_at AS modified_at` uniformly, which removes the
-- bug class that shipped in `8d0631be` — there, the correct column depended on
-- which table an alias happened to bind to, so `f.content_modified_at` was
-- right in five repositories and a nonexistent column in a sixth.

-- NOT NULL is declared HERE, at ADD COLUMN time, rather than tightened after
-- the backfill the way 20261103000006 does it for files. That migration's
-- order does not work on this table:
--
--   ERROR: cannot ALTER TABLE "folders" because it has pending trigger events
--
-- `storage.folders` carries AFTER ROW triggers (trg_folders_cascade_path,
-- trg_cleanup_role_grants_folder) on top of the statement ones, and Postgres
-- refuses an ALTER TABLE in a transaction that still has trigger events queued
-- from an earlier UPDATE. `storage.files` has only statement triggers, which
-- is why the same shape was fine there.
--
-- Declaring NOT NULL up front sidesteps it entirely: PG 11+ materialises a
-- non-volatile... in this case volatile-but-evaluated-once default for existing
-- rows without rewriting the table, every row is immediately non-NULL, and the
-- backfill below then corrects the VALUES rather than filling in NULLs. The
-- whole migration is one transaction, so no row is ever observable holding the
-- placeholder NOW().
ALTER TABLE storage.folders
    ADD COLUMN IF NOT EXISTS content_modified_at TIMESTAMPTZ NOT NULL DEFAULT NOW();

-- ── Backfill ───────────────────────────────────────────────────────────────
-- From `updated_at`, which is precisely what the wire reports as a folder's
-- `modified_at` today. So the deploy is INERT: every folder's reported
-- modification time is byte-identical before and after, nothing resyncs, and
-- divergence begins only with the first real entry change afterwards. Same
-- reasoning as the files column in 20261103000006 — and the same reason not to
-- backfill from `created_at`, which would report an older time than before and
-- can read to a sync client as "my local copy is newer".
--
-- `DISABLE TRIGGER USER`, not a named list. `storage.folders` carries seven
-- user triggers — three statement-level and four row-level — and the ones that
-- matter here are not only the statement ones:
--
--   * `folders_bump_tree_etag_upd` would materialise OLD and NEW transition
--     tables across every folder before concluding, from its
--     `(name, parent_id, is_trashed, updated_at)` tuple, that it has nothing
--     to do. That is the per-statement blow-up 20261103000006 documents.
--   * the AFTER ROW triggers queue trigger events for the rest of the
--     transaction, which is what makes a later ALTER TABLE fail.
--
-- None of them would take an action for a write that touches only
-- `content_modified_at`. Disabling the lot for one statement is both cheaper
-- and more honest than naming three and hoping the fourth never matters.
-- DDL is transactional, so any failure in this migration restores every
-- trigger along with everything else.
ALTER TABLE storage.folders DISABLE TRIGGER USER;

-- Corrects the placeholder NOW() written by the column default above. The
-- predicate is on `updated_at`, not on NULL, because the column is already
-- NOT NULL — it narrows the write to rows that actually move and keeps the
-- statement idempotent.
UPDATE storage.folders
   SET content_modified_at = updated_at
 WHERE content_modified_at IS DISTINCT FROM updated_at;

ALTER TABLE storage.folders ENABLE TRIGGER USER;

-- ── Triggers ───────────────────────────────────────────────────────────────
-- Statement-level with transition tables, mirroring the house pattern in
-- 20260626000000. Strictly cheaper than the tree-etag triggers they sit beside:
-- those walk `lpath @>` to collect an ancestor set, these join straight to the
-- parent. One level, no recursion.
--
-- ── Why the pg_trigger_depth() guard is mandatory ──────────────────────────
-- An earlier draft omitted it, reasoning that these functions only ever write
-- `content_modified_at`, which no change-tuple below covers — so the self-fire
-- would find an empty change set and stop. That reasoning is WRONG, and
-- produced `stack depth limit exceeded` on the first run.
--
-- A `FOR EACH STATEMENT` trigger fires once per STATEMENT, including a
-- statement that matches zero rows. So the empty change set does not
-- terminate anything: the 0-row UPDATE is still a statement, it re-fires the
-- trigger, which issues another 0-row UPDATE, forever. The row-count filter
-- never gets a chance to act as a base case.
--
-- This is exactly why the tree-etag triggers in 20260626000000 carry the same
-- guard. Depth > 1 means we were invoked by another trigger's write, and the
-- only such writes that reach these tables are cascades of `path`/`lpath`
-- (trg_folders_cascade_path) and `drive_id` — none of which is an entry-list
-- change, so skipping them is correct rather than merely expedient.
--
-- `is_trashed` is in the tuple because trashing a child removes it from the
-- folder's listing — an entry-list change in every sense that matters here,
-- and restore is the same event in reverse.
--
-- `blob_hash` and `size` are deliberately ABSENT. A child's bytes changing
-- does not touch its directory under POSIX, and including them would recreate
-- the saturation problem this column exists to avoid.
--
-- `NOW()` is stable within a transaction, so `IS DISTINCT FROM NOW()` is a
-- genuine no-op guard rather than a race — the same idiom the tree-etag
-- triggers use.

-- Files: INSERT / DELETE share one function (both expose `changed_rows`).
CREATE OR REPLACE FUNCTION storage.bump_folder_content_from_files_stmt()
RETURNS TRIGGER LANGUAGE plpgsql AS $$
BEGIN
    IF pg_trigger_depth() > 1 THEN
        RETURN NULL;
    END IF;
    UPDATE storage.folders f
       SET content_modified_at = NOW()
      FROM (SELECT DISTINCT folder_id
              FROM changed_rows
             WHERE folder_id IS NOT NULL) c
     WHERE f.id = c.folder_id
       AND f.content_modified_at IS DISTINCT FROM NOW();
    RETURN NULL;
END;
$$;

-- Files: UPDATE needs both sides — a move changes the entry list of the
-- source folder AND the destination, so both are bumped.
CREATE OR REPLACE FUNCTION storage.bump_folder_content_from_files_stmt_upd()
RETURNS TRIGGER LANGUAGE plpgsql AS $$
BEGIN
    IF pg_trigger_depth() > 1 THEN
        RETURN NULL;
    END IF;
    WITH changed AS (
        SELECT o.folder_id AS old_fid, n.folder_id AS new_fid
          FROM old_rows o
          JOIN new_rows n USING (id)
         WHERE (o.name, o.folder_id, o.is_trashed)
               IS DISTINCT FROM
               (n.name, n.folder_id, n.is_trashed)
    ),
    targets AS (
        SELECT old_fid AS fid FROM changed WHERE old_fid IS NOT NULL
        UNION
        SELECT new_fid        FROM changed WHERE new_fid IS NOT NULL
    )
    UPDATE storage.folders f
       SET content_modified_at = NOW()
      FROM targets t
     WHERE f.id = t.fid
       AND f.content_modified_at IS DISTINCT FROM NOW();
    RETURN NULL;
END;
$$;

-- Folders: a subfolder is an entry in its parent exactly as a file is.
CREATE OR REPLACE FUNCTION storage.bump_folder_content_from_folders_stmt()
RETURNS TRIGGER LANGUAGE plpgsql AS $$
BEGIN
    IF pg_trigger_depth() > 1 THEN
        RETURN NULL;
    END IF;
    UPDATE storage.folders f
       SET content_modified_at = NOW()
      FROM (SELECT DISTINCT parent_id
              FROM changed_rows
             WHERE parent_id IS NOT NULL) c
     WHERE f.id = c.parent_id
       AND f.content_modified_at IS DISTINCT FROM NOW();
    RETURN NULL;
END;
$$;

CREATE OR REPLACE FUNCTION storage.bump_folder_content_from_folders_stmt_upd()
RETURNS TRIGGER LANGUAGE plpgsql AS $$
BEGIN
    IF pg_trigger_depth() > 1 THEN
        RETURN NULL;
    END IF;
    WITH changed AS (
        SELECT o.parent_id AS old_pid, n.parent_id AS new_pid
          FROM old_rows o
          JOIN new_rows n USING (id)
         WHERE (o.name, o.parent_id, o.is_trashed)
               IS DISTINCT FROM
               (n.name, n.parent_id, n.is_trashed)
    ),
    targets AS (
        SELECT old_pid AS pid FROM changed WHERE old_pid IS NOT NULL
        UNION
        SELECT new_pid        FROM changed WHERE new_pid IS NOT NULL
    )
    UPDATE storage.folders f
       SET content_modified_at = NOW()
      FROM targets t
     WHERE f.id = t.pid
       AND f.content_modified_at IS DISTINCT FROM NOW();
    RETURN NULL;
END;
$$;

-- PG 13 compatibility: DROP-then-CREATE, no CREATE OR REPLACE TRIGGER.
DROP TRIGGER IF EXISTS files_bump_folder_content_ins ON storage.files;
CREATE TRIGGER files_bump_folder_content_ins
    AFTER INSERT ON storage.files
    REFERENCING NEW TABLE AS changed_rows
    FOR EACH STATEMENT EXECUTE FUNCTION storage.bump_folder_content_from_files_stmt();

DROP TRIGGER IF EXISTS files_bump_folder_content_del ON storage.files;
CREATE TRIGGER files_bump_folder_content_del
    AFTER DELETE ON storage.files
    REFERENCING OLD TABLE AS changed_rows
    FOR EACH STATEMENT EXECUTE FUNCTION storage.bump_folder_content_from_files_stmt();

DROP TRIGGER IF EXISTS files_bump_folder_content_upd ON storage.files;
CREATE TRIGGER files_bump_folder_content_upd
    AFTER UPDATE ON storage.files
    REFERENCING OLD TABLE AS old_rows NEW TABLE AS new_rows
    FOR EACH STATEMENT EXECUTE FUNCTION storage.bump_folder_content_from_files_stmt_upd();

DROP TRIGGER IF EXISTS folders_bump_folder_content_ins ON storage.folders;
CREATE TRIGGER folders_bump_folder_content_ins
    AFTER INSERT ON storage.folders
    REFERENCING NEW TABLE AS changed_rows
    FOR EACH STATEMENT EXECUTE FUNCTION storage.bump_folder_content_from_folders_stmt();

DROP TRIGGER IF EXISTS folders_bump_folder_content_del ON storage.folders;
CREATE TRIGGER folders_bump_folder_content_del
    AFTER DELETE ON storage.folders
    REFERENCING OLD TABLE AS changed_rows
    FOR EACH STATEMENT EXECUTE FUNCTION storage.bump_folder_content_from_folders_stmt();

DROP TRIGGER IF EXISTS folders_bump_folder_content_upd ON storage.folders;
CREATE TRIGGER folders_bump_folder_content_upd
    AFTER UPDATE ON storage.folders
    REFERENCING OLD TABLE AS old_rows NEW TABLE AS new_rows
    FOR EACH STATEMENT EXECUTE FUNCTION storage.bump_folder_content_from_folders_stmt_upd();

COMMENT ON COLUMN storage.folders.content_modified_at IS
    'POSIX directory mtime: when this folder''s ENTRY LIST last changed — a direct child added, removed, renamed, moved in or out, trashed or restored. NOT bumped by a child''s bytes changing, and NOT by anything deeper in the subtree. Symmetric with storage.files.content_modified_at: both answer "when did this resource''s own content last change". For "did anything beneath this change" use tree_modified_at, which is the recursive rollup behind the DAV collection ETag. See docs/architecture/dates.md.';
