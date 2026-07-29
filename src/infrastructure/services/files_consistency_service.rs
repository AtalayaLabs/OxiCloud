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
    /// File name (basename). Captured into finding `detail` so
    /// operators see a human identifier next to the UUID.
    name: String,
    folder_id: Option<Uuid>,
    is_trashed: bool,
    size: i64,
    blob_hash: String,
    /// `None` when `folder_id IS NULL` (file at drive root) — the
    /// LEFT JOIN yields no parent row.
    parent_is_trashed: Option<bool>,
    /// Parent folder's materialised `path` (post-D7 files carry no
    /// path themselves). `None` for root files.
    parent_path: Option<String>,
    /// Legacy whole-file blob row size (pre-CDC). `None` when the
    /// file was ingested via CDC (`chunk_manifests` path) OR when
    /// the blob is truly missing — disambiguated by `manifest_size`.
    blob_size: Option<i64>,
    /// CDC manifest total size. `Some` when the file was ingested
    /// via FastCDC (its bytes live as chunks referenced by
    /// `storage.chunk_manifests.chunk_hashes`, not as one
    /// `storage.blobs` row). `None` when there is no manifest for
    /// this hash.
    manifest_size: Option<i64>,
    /// Total chunks the manifest claims. `None` when the file is
    /// pre-CDC (whole-file blob path) or has no manifest.
    manifest_chunk_count: Option<i32>,
    /// Count of chunks referenced by the manifest that have NO
    /// matching row in `storage.blobs`. `None` when there's no
    /// manifest to check. `Some(n)` with `n > 0` means the manifest
    /// points at reaped chunks — a real data-loss condition, more
    /// precise than plain `missing_blob` (which only fires when the
    /// whole-file registry entry is absent). This is a DB-registry
    /// check; physical backend-existence checks belong in the
    /// future `storage_consistency` tenant.
    chunks_missing: Option<i64>,
}

/// Build the file's display path from its folder's `path` and its
/// own `name`. Root files just show the name. Trashed folder paths
/// still work (ltree keeps them intact under `is_trashed`).
fn display_path(folder_path: Option<&str>, name: &str) -> String {
    match folder_path {
        Some(p) if !p.is_empty() => format!("{p}/{name}"),
        _ => name.to_string(),
    }
}

#[async_trait]
impl RecoverableJobHandler for FilesConsistencyCheck {
    fn name(&self) -> &str {
        FILES_CONSISTENCY_JOB_NAME
    }

    /// Definitive count — one row per file. This is the largest table
    /// of the trio (millions on big installs); COUNT(*) is still an
    /// index-only scan but can take ~seconds. The tradeoff is worth
    /// it — an operator staring at a running files_consistency scan
    /// wants a bar, and a seconds-scale one-off at run start is
    /// invisible compared to the multi-minute scan that follows.
    async fn count_total(&self) -> Option<u64> {
        let row: Result<(i64,), sqlx::Error> = sqlx::query_as("SELECT COUNT(*) FROM storage.files")
            .fetch_one(self.pool.as_ref())
            .await;
        match row {
            Ok((n,)) => Some(n.max(0) as u64),
            Err(e) => {
                tracing::debug!(
                    target: "oxicloud::consistency",
                    event = "files_consistency.count_total_failed",
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
            // Three LEFT JOINs — the blob-existence check has to
            // handle BOTH storage paths OxiCloud uses:
            //
            //   * `storage.chunk_manifests` (CDC / FastCDC) — the
            //     dominant path for anything ingested after Apr 2026.
            //     Whole-file hash lives here; actual bytes are chunks
            //     referenced by `chunk_hashes[]`.
            //   * `storage.blobs` (legacy pre-CDC whole-file blob) —
            //     still supported via the read path's fallback for
            //     pre-CDC uploads.
            //
            // A file is "missing_blob" ONLY when NEITHER row exists.
            // Deep chunk validation (every chunk in `chunk_hashes[]`
            // present in `storage.blobs`) is out of scope here — it
            // belongs in the future `storage_consistency` tenant that
            // walks the backend against the blob registry.
            // Correlated subquery `chunks_missing` runs per-row over
            // the manifest's chunk_hashes array. `hash` is indexed
            // (PRIMARY KEY on storage.blobs), so each `NOT EXISTS`
            // probe is O(log n). NULL (not zero) when the file is
            // pre-CDC or has no manifest — the LEFT JOIN result on
            // `m` is NULL and `unnest(NULL::text[])` yields zero rows.
            let rows: Vec<FileRow> = match sqlx::query_as(
                r#"
                SELECT
                    f.id                                                        AS id,
                    f.name                                                      AS name,
                    f.folder_id                                                 AS folder_id,
                    f.is_trashed                                                AS is_trashed,
                    f.size                                                      AS size,
                    f.blob_hash                                                 AS blob_hash,
                    parent.is_trashed                                           AS parent_is_trashed,
                    parent.path                                                 AS parent_path,
                    b.size                                                      AS blob_size,
                    m.total_size                                                AS manifest_size,
                    m.chunk_count                                               AS manifest_chunk_count,
                    CASE WHEN m.chunk_hashes IS NULL THEN NULL
                         ELSE (
                             SELECT COUNT(*)::bigint
                               FROM unnest(m.chunk_hashes) AS ch(hash)
                              WHERE NOT EXISTS (
                                  SELECT 1 FROM storage.blobs bb
                                   WHERE bb.hash = ch.hash
                              )
                         )
                    END                                                          AS chunks_missing
                  FROM storage.files f
                  LEFT JOIN storage.folders          parent ON parent.id  = f.folder_id
                  LEFT JOIN storage.blobs            b      ON b.hash      = f.blob_hash
                  LEFT JOIN storage.chunk_manifests  m      ON m.file_hash = f.blob_hash
                 WHERE ($1::uuid IS NULL OR f.id > $1)
                   -- Grace: skip files < 1h old. Delta-upload inserts
                   -- chunks with ref_count=0 BEFORE the commit that
                   -- inserts the file row + manifest, so the normal
                   -- path is race-free — but replace/overwrite flows
                   -- have narrow windows where a mid-transaction scan
                   -- could see `missing_blob` or `chunk_missing`.
                   -- Same grace shape as `blobs_consistency`.
                   AND f.created_at < NOW() - INTERVAL '1 hour'
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
                // Human-readable path captured once per row and folded
                // into every finding on this row. `name` is the raw
                // basename (useful even when the parent is orphaned
                // and `parent_path` is None).
                let path = display_path(row.parent_path.as_deref(), &row.name);

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
                            "name":      row.name,
                            "path":      path,
                            "folder_id": row.folder_id,
                        }),
                    )
                    .await;
                }

                // Content-bearing size for this file, in priority
                // order: CDC manifest (dominant path — every file
                // uploaded after Apr 2026), then legacy pre-CDC
                // whole-file blob. `None` = no registry entry on
                // either path → real `missing_blob`.
                let content_size = row.manifest_size.or(row.blob_size);

                // (2) missing_blob: NEITHER the CDC manifest nor the
                // legacy blob row exists for this hash. Real data-loss
                // indicator — the read path checks manifest first and
                // falls back to blob; if both are missing, reading
                // the file will fail. NOT a false positive for CDC
                // files, because the manifest check catches them.
                if content_size.is_none() {
                    finding_count += 1;
                    record_or_log(
                        store,
                        FILES_CONSISTENCY_JOB_NAME,
                        "missing_blob",
                        "data_loss",
                        Some(row.id),
                        serde_json::json!({
                            "name":      row.name,
                            "path":      path,
                            "blob_hash": row.blob_hash,
                        }),
                    )
                    .await;
                    // No point checking size when neither registry
                    // entry exists — skip (3) for this row.
                    continue;
                }

                // (2b) chunk_missing: the file's CDC manifest exists
                // and points at N chunks, but K of them have no row
                // in `storage.blobs`. Real data-loss condition — the
                // read path will fail reassembly when it tries to
                // fetch a reaped chunk. Typically caused by a dedup
                // GC race (chunk reaped while a manifest still held
                // a reference) or partial pg_dump/restore that
                // dropped `storage.blobs` rows.
                if let Some(missing) = row.chunks_missing
                    && missing > 0
                {
                    finding_count += 1;
                    record_or_log(
                        store,
                        FILES_CONSISTENCY_JOB_NAME,
                        "chunk_missing",
                        "data_loss",
                        Some(row.id),
                        serde_json::json!({
                            "name":            row.name,
                            "path":            path,
                            "blob_hash":       row.blob_hash,
                            "chunks_missing":  missing,
                            "chunks_total":    row.manifest_chunk_count,
                        }),
                    )
                    .await;
                    // Deliberately DON'T `continue` — a
                    // chunk_missing finding does not preclude a
                    // size mismatch, and the two are independent
                    // signals worth surfacing separately.
                }

                // (3) blob_size_mismatch: denormalised size drifted
                // from the content-registry's authoritative size.
                // Prefers manifest.total_size when present (post-CDC
                // ingest path); falls back to blob.size (legacy).
                if let Some(bs) = content_size
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
                            "name":      row.name,
                            "path":      path,
                            "blob_hash": row.blob_hash,
                            "stored":    row.size,
                            "actual":    bs,
                            "delta":     row.size - bs,
                            "source":    if row.manifest_size.is_some() { "manifest" } else { "blob" },
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
