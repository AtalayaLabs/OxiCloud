-- Step 8 of `docs/plan/derived-blobs.md` — single-source the copy fan-out.
--
-- "What follows a file when the file is copied" was written twice: once in
-- the `copy_file` CTE (Rust, `file_blob_write_repository.rs`) and once in
-- `storage.copy_folder_tree`. They had already drifted — the tree path
-- bumped `storage.blobs` only, missing manifests entirely, which was silent
-- data loss on a multi-chunk file (fixed in `20261016000000`, and the fix
-- had to be written a second time rather than in one place).
--
-- The plan adds file-keyed satellite tables (`file_attached_blobs`, step 9).
-- Adding them against two copy sites means writing the same cascade a third
-- and fourth time, into sites that have already proven they drift. So the
-- fan-out gets exactly one home first.
--
-- Two functions land here:
--
--   * `storage.add_blob_references(TEXT[])` — the manifest-first reference
--     contract, expressed once for SQL callers. `DedupService::add_reference`
--     is the Rust twin; they must change together, which is why the shared
--     contract is spelled out in both doc comments.
--
--   * `storage.copy_file_satellites(UUID[], UUID[])` — everything that
--     follows a file on copy. The body IS the copy-semantics declaration:
--     what is absent is a documented decision (see the trailing comments),
--     not an omission someone has to notice.
--
-- Set-based rather than per-row on purpose. A per-row helper would have made
-- a 10k-file folder copy 10k function calls; taking arrays keeps the tree
-- path's single-statement cost while still having one implementation. The
-- single-file path passes one-element arrays.

-- ── The reference contract, for SQL callers ──────────────────────────────
--
-- Increment the reference count for each hash in `p_hashes`, counting
-- repeats (pass the hash once per referencing row). Returns the hashes that
-- matched NEITHER table, so callers can decide how loud to be — a copy
-- inherits a pre-existing breakage and should warn, whereas an ingest
-- referencing a nonexistent blob is a hard error.
--
-- MANIFEST FIRST, `storage.blobs` only as fallback. The order is the whole
-- point: a CDC file's `blob_hash` names a manifest
-- (`chunk_manifests.file_hash`), not a chunk, so bumping `storage.blobs`
-- first would match nothing for a multi-chunk file and take no reference at
-- all.
--
-- The `NOT EXISTS (bumped)` guard on the blobs branch is load-bearing. For a
-- SINGLE-chunk file the whole-file hash EQUALS its lone chunk's hash (both
-- are BLAKE3 over the same bytes), so without the guard one reference would
-- be counted at both levels — turning an under-count into an over-count.
--
-- Mirrors `DedupService::add_reference`, including the asymmetry on
-- `orphaned_at`: only `storage.blobs` carries that column, so only the blobs
-- branch clears it. A chunk resurrected inside its GC grace window must lose
-- its orphan stamp or `dedup_gc` reaps live content.
CREATE OR REPLACE FUNCTION storage.add_blob_references(p_hashes TEXT[])
RETURNS TEXT[] AS $$
DECLARE
    v_unmatched TEXT[];
BEGIN
    IF p_hashes IS NULL OR cardinality(p_hashes) = 0 THEN
        RETURN ARRAY[]::TEXT[];
    END IF;

    WITH hc AS (
        SELECT h AS blob_hash, COUNT(*)::int AS cnt
          FROM unnest(p_hashes) AS h
         WHERE h IS NOT NULL
         GROUP BY h
    ),
    bumped_manifests AS (
        UPDATE storage.chunk_manifests m
           SET ref_count = m.ref_count + hc.cnt
          FROM hc
         WHERE m.file_hash = hc.blob_hash
        RETURNING m.file_hash
    ),
    bumped_blobs AS (
        UPDATE storage.blobs b
           SET ref_count   = b.ref_count + hc.cnt,
               orphaned_at = NULL
          FROM hc
         WHERE b.hash = hc.blob_hash
           AND NOT EXISTS (
                   SELECT 1 FROM bumped_manifests WHERE file_hash = hc.blob_hash
               )
        RETURNING b.hash
    )
    SELECT COALESCE(array_agg(hc.blob_hash), ARRAY[]::TEXT[])
      INTO v_unmatched
      FROM hc
     WHERE NOT EXISTS (SELECT 1 FROM bumped_manifests WHERE file_hash = hc.blob_hash)
       AND NOT EXISTS (SELECT 1 FROM bumped_blobs     WHERE hash      = hc.blob_hash);

    RETURN v_unmatched;
END;
$$ LANGUAGE plpgsql;

COMMENT ON FUNCTION storage.add_blob_references(TEXT[]) IS
    'Manifest-first blob reference increment for SQL callers. Returns hashes '
    'that matched no registry row. Rust twin: DedupService::add_reference — '
    'change both together.';

-- ── What follows a file on copy ──────────────────────────────────────────
--
-- `p_old_ids[i]` is copied to `p_new_ids[i]`; the new `storage.files` rows
-- must already be inserted and visible (both callers insert in an earlier
-- statement of the same transaction).
--
-- Every satellite of a copied file belongs in this body. What is NOT here is
-- listed at the bottom, with the reason — the taxonomy is executable rather
-- than living in a document that drifts from the code.
CREATE OR REPLACE FUNCTION storage.copy_file_satellites(
    p_old_ids UUID[],
    p_new_ids UUID[]
) RETURNS void AS $$
DECLARE
    v_unmatched TEXT[];
BEGIN
    IF p_old_ids IS NULL OR cardinality(p_old_ids) = 0 THEN
        RETURN;
    END IF;

    IF p_new_ids IS NULL OR cardinality(p_old_ids) <> cardinality(p_new_ids) THEN
        -- Positional correspondence is the whole interface; a length
        -- mismatch would silently attach satellites to the wrong file.
        RAISE EXCEPTION
            'copy_file_satellites: id arrays must correspond positionally (% old vs % new)',
            cardinality(p_old_ids), COALESCE(cardinality(p_new_ids), 0);
    END IF;

    -- 1. WebDAV dead properties. RFC 4918 §8.8 requires COPY to duplicate
    --    them: properties describe the resource, and the copy is a resource.
    INSERT INTO storage.webdav_dead_properties
        (file_id, namespace, local_name, value)
    SELECT m.new_id, dp.namespace, dp.local_name, dp.value
      FROM unnest(p_old_ids, p_new_ids) AS m(old_id, new_id)
      JOIN storage.webdav_dead_properties dp ON dp.file_id = m.old_id;

    -- 2. A reference on the copied content, so deleting the original cannot
    --    reap bytes the copy still needs. Read from the NEW rows rather than
    --    the old ones: that is what makes an unreferenceable copy impossible
    --    to create, since a row that failed to insert contributes nothing.
    SELECT storage.add_blob_references(array_agg(f.blob_hash))
      INTO v_unmatched
      FROM unnest(p_new_ids) AS n(id)
      JOIN storage.files f ON f.id = n.id
     WHERE NOT f.is_trashed;

    IF v_unmatched IS NOT NULL AND cardinality(v_unmatched) > 0 THEN
        -- Warn, do not abort. A missing registry row means the SOURCE file
        -- was already broken; the copy merely inherits it. Failing here
        -- would abort an entire folder copy over one pre-existing fault,
        -- which is worse than completing it and reporting. The blob-level
        -- audit jobs are what surface the underlying breakage.
        RAISE WARNING
            'copy_file_satellites: % copied file(s) reference a blob with no registry row (first: %); source was already broken',
            cardinality(v_unmatched), v_unmatched[1];
    END IF;

    -- ── Deliberately absent ──────────────────────────────────────────────
    --
    -- storage.comments (future): NOT copied. A copy is a new artifact; the
    --   discussion belongs to the original.
    --
    -- storage.file_attached_blobs (step 9): WILL be copied here, with a
    --   reference taken per attached blob_hash via add_blob_references.
    --
    -- content_derived_blobs, blob_extracted_text, faces.faces: content-keyed.
    --   The copy shares the source's hash, so it already sees them — copying
    --   would duplicate rows that are keyed on the very thing being shared.
    --
    -- storage.favorites, recent_items, shares: properties of the ORIGINAL's
    --   relationship to users, not of its content.
END;
$$ LANGUAGE plpgsql;

COMMENT ON FUNCTION storage.copy_file_satellites(UUID[], UUID[]) IS
    'Single source of truth for what follows a file on copy. Both copy paths '
    '(single-file and copy_folder_tree) call it. Adding a file-keyed satellite '
    'table means editing this function, and only this function.';

-- ── Route copy_folder_tree through it ────────────────────────────────────
--
-- Only two blocks change versus `20261016000000`: the inline reference bump
-- and the per-file dead-property INSERT are both replaced by one
-- `copy_file_satellites` call. The folder dead-property INSERT stays inline
-- — folders are not files and have no satellite fan-out to share.
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
$$ LANGUAGE plpgsql;
