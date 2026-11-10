-- ════════════════════════════════════════════════════════════════════════════
-- Content provenance follows a file on COPY
-- ════════════════════════════════════════════════════════════════════════════
-- Design: `docs/plan/recently-modified.md` §1b. Semantics:
-- `docs/architecture/dates.md` § COPY, amended by this change.
--
-- Two arms added to `storage.copy_file_satellites`; arms 1-3 are the
-- `20261020000000` definition verbatim.
--
-- ── Why here, and why preserve rather than re-stamp ────────────────────────
-- A copy carried `created_by` / `updated_by` from the source already, and this
-- function's attached-blobs arm states the rule for the general case: "the
-- person who supplied the bytes did not change because someone copied the
-- file, and rewriting it to the copier would forge provenance." The content
-- clock and its principal are the same kind of fact — these bytes were last
-- authored by that person, at that time, and copying them did not change
-- either — so they follow the file, exactly as `media_sort_date` already does
-- so a copied photo keeps its place in the timeline.
--
-- Doing it HERE rather than in the two INSERTs is what keeps the single-file
-- and the folder-tree paths from drifting: `copy_file` and
-- `copy_folder_tree` both already call this function, which is the stated
-- single home for everything that follows a file on copy. The alternative —
-- projecting the columns in both INSERTs — needs the whole 170-line
-- `copy_folder_tree` body re-emitted and leaves two places to forget.
--
-- The consequence, deliberately accepted: a copy appears in the ORIGINAL
-- author's lane on `/api/files/recent-changes`, dated when they wrote the
-- bytes, and nobody is recorded as having copied it. Copying is file
-- management, not authorship.
--
-- ── Why the UPDATE is cheap despite the statement triggers ────────────────
-- `storage.files` carries two `AFTER UPDATE … FOR EACH STATEMENT` triggers.
-- Neither can act here: `files_bump_tree_etag_upd` compares
-- (name, folder_id, blob_hash, size, mime_type, is_trashed, updated_at) and
-- `files_enqueue_search_upd` compares (name, blob_hash, is_trashed) — this
-- statement writes none of those columns, so both evaluate to no-ops. They
-- still materialise their transition tables for the copied rows, which is a
-- per-statement cost proportional to the copy, not to the table.
-- ════════════════════════════════════════════════════════════════════════════

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
        RAISE WARNING
            'copy_file_satellites: % copied file(s) reference a blob with no registry row (first: %); source was already broken',
            cardinality(v_unmatched), v_unmatched[1];
    END IF;

    -- 3. File-keyed attachments — client previews, subtitles, cover art.
    --    DUPLICATED rather than shared, because the key is `file_id` and the
    --    copy is a different file. `uploaded_by` carries over: the person
    --    who supplied the bytes did not change because someone copied the
    --    file, and rewriting it to the copier would forge provenance.
    WITH copied AS (
        INSERT INTO storage.file_attached_blobs
            (file_id, kind, variant, blob_hash, content_type, uploaded_by)
        SELECT m.new_id, a.kind, a.variant, a.blob_hash, a.content_type, a.uploaded_by
          FROM unnest(p_old_ids, p_new_ids) AS m(old_id, new_id)
          JOIN storage.file_attached_blobs a ON a.file_id = m.old_id
        RETURNING blob_hash
    )
    SELECT storage.add_blob_references(array_agg(blob_hash))
      INTO v_unmatched
      FROM copied;

    IF v_unmatched IS NOT NULL AND cardinality(v_unmatched) > 0 THEN
        RAISE WARNING
            'copy_file_satellites: % attached blob(s) reference no registry row (first: %); source was already broken',
            cardinality(v_unmatched), v_unmatched[1];
    END IF;

    -- 4. The content clock and its principal, same argument as arm 3: these
    --    bytes were last authored by that person at that time, and copying
    --    them changed neither. Both INSERT paths leave the clock to its
    --    DEFAULT NOW() and the principal to NULL, so this is where a copy
    --    gets its real answer.
    UPDATE storage.files nf
       SET content_modified_at = src.content_modified_at,
           content_modified_by = src.content_modified_by
      FROM unnest(p_old_ids, p_new_ids) AS m(old_id, new_id)
      JOIN storage.files src ON src.id = m.old_id
     WHERE nf.id = m.new_id;

    -- 5. The lanes. One row per (file, person), so the copy inherits the
    --    source's editors and shows up where their work shows up. Keyed on
    --    the NEW file id, so this duplicates the mapping rather than sharing
    --    it — the same distinction arm 3 draws for attachments.
    INSERT INTO storage.file_content_touches (file_id, user_id, touched_at)
    SELECT m.new_id, t.user_id, t.touched_at
      FROM unnest(p_old_ids, p_new_ids) AS m(old_id, new_id)
      JOIN storage.file_content_touches t ON t.file_id = m.old_id
    ON CONFLICT (file_id, user_id) DO UPDATE
        SET touched_at = GREATEST(
                storage.file_content_touches.touched_at, EXCLUDED.touched_at);

    -- ── Deliberately absent ──────────────────────────────────────────────
    --
    -- storage.comments (future): NOT copied. A copy is a new artifact; the
    --   discussion belongs to the original.
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
    'Everything that follows a file on COPY, in one place for both copy paths: dead properties, a blob reference, file-keyed attachments, the content clock + principal, and the content-touch lanes. Provenance is PRESERVED throughout — copying bytes does not make the copier their author. Arrays correspond positionally; a length mismatch raises.';
