-- copy_folder_tree skips share mount rows (docs/plan/share-mounts.md R3); a mount
-- as the source is refused. Body otherwise identical to 20261103000009.
CREATE OR REPLACE FUNCTION storage.copy_folder_tree(p_source_id uuid, p_target_parent_id uuid, p_dest_name text DEFAULT NULL::text)
 RETURNS TABLE(new_root_id text, folders_copied bigint, files_copied bigint)
 LANGUAGE plpgsql
AS $function$
DECLARE
    v_root_lpath    ltree;
    v_root_depth    INT;
    v_max_depth     INT;
    v_level         INT;
    v_folders       BIGINT := 0;
    v_files         BIGINT := 0;
    v_inserted      BIGINT;
    v_new_root      UUID;
    v_dest_drive_id UUID;
BEGIN
    -- Validate source exists and is not a share mount.
    SELECT fo.lpath, nlevel(fo.lpath)
      INTO v_root_lpath, v_root_depth
      FROM storage.folders fo
     WHERE fo.id = p_source_id AND NOT fo.is_trashed AND fo.mount_target_id IS NULL;

    IF v_root_lpath IS NULL THEN
        RAISE EXCEPTION 'Source folder not found: %', p_source_id
            USING ERRCODE = 'P0002';  -- no_data_found
    END IF;

    -- Resolve destination drive_id once up front (cross-drive copy path).
    IF p_target_parent_id IS NULL THEN
        SELECT fo.drive_id INTO v_dest_drive_id
          FROM storage.folders fo
         WHERE fo.id = p_source_id;
    ELSE
        SELECT fo.drive_id INTO v_dest_drive_id
          FROM storage.folders fo
         WHERE fo.id = p_target_parent_id AND NOT fo.is_trashed;
        IF v_dest_drive_id IS NULL THEN
            RAISE EXCEPTION 'Target parent folder not found: %', p_target_parent_id
                USING ERRCODE = 'P0002';
        END IF;
    END IF;

    -- Temp mapping: every folder in the subtree → new UUID. Mount rows are skipped.
    CREATE TEMP TABLE IF NOT EXISTS _copy_map(
        old_id UUID PRIMARY KEY,
        new_id UUID NOT NULL DEFAULT gen_random_uuid()
    ) ON COMMIT DROP;
    TRUNCATE _copy_map;

    INSERT INTO _copy_map(old_id)
    SELECT fo.id
      FROM storage.folders fo
     WHERE NOT fo.is_trashed
       AND fo.mount_target_id IS NULL
       AND fo.lpath <@ v_root_lpath;

    SELECT cm.new_id INTO v_new_root
      FROM _copy_map cm WHERE cm.old_id = p_source_id;

    SELECT MAX(nlevel(fo.lpath))
      INTO v_max_depth
      FROM storage.folders fo
      JOIN _copy_map cm ON fo.id = cm.old_id;

    -- ── Insert folders level by level ──
    -- Post-D7: `user_id` intentionally omitted from the column list so
    -- copied rows leave the (now-nullable) column NULL. Provenance is
    -- carried by `created_by` / `updated_by` (§14 columns) — preserved
    -- from source so authorship survives the copy.
    FOR v_level IN v_root_depth .. v_max_depth LOOP
        INSERT INTO storage.folders(
            id, name, parent_id,
            drive_id, created_by, updated_by
        )
        SELECT cm.new_id,
               CASE WHEN fo.id = p_source_id AND p_dest_name IS NOT NULL
                    THEN p_dest_name ELSE fo.name END,
               CASE WHEN fo.id = p_source_id THEN p_target_parent_id
                    ELSE pm.new_id END,
               v_dest_drive_id,
               fo.created_by,
               fo.updated_by
          FROM storage.folders fo
          JOIN _copy_map cm ON fo.id = cm.old_id
          LEFT JOIN _copy_map pm ON fo.parent_id = pm.old_id
         WHERE NOT fo.is_trashed
           AND nlevel(fo.lpath) = v_level;

        GET DIAGNOSTICS v_inserted = ROW_COUNT;
        v_folders := v_folders + v_inserted;
    END LOOP;

    -- Temp mapping for files src→dst (dst ids pre-allocated so we can hand
    -- both sides to copy_file_satellites below).
    CREATE TEMP TABLE IF NOT EXISTS _copy_file_map(
        old_id UUID PRIMARY KEY,
        new_id UUID NOT NULL DEFAULT gen_random_uuid()
    ) ON COMMIT DROP;
    TRUNCATE _copy_file_map;

    INSERT INTO _copy_file_map(old_id)
    SELECT f.id
      FROM storage.files f
      JOIN _copy_map cm ON f.folder_id = cm.old_id
     WHERE NOT f.is_trashed;

    -- ── Batch copy all files (zero-copy: same blob_hash) ──
    -- Post-D7: `user_id` omitted. Provenance via `created_by`/`updated_by`.
    -- `category_order` is copied, not recomputed — see the header.
    INSERT INTO storage.files(
        id, name, folder_id, blob_hash, size, mime_type,
        media_sort_date, category_order, drive_id, created_by, updated_by
    )
    SELECT fm.new_id, f.name, cm.new_id, f.blob_hash, f.size,
           f.mime_type, f.media_sort_date, f.category_order, v_dest_drive_id,
           f.created_by, f.updated_by
      FROM storage.files f
      JOIN _copy_map      cm ON f.folder_id = cm.old_id
      JOIN _copy_file_map fm ON fm.old_id   = f.id
     WHERE NOT f.is_trashed;

    GET DIAGNOSTICS v_files = ROW_COUNT;

    -- Everything that follows a file on copy — blob references and dead
    -- properties — in one call, shared with the single-file copy path.
    --
    -- Both aggregates order by `old_id`, which is what makes the two arrays
    -- correspond positionally; `array_agg` without a matching ORDER BY would
    -- be free to pair a file with another file's satellites.
    IF v_files > 0 THEN
        PERFORM storage.copy_file_satellites(
            (SELECT array_agg(old_id ORDER BY old_id) FROM _copy_file_map),
            (SELECT array_agg(new_id ORDER BY old_id) FROM _copy_file_map)
        );
    END IF;

    -- Folder dead properties. Files are handled inside copy_file_satellites;
    -- folders have no other satellites, so this stays here.
    INSERT INTO storage.webdav_dead_properties
        (folder_id, namespace, local_name, value)
    SELECT cm.new_id, dp.namespace, dp.local_name, dp.value
      FROM storage.webdav_dead_properties dp
      JOIN _copy_map cm ON dp.folder_id = cm.old_id;

    RETURN QUERY SELECT v_new_root::text, v_folders, v_files;
END;
$function$;
