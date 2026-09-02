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
--
-- ═══════════════════════════════════════════════════════════════════
-- Performance envelope (rewrite 2026-09-02)
-- ═══════════════════════════════════════════════════════════════════
-- Original implementation used correlated subqueries in both SET and
-- WHERE clauses — PG evaluates each subquery twice per row, and the
-- `b.hash = ANY(m.chunk_hashes)` scan is O(blobs × manifests) without
-- a GIN index. On a production customer with a large storage.blobs +
-- storage.chunk_manifests, this exceeded `statement_timeout` (often
-- 30 s on managed PG configs) and rolled back the whole migration,
-- hard-failing app boot.
--
-- Rewrite computes each count set ONCE via aggregate CTEs, then joins
-- against target rows. Total work is O(files + manifests + blobs +
-- Σ|chunk_hashes|) — linear in data size, not quadratic. Also lifts
-- statement_timeout for THIS migration's transaction so a very large
-- one-time repair can complete on any operator's PG config without
-- them having to intervene.
--
-- Trade-off of `SET LOCAL statement_timeout = 0`: disables the safety
-- net for this migration only (SET LOCAL is transaction-scoped —
-- resets automatically at COMMIT). Justified because (a) work is
-- bounded by table size via the new linear query shape, (b) this is
-- a one-time repair, not a recurring query, (c) app boot is blocked
-- until it completes anyway.
--
-- Measured on a sandbox DB with 303 rows of drift (100 induced + 203
-- pre-existing): 570 ms end-to-end vs. timeout in the original form.

-- Lift the timeout for this migration only. Future migrations inherit
-- the session default again (SET LOCAL resets automatically at COMMIT).
SET LOCAL statement_timeout = 0;

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
    --
    -- Structure: one GROUP BY over storage.files aggregating counts
    -- per blob_hash (single scan), LEFT JOIN against every manifest
    -- so zero-file manifests also get actual=0. UPDATE ... FROM
    -- walks manifests once, writes only where stored <> actual.
    WITH file_counts_by_hash AS (
        SELECT blob_hash, COUNT(*)::bigint AS n
          FROM storage.files
         GROUP BY blob_hash
    ),
    actual_per_manifest AS (
        SELECT m.file_hash,
               COALESCE(fc.n, 0) AS actual
          FROM storage.chunk_manifests m
          LEFT JOIN file_counts_by_hash fc ON fc.blob_hash = m.file_hash
    )
    UPDATE storage.chunk_manifests m
       SET ref_count = a.actual
      FROM actual_per_manifest a
     WHERE a.file_hash = m.file_hash
       AND m.ref_count <> a.actual;
    GET DIAGNOSTICS v_m_fixed = ROW_COUNT;

    -- Blob counter: two-term formula mirroring
    -- `blobs_consistency_service.rs:395-408`:
    --   (files pointing at this blob AND having NO manifest for their
    --    blob_hash — legacy whole-file path)
    -- + (manifests including this hash as a chunk in chunk_hashes[])
    --
    -- Structure: two aggregate CTEs (one per term), then LEFT JOINed
    -- against every blob. `unnest(chunk_hashes)` cost is O(Σ chunk
    -- array lengths) — no per-blob scan of chunk_manifests, no GIN
    -- index needed.
    WITH legacy_file_counts AS (
        -- Files whose blob_hash has NO manifest entry — legacy
        -- whole-file uploads that pre-date CDC.
        SELECT f.blob_hash, COUNT(*)::bigint AS legacy_count
          FROM storage.files f
         WHERE NOT EXISTS (
             SELECT 1 FROM storage.chunk_manifests m
              WHERE m.file_hash = f.blob_hash
         )
         GROUP BY f.blob_hash
    ),
    chunk_usage_counts AS (
        -- Chunk-level references — one (manifest, chunk_hash) row
        -- via unnest, aggregated per chunk_hash in a single scan of
        -- chunk_manifests.
        SELECT ch AS hash, COUNT(*)::bigint AS chunk_count
          FROM storage.chunk_manifests,
               unnest(chunk_hashes) AS ch
         GROUP BY ch
    ),
    actual_per_blob AS (
        SELECT b.hash,
               COALESCE(l.legacy_count, 0) + COALESCE(u.chunk_count, 0) AS actual
          FROM storage.blobs b
          LEFT JOIN legacy_file_counts l ON l.blob_hash = b.hash
          LEFT JOIN chunk_usage_counts u ON u.hash        = b.hash
    )
    UPDATE storage.blobs b
       SET ref_count = a.actual
      FROM actual_per_blob a
     WHERE a.hash = b.hash
       AND b.ref_count <> a.actual;
    GET DIAGNOSTICS v_b_fixed = ROW_COUNT;

    -- Landed in the deploy log so an operator upgrading a huge instance
    -- can see the migration did work — silent no-op on fresh installs.
    RAISE NOTICE '[refcount_repair] fixed % manifest(s), % blob(s)',
                 v_m_fixed, v_b_fixed;
END;
$$;
