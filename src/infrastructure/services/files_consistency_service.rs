//! Third tenant of Part 2 (recoverable-run engine).
//!
//! Iterates `storage.files` and reports each row whose parent-folder
//! state, blob reference, or denormalised size has drifted from
//! what the join with `storage.folders` + `storage.blobs` says is
//! true. **Read-only** — the fix path is other jobs (trash cascade
//! repair, dedup GC, blob resurrection).
//!
//! Post-D7 files schema notable columns:
//!
//! * `folder_id` — nullable; `NULL` = file at drive root. Cascade FK
//!   to `storage.folders`.
//! * `blob_hash` — `NOT NULL`; MUST reference a row in
//!   `storage.blobs.hash`.
//! * `size` — denormalised copy of the blob's byte length; the
//!   original source of truth is `storage.blobs.size` (upload path
//!   sets both; a mismatch is drift).
//! * NO `path` column and NO `user_id` column (dropped in D7). The
//!   memory note's "path matches parent chain" check from the
//!   earlier taxonomy does NOT apply here — files carry no
//!   materialised path.
//!
//! ### v1 checks (three per-row branches)
//!
//! * `parent_folder_trashed` — a live file under a soft-deleted
//!   parent folder. FK cascade + trash cascade should make this
//!   impossible; occurrence means the cascade missed the row.
//!   Files at drive root (`folder_id IS NULL`) are exempt — there is
//!   no parent to check.
//! * `missing_blob` — file's `blob_hash` has no row in
//!   `storage.blobs`. **Severity `data_loss`**: the file record
//!   points at bytes the blob table doesn't know about, so any read
//!   attempt fails. Historically this happens when the dedup GC
//!   reaped a blob whose ref-count decrement raced with a fresh
//!   file INSERT — the two-phase mark/sweep is meant to prevent
//!   this, but the check surfaces regressions immediately.
//! * `blob_size_mismatch` — `files.size != blobs.size`. Cheap
//!   because the same LEFT JOIN already loads `blobs.size`. Would
//!   indicate the denormalised copy was ever set by a code path that
//!   didn't read the blob's real length — a bug we want to see fast.
//!
//! ### Room to grow (same self-join, one more `if`)
//!
//! * `drive_id_parent_mismatch` — `files.drive_id` differs from
//!   `parent.drive_id`. The join already loads it; adding this once
//!   drive-membership rules stabilise post-D7 costs one branch.
//! * `mime_type_reconciliation` — compare `files.mime_type` against
//!   the blob's `content_type`. Requires deciding which is
//!   authoritative first.

use std::sync::Arc;

use async_trait::async_trait;
use sqlx::PgPool;
use uuid::Uuid;

use crate::infrastructure::scheduler::{
    JobRegistry, JobRunArgs, JobStore, JobStoreProvider, RecoverableJobHandler, RunOutcome,
    RunStatus, record_or_log,
};

pub const FILES_CONSISTENCY_JOB_NAME: &str = "files_consistency";

/// Rows per batch. Files can be very numerous — hundreds of
/// thousands on medium installs, millions on large — but the per-row
/// work is a couple of comparisons. 500 keeps the cancel-poll cadence
/// sub-second while amortising round-trip overhead.
const BATCH_SIZE: i64 = 500;

pub struct FilesConsistencyCheck {
    pool: Arc<PgPool>,
}

impl FilesConsistencyCheck {
    pub fn new(pool: Arc<PgPool>) -> Self {
        Self { pool }
    }

    /// Chainable self-registration — mirrors the other consistency
    /// tenants. On-demand only (operators trigger from
    /// `POST /api/admin/jobs/files_consistency/trigger` or via
    /// `consistency_batch`).
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
struct FileRow {
    id: Uuid,
    folder_id: Option<Uuid>,
    is_trashed: bool,
    size: i64,
    blob_hash: String,
    /// `None` when `folder_id IS NULL` (file at drive root) — the
    /// LEFT JOIN yields no parent row.
    parent_is_trashed: Option<bool>,
    /// `None` when the blob row is missing — the LEFT JOIN yields
    /// no `blobs` side. This IS the `missing_blob` signal.
    blob_size: Option<i64>,
}

#[async_trait]
impl RecoverableJobHandler for FilesConsistencyCheck {
    fn name(&self) -> &str {
        FILES_CONSISTENCY_JOB_NAME
    }

    async fn run_resumable(
        &self,
        store: &dyn JobStore,
        _args: &JobRunArgs,
        resume_cursor: Option<Vec<u8>>,
    ) -> RunOutcome {
        // Cursor: 16 raw UUID bytes, empty/absent = start from
        // beginning. Same convention as the other UUID-cursor
        // tenants so the resume path in `PgJobStoreProvider` is
        // uniform.
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
                        event = "files_consistency.cancelled",
                        run_id = %store.run_id(),
                        finding_count = finding_count,
                        "files_consistency cancelled cooperatively, pausing"
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

            // One query, two LEFT JOINs: (parent folder) + (blob
            // row). Left-joining the blob is what lets us detect
            // `missing_blob` — a matched row has `blob.size`
            // populated; a miss surfaces as NULL.
            let rows: Vec<FileRow> = match sqlx::query_as(
                r#"
                SELECT
                    f.id                                                        AS id,
                    f.folder_id                                                 AS folder_id,
                    f.is_trashed                                                AS is_trashed,
                    f.size                                                      AS size,
                    f.blob_hash                                                 AS blob_hash,
                    parent.is_trashed                                           AS parent_is_trashed,
                    b.size                                                      AS blob_size
                  FROM storage.files f
                  LEFT JOIN storage.folders parent ON parent.id = f.folder_id
                  LEFT JOIN storage.blobs   b      ON b.hash     = f.blob_hash
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
                    event = "files_consistency.completed",
                    run_id = %store.run_id(),
                    finding_count = finding_count,
                    "files_consistency completed with {} finding(s)",
                    finding_count
                );
                return RunOutcome::Completed;
            }

            for row in &rows {
                // (1) parent_folder_trashed: live file under a
                // soft-deleted folder. Root files (`folder_id IS
                // NULL`) are exempt — `parent_is_trashed` is None
                // there.
                if !row.is_trashed && row.parent_is_trashed == Some(true) {
                    finding_count += 1;
                    record_or_log(
                        store,
                        FILES_CONSISTENCY_JOB_NAME,
                        "parent_folder_trashed",
                        "inconsistent",
                        Some(row.id),
                        serde_json::json!({
                            "folder_id": row.folder_id,
                        }),
                    )
                    .await;
                }

                // (2) missing_blob: `blob_hash` has no `storage.blobs`
                // row. Real data-loss indicator — reading the file
                // will fail.
                if row.blob_size.is_none() {
                    finding_count += 1;
                    record_or_log(
                        store,
                        FILES_CONSISTENCY_JOB_NAME,
                        "missing_blob",
                        "data_loss",
                        Some(row.id),
                        serde_json::json!({
                            "blob_hash": row.blob_hash,
                        }),
                    )
                    .await;
                    // No point checking size when the blob row is
                    // gone — skip (3) for this row.
                    continue;
                }

                // (3) blob_size_mismatch: denormalised size drifted
                // from the blob's real length. Cheap because we've
                // already loaded both.
                if let Some(bs) = row.blob_size
                    && bs != row.size
                {
                    finding_count += 1;
                    record_or_log(
                        store,
                        FILES_CONSISTENCY_JOB_NAME,
                        "blob_size_mismatch",
                        "inconsistent",
                        Some(row.id),
                        serde_json::json!({
                            "blob_hash": row.blob_hash,
                            "stored":    row.size,
                            "actual":    bs,
                            "delta":     row.size - bs,
                        }),
                    )
                    .await;
                }
            }

            // Advance cursor + checkpoint. `batch_len` feeds
            // `stats.scanned_count`.
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

            if (rows.len() as i64) < BATCH_SIZE {
                tracing::info!(
                    target: "oxicloud::consistency",
                    event = "files_consistency.completed",
                    run_id = %store.run_id(),
                    finding_count = finding_count,
                    "files_consistency completed with {} finding(s)",
                    finding_count
                );
                return RunOutcome::Completed;
            }
        }
    }
}
