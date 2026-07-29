//! Fourth tenant of Part 2 (recoverable-run engine).
//!
//! Iterates `storage.blobs` — the content-addressable registry —
//! and verifies each row against the physical backend AND against
//! the reference-counting invariants that `dedup_gc` relies on.
//!
//! Three per-row checks (subject-iteration principle in action —
//! one walk, multiple branches):
//!
//! * `blob_missing_from_backend` (severity `data_loss`) — the DB
//!   row says the hash exists but `BlobStorageBackend::blob_exists`
//!   returns false. Bytes gone from disk / S3 / Azure. Any file
//!   whose manifest references this hash (or whose whole-file
//!   `blob_hash` points at it) will fail to read.
//!
//! * `blob_corrupted` (severity `data_loss`, deep mode only) —
//!   bytes exist on the backend but their BLAKE3 no longer matches
//!   the hash under which they're indexed. Silent bit-rot. Only
//!   runs when the operator passes `?deep=true` because it costs a
//!   full read of every blob.
//!
//! * `refcount_mismatch` (severity `inconsistent`) —
//!   `storage.blobs.ref_count` disagrees with the actual reference
//!   count computed from `storage.files.blob_hash` +
//!   `storage.chunk_manifests.chunk_hashes[]`. Under-count means
//!   dedup GC could prematurely reap a live blob; over-count means
//!   a blob is being pinned longer than needed. Content-safe either
//!   way (the storage.blobs row is fine, the counter is wrong).
//!
//! ### Complements `files_consistency`
//!
//! `files_consistency` (Slice 6/10) iterates files and verifies DB
//! integrity. `blobs_consistency` iterates the storage registry and
//! verifies physical existence + counter integrity. Together they
//! cover both sides of the reference graph. Neither doubles the
//! other's work — probing per-blob (here) instead of per-file-chunk
//! preserves dedup savings: a chunk shared by 5 files gets probed
//! ONCE.
//!
//! ### Not covered here
//!
//! * **Orphan bytes on the backend** (files on disk with no DB row)
//!   — belongs in the future `backend_consistency` tenant which
//!   iterates the backend itself. Requires the `list_blob_hashes`
//!   trait extension and per-backend enumeration impls.
//! * **Manifest-level integrity** (`storage.chunk_manifests` rows
//!   pointing at reaped chunks) — already covered by
//!   `files_consistency::chunk_missing`.

use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use sqlx::PgPool;

use crate::application::ports::blob_storage_ports::BlobStorageBackend;
use crate::infrastructure::scheduler::{
    JobRegistry, JobRunArgs, JobStore, JobStoreProvider, RecoverableJobHandler, RunOutcome,
    RunStatus, record_or_log,
};

pub const BLOBS_CONSISTENCY_JOB_NAME: &str = "blobs_consistency";

/// Rows per batch. Blobs are numerous (millions on a busy install)
/// but per-row work is one indexed backend probe + one indexed SQL
/// ref-count query. 200 balances cancel-poll cadence against
/// round-trip amortisation.
const BATCH_SIZE: i64 = 200;

/// Grace window — rows created within this window are skipped by
/// the physical-existence probe because the write path is
/// durability-before-visibility: `dedup_service` writes bytes, then
/// registers the row a few ms later. A scan catching a row
/// mid-write would false-positive it as `blob_missing_from_backend`.
/// Same shape `dedup_gc` uses (see its `grace_secs`).
const CREATE_GRACE: Duration = Duration::hours(1);

/// Cap on reverse-lookup file names surfaced in a finding's detail.
/// Keeps detail JSON size bounded when a broken blob is referenced
/// by hundreds of files.
const AFFECTED_FILES_SAMPLE: i64 = 5;

pub struct BlobsConsistencyCheck {
    pool: Arc<PgPool>,
    backend: Arc<dyn BlobStorageBackend>,
}

impl BlobsConsistencyCheck {
    pub fn new(pool: Arc<PgPool>, backend: Arc<dyn BlobStorageBackend>) -> Self {
        Self { pool, backend }
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

#[derive(Debug, sqlx::FromRow)]
struct BlobRow {
    hash: String,
    size: i64,
    ref_count: i32,
    created_at: DateTime<Utc>,
    /// Real reference count derived from the actual references —
    /// files' whole-file `blob_hash` PLUS every chunk hash across
    /// `storage.chunk_manifests`. Compared to `ref_count` (the
    /// stored counter) to detect drift.
    actual_ref_count: i64,
}

#[async_trait]
impl RecoverableJobHandler for BlobsConsistencyCheck {
    fn name(&self) -> &str {
        BLOBS_CONSISTENCY_JOB_NAME
    }

    /// Definitive count. `storage.blobs` PK scan is index-only;
    /// even at millions of rows it's sub-second on modern PG.
    async fn count_total(&self) -> Option<u64> {
        let row: Result<(i64,), sqlx::Error> = sqlx::query_as("SELECT COUNT(*) FROM storage.blobs")
            .fetch_one(self.pool.as_ref())
            .await;
        match row {
            Ok((n,)) => Some(n.max(0) as u64),
            Err(e) => {
                tracing::debug!(
                    target: "oxicloud::consistency",
                    event = "blobs_consistency.count_total_failed",
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
        // Cursor = the last-visited `hash` string, UTF-8-encoded. On
        // resume, we walk `WHERE hash > $cursor` in ASC order. First
        // batch: NULL cursor → start from the smallest hash.
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

        // Per-run finding counter (feeds outer JobOutcome extras via
        // stats.finding_count — actual persistence happens in
        // `record_finding` on each emission).
        let mut finding_count = 0u64;

        // Deep mode = re-hash bytes for bit-rot detection. Logged
        // once at run start so operators tailing tracing know why the
        // scan is taking hours.
        if args.deep {
            tracing::info!(
                target: "oxicloud::consistency",
                event = "blobs_consistency.deep_mode_active",
                run_id = %store.run_id(),
                "deep mode: re-reading + re-hashing every blob (bit-rot detection)"
            );
        }

        loop {
            // Cooperative cancel poll between batches.
            match store.status().await {
                Ok(RunStatus::CancelRequested) => {
                    tracing::info!(
                        target: "oxicloud::consistency",
                        event = "blobs_consistency.cancelled",
                        run_id = %store.run_id(),
                        finding_count = finding_count,
                        "blobs_consistency cancelled cooperatively, pausing"
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

            // Fetch the next batch. Per-row `actual_ref_count`
            // computed inline via correlated subqueries — one for
            // legacy whole-file references (`files.blob_hash`), one
            // for CDC chunk references (`chunk_manifests.chunk_hashes`).
            // GIN index on `chunk_hashes` (migration
            // 20260628000000_delta_upload_gin_index) makes the
            // `= ANY(chunk_hashes)` probe cheap.
            // `storage.blobs.ref_count` semantics — what the invariant
            // dedup_service maintains actually is:
            //
            //   ref_count = (number of chunk_manifests whose
            //                chunk_hashes[] contains this hash)
            //             + (number of files.blob_hash pointing at
            //                this hash on the LEGACY whole-file path
            //                — i.e. files with NO manifest for their
            //                blob_hash)
            //
            // Naively `COUNT(files) + COUNT(manifests referring)`
            // double-counts single-chunk CDC files: for a file whose
            // whole-file hash == its single chunk's hash (any file
            // small enough to fit in one CDC chunk — under ~256 KB
            // average), the file appears BOTH in `files.blob_hash`
            // AND in the manifest's `chunk_hashes[]`. The `NOT
            // EXISTS` clause below excludes CDC-path files from the
            // legacy count so the two terms don't overlap.
            let rows: Vec<BlobRow> = match sqlx::query_as(
                r#"
                SELECT
                    b.hash                                             AS hash,
                    b.size                                             AS size,
                    b.ref_count                                        AS ref_count,
                    b.created_at                                       AS created_at,
                    (
                        (SELECT COUNT(*) FROM storage.files f
                          WHERE f.blob_hash = b.hash
                            AND NOT EXISTS (
                                SELECT 1 FROM storage.chunk_manifests m
                                 WHERE m.file_hash = f.blob_hash
                            ))
                      + (SELECT COUNT(*) FROM storage.chunk_manifests m
                          WHERE b.hash = ANY(m.chunk_hashes))
                    )::bigint                                          AS actual_ref_count
                  FROM storage.blobs b
                 WHERE ($1::text IS NULL OR b.hash > $1)
                 ORDER BY b.hash
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
                    target: "oxicloud::consistency",
                    event = "blobs_consistency.completed",
                    run_id = %store.run_id(),
                    finding_count = finding_count,
                    deep = args.deep,
                    "blobs_consistency completed with {} finding(s)",
                    finding_count
                );
                return RunOutcome::Completed;
            }

            let grace_cutoff = Utc::now() - CREATE_GRACE;

            for row in &rows {
                // (1) refcount_mismatch — content-safe check, cheap,
                // always runs. Emitted BEFORE the physical probe so
                // a broken-and-miscounted blob shows both findings.
                if row.ref_count as i64 != row.actual_ref_count {
                    finding_count += 1;
                    let affected = affected_files(self.pool.as_ref(), &row.hash).await;
                    record_or_log(
                        store,
                        BLOBS_CONSISTENCY_JOB_NAME,
                        "refcount_mismatch",
                        "inconsistent",
                        None, // hash isn't a UUID; resource identifier lives in detail
                        serde_json::json!({
                            "hash":            row.hash,
                            "stored":          row.ref_count,
                            "actual":          row.actual_ref_count,
                            "delta":           row.actual_ref_count - row.ref_count as i64,
                            "size":            row.size,
                            "affected_files":  affected,
                        }),
                    )
                    .await;
                }

                // Skip physical probes for rows within the write
                // grace window — writes-in-flight would false-positive.
                if row.created_at > grace_cutoff {
                    continue;
                }

                // (2) blob_missing_from_backend — normal mode
                // physical existence probe. Fails-open on backend
                // error (log + skip): a transient S3 network blip
                // shouldn't produce a flood of false data_loss
                // findings.
                let exists = match self.backend.blob_exists(&row.hash).await {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::warn!(
                            target: "oxicloud::consistency",
                            event = "blobs_consistency.blob_exists_error",
                            run_id = %store.run_id(),
                            hash = %row.hash,
                            error = %e,
                            "blob_exists probe failed; skipping this row"
                        );
                        continue;
                    }
                };

                if !exists {
                    finding_count += 1;
                    let affected = affected_files(self.pool.as_ref(), &row.hash).await;
                    record_or_log(
                        store,
                        BLOBS_CONSISTENCY_JOB_NAME,
                        "blob_missing_from_backend",
                        "data_loss",
                        None,
                        serde_json::json!({
                            "hash":            row.hash,
                            "size":            row.size,
                            "ref_count":       row.ref_count,
                            "affected_files":  affected,
                        }),
                    )
                    .await;
                    // No point re-hashing bytes that aren't there.
                    continue;
                }

                // (3) blob_corrupted — DEEP MODE only. Read the
                // whole blob, recompute BLAKE3, compare to the hash
                // it's indexed under. Any mismatch = silent bit-rot.
                if args.deep {
                    match verify_hash(self.backend.as_ref(), &row.hash).await {
                        Ok(true) => {}
                        Ok(false) => {
                            finding_count += 1;
                            let affected = affected_files(self.pool.as_ref(), &row.hash).await;
                            record_or_log(
                                store,
                                BLOBS_CONSISTENCY_JOB_NAME,
                                "blob_corrupted",
                                "data_loss",
                                None,
                                serde_json::json!({
                                    "hash":            row.hash,
                                    "size":            row.size,
                                    "ref_count":       row.ref_count,
                                    "affected_files":  affected,
                                }),
                            )
                            .await;
                        }
                        Err(e) => {
                            tracing::warn!(
                                target: "oxicloud::consistency",
                                event = "blobs_consistency.verify_hash_error",
                                run_id = %store.run_id(),
                                hash = %row.hash,
                                error = %e,
                                "verify_hash failed; not a corruption signal on its own"
                            );
                        }
                    }
                }
            }

            // Advance cursor + checkpoint.
            let last_hash = rows.last().map(|r| r.hash.clone()).expect("non-empty rows");
            cursor = Some(last_hash.clone());
            let batch_len = rows.len() as u64;
            if let Err(e) = store.checkpoint(last_hash.into_bytes(), batch_len).await {
                return RunOutcome::Failed {
                    message: format!("checkpoint: {e}"),
                };
            }

            if (rows.len() as i64) < BATCH_SIZE {
                tracing::info!(
                    target: "oxicloud::consistency",
                    event = "blobs_consistency.completed",
                    run_id = %store.run_id(),
                    finding_count = finding_count,
                    deep = args.deep,
                    "blobs_consistency completed with {} finding(s)",
                    finding_count
                );
                return RunOutcome::Completed;
            }
        }
    }
}

/// Sample of file names that reference this blob — either directly
/// (`files.blob_hash = $hash`, legacy pre-CDC) or transitively via a
/// manifest (`chunk_hashes @> ARRAY[$hash]`, post-CDC dominant path).
/// Capped so a chunk shared by 10 000 files doesn't blow up the
/// finding detail JSON. Order is arbitrary — sampling for
/// diagnosis, not enumeration.
async fn affected_files(pool: &PgPool, hash: &str) -> Vec<String> {
    let rows: Vec<(String,)> = sqlx::query_as(
        r#"
        SELECT DISTINCT f.name
          FROM storage.files f
         WHERE f.blob_hash = $1
            OR EXISTS (
                 SELECT 1 FROM storage.chunk_manifests m
                  WHERE m.file_hash = f.blob_hash
                    AND $1 = ANY(m.chunk_hashes)
               )
         LIMIT $2
        "#,
    )
    .bind(hash)
    .bind(AFFECTED_FILES_SAMPLE)
    .fetch_all(pool)
    .await
    .unwrap_or_default();
    rows.into_iter().map(|(n,)| n).collect()
}

/// Deep-mode helper — read the blob from the backend and recompute
/// its BLAKE3 hash. Returns `Ok(true)` when the recomputed hash
/// matches `expected_hash` (byte for byte), `Ok(false)` on mismatch
/// (bit-rot), `Err(_)` on any backend-side error (network blip,
/// permission issue) — callers log-and-skip errors since a transient
/// failure isn't a corruption signal.
async fn verify_hash(
    backend: &dyn BlobStorageBackend,
    expected_hash: &str,
) -> Result<bool, crate::common::errors::DomainError> {
    use crate::common::errors::DomainError;
    use futures::StreamExt;

    let mut stream = backend.get_blob_stream(expected_hash).await?;
    let mut hasher = blake3::Hasher::new();

    while let Some(chunk) = stream.next().await {
        let bytes = chunk.map_err(|e| {
            DomainError::internal_error("BlobsConsistency", format!("stream read: {e}"))
        })?;
        hasher.update(&bytes);
    }

    let actual = hasher.finalize().to_hex().to_string();
    Ok(actual == expected_hash)
}
