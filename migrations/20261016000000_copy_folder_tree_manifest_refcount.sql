-- Fix: `storage.copy_folder_tree` never incremented `chunk_manifests.ref_count`.
--
-- The function bumped only `storage.blobs`:
--
--     UPDATE storage.blobs b SET ref_count = ref_count + hc.cnt
--       FROM (...) hc WHERE b.hash = hc.blob_hash;
--
-- but a CDC file's `blob_hash` names a MANIFEST, not a chunk. For any
-- multi-chunk file that predicate matches nothing, so a folder copy took
-- NO reference. Delete the original afterwards and `remove_reference`
-- walks the manifest to 0, `dedup_gc` reaps the manifest and every chunk
-- behind it — and the copy is unreadable. Silent data loss on an ordinary
-- UI operation.
--
-- Single-chunk files escaped by accident: their whole-file hash equals
-- their lone chunk's hash, so the UPDATE did match — bumping the wrong
-- counter, which shows up as a manifest under-count plus a blob
-- over-count rather than as loss.
--
-- Reproduced on a 5 MiB / 18-chunk file copied through the UI:
-- `chunk_manifests.ref_count` stayed at 1 while two `storage.files` rows
-- referenced it; `manifests_consistency` reported
-- `manifest_refcount_mismatch` with `delta: 1, reap_risk: true`.
--
-- This migration only rewrites the reference-counting block; everything
-- else is `20260902000001_copy_folder_tree_drop_user_id.sql` verbatim.
--
-- NOTE: existing drift is NOT repaired here. Run `manifests_consistency`
-- to find it — a data fix belongs with the recovery framework, not in a
-- schema migration that cannot know which counter is authoritative.

CREATE OR REPLACE FUNCTION storage.copy_folder_tree(
    p_source_id UUID,
    p_target_parent_id UUID,       -- NULL = copy to root (keeps source drive)
    p_dest_name TEXT DEFAULT NULL   -- NULL = keep source folder name
) RETURNS TABLE(new_root_id TEXT, folders_copied BIGINT, files_copied BIGINT) AS $$
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
    -- Validate source exists.
    SELECT fo.lpath, nlevel(fo.lpath)
      INTO v_root_lpath, v_root_depth
      FROM storage.folders fo
     WHERE fo.id = p_source_id AND NOT fo.is_trashed;

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

    -- Temp mapping: every folder in the subtree → new UUID.
    CREATE TEMP TABLE IF NOT EXISTS _copy_map(
        old_id UUID PRIMARY KEY,
        new_id UUID NOT NULL DEFAULT gen_random_uuid()
    ) ON COMMIT DROP;
    TRUNCATE _copy_map;

    INSERT INTO _copy_map(old_id)
    SELECT fo.id
      FROM storage.folders fo
     WHERE NOT fo.is_trashed
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

    -- Temp mapping for files src→dst (dst ids pre-allocated so we can
    -- reference them in the dead-property duplication below).
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
    INSERT INTO storage.files(
        id, name, folder_id, blob_hash, size, mime_type,
        media_sort_date, drive_id, created_by, updated_by
    )
    SELECT fm.new_id, f.name, cm.new_id, f.blob_hash, f.size,
           f.mime_type, f.media_sort_date, v_dest_drive_id, f.created_by,
           f.updated_by
      FROM storage.files f
      JOIN _copy_map      cm ON f.folder_id = cm.old_id
      JOIN _copy_file_map fm ON fm.old_id   = f.id
     WHERE NOT f.is_trashed;

    GET DIAGNOSTICS v_files = ROW_COUNT;

    -- Batch increment reference counts — MANIFEST FIRST, blobs only as
    -- fallback. This mirrors `DedupService::add_reference`, and the order
    -- is the whole point:
    --
    -- A CDC file's `blob_hash` names a MANIFEST (`chunk_manifests.file_hash`),
    -- not a chunk. The previous version of this block updated only
    -- `storage.blobs`, so for a multi-chunk file the predicate
    -- `b.hash = hc.blob_hash` matched ZERO rows and the copy took no
    -- reference at all. Deleting the original then walked the manifest's
    -- ref_count to 0, dedup_gc reaped the manifest and every chunk behind
    -- it, and the copy became unreadable. Reproduced via the UI folder
    -- copy on a 5 MiB (18-chunk) file: ref_count stayed 1 with two files
    -- referencing it.
    --
    -- The `NOT EXISTS (bumped)` guard on the blobs branch is load-bearing.
    -- For a SINGLE-chunk file the whole-file hash equals its lone chunk's
    -- hash, so without it the copy would be counted at both levels and
    -- turn an under-count into an over-count.
    IF v_files > 0 THEN
        WITH hc AS (
            SELECT f.blob_hash, COUNT(*)::int AS cnt
              FROM storage.files f
              JOIN _copy_map cm ON f.folder_id = cm.new_id
             WHERE NOT f.is_trashed
             GROUP BY f.blob_hash
        ),
        bumped AS (
            UPDATE storage.chunk_manifests m
               SET ref_count = m.ref_count + hc.cnt
              FROM hc
             WHERE m.file_hash = hc.blob_hash
            RETURNING m.file_hash
        )
        UPDATE storage.blobs b
           SET ref_count  = b.ref_count + hc.cnt,
               -- Matches add_reference: a blob resurrected inside its GC
               -- grace window must lose its orphan stamp.
               orphaned_at = NULL
          FROM hc
         WHERE b.hash = hc.blob_hash
           AND NOT EXISTS (SELECT 1 FROM bumped WHERE file_hash = hc.blob_hash);
    END IF;

    -- Duplicate dead properties per RFC 4918 §8.8 — id-keyed store.
    INSERT INTO storage.webdav_dead_properties
        (folder_id, namespace, local_name, value)
    SELECT cm.new_id, dp.namespace, dp.local_name, dp.value
      FROM storage.webdav_dead_properties dp
      JOIN _copy_map cm ON dp.folder_id = cm.old_id;

    INSERT INTO storage.webdav_dead_properties
        (file_id, namespace, local_name, value)
    SELECT fm.new_id, dp.namespace, dp.local_name, dp.value
      FROM storage.webdav_dead_properties dp
      JOIN _copy_file_map fm ON dp.file_id = fm.old_id;

    RETURN QUERY SELECT v_new_root::text, v_folders, v_files;
END;
$$ LANGUAGE plpgsql;
