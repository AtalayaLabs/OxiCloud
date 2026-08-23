//! Reconciles `storage.chunk_manifests.ref_count` against its actual
//! referrers.
//!
//! ### Why this exists
//!
//! There are **two** reference counters, and only one of them was ever
//! verified. `DedupService::add_reference` bumps
//! `chunk_manifests.ref_count` first and only falls back to
//! `storage.blobs.ref_count`, so a reference lands on whichever counter
//! its hash names:
//!
//! * a **chunk** reference → `storage.blobs.ref_count`, reconciled by
//!   `blobs_consistency::refcount_mismatch`;
//! * a **Blob** reference (a CDC file, and now every derived artifact) →
//!   `chunk_manifests.ref_count`, reconciled by **nothing** before this
//!   job existed.
//!
//! That gap was survivable only because `dedup_gc`'s reap predicate had a
//! second clause — "no `storage.files` row references this manifest" —
//! which quietly compensated for drift on the bulk-delete paths where
//! `ref_count` is never decremented. Generalising that clause to the
//! reference registry (so thumbnails stop being reaped) removes the
//! compensation, which is exactly why the manifest counter now has to be
//! checked directly. See `docs/plan/derived-blobs.md`.
//!
//! ### The check
//!
//! * `manifest_refcount_mismatch` (severity `inconsistent`) —
//!   `chunk_manifests.ref_count` disagrees with the number of registered
//!   referrers. An **under**-count is the dangerous direction: GC reaps a
//!   manifest whose content is still reachable, taking its chunks with it.
//!   An over-count merely pins storage. Content-safe to report either way
//!   — the manifest row and its chunks are intact, the counter is wrong.
//!
//! ### Why a separate job rather than a phase of `blobs_consistency`
//!
//! One subject per job, per the subject-iteration principle the other five
//! consistency tenants follow. It also avoids changing the cursor format of
//! an existing *recoverable* job, which would strand any run paused across
//! the deploy.

use std::sync::Arc;

use async_trait::async_trait;
use sqlx::PgPool;

use crate::application::ports::blob_reference_ports::{BlobReferenceRegistry, RefLevel};
use crate::infrastructure::scheduler::{
    JobRegistry, JobRunArgs, JobStore, JobStoreProvider, RecoverableJobHandler, RunOutcome,
    RunStatus, record_or_log,
};

pub const MANIFESTS_CONSISTENCY_JOB_NAME: &str = "manifests_consistency";

/// Rows per batch. Each row costs one indexed subquery per registered
/// source; 200 matches `blobs_consistency` so the cancel-poll cadence is
/// the same for an operator watching either job.
const BATCH_SIZE: i64 = 200;

/// The page query, with `actual_ref_count` summed from the registered
/// reference sources at [`RefLevel::Manifest`].
///
/// Only sources that reference a **Blob** contribute — `storage.files`
/// today, plus `storage.content_derived_blobs` and
/// `storage.file_attached_blobs` once they exist.
/// `ChunksReferenceSource` returns `None` here: a manifest is never
/// referenced by another manifest, and including it would count this
/// manifest's own chunks as referrers of itself.
///
/// # Panics
///
/// If no source contributes at [`RefLevel::Manifest`] — a wiring bug that
/// would report every manifest as mismatched.
fn manifest_page_sql(registry: &BlobReferenceRegistry) -> String {
    let expected = registry.ref_count_expr(RefLevel::Manifest, "m.file_hash");
    assert!(
        expected != "0",
        "no manifest-level blob reference source registered: every manifest \
         would appear unreferenced"
    );

    format!(
        "SELECT
     m.file_hash  AS file_hash,
     m.ref_count  AS ref_count,
     m.total_size AS total_size,
     m.chunk_count AS chunk_count,
     ({expected})::bigint AS actual_ref_count
   FROM storage.chunk_manifests m
  WHERE ($1::text IS NULL OR m.file_hash > $1)
  ORDER BY m.file_hash
  LIMIT $2"
    )
}

pub struct ManifestsConsistencyCheck {
    pool: Arc<PgPool>,
    /// Built once from the blob-reference registry so this recompute and
    /// `dedup_gc`'s reap predicate answer "what references this manifest"
    /// identically. Assembled at construction rather than per page so the
    /// sweep runs a fixed statement.
    page_sql: String,
}

impl ManifestsConsistencyCheck {
    pub fn new(pool: Arc<PgPool>, reference_registry: Arc<BlobReferenceRegistry>) -> Self {
        Self {
            pool,
            page_sql: manifest_page_sql(&reference_registry),
        }
    }

    /// Chainable self-registration. On-demand only — operators fire it
    /// from `POST /api/admin/jobs/manifests_consistency/trigger`.
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
struct ManifestRow {
    file_hash: String,
    ref_count: i32,
    total_size: i64,
    chunk_count: i32,
    actual_ref_count: i64,
}

#[async_trait]
impl RecoverableJobHandler for ManifestsConsistencyCheck {
    fn name(&self) -> &str {
        MANIFESTS_CONSISTENCY_JOB_NAME
    }

    async fn count_total(&self) -> Option<u64> {
        let row: Result<(i64,), sqlx::Error> =
            sqlx::query_as("SELECT COUNT(*) FROM storage.chunk_manifests")
                .fetch_one(self.pool.as_ref())
                .await;
        match row {
            Ok((n,)) => Some(n.max(0) as u64),
            Err(e) => {
                tracing::debug!(
                    target: "oxicloud::consistency",
                    event = "manifests_consistency.count_total_failed",
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
        // Cursor: the last `file_hash` as UTF-8. Same convention as
        // `blobs_consistency`, which also pages a hash-keyed table.
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
            // Cooperative cancel poll between batches.
            match store.status().await {
                Ok(RunStatus::CancelRequested) => {
                    tracing::info!(
                        target: "oxicloud::consistency",
                        event = "manifests_consistency.cancelled",
                        run_id = %store.run_id(),
                        finding_count = finding_count,
                        "manifests_consistency cancelled cooperatively, pausing"
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

            let rows: Vec<ManifestRow> = match sqlx::query_as(&self.page_sql)
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
                    event = "manifests_consistency.completed",
                    run_id = %store.run_id(),
                    finding_count = finding_count,
                    "manifests_consistency completed with {} finding(s)",
                    finding_count
                );
                return RunOutcome::completed();
            }

            for row in &rows {
                if row.ref_count as i64 == row.actual_ref_count {
                    continue;
                }
                finding_count += 1;
                let delta = row.actual_ref_count - row.ref_count as i64;
                record_or_log(
                    store,
                    MANIFESTS_CONSISTENCY_JOB_NAME,
                    "manifest_refcount_mismatch",
                    "inconsistent",
                    None, // a hash isn't a UUID; the identifier lives in detail
                    serde_json::json!({
                        "file_hash":   row.file_hash,
                        "stored":      row.ref_count,
                        "actual":      row.actual_ref_count,
                        "delta":       delta,
                        "total_size":  row.total_size,
                        "chunk_count": row.chunk_count,
                        // Under-count is the dangerous direction: GC reaps a
                        // manifest whose content is still reachable.
                        "reap_risk":   delta > 0,
                    }),
                )
                .await;
            }

            // Advance cursor + checkpoint.
            let last_hash = rows
                .last()
                .map(|r| r.file_hash.clone())
                .expect("non-empty rows");
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
                    event = "manifests_consistency.completed",
                    run_id = %store.run_id(),
                    finding_count = finding_count,
                    "manifests_consistency completed with {} finding(s)",
                    finding_count
                );
                return RunOutcome::completed();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infrastructure::repositories::pg::blob_reference_sources::{
        ChunksReferenceSource, FilesReferenceSource,
    };

    fn default_registry() -> BlobReferenceRegistry {
        let pool = Arc::new(
            sqlx::pool::PoolOptions::<sqlx::Postgres>::new()
                .connect_lazy("postgres://invalid/invalid")
                .expect("lazy pool never connects"),
        );
        let mut registry = BlobReferenceRegistry::new();
        registry.register(Arc::new(FilesReferenceSource::new(pool.clone())));
        registry.register(Arc::new(ChunksReferenceSource::new(pool)));
        registry
    }

    /// Golden test — the statement is assembled from the registry, so pin it
    /// byte-for-byte and read the SQL here rather than deriving it mentally.
    ///
    /// Two invariants a future source must not break: the files term carries
    /// **no** `NOT EXISTS` guard (that guard exists to keep CDC rows out of
    /// the *chunk* level; applying it here would count nothing), and
    /// `chunk_hashes` appears nowhere — a manifest citing its own chunks is
    /// not a referrer of itself.
    #[tokio::test]
    async fn manifest_page_statement_is_stable() {
        let sql = manifest_page_sql(&default_registry());
        let expected = r#"SELECT
     m.file_hash  AS file_hash,
     m.ref_count  AS ref_count,
     m.total_size AS total_size,
     m.chunk_count AS chunk_count,
     ((SELECT COUNT(*) FROM storage.files cnt_f
               WHERE cnt_f.blob_hash = m.file_hash))::bigint AS actual_ref_count
   FROM storage.chunk_manifests m
  WHERE ($1::text IS NULL OR m.file_hash > $1)
  ORDER BY m.file_hash
  LIMIT $2"#;
        assert_eq!(sql, expected, "manifest page statement changed:\n{sql}");
    }

    #[tokio::test]
    async fn chunks_source_contributes_nothing_at_manifest_level() {
        let sql = manifest_page_sql(&default_registry());
        assert!(
            !sql.contains("chunk_hashes"),
            "a manifest must not count its own chunks as referrers: {sql}"
        );
    }

    #[test]
    #[should_panic(expected = "no manifest-level blob reference source")]
    fn empty_registry_refuses_to_build_page_statement() {
        let _ = manifest_page_sql(&BlobReferenceRegistry::new());
    }
}
