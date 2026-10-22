-- Cascading folder copy drops `category_order`, so every copied file lands
-- in the "Other" bucket.
--
-- `storage.files.category_order` is `SMALLINT NOT NULL DEFAULT 9999`, where
-- 9999 is the catch-all "Other" rank and real files carry 100 (image),
-- 200 (video), 300 (audio), 400 (pdf), 500 (document) and so on. The column
-- list in `copy_folder_tree`'s file INSERT never named it, so copied rows
-- silently took the default and sorted to the end of every type-grouped
-- listing regardless of what they actually are.
--
-- Only the cascading path is affected. A single-file copy goes through
-- `save_file_with_blob`, which binds `category_order_for(&name,
-- &content_type)` explicitly — so the two copy paths disagreed, which is
-- exactly the kind of divergence an explicit column list invites.
--
-- The value is copied from the source rather than recomputed: `name` and
-- `mime_type` are copied unchanged, so the source's rank is correct by
-- construction, and deriving it again in SQL would duplicate
-- `display_helpers::category_order_for` into a second place that can drift.
--
-- ── On the columns still omitted, which is now a deliberate list ──────────
-- This INSERT names columns explicitly, so every column added to
-- `storage.files` from here on is silently dropped by the copy unless
-- someone remembers. The omissions as of this migration, and why:
--
--   id, folder_id, drive_id      → remapped to the copy's own identity
--   created_at, updated_at       → DEFAULT NOW(); the copy is a new row
--   content_modified_at          → DEFAULT NOW(), DELIBERATE. A copy is a
--                                  new file whose content came into
--                                  existence here — `cp` without `-p`.
--                                  Decided 2026-10-11, reaffirmed
--                                  2026-10-18. See docs/architecture/dates.md
--                                  § What bumps content_modified_at.
--   is_trashed, trashed_at,
--   original_folder_id           → a copy is never born trashed
--   media_sort_date              → COPIED, so a copied photo keeps its place
--                                  in the timeline rather than jumping to
--                                  "now"
--
-- Two known gaps left as-is, recorded so they are not rediscovered:
--
--   * `storage.file_metadata` (EXIF: captured_at, camera, GPS, dimensions)
--     is copied by NEITHER this function nor `copy_file_satellites`, so a
--     copied photo loses its EXIF row. Thumbnails and previews are fine —
--     `content_derived_blobs` is keyed `(source_hash, kind, variant)`, so a
--     copy shares them through its identical `blob_hash`. `media_sort_date`
--     is copied, so the timeline position survives too; what a copy loses is
--     People/Places and camera info.
--
--     **Do not "fix" this here, and do not re-key the table.** Re-keying
--     `file_metadata` to the content hash would make copies correct for
--     free and looks like the consistent move next to
--     `content_derived_blobs` — it is nonetheless NOT the plan.
--     `file_metadata` stays `file_id`-keyed, and the change in this area is
--     owned by separate work (confirmed with Ed 2026-10-19). Adding it to
--     the copy list above, or re-keying it, would collide with that.
--   * `source_created_at` does not exist yet (plan §4). When it lands,
--     decide explicitly whether a copy inherits it — unlike
--     `content_modified_at`, a source creation date arguably travels with
--     the bytes.

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

-- ── Repair rows already mis-bucketed by the old function ──────────────────
-- Derived from sibling rows rather than from a transcribed mapping: any file
-- sharing a mime_type with a correctly-ranked file takes that rank. This
-- avoids copying `category_order_for`'s table into SQL, where it would be a
-- second source of truth free to drift from the Rust one.
--
-- Rows whose mime_type appears nowhere with a real rank keep 9999, which is
-- the honest answer — we cannot derive what we have never seen, and for a
-- genuinely uncategorised type 9999 is correct anyway.
UPDATE storage.files f
   SET category_order = s.category_order
  FROM (
        SELECT DISTINCT ON (mime_type) mime_type, category_order
          FROM storage.files
         WHERE category_order <> 9999
         ORDER BY mime_type, category_order
       ) s
 WHERE f.category_order = 9999
   AND f.mime_type = s.mime_type;
