//! Fifth tenant of Part 2 (recoverable-run engine).
//!
//! Iterates the storage backend's blob-enumeration surface and
//! reports every blob physically present on the backend that has NO
//! matching row in `storage.blobs`. Complements
//! `blobs_consistency` (which walks the DB and probes the backend):
//! together they close the reference graph.
//!
//! ### Per-row check
//!
//! * `orphan_blob` (severity `inconsistent`) — bytes on disk / S3 /
//!   Azure with no registry row. Not data-loss (nothing broken —
//!   just storage overhead), but points at dedup_gc or
//!   ingest-path drift. Recovery = register-registry-row (if the
//!   bytes are still needed) OR delete the file (if truly orphan).
//!
//! ### Run-level check
//!
//! * `backend_unenumerable` (severity `anomaly`) — the backend
//!   returned `operation_not_supported` on the first
//!   `list_blob_hashes` call. Currently this fires when a
//!   `MigrationBlobBackend` is active (refuses enumeration
//!   mid-migration by design) or on an Azure backend (Azure impl
//!   deferred). Informational — operators know they can't rely on
//!   this scan under that config.
//!
//! ### Grace window
//!
//! Skip orphans whose backend mtime is within the last hour. Same
//! shape as `blobs_consistency` + `dedup_gc`: matches the
//! durability-before-visibility gap in the write path.
//!
//! ### Cost profile
//!
//! Batched: fetch N hashes from the backend, do one
//! `WHERE hash = ANY($1)` DB probe per batch, set-difference in
//! Rust. Dedup savings: yes — a chunk shared by 5 files still
//! walks once. On local backend the walk is
//! `walkdir + fs::metadata` per file (fast). On S3 the walk is
//! `ListObjectsV2` (rate-limited but paginated). Progress bar
//! uses `COUNT(*) FROM storage.blobs` as the approximate
//! denominator (backend count ≈ blob count on a healthy install;
//! deviation IS the finding).

use std::collections::HashSet;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{Duration, Utc};
use sqlx::PgPool;

use crate::application::ports::blob_storage_ports::BlobStorageBackend;
use crate::infrastructure::scheduler::{
    JobRegistry, JobRunArgs, JobStore, JobStoreProvider, ProgressKind, RecoverableJobHandler,
    RunOutcome, RunStatus, record_or_log,
};

pub const BACKEND_CONSISTENCY_JOB_NAME: &str = "backend_consistency";

/// Same `params` JSONB key `blobs_consistency` uses — kept identical
/// so operators grepping run rows see the same convention across
/// both storage-audit tenants.
pub const PROBED_STORAGE_PARAM: &str =
    crate::infrastructure::services::blobs_consistency_service::PROBED_STORAGE_PARAM;

/// Batch size for backend enumeration + DB probe. 500 is enough to
/// amortise the DB round-trip while keeping the cancel-poll cadence
/// sub-second (each batch = one backend list + one DB probe + Rust
/// set-difference). Larger batches on S3 hit ListObjectsV2's
/// per-request limit (1000) with wasted rows filtered client-side;
/// smaller batches over-poll the DB.
const BATCH_SIZE: usize = 500;

/// Grace window — orphans younger than this are skipped, since the
/// write path is durability-before-visibility: bytes hit disk before
/// the `storage.blobs` row is inserted. A scan catching a blob
/// mid-write would false-positive it as orphan. Matches
/// `blobs_consistency` + `dedup_gc`.
const CREATE_GRACE: Duration = Duration::hours(1);

/// Cap on affected-blob examples surfaced in the run-level
/// `backend_unenumerable` finding. Keeps the finding detail bounded.
const _MAX_EXAMPLES: usize = 5;

pub struct BackendConsistencyCheck {
    pool: Arc<PgPool>,
    /// Default backend to enumerate when `args.storage` is `None` —
    /// the live LIVE backend, injected at DI. `?storage=<name>`
    /// swaps in a fresh backend for the named entry (via
    /// [`build_entry_backend`]).
    backend: Arc<dyn BlobStorageBackend>,
    /// Snapshot of `AppConfig.storage_entries` for `?storage=<name>`
    /// resolution. Same rule blobs_consistency uses.
    storage_entries: Vec<crate::common::config::NamedStorageEntry>,
    /// `OXICLOUD_STORAGE_PATH` fallback for Local entries with no
    /// `_ROOT_DIR`. Same fallback rule as boot.
    storage_path_fallback: std::path::PathBuf,
}

impl BackendConsistencyCheck {
    pub fn new(
        pool: Arc<PgPool>,
        backend: Arc<dyn BlobStorageBackend>,
        storage_entries: Vec<crate::common::config::NamedStorageEntry>,
        storage_path_fallback: std::path::PathBuf,
    ) -> Self {
        Self {
            pool,
            backend,
            storage_entries,
            storage_path_fallback,
        }
    }

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
impl RecoverableJobHandler for BackendConsistencyCheck {
    fn name(&self) -> &str {
        BACKEND_CONSISTENCY_JOB_NAME
    }

    /// Approximate total: on a healthy install every backend blob
    /// has a `storage.blobs` row, so the DB count is a proxy for
    /// the backend count. The fraction deviating from 1.0 at run
    /// end IS informative — a fraction of 1.05 means the backend
    /// holds ~5% orphan bytes, which is exactly what this check
    /// surfaces per-row.
    async fn count_total(&self) -> Option<u64> {
        let row: Result<(i64,), sqlx::Error> = sqlx::query_as("SELECT COUNT(*) FROM storage.blobs")
            .fetch_one(self.pool.as_ref())
            .await;
        match row {
            Ok((n,)) => Some(n.max(0) as u64),
            Err(e) => {
                tracing::debug!(
                    target: "oxicloud::consistency",
                    event = "backend_consistency.count_total_failed",
                    error = %e,
                    "count_total failed — run will not surface a progress bar"
                );
                None
            }
        }
    }

    fn progress_kind(&self) -> ProgressKind {
        // Approximate — the denominator (DB count) is a proxy for
        // the backend count. Deviation is meaningful (see the
        // count_total doc).
        ProgressKind::Approximate
    }

    async fn run_resumable(
        &self,
        store: &dyn JobStore,
        args: &JobRunArgs,
        resume_cursor: Option<Vec<u8>>,
    ) -> RunOutcome {
        // Resolve the backend to probe. Mirrors the shape
        // `blobs_consistency` uses — Fresh + args.storage=Some stamps
        // probed_storage into params; Resumed reads it back so a
        // mid-audit restart re-uses the same target.
        let is_fresh = resume_cursor.is_none();
        let probed_storage: Option<String> = if is_fresh {
            let name = args.storage.clone();
            if let Some(n) = &name
                && let Err(e) = store.set_string_param(PROBED_STORAGE_PARAM, n).await
            {
                return RunOutcome::Failed {
                    message: format!("persist {PROBED_STORAGE_PARAM} to params: {e}"),
                };
            }
            name
        } else {
            match store.get_string_param(PROBED_STORAGE_PARAM).await {
                Ok(v) => v,
                Err(e) => {
                    return RunOutcome::Failed {
                        message: format!("read {PROBED_STORAGE_PARAM} from params: {e}"),
                    };
                }
            }
        };
        let backend: Arc<dyn BlobStorageBackend> = match &probed_storage {
            None => self.backend.clone(),
            Some(name) => match self.storage_entries.iter().find(|e| &e.name == name) {
                Some(entry) => crate::infrastructure::services::entry_backend::build_entry_backend(
                    entry,
                    &self.storage_path_fallback,
                ),
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
                            "storage entry `{name}` not found in OXICLOUD_STORAGE_ENTRIES. \
                             Available: [{available}]"
                        ),
                    };
                }
            },
        };
        if let Err(e) = backend.initialize().await {
            return RunOutcome::Failed {
                message: format!("probed backend init: {e}"),
            };
        }
        if let Some(name) = &probed_storage {
            tracing::info!(
                target: "audit",
                event = "backend_consistency.probe_scoped",
                run_id = %store.run_id(),
                probed_storage = %name,
                "backend_consistency enumerating entry `{name}` (via ?storage=<name>) instead \
                 of live backend"
            );
        }

        // Cursor = opaque backend continuation token, UTF-8-encoded.
        // Each backend defines its own format (local = shard/hash,
        // S3 = ListObjectsV2 continuation token, Azure = list
        // marker); the tenant just passes it through.
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

        let mut finding_count = 0u64;

        loop {
            // Cancel poll between batches.
            match store.status().await {
                Ok(RunStatus::CancelRequested) => {
                    tracing::info!(
                        target: "oxicloud::consistency",
                        event = "backend_consistency.cancelled",
                        run_id = %store.run_id(),
                        finding_count = finding_count,
                        "backend_consistency cancelled cooperatively, pausing"
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

            // Fetch next batch from the backend. `BlobListPage`
            // splits canonical blobs (checked for orphan) from
            // "unknown" entries (sidecar files, foreign namespaces —
            // emitted as informational notices).
            let page = match backend.list_blob_hashes(cursor.clone(), BATCH_SIZE).await {
                Ok(v) => v,
                Err(e) => {
                    // Backend refuses / can't enumerate. First-batch
                    // failure = we emit ONE run-level anomaly and
                    // complete cleanly (the run stays useful — the
                    // operator learns why nothing was checked
                    // instead of getting a red error). Mid-scan
                    // failure = we fail the run.

                    let is_first_batch = cursor.is_none() && finding_count == 0;
                    if is_first_batch {
                        // No local increment — the local
                        // `finding_count` is only used for the
                        // completion log below, but this branch
                        // returns immediately. The finding IS
                        // persisted + counted in `stats.finding_count`
                        // by `record_or_log` → `store.record_finding`.
                        record_or_log(
                            store,
                            BACKEND_CONSISTENCY_JOB_NAME,
                            "backend_unenumerable",
                            "anomaly",
                            None,
                            serde_json::json!({
                                "backend": backend.backend_type(),
                                "error":   format!("{e}"),
                                "note":    "backend refused enumeration; no per-blob orphan probes attempted",
                            }),
                        )
                        .await;
                        tracing::info!(
                            target: "oxicloud::consistency",
                            event = "backend_consistency.unenumerable",
                            run_id = %store.run_id(),
                            backend = backend.backend_type(),
                            "backend refused enumeration (typical during migration or on backends without list support)"
                        );
                        return RunOutcome::Completed;
                    }
                    return RunOutcome::Failed {
                        message: format!("backend list failed mid-scan: {e}"),
                    };
                }
            };

            let grace_cutoff = Utc::now() - CREATE_GRACE;

            // Non-canonical files in the blob namespace — sidecars,
            // wrong extensions, foreign namespaces. Informational
            // only (severity `anomaly`, blue notice pill). Emitted
            // BEFORE the blob orphan probes so operators see them
            // grouped near the top of the findings list per batch.
            // Grace-window filter applies here too — a temp file
            // being written should not fire a notice.
            for unknown in &page.unknowns {
                if let Some(mtime) = unknown.mtime
                    && mtime > grace_cutoff
                {
                    continue;
                }
                finding_count += 1;
                record_or_log(
                    store,
                    BACKEND_CONSISTENCY_JOB_NAME,
                    "unknown_backend_file",
                    "anomaly",
                    None,
                    serde_json::json!({
                        "path":    unknown.path,
                        "mtime":   unknown.mtime.map(|t| t.to_rfc3339()),
                        "backend": backend.backend_type(),
                        "note":    "non-canonical file in blob namespace (sidecar / wrong extension); not managed by dedup",
                    }),
                )
                .await;
            }

            if page.blobs.is_empty() && page.next_cursor.is_none() {
                // Nothing more to enumerate. Empty-blobs batches
                // with unknowns still emitted above are fine — we
                // fall through to completion.
                tracing::info!(
                    target: "oxicloud::consistency",
                    event = "backend_consistency.completed",
                    run_id = %store.run_id(),
                    finding_count = finding_count,
                    "backend_consistency completed with {} finding(s)",
                    finding_count
                );
                return RunOutcome::Completed;
            }

            // Batch DB probe: which of these hashes have a
            // `storage.blobs` row? One `WHERE hash = ANY($1)` per
            // batch — indexed lookup, cheap even on millions of
            // rows.
            let batch_hashes: Vec<String> = page.blobs.iter().map(|e| e.hash.clone()).collect();
            let db_present: HashSet<String> = if batch_hashes.is_empty() {
                HashSet::new()
            } else {
                match sqlx::query_as::<_, (String,)>(
                    r#"SELECT hash FROM storage.blobs WHERE hash = ANY($1)"#,
                )
                .bind(&batch_hashes[..])
                .fetch_all(self.pool.as_ref())
                .await
                {
                    Ok(rows) => rows.into_iter().map(|(h,)| h).collect(),
                    Err(e) => {
                        return RunOutcome::Failed {
                            message: format!("db probe: {e}"),
                        };
                    }
                }
            };

            for entry in &page.blobs {
                if db_present.contains(&entry.hash) {
                    continue;
                }
                if let Some(mtime) = entry.mtime
                    && mtime > grace_cutoff
                {
                    continue;
                }

                finding_count += 1;
                record_or_log(
                    store,
                    BACKEND_CONSISTENCY_JOB_NAME,
                    "orphan_blob",
                    "inconsistent",
                    None,
                    serde_json::json!({
                        "hash":    entry.hash,
                        "mtime":   entry.mtime.map(|t| t.to_rfc3339()),
                        "backend": backend.backend_type(),
                    }),
                )
                .await;
            }

            // Advance cursor + checkpoint. Scanned count tracks
            // both blobs and unknowns since we walked both.
            let batch_len = (page.blobs.len() + page.unknowns.len()) as u64;
            cursor = page.next_cursor;
            let cursor_bytes = cursor
                .as_ref()
                .map(|s| s.as_bytes().to_vec())
                .unwrap_or_default();
            if let Err(e) = store.checkpoint(cursor_bytes, batch_len).await {
                return RunOutcome::Failed {
                    message: format!("checkpoint: {e}"),
                };
            }

            // Backend returned no next_cursor → enumeration
            // complete. Emit the completion log and return.
            if cursor.is_none() {
                tracing::info!(
                    target: "oxicloud::consistency",
                    event = "backend_consistency.completed",
                    run_id = %store.run_id(),
                    finding_count = finding_count,
                    "backend_consistency completed with {} finding(s)",
                    finding_count
                );
                return RunOutcome::Completed;
            }
        }
    }
}
