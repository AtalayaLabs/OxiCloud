-- RFC 6578 incremental sync-collection: durable change log for WebDAV
-- files/folders.
--
-- Prior state: `sync-collection` REPORT (see webdav_handler.rs::handle_report
-- and nextcloud/report_handler.rs::handle_sync_collection) parses the
-- client's sync-token but never acts on it — every call re-lists the
-- entire collection and mints a fresh timestamp-shaped token. There is no
-- persisted record of what changed since a prior sync, and deletions are
-- never reported (RFC 6578 §3.7 requires a 404 sub-response per removed
-- member).
--
-- This migration adds a per-collection append-only log, populated by
-- statement-level triggers on `storage.files`/`storage.folders`, mirroring
-- the trigger shape already proven by the `tree_etag_dirty` queue
-- (`20260627000000_async_tree_etag_queue.sql`) — same
-- `pg_trigger_depth() > 1` reentrancy guard, same DAV-observable-columns
-- value filter for UPDATE. Unlike that queue, rows here are NOT drained on
-- flush: they persist until the retention sweep (`SyncLogRetentionService`,
-- application-layer) deletes rows past the configured retention window and
-- advances `folder_sync_watermark.low_water_seq` accordingly.
--
-- Scope: only rows with a NOT NULL collection target are logged — i.e.
-- ordinary folder membership (a file's `folder_id`, a folder's
-- `parent_id`). This mirrors the pre-existing scope limit in
-- `bump_tree_from_folders_stmt` (`nlevel(lpath) > 1`, folders' own
-- creation never bumps its own ancestor-ETag): root-level files
-- (`folder_id IS NULL`) and root folders themselves (`parent_id IS NULL`)
-- have no real "collection" row to log against under the current schema,
-- and `sync-collection` REPORT against that exact synthetic path
-- (`webdav_handler.rs::handle_report`, `path.is_empty()` branch) is out of
-- scope for this phase — it keeps returning a full listing every call
-- (rare churn: creating/deleting a whole drive's root, not everyday
-- file activity).
--
-- Tombstones are written at the instant `is_trashed` flips either
-- direction, not at hard-delete/purge time — a trashed member vanishes
-- from its parent's listing immediately, which IS a deletion from a sync
-- client's point of view, regardless of whether the row still physically
-- exists in `storage.trash_items`. Restoring is correctly logged as a
-- fresh `created`. The retention-window hard purge
-- (`trash_db_repository.rs`) needs no additional log-writing: the member
-- was already tombstoned at trash-time.

CREATE TABLE IF NOT EXISTS storage.folder_sync_changes (
    seq                  BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    collection_folder_id UUID NOT NULL REFERENCES storage.folders(id) ON DELETE CASCADE,
    member_type          TEXT NOT NULL CHECK (member_type IN ('file', 'folder')),
    member_id            UUID NOT NULL,
    member_href_name     TEXT NOT NULL,
    change_kind          TEXT NOT NULL CHECK (change_kind IN ('created', 'updated', 'deleted')),
    changed_at           TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Query shape is always "changes for collection X since seq Y" — the
-- composite index serves that directly (seq is already unique/ordered so
-- a plain (collection_folder_id) index would still require a sort; this
-- avoids it).
CREATE INDEX IF NOT EXISTS idx_folder_sync_changes_collection_seq
    ON storage.folder_sync_changes (collection_folder_id, seq);

-- Retention sweep's cutoff scan.
CREATE INDEX IF NOT EXISTS idx_folder_sync_changes_changed_at
    ON storage.folder_sync_changes (changed_at);

-- Singleton low-water-mark row: durable record of "rows below this seq
-- have been purged by retention," needed because an empty/sparse table
-- alone can't distinguish "nothing has ever happened" from "so much
-- happened your token's rows are long gone." Monotonically advanced
-- (never decreased) by `SyncLogRetentionService`.
CREATE TABLE IF NOT EXISTS storage.folder_sync_watermark (
    singleton     BOOLEAN NOT NULL DEFAULT TRUE PRIMARY KEY CHECK (singleton),
    low_water_seq BIGINT NOT NULL DEFAULT 0
);

INSERT INTO storage.folder_sync_watermark (singleton, low_water_seq)
VALUES (TRUE, 0)
ON CONFLICT (singleton) DO NOTHING;

-- ── File side: INSERT ────────────────────────────────────────────────
CREATE OR REPLACE FUNCTION storage.log_file_sync_changes_ins()
RETURNS TRIGGER LANGUAGE plpgsql AS $$
BEGIN
    IF pg_trigger_depth() > 1 THEN
        RETURN NULL;
    END IF;

    INSERT INTO storage.folder_sync_changes
        (collection_folder_id, member_type, member_id, member_href_name, change_kind)
    SELECT folder_id, 'file', id, name, 'created'
      FROM changed_rows
     WHERE folder_id IS NOT NULL;

    RETURN NULL;
END;
$$;

-- ── File side: DELETE (hard delete bypassing trash) ─────────────────
CREATE OR REPLACE FUNCTION storage.log_file_sync_changes_del()
RETURNS TRIGGER LANGUAGE plpgsql AS $$
BEGIN
    IF pg_trigger_depth() > 1 THEN
        RETURN NULL;
    END IF;

    INSERT INTO storage.folder_sync_changes
        (collection_folder_id, member_type, member_id, member_href_name, change_kind)
    SELECT folder_id, 'file', id, name, 'deleted'
      FROM changed_rows
     WHERE folder_id IS NOT NULL;

    RETURN NULL;
END;
$$;

-- ── File side: UPDATE ────────────────────────────────────────────────
-- Same value filter as `bump_tree_from_files_stmt_upd` (DAV-observable
-- columns only — the EXIF media_sort_date sync never logs a change).
-- Branches mutually-exclusively on what actually changed:
--   * folder_id changed              → deleted (old parent) + created (new parent)
--   * is_trashed false→true          → deleted (member vanishes from listing)
--   * is_trashed true→false          → created (member reappears)
--   * anything else observable       → updated
CREATE OR REPLACE FUNCTION storage.log_file_sync_changes_upd()
RETURNS TRIGGER LANGUAGE plpgsql AS $$
BEGIN
    IF pg_trigger_depth() > 1 THEN
        RETURN NULL;
    END IF;

    WITH changed AS (
        SELECT o.id,
               o.folder_id AS old_folder_id, n.folder_id AS new_folder_id,
               o.name AS old_name, n.name AS new_name,
               o.is_trashed AS old_trashed, n.is_trashed AS new_trashed
          FROM old_rows o
          JOIN new_rows n USING (id)
         WHERE (o.name, o.folder_id, o.blob_hash, o.size,
                o.mime_type, o.is_trashed, o.updated_at)
               IS DISTINCT FROM
               (n.name, n.folder_id, n.blob_hash, n.size,
                n.mime_type, n.is_trashed, n.updated_at)
    )
    INSERT INTO storage.folder_sync_changes
        (collection_folder_id, member_type, member_id, member_href_name, change_kind)
    SELECT old_folder_id, 'file', id, old_name, 'deleted'
      FROM changed
     WHERE old_folder_id IS NOT NULL
       AND old_folder_id IS DISTINCT FROM new_folder_id
    UNION ALL
    SELECT new_folder_id, 'file', id, new_name, 'created'
      FROM changed
     WHERE new_folder_id IS NOT NULL
       AND old_folder_id IS DISTINCT FROM new_folder_id
    UNION ALL
    SELECT new_folder_id, 'file', id, new_name, 'deleted'
      FROM changed
     WHERE new_folder_id IS NOT NULL
       AND old_folder_id IS NOT DISTINCT FROM new_folder_id
       AND old_trashed = FALSE AND new_trashed = TRUE
    UNION ALL
    SELECT new_folder_id, 'file', id, new_name, 'created'
      FROM changed
     WHERE new_folder_id IS NOT NULL
       AND old_folder_id IS NOT DISTINCT FROM new_folder_id
       AND old_trashed = TRUE AND new_trashed = FALSE
    UNION ALL
    SELECT new_folder_id, 'file', id, new_name, 'updated'
      FROM changed
     WHERE new_folder_id IS NOT NULL
       AND old_folder_id IS NOT DISTINCT FROM new_folder_id
       AND old_trashed = new_trashed;

    RETURN NULL;
END;
$$;

-- ── Folder side: INSERT ──────────────────────────────────────────────
-- Member is the folder itself; collection is its PARENT. Root folders
-- (parent_id IS NULL) are out of scope (see header).
CREATE OR REPLACE FUNCTION storage.log_folder_sync_changes_ins()
RETURNS TRIGGER LANGUAGE plpgsql AS $$
BEGIN
    IF pg_trigger_depth() > 1 THEN
        RETURN NULL;
    END IF;

    INSERT INTO storage.folder_sync_changes
        (collection_folder_id, member_type, member_id, member_href_name, change_kind)
    SELECT parent_id, 'folder', id, name, 'created'
      FROM changed_rows
     WHERE parent_id IS NOT NULL;

    RETURN NULL;
END;
$$;

-- ── Folder side: DELETE ──────────────────────────────────────────────
CREATE OR REPLACE FUNCTION storage.log_folder_sync_changes_del()
RETURNS TRIGGER LANGUAGE plpgsql AS $$
BEGIN
    IF pg_trigger_depth() > 1 THEN
        RETURN NULL;
    END IF;

    INSERT INTO storage.folder_sync_changes
        (collection_folder_id, member_type, member_id, member_href_name, change_kind)
    SELECT parent_id, 'folder', id, name, 'deleted'
      FROM changed_rows
     WHERE parent_id IS NOT NULL;

    RETURN NULL;
END;
$$;

-- ── Folder side: UPDATE ──────────────────────────────────────────────
-- Same value filter as `bump_tree_from_folders_stmt_upd`; same
-- move/trash/restore/rename branching as the file side, keyed on
-- parent_id instead of folder_id.
CREATE OR REPLACE FUNCTION storage.log_folder_sync_changes_upd()
RETURNS TRIGGER LANGUAGE plpgsql AS $$
BEGIN
    IF pg_trigger_depth() > 1 THEN
        RETURN NULL;
    END IF;

    WITH changed AS (
        SELECT o.id,
               o.parent_id AS old_parent_id, n.parent_id AS new_parent_id,
               o.name AS old_name, n.name AS new_name,
               o.is_trashed AS old_trashed, n.is_trashed AS new_trashed
          FROM old_rows o
          JOIN new_rows n USING (id)
         WHERE (o.name, o.parent_id, o.is_trashed, o.updated_at)
               IS DISTINCT FROM
               (n.name, n.parent_id, n.is_trashed, n.updated_at)
    )
    INSERT INTO storage.folder_sync_changes
        (collection_folder_id, member_type, member_id, member_href_name, change_kind)
    SELECT old_parent_id, 'folder', id, old_name, 'deleted'
      FROM changed
     WHERE old_parent_id IS NOT NULL
       AND old_parent_id IS DISTINCT FROM new_parent_id
    UNION ALL
    SELECT new_parent_id, 'folder', id, new_name, 'created'
      FROM changed
     WHERE new_parent_id IS NOT NULL
       AND old_parent_id IS DISTINCT FROM new_parent_id
    UNION ALL
    SELECT new_parent_id, 'folder', id, new_name, 'deleted'
      FROM changed
     WHERE new_parent_id IS NOT NULL
       AND old_parent_id IS NOT DISTINCT FROM new_parent_id
       AND old_trashed = FALSE AND new_trashed = TRUE
    UNION ALL
    SELECT new_parent_id, 'folder', id, new_name, 'created'
      FROM changed
     WHERE new_parent_id IS NOT NULL
       AND old_parent_id IS NOT DISTINCT FROM new_parent_id
       AND old_trashed = TRUE AND new_trashed = FALSE
    UNION ALL
    SELECT new_parent_id, 'folder', id, new_name, 'updated'
      FROM changed
     WHERE new_parent_id IS NOT NULL
       AND old_parent_id IS NOT DISTINCT FROM new_parent_id
       AND old_trashed = new_trashed;

    RETURN NULL;
END;
$$;

-- ── Wire the triggers ────────────────────────────────────────────────
CREATE TRIGGER files_log_sync_changes_ins
    AFTER INSERT ON storage.files
    REFERENCING NEW TABLE AS changed_rows
    FOR EACH STATEMENT EXECUTE FUNCTION storage.log_file_sync_changes_ins();

CREATE TRIGGER files_log_sync_changes_del
    AFTER DELETE ON storage.files
    REFERENCING OLD TABLE AS changed_rows
    FOR EACH STATEMENT EXECUTE FUNCTION storage.log_file_sync_changes_del();

CREATE TRIGGER files_log_sync_changes_upd
    AFTER UPDATE ON storage.files
    REFERENCING OLD TABLE AS old_rows NEW TABLE AS new_rows
    FOR EACH STATEMENT EXECUTE FUNCTION storage.log_file_sync_changes_upd();

CREATE TRIGGER folders_log_sync_changes_ins
    AFTER INSERT ON storage.folders
    REFERENCING NEW TABLE AS changed_rows
    FOR EACH STATEMENT EXECUTE FUNCTION storage.log_folder_sync_changes_ins();

CREATE TRIGGER folders_log_sync_changes_del
    AFTER DELETE ON storage.folders
    REFERENCING OLD TABLE AS changed_rows
    FOR EACH STATEMENT EXECUTE FUNCTION storage.log_folder_sync_changes_del();

CREATE TRIGGER folders_log_sync_changes_upd
    AFTER UPDATE ON storage.folders
    REFERENCING OLD TABLE AS old_rows NEW TABLE AS new_rows
    FOR EACH STATEMENT EXECUTE FUNCTION storage.log_folder_sync_changes_upd();
