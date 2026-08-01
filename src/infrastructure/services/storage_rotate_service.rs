//! Storage-format rotation as a recoverable-run tenant (K3 of
//! `docs/plan/storage-key-rotation.md`).
//!
//! Iterates `storage.blobs` for a target entry, decides per blob
//! whether the on-disk format matches what the entry's head pair
//! would write, and rewrites in place when it doesn't. Covers four
//! transitions with a single equality check:
//!
//! * Legacy blob (no `OXCPT` magic) → rewrite as v1 with the head
//!   pair's format.
//! * v1 encrypted, decrypted under a pair-index other than head →
//!   rewrite (key rotation).
//! * v1 plaintext with head=`aes:K` → rewrite (encrypt-in-place).
//! * v1 encrypted with head=`none:` → rewrite (decrypt-in-place).
//!
//! ### No readonly, no cutover
//!
//! `storage_rotate` is per-blob idempotent — repeat rewrites are
//! byte-safe (content-addressability holds; the wrapper always
//! produces the head format). Concurrent user writes coexist: they
//! land as head-format themselves, so when the walk reaches that
//! hash the classifier reports "already at head format" and the
//! decision tree collapses to `skip`. No app-wide read-only gate is
//! ever engaged — a critical improvement over `storage_migration`,
//! whose target-different-from-source cutover forces one.
//!
//! ### Restart survival
//!
//! Cursor + per-blob failure findings are persisted after every
//! batch. On restart, boot flips any abandoned `Running` row to
//! `Paused`; an admin trigger resumes from the checkpointed cursor.
//! The last checkpoint window (~100 blobs) re-processes; each of
//! those blobs is now head-format from the previous run's rewrite,
//! so the walk short-circuits without re-writing. Effectively free.
//!
//! ### Design notes
//!
//! * **Cursor** — UTF-8 hex of the last-processed blob hash (64
//!   chars). Same encoding as `storage_migration` and
//!   `blobs_consistency`.
//! * **Target lookup** — the entry NAME is stashed in `params` at
//!   Fresh-open time and re-read on Resume. The wrapper for that
//!   entry is rebuilt at the top of every run via
//!   `build_entry_backend_typed`; mid-run config changes are
//!   ignored until the next run (mirrors `storage_migration`).
//! * **Per-blob failures don't fail the run** — each failure records
//!   a `rotation_failed` finding (severity `data_loss` — the bytes
//!   didn't get rewritten) and the walk continues. A run that
//!   completes with zero findings is proof every blob is at head
//!   format.
//! * **`?deep=true` is unused** — rotation has no slow variant.
//!   Parameter accepted for uniformity with other tenants; ignored.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use sqlx::PgPool;

use crate::application::ports::blob_storage_ports::BlobStorageBackend;
use crate::common::config::NamedStorageEntry;
use crate::common::migration_progress::MigrationProgress;
use crate::infrastructure::scheduler::{
    JobRegistry, JobRunArgs, JobStore, JobStoreProvider, RecoverableJobHandler, RunOutcome,
    RunStatus, record_or_log,
};
use crate::infrastructure::services::entry_backend::build_entry_backend_typed;

pub const STORAGE_ROTATE_JOB_NAME: &str = "storage_rotate";

/// The `params` JSONB key under which the run's target entry name is
/// stashed at Fresh-open time via `JobStore::set_string_param`.
/// Kept identical to `storage_migration`'s TARGET_NAME_PARAM so
/// operators grepping run rows see the same convention across both
/// storage-touching tenants.
pub const TARGET_NAME_PARAM: &str = "target_name";

/// Rows per batch. Matches `storage_migration` / `blobs_consistency`
/// so the checkpoint + cancel-poll cadence is uniform across tenants.
const BATCH_SIZE: i64 = 100;

pub struct StorageRotateService {
    pool: Arc<PgPool>,
    /// Immutable per-deploy snapshot; used to look up the target
    /// entry by name at run start. Matches `AppConfig.storage_entries`.
    storage_entries: Vec<NamedStorageEntry>,
    /// Ambient `AppConfig.storage_path` used as the `root_dir`
    /// fallback for a Local target entry that doesn't declare its
    /// own `_ROOT_DIR`. Same fallback rule as boot
    /// (`build_entry_backend`).
    storage_path_fallback: PathBuf,
    /// Shared in-memory progress snapshot for the server-status
    /// header middleware. `Some(_)` while a rotation is
    /// running/paused, `None` otherwise. Distinct from
    /// `AppState.migration_progress` so the header can broadcast
    /// migration + rotation states independently.
    rotation_progress: Arc<std::sync::RwLock<Option<MigrationProgress>>>,
}

impl StorageRotateService {
    pub fn new(
        pool: Arc<PgPool>,
        storage_entries: Vec<NamedStorageEntry>,
        storage_path_fallback: PathBuf,
        rotation_progress: Arc<std::sync::RwLock<Option<MigrationProgress>>>,
    ) -> Self {
        Self {
            pool,
            storage_entries,
            storage_path_fallback,
            rotation_progress,
        }
    }

    /// Chainable self-registration — mirrors the `*_consistency`
    /// tenants and `storage_migration`. On-demand only (no periodic
    /// tick).
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
impl RecoverableJobHandler for StorageRotateService {
    fn name(&self) -> &str {
        STORAGE_ROTATE_JOB_NAME
    }

    /// Definitive count — one row per blob. Same query as
    /// `storage_migration::count_total`; the two walk the same rows.
    async fn count_total(&self) -> Option<u64> {
        let row: Result<(i64,), sqlx::Error> = sqlx::query_as("SELECT COUNT(*) FROM storage.blobs")
            .fetch_one(self.pool.as_ref())
            .await;
        match row {
            Ok((n,)) => Some(n.max(0) as u64),
            Err(e) => {
                tracing::debug!(
                    target: "oxicloud::rotate",
                    event = "storage_rotate.count_total_failed",
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
        args: &JobRunArgs,
        resume_cursor: Option<Vec<u8>>,
    ) -> RunOutcome {
        // Resolve target entry name — same shape as `storage_migration`.
        let is_fresh = resume_cursor.is_none();
        let target_name = if is_fresh {
            let Some(name) = args.storage.clone() else {
                return RunOutcome::Failed {
                    message: "storage_rotate requires `target_name` on a fresh run — trigger via \
                              POST /api/admin/storage/entries/{name}/rotate"
                        .to_string(),
                };
            };
            if let Err(e) = store.set_string_param(TARGET_NAME_PARAM, &name).await {
                return RunOutcome::Failed {
                    message: format!("failed to persist target_name to params: {e}"),
                };
            }
            name
        } else {
            match store.get_string_param(TARGET_NAME_PARAM).await {
                Ok(Some(name)) => name,
                Ok(None) => {
                    return RunOutcome::Failed {
                        message: format!(
                            "resumed run has no {TARGET_NAME_PARAM} in params — cancel + trigger \
                             fresh."
                        ),
                    };
                }
                Err(e) => {
                    return RunOutcome::Failed {
                        message: format!("read {TARGET_NAME_PARAM} from params: {e}"),
                    };
                }
            }
        };

        // Look up the target entry.
        let target_entry = match self.storage_entries.iter().find(|e| e.name == target_name) {
            Some(e) => e,
            None => {
                let available = if self.storage_entries.is_empty() {
                    "(none)".to_string()
                } else {
                    self.storage_entries
                        .iter()
                        .map(|e| e.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                };
                return RunOutcome::Failed {
                    message: format!(
                        "target entry `{target_name}` not declared in `OXICLOUD_STORAGE_ENTRIES` — \
                         available: {available}."
                    ),
                };
            }
        };

        // Build the wrapper for this entry — typed so we can call
        // `read_and_classify` + `head_format` directly.
        let wrapper = build_entry_backend_typed(target_entry, &self.storage_path_fallback);
        if let Err(e) = wrapper.initialize().await {
            return RunOutcome::Failed {
                message: format!("target entry `{target_name}` failed to initialize: {e}"),
            };
        }
        let head_format = wrapper.head_format();

        tracing::info!(
            target: "audit",
            event = "storage_rotate.run_started",
            run_id = %store.run_id(),
            target_name = %target_name,
            head_format = ?head_format,
            resuming = !is_fresh,
            "storage_rotate started on `{target_name}` (head_format = {head_format:?})"
        );

        // Seed the progress snapshot. Total = count_total's estimate;
        // if that failed we still surface the header without a
        // denominator so the banner shows "rotation in progress" at
        // minimum.
        let total = self.count_total().await.unwrap_or(0);
        {
            let mut guard = self
                .rotation_progress
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *guard = Some(MigrationProgress::new(target_name.clone(), total));
        }

        let mut cursor: Option<String> = match resume_cursor {
            None => None,
            Some(bytes) if bytes.is_empty() => None,
            Some(bytes) => match String::from_utf8(bytes) {
                Ok(s) => Some(s),
                Err(e) => {
                    self.clear_progress();
                    return RunOutcome::Failed {
                        message: format!("invalid cursor: not valid UTF-8: {e}"),
                    };
                }
            },
        };

        let mut rewritten_count = 0u64;
        let mut skipped_count = 0u64;
        let mut failed_count = 0u64;

        loop {
            // Cooperative cancel poll between batches.
            match store.status().await {
                Ok(RunStatus::CancelRequested) => {
                    self.clear_progress();
                    tracing::info!(
                        target: "oxicloud::rotate",
                        event = "storage_rotate.cancelled",
                        run_id = %store.run_id(),
                        rewritten = rewritten_count,
                        skipped = skipped_count,
                        failed = failed_count,
                        "storage_rotate cancelled cooperatively, pausing"
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
                    self.clear_progress();
                    return RunOutcome::Failed {
                        message: format!("status poll: {e}"),
                    };
                }
            }

            // Fetch the next batch. Same keyset pagination shape as
            // `storage_migration` — `hash > $1` on the PK, index-only.
            let rows: Vec<(String,)> = match sqlx::query_as(
                r#"
                SELECT hash
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
                    self.clear_progress();
                    return RunOutcome::Failed {
                        message: format!("batch fetch: {e}"),
                    };
                }
            };

            if rows.is_empty() {
                return self
                    .finish_completed(
                        store,
                        &target_name,
                        rewritten_count,
                        skipped_count,
                        failed_count,
                    )
                    .await;
            }

            for (hash,) in &rows {
                // Read + classify in one round-trip. Failure here is
                // a real read failure (e.g. blob missing on disk),
                // recorded as a finding.
                let (plaintext, current_format) = match wrapper.read_and_classify(hash).await {
                    Ok(pair) => pair,
                    Err(e) => {
                        failed_count += 1;
                        tracing::warn!(
                            target: "oxicloud::rotate",
                            event = "storage_rotate.read_failed",
                            run_id = %store.run_id(),
                            hash = %hash,
                            error = %e,
                            "failed to read blob for classification; recording finding"
                        );
                        record_or_log(
                            store,
                            STORAGE_ROTATE_JOB_NAME,
                            "rotation_failed",
                            "data_loss",
                            None,
                            serde_json::json!({
                                "hash":  hash,
                                "phase": "read",
                                "error": e.to_string(),
                            }),
                        )
                        .await;
                        continue;
                    }
                };

                // The whole decision tree collapses to one equality
                // check thanks to `BlobFormat`'s `PartialEq`. Six
                // cases in the plan → one branch here.
                if current_format == head_format {
                    skipped_count += 1;
                    continue;
                }

                // Rewrite via the standard write path — atomic
                // replace at the same object key. `put_blob_from_bytes`
                // frames the plaintext with the head pair's format
                // (encrypted-v1 or plaintext-v1) and hands the
                // resulting bytes to the inner backend.
                if let Err(e) = wrapper
                    .put_blob_from_bytes(hash, Bytes::from(plaintext.to_vec()))
                    .await
                {
                    failed_count += 1;
                    tracing::warn!(
                        target: "oxicloud::rotate",
                        event = "storage_rotate.write_failed",
                        run_id = %store.run_id(),
                        hash = %hash,
                        error = %e,
                        "failed to rewrite blob; recording finding"
                    );
                    record_or_log(
                        store,
                        STORAGE_ROTATE_JOB_NAME,
                        "rotation_failed",
                        "data_loss",
                        None,
                        serde_json::json!({
                            "hash":  hash,
                            "phase": "write",
                            "from":  format!("{current_format:?}"),
                            "to":    format!("{head_format:?}"),
                            "error": e.to_string(),
                        }),
                    )
                    .await;
                    continue;
                }
                rewritten_count += 1;
            }

            // Advance cursor + checkpoint. `delta_count` = work
            // attempted this batch, so the progress bar advances even
            // when a batch is dominated by skips (steady-state
            // re-run) or failures.
            let last_hash = rows.last().map(|(h,)| h.clone()).expect("non-empty rows");
            cursor = Some(last_hash.clone());
            let batch_len = rows.len() as u64;
            if let Err(e) = store.checkpoint(last_hash.into_bytes(), batch_len).await {
                self.clear_progress();
                return RunOutcome::Failed {
                    message: format!("checkpoint: {e}"),
                };
            }
            {
                let mut guard = self
                    .rotation_progress
                    .write()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if let Some(progress) = guard.as_mut() {
                    progress.bump(batch_len);
                }
            }

            if (rows.len() as i64) < BATCH_SIZE {
                return self
                    .finish_completed(
                        store,
                        &target_name,
                        rewritten_count,
                        skipped_count,
                        failed_count,
                    )
                    .await;
            }
        }
    }
}

impl StorageRotateService {
    /// Terminal successful path — clear the header snapshot and log a
    /// final audit line. Unlike `storage_migration::finish_completed`
    /// there's no cutover / hot-swap step: rotation writes in place
    /// on the entry that's already there.
    async fn finish_completed(
        &self,
        store: &dyn JobStore,
        target_name: &str,
        rewritten: u64,
        skipped: u64,
        failed: u64,
    ) -> RunOutcome {
        self.clear_progress();
        tracing::info!(
            target: "audit",
            event = "storage_rotate.run_completed",
            run_id = %store.run_id(),
            target_name = %target_name,
            rewritten = rewritten,
            skipped = skipped,
            failed = failed,
            "storage_rotate completed on `{target_name}` — {rewritten} rewritten, {skipped} skipped, {failed} failed"
        );
        RunOutcome::Completed
    }

    fn clear_progress(&self) {
        let mut guard = self
            .rotation_progress
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *guard = None;
    }
}
