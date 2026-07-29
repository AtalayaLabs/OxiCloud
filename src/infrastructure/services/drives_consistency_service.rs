//! First tenant of Part 2 (recoverable-run engine).
//!
//! Iterates `storage.drives` and reports each drive whose cached
//! `used_bytes` differs from `SUM(files.size) WHERE NOT is_trashed`
//! for that drive. **Read-only** — reports drift as findings but does
//! NOT fix it. The existing `storage_reconcile` job (Part 1) is what
//! corrects the counter; this check surfaces WHEN drift happens so
//! operators can trace it back to root cause (missed delta call,
//! delta failed silently, race, etc.).
//!
//! One check today — `used_bytes` drift — but structured so more
//! checks can slot in as per-row branches (quota-vs-usage inversion,
//! `kind` vs `default_for_user` invariants, ...). See memory note
//! `project_consistency_jobs_landscape`.
//!
//! Findings are LOGGED to `target: "oxicloud::consistency"` for now.
//! Persistence to `jobs.run_findings` lands with the findings-table
//! migration (deferred; see the plan doc). Once landed, this handler
//! swaps its `tracing::warn!` finding calls for
//! `store.record_finding(...)` — nothing else changes.

use std::sync::Arc;

use async_trait::async_trait;
use sqlx::PgPool;
use uuid::Uuid;

use crate::infrastructure::scheduler::{
    JobRegistry, JobRunArgs, JobStore, JobStoreProvider, RecoverableJobHandler, RunOutcome,
    RunStatus, record_or_log,
};

pub const DRIVES_CONSISTENCY_JOB_NAME: &str = "drives_consistency";

/// Rows per batch. Drives are few (dozens per install), so this only
/// matters for the cancel-poll cadence — smaller batch = more frequent
/// status polls but more DB round-trips. 100 is comfortably fast for
/// any realistic drive count.
const BATCH_SIZE: i64 = 100;

pub struct DrivesConsistencyCheck {
    pool: Arc<PgPool>,
}

impl DrivesConsistencyCheck {
    pub fn new(pool: Arc<PgPool>) -> Self {
        Self { pool }
    }

    /// Register self with the periodic-job scheduler as a recoverable
    /// job, on-demand only (no periodic tick). Follows the same
    /// chainable pattern as Part 1 tenants' `register_job` — DI stays
    /// one line.
    pub async fn register_recoverable_job(
        self: Arc<Self>,
        registry: &JobRegistry,
        provider: &Arc<dyn JobStoreProvider>,
    ) -> Arc<Self> {
        registry
            .register_recoverable_job(self.clone(), provider.clone(), None)
            .await;
        self
    }
}

#[async_trait]
impl RecoverableJobHandler for DrivesConsistencyCheck {
    fn name(&self) -> &str {
        DRIVES_CONSISTENCY_JOB_NAME
    }

    /// Definitive count — one row per drive, table is tiny (dozens per
    /// install), COUNT(*) is trivially fast. Enables progress bar on
    /// the admin UI.
    async fn count_total(&self) -> Option<u64> {
        let row: Result<(i64,), sqlx::Error> =
            sqlx::query_as("SELECT COUNT(*) FROM storage.drives")
                .fetch_one(self.pool.as_ref())
                .await;
        match row {
            Ok((n,)) => Some(n.max(0) as u64),
            Err(e) => {
                tracing::debug!(
                    target: "oxicloud::consistency",
                    event = "drives_consistency.count_total_failed",
                    error = %e,
                    "count_total failed — run will not surface a progress bar"
                );
                None
            }
        }
    }

    async fn run_resumable(
        &self,
        store: &dyn JobStore,
        _args: &JobRunArgs,
        resume_cursor: Option<Vec<u8>>,
    ) -> RunOutcome {
        // Decode cursor. Convention for this job: 16 raw UUID bytes,
        // or empty/absent = start from the beginning.
        let mut cursor: Option<Uuid> = match resume_cursor {
            None => None,
            Some(bytes) if bytes.is_empty() => None,
            Some(bytes) if bytes.len() == 16 => {
                let mut arr = [0u8; 16];
                arr.copy_from_slice(&bytes);
                Some(Uuid::from_bytes(arr))
            }
            Some(bytes) => {
                return RunOutcome::Failed {
                    message: format!("invalid cursor: expected 16 bytes, got {}", bytes.len()),
                };
            }
        };

        let mut drift_count = 0u64;

        loop {
            // Cancel poll BETWEEN batches — the cooperative cancel
            // contract (`RecoverableJobHandler` trait doc).
            match store.status().await {
                Ok(RunStatus::CancelRequested) => {
                    tracing::info!(
                        target: "oxicloud::consistency",
                        event = "drives_consistency.cancelled",
                        run_id = %store.run_id(),
                        drift_count = drift_count,
                        "drives_consistency cancelled cooperatively, pausing"
                    );
                    return RunOutcome::Paused {
                        cursor: cursor.map(|u| u.as_bytes().to_vec()).unwrap_or_default(),
                    };
                }
                Ok(_) => {}
                Err(e) => {
                    return RunOutcome::Failed {
                        message: format!("status poll: {e}"),
                    };
                }
            }

            // Fetch next batch of drives + their actual SUM in one
            // query. LEFT JOIN via correlated subquery gets us both
            // sides in one round-trip; the storage_reconcile sweep
            // uses the same shape.
            // Grace window: skip drives created within the last hour.
            // A drive being created RIGHT NOW may still have its first
            // upload's `used_bytes` counter not-yet-incremented while
            // the `files` row is already visible — that would false-
            // positive as `stale_used_bytes`. 1h matches the window
            // `blobs_consistency` uses; same rationale (writes-in-flight).
            // NOTE: `storage.drives` has no `name` column. The drive's
            // display name lives on its root folder (see the schema
            // comment on `drives.root_folder_id` — "The display name
            // lives here"). LEFT JOIN storage.folders ON id =
            // drive.root_folder_id and read `folders.name` as the
            // drive's human identifier. `COALESCE` handles the
            // (bug-only) case where root_folder_id is NULL.
            let rows: Vec<(Uuid, String, i64, i64)> = match sqlx::query_as(
                r#"
                SELECT
                    d.id                          AS id,
                    COALESCE(rf.name, '?')        AS name,
                    d.used_bytes                  AS used_bytes,
                    COALESCE((
                        SELECT SUM(size)::bigint
                          FROM storage.files
                         WHERE drive_id = d.id
                           AND NOT is_trashed
                    ), 0)                          AS actual_bytes
                  FROM storage.drives d
                  LEFT JOIN storage.folders rf ON rf.id = d.root_folder_id
                 WHERE ($1::uuid IS NULL OR d.id > $1)
                   AND d.created_at < NOW() - INTERVAL '1 hour'
                 ORDER BY d.id
                 LIMIT $2
                "#,
            )
            .bind(cursor)
            .bind(BATCH_SIZE)
            .fetch_all(self.pool.as_ref())
            .await
            {
                Ok(r) => r,
                Err(e) => {
                    return RunOutcome::Failed {
                        message: format!("batch fetch: {e}"),
                    };
                }
            };

            if rows.is_empty() {
                tracing::info!(
                    target: "oxicloud::consistency",
                    event = "drives_consistency.completed",
                    run_id = %store.run_id(),
                    drift_count = drift_count,
                    "drives_consistency completed with {} drift finding(s)",
                    drift_count
                );
                return RunOutcome::Completed;
            }

            // Per-row check: cached vs actual. This is the ONE check
            // in v1 — more per-row branches (quota inversion, kind vs
            // default_for_user, …) slot in here.
            for (drive_id, drive_name, cached, actual) in &rows {
                if *cached != *actual {
                    drift_count += 1;
                    // Persisted finding via the shared helper.
                    // `stale_used_bytes` + severity `inconsistent`
                    // (counters wrong, content intact — the
                    // reconciliation sweep will fix on its next tick).
                    record_or_log(
                        store,
                        DRIVES_CONSISTENCY_JOB_NAME,
                        "stale_used_bytes",
                        "inconsistent",
                        Some(*drive_id),
                        serde_json::json!({
                            "name":   drive_name,
                            "cached": cached,
                            "actual": actual,
                            "delta":  cached - actual,
                        }),
                    )
                    .await;
                }
            }

            // Advance cursor to the last row's id + checkpoint.
            let last_id = rows
                .last()
                .map(|(id, _, _, _)| *id)
                .expect("non-empty rows");
            cursor = Some(last_id);
            let batch_len = rows.len() as u64;
            if let Err(e) = store
                .checkpoint(last_id.as_bytes().to_vec(), batch_len)
                .await
            {
                return RunOutcome::Failed {
                    message: format!("checkpoint: {e}"),
                };
            }

            // Short batch = drained the drives table.
            if (rows.len() as i64) < BATCH_SIZE {
                tracing::info!(
                    target: "oxicloud::consistency",
                    event = "drives_consistency.completed",
                    run_id = %store.run_id(),
                    drift_count = drift_count,
                    "drives_consistency completed with {} drift finding(s)",
                    drift_count
                );
                return RunOutcome::Completed;
            }
        }
    }
}

// ─── Integration tests — real PG round-trip ─────────────────────────────────
//
// Gated on `--cfg integration_tests` (see `just test-integration`).
// Requires a running test PG on 5433 with `oxicloud_test` DB, schema
// applied via `tests/common/init-test-schema.sh`. Runs:
//   just test-integration --  drives_consistency_service
//
// Tests exercise the full recoverable-run engine against real PG:
//   - PgJobStoreProvider::open_or_start creates a run row.
//   - Handler walks a seeded drive, checkpoints, marks Completed.
//   - `stats.scanned_count` bumped, drive row untouched (read-only).
//   - Drift is DETECTED — surfaced as a `consistency_finding` event
//     on the `oxicloud::consistency` tracing target. Captured via a
//     scoped subscriber.

#[cfg(integration_tests)]
#[allow(dead_code, unused_imports)] // items are exercised by #[tokio::test]
// fns; cargo check --lib doesn't see the
// test entry-point call graph.
mod integration_tests {
    use super::*;
    use crate::infrastructure::scheduler::{JobStoreProvider, OpenedRun, RunStatus};
    use sqlx::Row;
    use sqlx::postgres::PgPoolOptions;

    async fn test_pool() -> Arc<sqlx::PgPool> {
        let url = crate::integration_test_support::test_db_url();
        let pool = PgPoolOptions::new()
            .max_connections(4)
            .connect(&url)
            .await
            .expect("connect to test DB — run tests/common/spawn-db.sh first");
        Arc::new(pool)
    }

    /// Seed a personal drive with `used_bytes = cached` and one file
    /// of size `actual` (post-D7 schema — no `user_id` on files /
    /// folders, drives created via the circular-FK dance).
    ///
    /// `default_for_user = NULL` on the drive so we don't collide
    /// with the seeded user's real default (partial unique index).
    /// Returns the drive id.
    async fn seed_drift(pool: &sqlx::PgPool, cached: i64, actual: i64) -> Uuid {
        let owner_id: Uuid = sqlx::query("SELECT id FROM auth.users LIMIT 1")
            .fetch_one(pool)
            .await
            .expect("test DB must have at least one user")
            .get(0);

        // Steps 1-3 MUST run inside one transaction because
        // `trg_no_orphan_root_folder` is DEFERRABLE INITIALLY DEFERRED
        // (fires at COMMIT). Autocommit-per-statement would trip the
        // trigger on the folder INSERT before the drive UPDATE gets a
        // chance to close the FK. Mirrors `DrivePgRepository::
        // create_personal_drive_atomic`.
        let mut tx = pool.begin().await.expect("begin drive-create tx");

        // 1. Drive with kind=personal, no default_for_user (avoids
        //    partial-unique conflict with owner's real default),
        //    used_bytes=0 for now — we set the fake value LAST.
        let drive_id: Uuid = sqlx::query_scalar(
            r#"
            INSERT INTO storage.drives
                (kind, default_for_user, quota_bytes, used_bytes)
            VALUES ('personal', NULL, NULL, 0)
            RETURNING id
            "#,
        )
        .fetch_one(&mut *tx)
        .await
        .expect("insert test drive");

        // 2. Root folder for the drive (parent_id = NULL = drive root).
        //    Post-D7: only `name`, `parent_id`, `drive_id`, `created_by`,
        //    `updated_by` on the INSERT — `user_id`/`path`/`ltree` are
        //    dropped or derived.
        let root_folder_id: Uuid = sqlx::query_scalar(
            r#"
            INSERT INTO storage.folders
                (name, parent_id, drive_id, created_by, updated_by)
            VALUES ('drift-test-root', NULL, $1, $2, $2)
            RETURNING id
            "#,
        )
        .bind(drive_id)
        .bind(owner_id)
        .fetch_one(&mut *tx)
        .await
        .expect("insert root folder for test drive");

        // 3. Close the circular FK: drive.root_folder_id points at
        //    the folder we just created.
        sqlx::query("UPDATE storage.drives SET root_folder_id = $1 WHERE id = $2")
            .bind(root_folder_id)
            .bind(drive_id)
            .execute(&mut *tx)
            .await
            .expect("wire drive.root_folder_id");

        tx.commit()
            .await
            .expect("commit drive-create tx (deferred trigger fires here)");

        // 4. Optionally insert a file summing to `actual`. Also insert
        //    a matching `storage.blobs` row so `trg_files_decrement_blob_ref`
        //    stays happy on cleanup. Fake hash is 64-char hex derived
        //    from a UUID — plausible shape, unique per fixture invocation.
        if actual > 0 {
            let fake_hash = format!(
                "{:032x}{:032x}",
                Uuid::new_v4().as_u128(),
                Uuid::new_v4().as_u128()
            );
            sqlx::query(
                r#"
                INSERT INTO storage.blobs (hash, size, ref_count, content_type)
                VALUES ($1, $2, 1, 'application/octet-stream')
                ON CONFLICT (hash) DO NOTHING
                "#,
            )
            .bind(&fake_hash)
            .bind(actual)
            .execute(pool)
            .await
            .expect("insert fixture blob");

            sqlx::query(
                r#"
                INSERT INTO storage.files
                    (name, folder_id, drive_id, blob_hash, size,
                     mime_type, is_trashed, created_by, updated_by)
                VALUES ($1, $2, $3, $4, $5,
                        'application/octet-stream', false, $6, $6)
                "#,
            )
            .bind(format!("drift-fixture-{}.bin", Uuid::new_v4()))
            .bind(root_folder_id)
            .bind(drive_id)
            .bind(&fake_hash)
            .bind(actual)
            .bind(owner_id)
            .execute(pool)
            .await
            .expect("insert fixture file");
        }

        // 5. Set the artificially-wrong cached used_bytes. LAST, so
        //    no INSERT-side trigger overwrites our fake (there is no
        //    such trigger today, but ordering is cheap insurance).
        sqlx::query("UPDATE storage.drives SET used_bytes = $1 WHERE id = $2")
            .bind(cached)
            .bind(drive_id)
            .execute(pool)
            .await
            .expect("set fake used_bytes");

        drive_id
    }

    async fn cleanup_test_drive(pool: &sqlx::PgPool, drive_id: Uuid) {
        // Cascading FK on files.drive_id, folders.drive_id kicks in.
        sqlx::query("DELETE FROM storage.files WHERE drive_id = $1")
            .bind(drive_id)
            .execute(pool)
            .await
            .ok();
        sqlx::query("DELETE FROM storage.folders WHERE drive_id = $1")
            .bind(drive_id)
            .execute(pool)
            .await
            .ok();
        sqlx::query("DELETE FROM storage.drives WHERE id = $1")
            .bind(drive_id)
            .execute(pool)
            .await
            .ok();
    }

    async fn cleanup_run(pool: &sqlx::PgPool, run_id: Uuid) {
        sqlx::query("DELETE FROM jobs.recoverable_runs WHERE id = $1")
            .bind(run_id)
            .execute(pool)
            .await
            .ok();
    }

    // ─── Parallel-test serialization ───────────────────────────────────────
    //
    // Cargo runs `#[test]` fns in parallel; the three tests in this
    // module all target `job_name = 'drives_consistency'` in
    // `jobs.recoverable_runs`. Without serialization,
    // `second_trigger_is_already_active` seeds a Running row that
    // makes `detects_used_bytes_drift`'s `open_or_start` short-circuit
    // with `AlreadyActive` — its handler never dispatches, no
    // `consistency_finding` fires, and the drift assertion sees empty
    // events. Holding `TEST_LOCK` for each test's full body prevents
    // that interleaving; `wipe_our_runs` on entry defends against
    // stale rows left by a crashed / cancelled prior run.

    static TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    async fn wipe_our_runs(pool: &sqlx::PgPool) {
        sqlx::query("DELETE FROM jobs.recoverable_runs WHERE job_name = 'drives_consistency'")
            .execute(pool)
            .await
            .ok();
    }

    // ─── The tests ─────────────────────────────────────────────────────────

    #[tokio::test]
    async fn drives_consistency_detects_used_bytes_drift() {
        let _lock = TEST_LOCK.lock().await;
        let pool = test_pool().await;
        wipe_our_runs(pool.as_ref()).await;
        // Cached = 999, actual = 200 → delta = 799 (positive = cached over-reports).
        let drive_id = seed_drift(pool.as_ref(), 999, 200).await;

        // Run end-to-end through the recoverable engine: PgJobStoreProvider
        // creates a run row, run_or_resume dispatches DrivesConsistencyCheck,
        // handler walks the drive, marks Completed. Findings land in
        // `jobs.run_findings` via `store.record_finding()` — asserted
        // via `provider.list_findings(run_id, ...)` below (post-Slice 7,
        // no more tracing-event capture).
        let provider: Arc<dyn JobStoreProvider> = Arc::new(
            crate::infrastructure::scheduler::PgJobStoreProvider::new(pool.clone()),
        );
        let handler: Arc<dyn RecoverableJobHandler> =
            Arc::new(DrivesConsistencyCheck::new(pool.clone()));
        let outcome = crate::infrastructure::scheduler::run_or_resume(
            handler,
            provider.clone(),
            &JobRunArgs::default(),
        )
        .await;

        // Framework assertions.
        assert!(outcome.is_ok(), "run must complete: {outcome:?}");

        // Read-only invariant — drive's used_bytes is UNCHANGED by the check.
        let post_cached: i64 = sqlx::query("SELECT used_bytes FROM storage.drives WHERE id = $1")
            .bind(drive_id)
            .fetch_one(pool.as_ref())
            .await
            .expect("drive still exists")
            .get(0);
        assert_eq!(post_cached, 999, "drives_consistency must be read-only");

        // Find the run row that was created and verify its state.
        let latest_run: Option<(Uuid, String, i64)> = sqlx::query_as(
            r#"
            SELECT id, status, COALESCE((stats->>'scanned_count')::bigint, 0)
              FROM jobs.recoverable_runs
             WHERE job_name = 'drives_consistency'
             ORDER BY started_at DESC
             LIMIT 1
            "#,
        )
        .fetch_optional(pool.as_ref())
        .await
        .expect("query recoverable_runs");
        let (run_id, status, scanned) = latest_run.expect("run row must exist after run_or_resume");
        assert_eq!(status, "Completed", "run must be Completed");
        assert!(
            scanned >= 1,
            "scanned_count must include at least our drive, got {scanned}"
        );

        // Drift-detection assertion — the finding for our seeded drive
        // is now a persisted row. Query via the same interface the
        // admin endpoint uses so the test also pins the read path.
        let findings = provider
            .list_findings(run_id, 500, 0)
            .await
            .expect("list_findings");
        let ours = findings
            .iter()
            .find(|f| f.resource_id == Some(drive_id))
            .unwrap_or_else(|| {
                panic!("expected a persisted finding for drive {drive_id}, got: {findings:?}")
            });
        assert_eq!(ours.kind, "stale_used_bytes", "wrong kind: {ours:?}");
        assert_eq!(ours.severity, "inconsistent", "wrong severity: {ours:?}");
        assert_eq!(
            ours.detail.get("cached").and_then(|v| v.as_i64()),
            Some(999),
            "cached mismatch in detail: {ours:?}"
        );
        assert_eq!(
            ours.detail.get("actual").and_then(|v| v.as_i64()),
            Some(200),
            "actual mismatch in detail: {ours:?}"
        );
        assert_eq!(
            ours.detail.get("delta").and_then(|v| v.as_i64()),
            Some(799),
            "delta mismatch in detail: {ours:?}"
        );

        // Counter mirror — record_finding also bumps stats.finding_count.
        let stored_finding_count: i64 = sqlx::query(
            "SELECT COALESCE((stats->>'finding_count')::bigint, 0) FROM jobs.recoverable_runs WHERE id = $1",
        )
        .bind(run_id)
        .fetch_one(pool.as_ref())
        .await
        .expect("query stats.finding_count")
        .get(0);
        assert!(
            stored_finding_count >= 1,
            "finding_count must be bumped, got {stored_finding_count}"
        );

        // Cleanup — even on assertion failure the test panics before this,
        // leaving the test DB slightly dirty. That's fine per session; the
        // next spawn-db.sh reset clears everything.
        cleanup_run(pool.as_ref(), run_id).await;
        cleanup_test_drive(pool.as_ref(), drive_id).await;
    }

    #[tokio::test]
    async fn drives_consistency_no_drift_emits_no_finding() {
        let _lock = TEST_LOCK.lock().await;
        let pool = test_pool().await;
        wipe_our_runs(pool.as_ref()).await;
        // cached == actual → no drift.
        let drive_id = seed_drift(pool.as_ref(), 500, 500).await;

        let provider: Arc<dyn JobStoreProvider> = Arc::new(
            crate::infrastructure::scheduler::PgJobStoreProvider::new(pool.clone()),
        );
        let handler: Arc<dyn RecoverableJobHandler> =
            Arc::new(DrivesConsistencyCheck::new(pool.clone()));
        let outcome = crate::infrastructure::scheduler::run_or_resume(
            handler,
            provider.clone(),
            &JobRunArgs::default(),
        )
        .await;

        assert!(outcome.is_ok());

        // Locate the run row we just wrote.
        let latest_run: Option<(Uuid,)> = sqlx::query_as(
            "SELECT id FROM jobs.recoverable_runs WHERE job_name='drives_consistency' ORDER BY started_at DESC LIMIT 1",
        )
        .fetch_optional(pool.as_ref())
        .await
        .expect("query recoverable_runs");
        let (run_id,) = latest_run.expect("run row must exist");

        // For THIS drive, no persisted finding. Other drives in the test
        // DB may still surface findings (unrelated fixture data); we only
        // assert the invariant scoped to our drive_id.
        let findings = provider
            .list_findings(run_id, 500, 0)
            .await
            .expect("list_findings");
        let ours = findings
            .iter()
            .filter(|f| f.resource_id == Some(drive_id))
            .count();
        assert_eq!(
            ours, 0,
            "no drift on this drive, expected 0 findings, got {ours}: {findings:?}"
        );

        cleanup_run(pool.as_ref(), run_id).await;
        cleanup_test_drive(pool.as_ref(), drive_id).await;
    }

    #[tokio::test]
    async fn drives_consistency_second_trigger_is_already_active() {
        let _lock = TEST_LOCK.lock().await;
        let pool = test_pool().await;
        wipe_our_runs(pool.as_ref()).await;

        // Directly INSERT a Running row for drives_consistency to
        // simulate an in-flight prior dispatch, then observe that
        // open_or_start refuses to spawn a parallel run.
        let seeded_run_id = Uuid::new_v4();
        sqlx::query(
            r#"
            INSERT INTO jobs.recoverable_runs (id, job_name, status, started_at, last_progress_at)
            VALUES ($1, 'drives_consistency', 'Running', NOW(), NOW())
            ON CONFLICT (job_name) WHERE status IN ('Running', 'Paused', 'CancelRequested') DO NOTHING
            "#,
        )
        .bind(seeded_run_id)
        .execute(pool.as_ref())
        .await
        .expect("insert seed Running row");

        let provider: Arc<dyn JobStoreProvider> = Arc::new(
            crate::infrastructure::scheduler::PgJobStoreProvider::new(pool.clone()),
        );
        let opened = provider
            .open_or_start("drives_consistency")
            .await
            .expect("open_or_start");
        match opened {
            OpenedRun::AlreadyActive { status, .. } => {
                assert_eq!(status, RunStatus::Running);
            }
            _ => panic!("expected AlreadyActive for a job with a Running row"),
        }

        // Cleanup.
        sqlx::query("DELETE FROM jobs.recoverable_runs WHERE job_name = 'drives_consistency'")
            .execute(pool.as_ref())
            .await
            .ok();
    }
}
