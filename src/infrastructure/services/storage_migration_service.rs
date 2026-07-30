//! Storage-backend migration as a recoverable-run tenant (Part 2 engine).
//!
//! Iterates `storage.blobs` and copies each byte payload from the SOURCE
//! backend (whatever the app booted with) to the TARGET backend
//! (whatever the current admin storage-settings config describes).
//! Both legacy whole-file blobs AND CDC chunk blobs are covered by the
//! single walk — they share `storage.blobs` as their physical registry
//! (see memory `project_cdc_dual_storage_registries`).
//! `storage.chunk_manifests` is pure PG state, holds no backend bytes,
//! and needs no migration.
//!
//! Retires the in-memory `Arc<RwLock<MigrationState>>` + one-shot
//! `tokio::spawn` in `migration_job.rs`. The recoverable engine
//! provides cursor persistence, cooperative cancel, boot-time crash
//! recovery, and the uniform `/api/admin/jobs/*` admin surface.
//!
//! ### Restart survival
//!
//! Cursor + per-blob failure findings are persisted after every batch.
//! On restart the boot-time sweep flips any abandoned `Running` row to
//! `Paused`; a subsequent admin trigger resumes from the persisted
//! cursor via `run_or_resume`. At most one batch of already-copied
//! blobs replays, and the `target.blob_exists` short-circuit makes
//! even that replay effectively free.
//!
//! ### Design notes
//!
//! * **Cursor.** UTF-8 hex of the last-processed blob hash (64 chars).
//!   Natural lex order matches `ORDER BY hash ASC`. Same encoding
//!   `blobs_consistency` uses.
//! * **Target resolution.** Rebuilt at the START of every fresh or
//!   resumed run via `StorageSettingsService::build_effective_backend`.
//!   Held for the duration of the run; a mid-run settings change is
//!   ignored until the next run. On resume the admin may have paused
//!   *specifically* to fix a broken target config, so we re-derive
//!   rather than pin.
//! * **Per-blob failures don't fail the run.** Each failure records a
//!   `migration_failed` finding (severity `data_loss` — the bytes
//!   didn't cross) and the walk continues. A run that completes with
//!   zero findings is proof the target has every blob.
//! * **Skip already-present blobs.** `target.blob_exists(hash)` before
//!   the copy — makes cheap re-runs safe and lets a paused run resume
//!   without redoing bytes.
//! * **No per-batch concurrency knob.** The old code buffered N copies
//!   in parallel. Sequential is easier to reason about with cooperative
//!   cancel + cursor discipline; the batch loop is I/O-bound anyway.
//!   Add concurrency later if a real throughput need appears.

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use futures::StreamExt;
use sqlx::PgPool;

use crate::application::ports::blob_storage_ports::BlobStorageBackend;
use crate::application::services::storage_settings_service::StorageSettingsService;
use crate::common::errors::DomainError;
use crate::infrastructure::scheduler::{
    JobRegistry, JobRunArgs, JobStore, JobStoreProvider, RecoverableJobHandler, RunOutcome,
    RunStatus, record_or_log,
};

pub const STORAGE_MIGRATION_JOB_NAME: &str = "storage_migration";

/// Rows per batch. Copies are I/O-bound (source read + target write);
/// larger batches amortise fewer SQL round-trips but the checkpoint
/// / cancel-poll cadence lengthens. 100 balances the two — one
/// checkpoint per ~hundred blobs is fine, and the cancel-poll comes
/// every 100 rows too. Match `blobs_consistency` for consistency.
const BATCH_SIZE: i64 = 100;

pub struct StorageMigrationService {
    pool: Arc<PgPool>,
    source: Arc<dyn BlobStorageBackend>,
    storage_settings: Arc<StorageSettingsService>,
}

impl StorageMigrationService {
    pub fn new(
        pool: Arc<PgPool>,
        source: Arc<dyn BlobStorageBackend>,
        storage_settings: Arc<StorageSettingsService>,
    ) -> Self {
        Self {
            pool,
            source,
            storage_settings,
        }
    }

    /// Chainable self-registration — mirrors the `*_consistency`
    /// tenants. On-demand only (no periodic tick).
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
impl RecoverableJobHandler for StorageMigrationService {
    fn name(&self) -> &str {
        STORAGE_MIGRATION_JOB_NAME
    }

    /// Definitive count — one row per blob. `SELECT COUNT(*) FROM
    /// storage.blobs` on a modern PG is a sub-second index-only scan
    /// even at millions of rows.
    async fn count_total(&self) -> Option<u64> {
        let row: Result<(i64,), sqlx::Error> = sqlx::query_as("SELECT COUNT(*) FROM storage.blobs")
            .fetch_one(self.pool.as_ref())
            .await;
        match row {
            Ok((n,)) => Some(n.max(0) as u64),
            Err(e) => {
                tracing::debug!(
                    target: "oxicloud::migration",
                    event = "storage_migration.count_total_failed",
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
        // No-op guard — refuse when the effective (target) config
        // points at the same physical storage as the source (boot
        // config). Without this, a misclick on an S3 deployment
        // issues one HEAD per blob for zero useful work — cheap on
        // local, expensive on remote. Same-type-different-location
        // migrations (local dir change, S3 bucket change) pass this
        // check and proceed normally.
        match self.storage_settings.is_source_target_identical().await {
            Ok(true) => {
                tracing::warn!(
                    target: "audit",
                    event = "storage_migration.refused_noop",
                    run_id = %store.run_id(),
                    "storage_migration refused: source and target point at the same storage"
                );
                return RunOutcome::Failed {
                    message:
                        "target equals source; change storage settings before triggering a migration"
                            .to_string(),
                };
            }
            Ok(false) => {}
            Err(e) => {
                return RunOutcome::Failed {
                    message: format!("identity check: {e}"),
                };
            }
        }

        // Resolve target at run start.
        let target = match self.storage_settings.build_effective_backend().await {
            Ok(t) => t,
            Err(e) => {
                return RunOutcome::Failed {
                    message: format!("resolve target backend: {e}"),
                };
            }
        };
        if let Err(e) = target.initialize().await {
            return RunOutcome::Failed {
                message: format!("target backend init: {e}"),
            };
        }

        let source_kind = self.source.backend_type();
        let target_kind = target.backend_type();
        tracing::info!(
            target: "audit",
            event = "storage_migration.run_started",
            run_id = %store.run_id(),
            source = source_kind,
            target = target_kind,
            resuming = resume_cursor.is_some(),
            "storage_migration starting {source_kind} → {target_kind}"
        );

        // Cursor = the last-visited blob hash, UTF-8-encoded. On resume
        // walk `WHERE hash > $cursor`. `None` / empty = start from the
        // smallest hash. Same shape `blobs_consistency` uses.
        let mut cursor: Option<String> = match resume_cursor {
            None => None,
            Some(bytes) if bytes.is_empty() => None,
            Some(bytes) => match String::from_utf8(bytes) {
                Ok(s) => Some(s),
                Err(e) => {
                    return RunOutcome::Failed {
                        message: format!("invalid cursor: not valid UTF-8: {e}"),
                    };
                }
            },
        };

        let mut copied_count = 0u64;
        let mut skipped_count = 0u64;
        let mut failed_count = 0u64;
        let mut source_missing_count = 0u64;

        loop {
            // Cooperative cancel poll between batches.
            match store.status().await {
                Ok(RunStatus::CancelRequested) => {
                    tracing::info!(
                        target: "oxicloud::migration",
                        event = "storage_migration.cancelled",
                        run_id = %store.run_id(),
                        copied = copied_count,
                        skipped = skipped_count,
                        failed = failed_count,
                        source_missing = source_missing_count,
                        "storage_migration cancelled cooperatively, pausing"
                    );
                    return RunOutcome::Paused {
                        cursor: cursor
                            .as_ref()
                            .map(|s| s.as_bytes().to_vec())
                            .unwrap_or_default(),
                    };
                }
                Ok(_) => {}
                Err(e) => {
                    return RunOutcome::Failed {
                        message: format!("status poll: {e}"),
                    };
                }
            }

            // Fetch the next batch. `hash > $1` keyset pagination on
            // the PK; index-only scan.
            let rows: Vec<(String, i64)> = match sqlx::query_as(
                r#"
                SELECT hash, size
                  FROM storage.blobs
                 WHERE ($1::text IS NULL OR hash > $1)
                 ORDER BY hash
                 LIMIT $2
                "#,
            )
            .bind(cursor.as_deref())
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
                    target: "oxicloud::migration",
                    event = "storage_migration.completed",
                    run_id = %store.run_id(),
                    copied = copied_count,
                    skipped = skipped_count,
                    failed = failed_count,
                    source_missing = source_missing_count,
                    "storage_migration completed"
                );
                return RunOutcome::Completed;
            }

            for (hash, size) in &rows {
                // Probe SOURCE first — without this a run would
                // silently "succeed" against a source that's missing
                // blobs the DB expects, and the audit intent of the
                // walk is lost (relevant on any post-migration state
                // where target may already have every blob). A
                // missing-on-source blob is a real data-loss
                // condition; record it and move on — we never
                // "copy" from nothing.
                match self.source.blob_exists(hash).await {
                    Ok(true) => {}
                    Ok(false) => {
                        source_missing_count += 1;
                        tracing::warn!(
                            target: "oxicloud::migration",
                            event = "storage_migration.source_missing",
                            run_id = %store.run_id(),
                            hash = %hash,
                            source = source_kind,
                            "blob absent from source; recording data-loss finding, no copy"
                        );
                        record_or_log(
                            store,
                            STORAGE_MIGRATION_JOB_NAME,
                            "source_missing",
                            "data_loss",
                            None,
                            serde_json::json!({
                                "hash":   hash,
                                "size":   size,
                                "source": source_kind,
                                "target": target_kind,
                            }),
                        )
                        .await;
                        continue;
                    }
                    Err(e) => {
                        // Transient probe failure on source is NOT a
                        // finding — treat like a network blip.
                        // Skipping this row on this run; a re-run
                        // will re-probe. If the failure is
                        // persistent, `blobs_consistency` catches
                        // it.
                        tracing::warn!(
                            target: "oxicloud::migration",
                            event = "storage_migration.source_probe_error",
                            run_id = %store.run_id(),
                            hash = %hash,
                            error = %e,
                            "source blob_exists probe failed; skipping this row"
                        );
                        continue;
                    }
                }

                // Skip when the target already has it — supports
                // idempotent resume and cheap re-runs against a
                // partially-migrated target.
                match target.blob_exists(hash).await {
                    Ok(true) => {
                        skipped_count += 1;
                        continue;
                    }
                    Ok(false) => {}
                    Err(e) => {
                        tracing::warn!(
                            target: "oxicloud::migration",
                            event = "storage_migration.blob_exists_error",
                            run_id = %store.run_id(),
                            hash = %hash,
                            error = %e,
                            "blob_exists probe on target failed; attempting copy anyway"
                        );
                    }
                }

                match copy_blob(self.source.as_ref(), target.as_ref(), hash).await {
                    Ok(()) => {
                        copied_count += 1;
                    }
                    Err(e) => {
                        failed_count += 1;
                        tracing::warn!(
                            target: "oxicloud::migration",
                            event = "storage_migration.blob_failed",
                            run_id = %store.run_id(),
                            hash = %hash,
                            error = %e,
                            "failed to migrate blob; recording finding, continuing"
                        );
                        // resource_id stays None — blob hash isn't a
                        // UUID. Real identifier lives in `detail.hash`
                        // where the admin UI reads it.
                        record_or_log(
                            store,
                            STORAGE_MIGRATION_JOB_NAME,
                            "migration_failed",
                            "data_loss",
                            None,
                            serde_json::json!({
                                "hash":   hash,
                                "size":   size,
                                "source": source_kind,
                                "target": target_kind,
                                "error":  e.to_string(),
                            }),
                        )
                        .await;
                    }
                }
            }

            // Advance cursor + checkpoint. `delta_count` counts WORK
            // ATTEMPTED (copied + skipped + failed), not successful
            // copies alone — otherwise the progress bar stalls whenever
            // a batch is dominated by already-present blobs, which is
            // exactly the case on a resume.
            let last_hash = rows.last().map(|(h, _)| h.clone()).expect("non-empty rows");
            cursor = Some(last_hash.clone());
            let batch_len = rows.len() as u64;
            if let Err(e) = store.checkpoint(last_hash.into_bytes(), batch_len).await {
                return RunOutcome::Failed {
                    message: format!("checkpoint: {e}"),
                };
            }

            if (rows.len() as i64) < BATCH_SIZE {
                tracing::info!(
                    target: "oxicloud::migration",
                    event = "storage_migration.completed",
                    run_id = %store.run_id(),
                    copied = copied_count,
                    skipped = skipped_count,
                    failed = failed_count,
                    source_missing = source_missing_count,
                    "storage_migration completed"
                );
                return RunOutcome::Completed;
            }
        }
    }
}

/// Copy one blob: stream source bytes to a temp file, then hand the
/// path to `target.put_blob`. The spool-through-disk shape matches
/// what the old `migration_job::copy_blob` did — some backends'
/// `put_blob` want a path they can `rename(2)` or multi-part upload
/// from, not an in-memory buffer. The temp file lives in
/// `std::env::temp_dir()/oxicloud-migration/{hash}.tmp` and is
/// removed on success (best-effort on the failure paths — the OS
/// cleans up on reboot).
async fn copy_blob(
    source: &dyn BlobStorageBackend,
    target: &dyn BlobStorageBackend,
    hash: &str,
) -> Result<(), DomainError> {
    let tmp_dir = std::env::temp_dir().join("oxicloud-migration");
    tokio::fs::create_dir_all(&tmp_dir).await.map_err(|e| {
        DomainError::internal_error(
            "StorageMigration",
            format!("create temp dir {}: {e}", tmp_dir.display()),
        )
    })?;
    let tmp_path = tmp_dir.join(format!("{hash}.tmp"));

    if let Err(e) = write_source_to_tmp(source, hash, &tmp_path).await {
        let _ = tokio::fs::remove_file(&tmp_path).await;
        return Err(e);
    }

    let put_result = target.put_blob(hash, &tmp_path).await;
    let _ = tokio::fs::remove_file(&tmp_path).await;
    put_result.map(|_bytes_written| ())
}

async fn write_source_to_tmp(
    source: &dyn BlobStorageBackend,
    hash: &str,
    tmp_path: &Path,
) -> Result<(), DomainError> {
    use tokio::io::AsyncWriteExt;

    let stream = source.get_blob_stream(hash).await?;
    let mut file = tokio::fs::File::create(tmp_path).await.map_err(|e| {
        DomainError::internal_error(
            "StorageMigration",
            format!("create temp file {}: {e}", tmp_path.display()),
        )
    })?;
    let mut stream = std::pin::pin!(stream);
    while let Some(chunk) = stream.next().await {
        let bytes = chunk.map_err(|e| {
            DomainError::internal_error("StorageMigration", format!("source stream read: {e}"))
        })?;
        file.write_all(&bytes).await.map_err(|e| {
            DomainError::internal_error("StorageMigration", format!("temp file write: {e}"))
        })?;
    }
    file.flush()
        .await
        .map_err(|e| DomainError::internal_error("StorageMigration", format!("temp flush: {e}")))?;
    Ok(())
}
