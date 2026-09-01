-- Step 9 of `docs/plan/derived-blobs.md` — the file-keyed half of the pair.
--
-- `content_derived_blobs` holds bytes that are a pure deterministic function
-- of a file's content, so they are keyed by that content and shared across
-- every file holding it. This table holds the opposite: bytes a USER supplied
-- or chose. Those must never be shared across files, and the key is what
-- enforces it.
--
-- The distinction is a security boundary, not a modelling preference. If a
-- client-uploaded preview were content-keyed, user A could upload a file plus
-- a preview that misrepresents it; when user B later uploads the same bytes,
-- dedup would match and B would be served A's preview. Content-keying is only
-- safe when the bytes are derivable from the content by the server — nothing
-- to poison, because anyone with the same input gets the same output.
--
-- Required now rather than deferred: the SPA already generates and PUTs
-- previews for PDFs, and there is no server-side regeneration path for them,
-- so the sidecar migration has nowhere else to put those bytes.

CREATE TABLE IF NOT EXISTS storage.file_attached_blobs (
    file_id      UUID NOT NULL REFERENCES storage.files(id) ON DELETE CASCADE,
    kind         TEXT NOT NULL CHECK (kind IN ('preview', 'subtitle', 'cover_art')),
    variant      TEXT NOT NULL,
    blob_hash    VARCHAR(64) NOT NULL,
    content_type TEXT NOT NULL,
    -- Provenance convention: NOT NULL and NO foreign key. A FK with
    -- ON DELETE SET NULL loses the audit trail exactly when it matters most,
    -- and without an ON DELETE clause it would block deleting a user
    -- outright. Deleting the uploader must not rewrite history, so the id is
    -- retained even once it no longer resolves. Rows imported with no known
    -- uploader carry the all-zeros sentinel.
    uploaded_by  UUID NOT NULL,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (file_id, kind, variant)
);

-- Reverse lookup for the reference recompute and for dedup_gc's reap
-- predicate: both ask "does any row reference this blob?".
CREATE INDEX IF NOT EXISTS idx_file_attached_blobs_blob_hash
    ON storage.file_attached_blobs (blob_hash);

-- ── The routing rule, recorded on both tables ────────────────────────────
-- Choosing the wrong table is a silent poisoning bug rather than a compile
-- error, so the rule lives where an implementor will actually meet it.

COMMENT ON TABLE storage.file_attached_blobs IS
    'User-supplied or user-chosen artifacts (client previews, subtitles, cover art) keyed by FILE. File-keyed on purpose: these bytes are not derivable from the file''s content, so sharing them across files with identical content would let one user''s upload be served for another user''s file. Server-derived bytes must NOT be stored here — see docs/plan/derived-blobs.md.';

COMMENT ON COLUMN storage.file_attached_blobs.file_id IS
    'The file these bytes are attached to. ON DELETE CASCADE: the attachment has no meaning without it. Deleting the row does NOT release the blob reference — the owning service does that in its on_file_deleted hook.';

COMMENT ON COLUMN storage.file_attached_blobs.blob_hash IS
    'The attached Blob. Reference HOLDER — bumps chunk_manifests.ref_count via DedupService::add_reference. Dedup still applies to the bytes themselves; what is forbidden is sharing the MAPPING across files.';

COMMENT ON COLUMN storage.file_attached_blobs.variant IS
    'Opaque discriminator within a kind (preview | en | fr | cover...). New axes go inside this string, never into new columns.';

COMMENT ON COLUMN storage.file_attached_blobs.uploaded_by IS
    'Who supplied these bytes. Retained after the user is deleted — deleting a user must not rewrite provenance. The only trace that an Editor on a shared file replaced the owner''s preview.';

COMMENT ON TABLE storage.content_derived_blobs IS
    'Server-derived artifacts (thumbnails, transcodes) keyed by the BLAKE3 of their SOURCE content. Content-keyed on purpose: identical content shares one derivation. ROUTING RULE — bytes that are a pure deterministic function of the file''s content belong here; bytes that are user-supplied or user-chosen belong in storage.file_attached_blobs, which is file-keyed and never shared. See docs/plan/derived-blobs.md.';

-- ── Teach the copy fan-out about it ──────────────────────────────────────
--
-- Only the attached-blobs arm is new versus `20261019000000`; everything
-- else is that definition verbatim. Adding a file-keyed table is now one
-- edit in one function, which is the whole point of having consolidated the
-- two copy paths first.
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

    -- 3. File-keyed attachments — client previews, subtitles, cover art.
    --    DUPLICATED rather than shared, because the key is `file_id` and the
    --    copy is a different file. `uploaded_by` carries over: the person
    --    who supplied the bytes did not change because someone copied the
    --    file, and rewriting it to the copier would forge provenance.
    --
    --    Each duplicated row is a new reference on the same blob, so the
    --    bytes are still deduplicated — it is the MAPPING that must not be
    --    shared, not the content.
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
