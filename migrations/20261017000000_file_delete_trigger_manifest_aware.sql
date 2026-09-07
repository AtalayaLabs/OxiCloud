-- Fix: `trg_files_decrement_blob_ref` decremented the wrong counter for
-- CDC files.
--
-- The original trigger (2026-03-07 initial schema) unconditionally ran:
--
--     UPDATE storage.blobs
--        SET ref_count = GREATEST(ref_count - 1, 0)
--      WHERE hash = OLD.blob_hash;
--
-- That's correct for a legacy whole-file blob, where `OLD.blob_hash`
-- names a `storage.blobs` row directly. For a CDC file, `OLD.blob_hash`
-- names a `storage.chunk_manifests.file_hash` — the blob table row (if
-- one exists at all) holds a DIFFERENT counter, incremented by the
-- MANIFEST's presence in its own `chunk_hashes[]`, not by the file.
--
-- Consequences before this fix:
--   1. `storage.chunk_manifests.ref_count` never decremented on file
--      DELETE → over-count grows unboundedly across delete/purge
--      cycles.
--   2. `storage.blobs.ref_count` decremented for hashes it shouldn't
--      (CDC whole-file hashes) → the counter drops toward 0 while the
--      manifest still legitimately references the chunk. GC then reaps
--      a live blob → downloadable-then-404 data loss.
--
-- Both bugs surfaced by `tests/api/refcount_cascade.hurl` on the
-- 135-byte fixture (single-chunk CDC file, worst case for confusion
-- because the whole-file hash equals its lone chunk's hash). The
-- 2026-08-22 sandbox drift (`storage.blobs.ref_count = 0`,
-- `actual_auditor = 1`) is the same bug at rest.
--
-- Sibling fix: `20261016000000_copy_folder_tree_manifest_refcount.sql`
-- fixed the mirror-image INCREMENT bug in `storage.copy_folder_tree`.
-- This migration closes the decrement half.
--
-- Cross-references:
--   - `DedupService::add_reference` (dedup_service.rs:1703) — app-layer
--     twin for the increment direction: manifest first, blob fallback.
--   - `manifests_consistency` tenant (2026-08-23) — surfaces any
--     residual drift after this fix lands.
--
-- ── DESIGN NOTE — decrement only, no manifest reap here ──
--
-- The trigger DELIBERATELY does not delete manifests or walk chunks on
-- a last-ref decrement. Both actions used to live inside
-- `DedupService::cleanup_if_orphaned` and its callee
-- `remove_manifest_reference`, and both fire `fire_blob_hooks` —
-- the Rust callback that reaps disk artefacts keyed by the whole-file
-- content hash (thumbnails, face embeddings, audio tags, media
-- metadata). SQL triggers can't invoke Rust callbacks, so if this
-- trigger reaped the manifest itself, dedup_gc Phase 1
-- (`dedup_service.rs:2660-2772`) — the ONLY code path that knows to
-- fire `fire_blob_hooks` for a reaped manifest's `file_hash` — would
-- find nothing to do on its next sweep, and every derived artefact
-- would leak on disk. `storage_cleanup_check.sh`'s "N thumbnail
-- file(s) remain on disk" gate catches this class immediately.
--
-- Contract: trigger decrements the correct counter atomically inside
-- the DELETE txn. GC (`dedup_gc`) is responsible for:
--   • finding manifests whose ref_count hit 0 (or that no reference
--     source references, covering bulk-delete paths),
--   • deleting them,
--   • decrementing each chunk in `chunk_hashes[]`,
--   • firing `fire_blob_hooks(file_hash)` so Rust callbacks reap
--     derived disk artefacts,
--   • the corresponding legacy-blob path for ref_count = 0 blobs.
--
-- NOTE: pre-existing drift is NOT repaired here. Run `manifests_
-- consistency` + `blobs_consistency` after deploy; feed the findings
-- into the recovery framework.

CREATE OR REPLACE FUNCTION storage.decrement_blob_ref()
RETURNS trigger AS $$
BEGIN
    -- Manifest-first, mirroring the increment side. We touch ONE
    -- counter and return — the manifest reap + chunk walk + hook
    -- firing lives in `dedup_gc` where Rust callbacks can run.
    IF EXISTS (
        SELECT 1 FROM storage.chunk_manifests
         WHERE file_hash = OLD.blob_hash
    ) THEN
        UPDATE storage.chunk_manifests
           SET ref_count = GREATEST(ref_count - 1, 0)
         WHERE file_hash = OLD.blob_hash;
    ELSE
        -- Legacy whole-file blob path: no manifest, blob is referenced
        -- directly by this file row. Preserves the original behaviour
        -- verbatim for the pre-CDC path.
        UPDATE storage.blobs
           SET ref_count   = GREATEST(ref_count - 1, 0),
               orphaned_at = CASE
                                WHEN GREATEST(ref_count - 1, 0) = 0
                                THEN now()
                                ELSE orphaned_at
                             END
         WHERE hash = OLD.blob_hash;
    END IF;

    RETURN OLD;
END;
$$ LANGUAGE plpgsql;

COMMENT ON FUNCTION storage.decrement_blob_ref() IS
    'Decrement the correct ref_count when a file is deleted. '
    'Manifest-aware (2026-10-17): dispatches to chunk_manifests.ref_count '
    'when the file''s blob_hash names a manifest, else to '
    'storage.blobs.ref_count for legacy whole-file blobs. Decrement-only: '
    'physical cleanup + Rust lifecycle hooks fire from dedup_gc, which '
    'can invoke callbacks a SQL trigger cannot.';
