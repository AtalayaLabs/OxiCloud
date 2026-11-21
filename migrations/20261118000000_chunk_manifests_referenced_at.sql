-- GC phase 1 grace for manifests (#917): an upload references its manifest
-- before its storage.files row commits, so GC must not reap a manifest whose
-- reference was taken within the grace window. The trigger covers every path
-- that raises ref_count.

ALTER TABLE storage.chunk_manifests
    ADD COLUMN IF NOT EXISTS referenced_at TIMESTAMPTZ NOT NULL DEFAULT now();

CREATE OR REPLACE FUNCTION storage.chunk_manifests_stamp_referenced_at()
RETURNS trigger
LANGUAGE plpgsql
AS $$
BEGIN
    NEW.referenced_at := now();
    RETURN NEW;
END;
$$;

DROP TRIGGER IF EXISTS trg_chunk_manifests_referenced_at ON storage.chunk_manifests;
CREATE TRIGGER trg_chunk_manifests_referenced_at
    BEFORE UPDATE OF ref_count ON storage.chunk_manifests
    FOR EACH ROW
    WHEN (NEW.ref_count > OLD.ref_count)
    EXECUTE FUNCTION storage.chunk_manifests_stamp_referenced_at();
