-- Derived content as blobs — tier-2 refactor, step 5.
-- See `docs/plan/derived-blobs.md`.
--
-- Maps a source Blob to the artifacts derived FROM it: thumbnails today,
-- transcodes next. Both the mapping key and the value are BLAKE3 hashes,
-- but they mean different things:
--
--   * `source_hash` — the Blob the artifact was derived from. A
--     *dependent* reference: it keeps nothing alive (the file does), and
--     when that Blob dies these rows are deleted with it.
--   * `blob_hash`   — the derived Blob itself. A reference *holder*: it
--     bumps `chunk_manifests.ref_count`, which is why
--     `ContentDerivedReferenceSource` must be registered before the first
--     row is written, or `dedup_gc` reaps the content on its next sweep.
--
-- KEYING — the rule this table exists to enforce:
--
--   Bytes that are a pure deterministic function of the source content
--   belong here, content-keyed, and dedupe across every file holding
--   that content. Bytes that are user-supplied or user-chosen do NOT:
--   they must be file-keyed, because content-keying them lets one user's
--   upload be served for another user's identical file. Client-uploaded
--   previews (PDF page 1, video poster frames) are the live example and
--   belong in a separate file-keyed table.
--
-- `variant` is opaque text. New axes go INSIDE it, never into new
-- columns: 'preview-avif' beside 'preview', '720p-av1' beside '720p'.
-- That is what keeps this table from growing a column per rendering
-- parameter.
--
-- No FK on either hash column, for the reason
-- `20260701000000_content_search_index.sql` already documents: a hash
-- resolves to either `storage.blobs` (legacy whole blob) or
-- `storage.chunk_manifests` (CDC file hash), so the reference cannot be
-- expressed as a single FK. Orphans are reclaimed by GC and reported by
-- the consistency jobs instead.
--
-- No `size` column: the bytes are content-addressed, so their length is
-- an immutable fact the blob layer already owns via `blob_hash`.
-- `content_type` IS stored — the thumbnail handler byte-sniffs every
-- response today, and this retires that.

CREATE TABLE IF NOT EXISTS storage.content_derived_blobs (
    source_hash  VARCHAR(64) NOT NULL,
    kind         TEXT        NOT NULL CHECK (kind IN ('thumbnail', 'transcode')),
    variant      TEXT        NOT NULL,
    blob_hash    VARCHAR(64) NOT NULL,
    content_type TEXT        NOT NULL,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (source_hash, kind, variant)
);

-- Reverse lookup: "what still references this derived Blob?" — used by
-- the manifest-level refcount recompute in `manifests_consistency` and by
-- `dedup_gc`'s reap predicate.
CREATE INDEX IF NOT EXISTS idx_content_derived_blobs_blob_hash
    ON storage.content_derived_blobs (blob_hash);

COMMENT ON TABLE storage.content_derived_blobs IS
    'Server-derived artifacts (thumbnails, transcodes) keyed by the BLAKE3 of their SOURCE content. Content-keyed on purpose: identical content shares one derivation. User-supplied bytes must NOT be stored here — see docs/plan/derived-blobs.md.';

COMMENT ON COLUMN storage.content_derived_blobs.source_hash IS
    'The Blob this was derived from. Dependent reference — holds no ref_count; rows are deleted when the source Blob is reaped.';

COMMENT ON COLUMN storage.content_derived_blobs.blob_hash IS
    'The derived Blob. Reference HOLDER — bumps chunk_manifests.ref_count via DedupService::add_reference.';

COMMENT ON COLUMN storage.content_derived_blobs.variant IS
    'Opaque rendering discriminator (icon | preview | large | 720p...). New axes go inside this string, never into new columns.';
