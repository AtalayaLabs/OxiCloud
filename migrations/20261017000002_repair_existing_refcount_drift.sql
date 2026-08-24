-- One-time repair of ref_count drift accumulated under the pre-fix
-- copy/delete code paths.
--
-- Why this is atomic with the upgrade rather than a manual admin action
-- ─────────────────────────────────────────────────────────────────────
-- The two prior migrations on this branch:
--   * `20261016000000_copy_folder_tree_manifest_refcount.sql`
--     (fix the INCREMENT path — copy was bumping the wrong counter for
--     CDC files)
--   * `20261017000000_file_delete_trigger_manifest_aware.sql`
--     (fix the DECREMENT path — trigger was decrementing the wrong
--     counter for CDC files; folder-cascade + trash-empty paths
--     inherited that drift silently)
-- both close the bugs going forward, but production DBs upgrading
-- through this branch may carry accumulated drift from every prior
-- copy → delete cycle a CDC file went through. Under-count is the
-- dangerous direction: the next `dedup_gc` pass would reap a live
-- blob → user-facing 404 → silent data loss.
--
-- Waiting for an operator to open the admin panel and click "Repair
-- ref_counts" is the wrong default for a data-loss-preventing fix.
-- Ed's rule (`[[feedback_no_silent_auto_repair]]`): consistency
-- tenants must default to discovery-only so future bugs surface — but
-- fixing KNOWN pre-existing drift on the upgrade itself is the
-- bounded exception, because at that specific moment the source of
-- drift is known + closed, and there is no upstream mystery to
-- preserve.
--
-- Content-safety guarantees:
--   * Only counter columns change (`storage.chunk_manifests.ref_count`,
--     `storage.blobs.ref_count`). No file rows, no blob rows, no
--     manifest rows, no chunk arrays, no backend files.
--   * The corrective UPDATE sets `stored = actual` where `actual` is
--     computed from the SAME auditor formulas that
--     `manifests_consistency` / `blobs_consistency` use, so this
--     migration and those tenants agree by construction.
--   * Race-safe against concurrent writes (migrations run
--     single-connection at startup before the server serves any
--     traffic; nobody else is writing).
--   * Idempotent — fresh installs and already-clean DBs no-op (both
--     `stored` and `actual` are equal, the `WHERE <>` filters
--     everything out).
--
-- The panel button + `?repair=true` on the trigger endpoints stay for
-- FUTURE drift (regression detector; not for repeat use on this
-- accumulated set).

DO $$
DECLARE
    v_m_fixed int;
    v_b_fixed int;
BEGIN
    -- Manifest counter: `actual` = # files whose blob_hash names this
    -- manifest's file_hash. Same formula as
    -- `manifests_consistency_service::manifest_page_sql` (via the
    -- BlobReferenceRegistry at RefLevel::Manifest) — inline here
    -- because migrations can't call Rust.
    UPDATE storage.chunk_manifests m
       SET ref_count = (SELECT COUNT(*) FROM storage.files
                         WHERE blob_hash = m.file_hash)
     WHERE m.ref_count <> (SELECT COUNT(*) FROM storage.files
                            WHERE blob_hash = m.file_hash);
    GET DIAGNOSTICS v_m_fixed = ROW_COUNT;

    -- Blob counter: two-term formula mirroring
    -- `blobs_consistency_service.rs:395-408`:
    --   (files pointing at this blob AND having NO manifest for their
    --    blob_hash — legacy whole-file path)
    -- + (manifests including this hash as a chunk in chunk_hashes[])
    UPDATE storage.blobs b
       SET ref_count = (
           (SELECT COUNT(*) FROM storage.files f
             WHERE f.blob_hash = b.hash
               AND NOT EXISTS (SELECT 1 FROM storage.chunk_manifests m
                                WHERE m.file_hash = f.blob_hash))
         + (SELECT COUNT(*) FROM storage.chunk_manifests m
             WHERE b.hash = ANY(m.chunk_hashes))
       )
     WHERE b.ref_count <> (
           (SELECT COUNT(*) FROM storage.files f
             WHERE f.blob_hash = b.hash
               AND NOT EXISTS (SELECT 1 FROM storage.chunk_manifests m
                                WHERE m.file_hash = f.blob_hash))
         + (SELECT COUNT(*) FROM storage.chunk_manifests m
             WHERE b.hash = ANY(m.chunk_hashes))
       );
    GET DIAGNOSTICS v_b_fixed = ROW_COUNT;

    -- Landed in the deploy log so an operator upgrading a huge instance
    -- can see the migration did work — silent no-op on fresh installs.
    RAISE NOTICE '[refcount_repair] fixed % manifest(s), % blob(s)',
                 v_m_fixed, v_b_fixed;
END;
$$;
