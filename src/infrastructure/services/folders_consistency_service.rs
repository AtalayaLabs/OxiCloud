//! Second tenant of Part 2 (recoverable-run engine).
//!
//! Iterates `storage.folders` and reports each row whose maintained-
//! by-trigger materialised state has drifted from what walking its
//! `parent_id` chain would produce. **Read-only** — reports drift as
//! findings but does NOT rewrite anything; a curative sweep (or a
//! targeted repair endpoint) is a separate concern.
//!
//! Why this matters: the ltree cascade trigger `trg_folders_cascade_path`
//! is what keeps `path` and `lpath` in sync with `parent_id`. Any
//! bulk write path that bypasses the trigger (raw COPY, migration
//! backfill with FOR EACH STATEMENT triggers disabled, hand-rolled
//! UPDATE with `SET LOCAL session_replication_role = 'replica'`) can
//! leave the materialised columns wrong. Silent divergence breaks
//! every `WHERE lpath <@ ancestor` query — subtree list, breadcrumb
//! walk, recursive copy/move/delete. Detecting the drift is what
//! surfaces the underlying misuse.
//!
//! ### v1 checks (three per-row branches)
//!
//! * `parent_trashed_mismatch` — a non-trashed folder whose parent
//!   IS trashed. FK enforcement + trash cascade should make this
//!   impossible; when it does happen the cascade missed a row.
//! * `path_mismatch` — materialised `folders.path` differs from
//!   the parent-chain reconstruction.
//! * `lpath_mismatch` — materialised `folders.lpath` differs from
//!   the parent-chain reconstruction.
//!
//! Reconstruction convention (mirrors `storage.compute_folder_path`
//! from `20260307000000_initial_schema.sql`):
//!
//! ```text
//! my_label       = replace(id::text, '-', '_')
//! root:      path  = name              lpath = my_label
//! non-root:  path  = parent.path || '/' || name
//!            lpath = parent.lpath || my_label
//! ```
//!
//! Findings are LOGGED to `target: "oxicloud::consistency"` for now,
//! same as `drives_consistency`. Persistence to `jobs.run_findings`
//! lands with the findings-table migration (deferred); at that point
//! this handler swaps its `tracing::warn!` calls for
//! `store.record_finding(...)` — nothing else changes.
//!
//! ### Room to grow (already-cheap branches deferred)
//!
//! * `drive_id_parent_mismatch` — parent + child in different drives.
//!   The self-join already loads `parent.drive_id`; one more per-row
//!   `if` when the drive-membership rules stabilise post-D7.
//! * `orphan_root` — a non-trashed `parent_id IS NULL` folder no
//!   `drives.root_folder_id` points at. The `check_no_orphan_root_folder`
//!   trigger from `20260803000000_*` blocks new orphans; a check here
//!   would surface pre-trigger legacy rows.
//! * Name-vs-path terminal drift (`path` ending in the folder's `name`).
//!   Redundant with `path_mismatch` unless we ever start storing them
//!   independently.

use std::sync::Arc;

use async_trait::async_trait;
use sqlx::PgPool;
use uuid::Uuid;

use crate::infrastructure::scheduler::{
    JobRegistry, JobRunArgs, JobStore, JobStoreProvider, RecoverableJobHandler, RunOutcome,
    RunStatus, record_or_log,
};

pub const FOLDERS_CONSISTENCY_JOB_NAME: &str = "folders_consistency";

/// Rows per batch. Folders can be numerous (millions on big
/// installs), each row is light. 500 keeps the cancel-poll cadence
/// sub-second on a warm cache while amortising round-trip overhead.
const BATCH_SIZE: i64 = 500;

pub struct FoldersConsistencyCheck {
    pool: Arc<PgPool>,
}

impl FoldersConsistencyCheck {
    pub fn new(pool: Arc<PgPool>) -> Self {
        Self { pool }
    }

    /// Chainable self-registration — mirrors `DrivesConsistencyCheck`.
    /// On-demand only (no periodic tick); operators fire it from
    /// `POST /api/admin/jobs/folders_consistency/trigger`.
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

#[derive(Debug, sqlx::FromRow)]
struct FolderRow {
    id: Uuid,
    parent_id: Option<Uuid>,
    is_trashed: bool,
    path: String,
    lpath_text: String,
    parent_is_trashed: Option<bool>,
    parent_path: Option<String>,
    parent_lpath_text: Option<String>,
    expected_path: String,
    expected_lpath_text: String,
}

#[async_trait]
impl RecoverableJobHandler for FoldersConsistencyCheck {
    fn name(&self) -> &str {
        FOLDERS_CONSISTENCY_JOB_NAME
    }

    /// Definitive count — one row per folder. Larger table than drives
    /// but the COUNT(*) is still index-only on PG. On multi-million-row
    /// deployments this is ~100ms at run start; acceptable given the
    /// progress bar is only rendered when the operator is watching.
    async fn count_total(&self) -> Option<u64> {
        let row: Result<(i64,), sqlx::Error> =
            sqlx::query_as("SELECT COUNT(*) FROM storage.folders")
                .fetch_one(self.pool.as_ref())
                .await;
        match row {
            Ok((n,)) => Some(n.max(0) as u64),
            Err(e) => {
                tracing::debug!(
                    target: "oxicloud::consistency",
                    event = "folders_consistency.count_total_failed",
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
        // Cursor: 16 raw UUID bytes, empty/absent = start from beginning.
        // Same convention as `drives_consistency` so the resume path
        // in `PgJobStoreProvider` treats every UUID-cursor tenant the
        // same way.
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

        let mut finding_count = 0u64;

        loop {
            // Cancel poll BETWEEN batches — the cooperative cancel
            // contract (see `RecoverableJobHandler` trait doc).
            match store.status().await {
                Ok(RunStatus::CancelRequested) => {
                    tracing::info!(
                        target: "oxicloud::consistency",
                        event = "folders_consistency.cancelled",
                        run_id = %store.run_id(),
                        finding_count = finding_count,
                        "folders_consistency cancelled cooperatively, pausing"
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

            // Fetch a batch of folders + reconstruct expected
            // path/lpath in SQL via LEFT JOIN on parent. The
            // reconstruction formula mirrors `compute_folder_path`
            // 1:1; if that trigger's convention ever changes both
            // must move together.
            let rows: Vec<FolderRow> = match sqlx::query_as(
                r#"
                SELECT
                    f.id                                                        AS id,
                    f.parent_id                                                 AS parent_id,
                    f.is_trashed                                                AS is_trashed,
                    f.path                                                      AS path,
                    f.lpath::text                                               AS lpath_text,
                    parent.is_trashed                                           AS parent_is_trashed,
                    parent.path                                                 AS parent_path,
                    parent.lpath::text                                          AS parent_lpath_text,
                    CASE
                        WHEN f.parent_id IS NULL THEN f.name
                        ELSE parent.path || '/' || f.name
                    END                                                         AS expected_path,
                    CASE
                        WHEN f.parent_id IS NULL THEN replace(f.id::text, '-', '_')
                        ELSE parent.lpath::text || '.' || replace(f.id::text, '-', '_')
                    END                                                         AS expected_lpath_text
                  FROM storage.folders f
                  LEFT JOIN storage.folders parent ON parent.id = f.parent_id
                 WHERE ($1::uuid IS NULL OR f.id > $1)
                 ORDER BY f.id
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
                    event = "folders_consistency.completed",
                    run_id = %store.run_id(),
                    finding_count = finding_count,
                    "folders_consistency completed with {} finding(s)",
                    finding_count
                );
                return RunOutcome::Completed;
            }

            // Per-row branches. Add new ones here — same pattern as
            // `drives_consistency`. Emit at most one finding per
            // (row, kind); multiple different kinds for the same row
            // are fine and independent.
            for row in &rows {
                // (1) parent_trashed_mismatch: a live folder under a
                // soft-deleted parent. Cascade missed.
                if !row.is_trashed && row.parent_is_trashed == Some(true) {
                    finding_count += 1;
                    record_or_log(
                        store,
                        FOLDERS_CONSISTENCY_JOB_NAME,
                        "parent_trashed_mismatch",
                        "inconsistent",
                        Some(row.id),
                        serde_json::json!({
                            "parent_id": row.parent_id,
                        }),
                    )
                    .await;
                }

                // (2) path_mismatch: materialised path drifted from
                // the parent-chain reconstruction.
                if row.path != row.expected_path {
                    finding_count += 1;
                    record_or_log(
                        store,
                        FOLDERS_CONSISTENCY_JOB_NAME,
                        "path_mismatch",
                        "inconsistent",
                        Some(row.id),
                        serde_json::json!({
                            "stored":      row.path,
                            "expected":    row.expected_path,
                            "parent_path": row.parent_path,
                        }),
                    )
                    .await;
                }

                // (3) lpath_mismatch: materialised lpath drifted from
                // the parent-chain reconstruction. Independent of (2)
                // — either can be wrong without the other, and both
                // silently break different query shapes.
                if row.lpath_text != row.expected_lpath_text {
                    finding_count += 1;
                    record_or_log(
                        store,
                        FOLDERS_CONSISTENCY_JOB_NAME,
                        "lpath_mismatch",
                        "inconsistent",
                        Some(row.id),
                        serde_json::json!({
                            "stored":       row.lpath_text,
                            "expected":     row.expected_lpath_text,
                            "parent_lpath": row.parent_lpath_text,
                        }),
                    )
                    .await;
                }
            }

            // Advance cursor + checkpoint. Batch length is what we
            // report to `stats.scanned_count`; findings are separate
            // (they arrive via the tracing subscriber / eventually
            // `run_findings`).
            let last_id = rows.last().map(|r| r.id).expect("non-empty rows");
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

            // Short batch = drained the folders table.
            if (rows.len() as i64) < BATCH_SIZE {
                tracing::info!(
                    target: "oxicloud::consistency",
                    event = "folders_consistency.completed",
                    run_id = %store.run_id(),
                    finding_count = finding_count,
                    "folders_consistency completed with {} finding(s)",
                    finding_count
                );
                return RunOutcome::Completed;
            }
        }
    }
}
