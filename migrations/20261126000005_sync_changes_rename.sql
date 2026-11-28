-- RFC 6578 sync-collection: a rename is a delete of the old href plus a
-- create of the new one.
--
-- The bug: renaming a file or folder in place (same parent) was logged as
-- one 'updated' row carrying only the NEW name. A client keys its local
-- state by href, so it received the new href as an upsert and was never
-- told the old href was gone: it kept a ghost copy of the member under
-- its old name forever (and, for a folder, the whole subtree under it).
--
-- The fix: a name change takes the same branch as a parent change — a
-- 'deleted' row under the old name and a 'created' row under the new one.
-- The delta query collapses per (member_id, label) instead of per
-- member_id, so the old name's tombstone survives next to the new name's
-- creation (see `sync_change_log_pg_repository.rs::changes_since`).

-- ── File side: UPDATE ──
--   * parent or name changed      → deleted (old parent, old name) + created (new parent, new name)
--   * is_trashed false→true       → deleted
--   * is_trashed true→false       → created
--   * anything else observable    → updated
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
       AND (old_folder_id IS DISTINCT FROM new_folder_id OR old_name IS DISTINCT FROM new_name)
    UNION ALL
    SELECT new_folder_id, 'file', id, new_name, 'created'
      FROM changed
     WHERE new_folder_id IS NOT NULL
       AND (old_folder_id IS DISTINCT FROM new_folder_id OR old_name IS DISTINCT FROM new_name)
    UNION ALL
    SELECT new_folder_id, 'file', id, new_name, 'deleted'
      FROM changed
     WHERE new_folder_id IS NOT NULL
       AND old_folder_id IS NOT DISTINCT FROM new_folder_id
       AND old_name IS NOT DISTINCT FROM new_name
       AND old_trashed = FALSE AND new_trashed = TRUE
    UNION ALL
    SELECT new_folder_id, 'file', id, new_name, 'created'
      FROM changed
     WHERE new_folder_id IS NOT NULL
       AND old_folder_id IS NOT DISTINCT FROM new_folder_id
       AND old_name IS NOT DISTINCT FROM new_name
       AND old_trashed = TRUE AND new_trashed = FALSE
    UNION ALL
    SELECT new_folder_id, 'file', id, new_name, 'updated'
      FROM changed
     WHERE new_folder_id IS NOT NULL
       AND old_folder_id IS NOT DISTINCT FROM new_folder_id
       AND old_name IS NOT DISTINCT FROM new_name
       AND old_trashed = new_trashed;

    RETURN NULL;
END;
$$;

-- ── Folder side: UPDATE ──
--   * parent or name changed      → deleted (old parent, old name) + created (new parent, new name)
--   * is_trashed false→true       → deleted
--   * is_trashed true→false       → created
--   * anything else observable    → updated
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
       AND (old_parent_id IS DISTINCT FROM new_parent_id OR old_name IS DISTINCT FROM new_name)
    UNION ALL
    SELECT new_parent_id, 'folder', id, new_name, 'created'
      FROM changed
     WHERE new_parent_id IS NOT NULL
       AND (old_parent_id IS DISTINCT FROM new_parent_id OR old_name IS DISTINCT FROM new_name)
    UNION ALL
    SELECT new_parent_id, 'folder', id, new_name, 'deleted'
      FROM changed
     WHERE new_parent_id IS NOT NULL
       AND old_parent_id IS NOT DISTINCT FROM new_parent_id
       AND old_name IS NOT DISTINCT FROM new_name
       AND old_trashed = FALSE AND new_trashed = TRUE
    UNION ALL
    SELECT new_parent_id, 'folder', id, new_name, 'created'
      FROM changed
     WHERE new_parent_id IS NOT NULL
       AND old_parent_id IS NOT DISTINCT FROM new_parent_id
       AND old_name IS NOT DISTINCT FROM new_name
       AND old_trashed = TRUE AND new_trashed = FALSE
    UNION ALL
    SELECT new_parent_id, 'folder', id, new_name, 'updated'
      FROM changed
     WHERE new_parent_id IS NOT NULL
       AND old_parent_id IS NOT DISTINCT FROM new_parent_id
       AND old_name IS NOT DISTINCT FROM new_name
       AND old_trashed = new_trashed;

    RETURN NULL;
END;
$$;
