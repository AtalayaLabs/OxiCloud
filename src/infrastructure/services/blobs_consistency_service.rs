//! Fourth tenant of Part 2 (recoverable-run engine).
//!
//! Iterates `storage.blobs` — the content-addressable registry — and
//! checks the reference-counting invariant `dedup_gc` relies on.
//!
//! **Database only.** It opens no backend and makes no network call;
//! `?storage=<name>` and `?deep=true` are both inert here.
//!
//! One per-row check:
//!
//! * `refcount_mismatch` (severity `inconsistent`) —
//!   `storage.blobs.ref_count` disagrees with the actual reference
//!   count computed from `storage.files.blob_hash` +
//!   `storage.chunk_manifests.chunk_hashes[]`. Under-count means
//!   dedup GC could prematurely reap a live blob; over-count means
//!   a blob is being pinned longer than needed. Content-safe either
//!   way (the storage.blobs row is fine, the counter is wrong).
//!
//! ### Why nothing physical lives here any more
//!
//! This tenant used to probe `BlobStorageBackend::blob_exists` once
//! per row for `blob_missing_from_backend`, and under `?deep=true`
//! read and re-hashed every blob for `blob_corrupted` /
//! `blob_unreadable`.
//!
//! All three moved to `backend_consistency`, which merge-joins the
//! backend's enumeration against this same table in one ordered pass.
//! It reports the same missing bytes, plus the backend-only orphans a
//! DB walk cannot see by construction, at one enumeration instead of
//! N round-trips — and a deep pass there re-hashes the matched pairs
//! it already holds. Keeping the probe here bought nothing and made
//! every scheduled sweep pay for it.
//!
//! What is left is the half that needs no backend at all: a counter,
//! and the two tables that determine what it should be.
//!
//! ### Elsewhere in the graph
//!
//! * **Physical existence, orphan bytes, bit-rot** —
//!   `backend_consistency`.
//! * **File-side DB integrity** (parent folder, blob reference,
//!   denormalised size) — `files_consistency`.
//! * **Manifest-level integrity** (`storage.chunk_manifests` rows
//!   pointing at reaped chunks) — `files_consistency::chunk_missing`.
//! * **The OTHER refcount** (`chunk_manifests.ref_count`, which every
//!   whole-Blob reference lands on) — `manifests_consistency`.

use std::sync::Arc;

use async_trait::async_trait;
use sqlx::PgPool;

use crate::application::ports::blob_reference_ports::{BlobReferenceRegistry, RefLevel};
use crate::infrastructure::scheduler::{
    JobRegistry, JobRunArgs, JobStore, JobStoreProvider, Mutates, RecoverableJobHandler,
    RunOutcome, RunStatus, record_or_log,
};
use crate::infrastructure::services::blob_diagnostics::affected_files;

pub const BLOBS_CONSISTENCY_JOB_NAME: &str = "blobs_consistency";

/// Rows per batch. Blobs are numerous (millions on a busy install)
/// but per-row work is now a single indexed SQL ref-count comparison,
/// with no backend round-trip. 200 balances cancel-poll cadence
/// against round-trip amortisation.
const BATCH_SIZE: i64 = 200;

pub struct BlobsConsistencyCheck {
    pool: Arc<PgPool>,
    /// The chunk-level page query, assembled once from the blob-reference
    /// registry so this recompute and `dedup_gc` agree on what "referenced"
    /// means. Built at construction rather than per page so the sweep runs a
    /// fixed statement — same reasoning as `DedupService::manifest_reap_sql`.
    /// See `docs/plan/derived-blobs.md`.
    chunk_page_sql: String,
    /// Per-row repair UPDATE, built from the SAME registry as
    /// `chunk_page_sql` so detection and repair use identical formulas
    /// by construction. A future `RefLevel::Chunk` ref source added to
    /// the registry flows into both without a code change here.
    ///
    /// Previously the repair query was inlined with a hardcoded
    /// 2-term formula (accidentally matching detection today). Would
    /// silently diverge the moment a new chunk-level ref source
    /// landed — same class of latent bug the sibling
    /// `manifests_consistency` service hit 2026-09-02. Preemptively
    /// pulled from the registry here to keep the pair symmetric.
    chunk_repair_sql: String,
}

/// The chunk-level page query, with `actual_ref_count` summed from the
/// registered reference sources.
///
/// `storage.blobs.ref_count` semantics — the invariant `dedup_service`
/// actually maintains:
///
/// ```text
/// ref_count = (number of chunk_manifests whose chunk_hashes[] contains
///              this hash)
///           + (number of files.blob_hash pointing at this hash on the
///              LEGACY whole-file path — files with NO manifest for their
///              blob_hash)
/// ```
///
/// Naively `COUNT(files) + COUNT(manifests referring)` double-counts
/// single-chunk CDC files: where a file's whole-file hash equals its lone
/// chunk's hash (anything under one CDC chunk), the file appears BOTH in
/// `files.blob_hash` and in the manifest's `chunk_hashes[]`. The
/// `NOT EXISTS` guard inside `FilesReferenceSource`'s chunk-level fragment
/// excludes CDC-path files from the legacy term so the two don't overlap.
///
/// The GIN index on `chunk_hashes` (migration
/// `20260628000000_delta_upload_gin_index`) keeps the `= ANY(chunk_hashes)`
/// probe cheap.
///
/// # Panics
///
/// If no source contributes at [`RefLevel::Chunk`] — a wiring bug that
/// would make every blob look unreferenced and flag the whole table as
/// `refcount_mismatch`.
fn chunk_page_sql(registry: &BlobReferenceRegistry) -> String {
    let expected = registry.ref_count_expr(RefLevel::Chunk, "b.hash");
    assert!(
        expected != "0",
        "no chunk-level blob reference source registered: every blob would \
         appear unreferenced"
    );

    format!(
        "SELECT
     b.hash       AS hash,
     b.size       AS size,
     b.ref_count  AS ref_count,
     ({expected})::bigint AS actual_ref_count
   FROM storage.blobs b
  WHERE ($1::text IS NULL OR b.hash > $1)
  ORDER BY b.hash
  LIMIT $2"
    )
}

/// Per-row corrective UPDATE for `storage.blobs.ref_count`, targeting
/// one blob by `hash`. Uses the SAME registry-derived expression as
/// [`chunk_page_sql`] so detection and repair agree on "actual" by
/// construction. A future `RefLevel::Chunk` ref source added to the
/// registry flows into both queries with no code change here.
///
/// The `<> (subquery)` guard makes the UPDATE a no-op when the value
/// is already correct — idempotent under concurrent-repair races and
/// under retry. The subquery re-reads inside the same statement, so a
/// concurrent write between page fetch and this UPDATE can't leave a
/// stale value.
fn chunk_repair_sql(registry: &BlobReferenceRegistry) -> String {
    let expected = registry.ref_count_expr(RefLevel::Chunk, "b.hash");
    format!(
        "UPDATE storage.blobs b
            SET ref_count = ({expected})::bigint
          WHERE b.hash = $1
            AND b.ref_count <> ({expected})::bigint"
    )
}

impl BlobsConsistencyCheck {
    pub fn new(pool: Arc<PgPool>, reference_registry: Arc<BlobReferenceRegistry>) -> Self {
        Self {
            pool,
            chunk_page_sql: chunk_page_sql(&reference_registry),
            chunk_repair_sql: chunk_repair_sql(&reference_registry),
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

#[derive(Debug, sqlx::FromRow)]
struct BlobRow {
    hash: String,
    size: i64,
    ref_count: i32,
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

    fn description(&self) -> &'static str {
        "Walks storage.blobs and reports rows whose ref_count disagrees \
         with the references that actually exist. An under-count lets \
         dedup_gc reap a blob that is still in use; an over-count pins \
         one nothing needs. Database only — it never touches the storage \
         backend, so it is cheap and safe to run at any time. Missing, \
         orphaned or corrupted bytes are backend_consistency's job."
    }

    fn mutates(&self) -> Mutates {
        Mutates::OnRepairOnly
    }

    fn repair_description(&self) -> Option<&'static str> {
        Some(
            "Rewrites drifted ref_count values to the recomputed truth. \
             Does not delete blobs or resurrect missing bytes — an \
             over-counted blob simply becomes eligible for the next \
             dedup_gc sweep.",
        )
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
        // No backend is resolved here, and `?storage=<name>` is inert:
        // this tenant reads nothing but the database. Everything physical
        // — existence, orphan bytes, bit-rot — belongs to
        // `backend_consistency`, which finds it in one enumeration pass
        // instead of one probe per row.
        //
        // Snapshot "is this a Fresh run?" BEFORE the resume_cursor
        // match consumes it — otherwise the `is_none()` check later
        // borrows a partially-moved value. Fresh = no cursor bytes
        // at all; Resumed = cursor bytes present (possibly empty).
        let is_fresh = resume_cursor.is_none();

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
        // Only touched when `args.repair == true`. Symmetric with
        // `manifests_consistency`; reported in completion log +
        // `extra_stats` so operators see "found N, fixed M" in one line.
        let mut repaired_count = 0u64;

        // `?deep=true` is not handled here. Re-reading and re-hashing
        // bytes is backend work end to end, so it moved to
        // `backend_consistency`, where the merge-join already holds the
        // matched key pairs worth verifying. A deep flag on this tenant
        // would be a flag with nothing to do.

        // Repair mode persisted to `params.repair` so the admin run-detail
        // view can display it. Fresh persists what the trigger asked for;
        // Resume reads back so a paused repair scan stays a repair
        // scan (a mid-scan crash mustn't silently downgrade to
        // discovery-only for the remaining rows).
        let repair = if is_fresh {
            let v = if args.repair { "true" } else { "false" };
            if let Err(e) = store.set_string_param("repair", v).await {
                return RunOutcome::Failed {
                    message: format!("failed to persist repair flag to params: {e}"),
                };
            }
            args.repair
        } else {
            match store.get_string_param("repair").await {
                Ok(Some(v)) => v == "true",
                Ok(None) => false,
                Err(e) => {
                    return RunOutcome::Failed {
                        message: format!("read `repair` from params: {e}"),
                    };
                }
            }
        };

        if repair {
            tracing::info!(
                target: "oxicloud::consistency",
                event = "blobs_consistency.repair_mode_active",
                run_id = %store.run_id(),
                "repair mode: refcount_mismatch findings will trigger corrective UPDATE"
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

            // Fetch the next batch. `actual_ref_count` is summed from the
            // registered reference sources — see `chunk_page_sql`, which
            // documents the invariant and the single-chunk double-count trap.
            let rows: Vec<BlobRow> = match sqlx::query_as(&self.chunk_page_sql)
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
                    repaired_count = repaired_count,
                    repair_requested = repair,
                    "blobs_consistency completed with {} finding(s), {} repaired",
                    finding_count,
                    repaired_count
                );
                return RunOutcome::completed_with(serde_json::json!({
                    "repair_requested": repair,
                    "repaired_count":   repaired_count,
                }));
            }

            // No grace window here any more. It existed to keep the
            // physical probe from flagging a blob whose bytes had landed
            // but whose row hadn't — a write-path race this tenant no
            // longer looks at. The refcount comparison reads one
            // consistent DB snapshot, so there is nothing to wait for.
            for row in &rows {
                if row.ref_count as i64 == row.actual_ref_count {
                    continue;
                }
                finding_count += 1;
                let affected = affected_files(self.pool.as_ref(), &row.hash).await;
                let detail = serde_json::json!({
                    "hash":            row.hash,
                    "stored":          row.ref_count,
                    "actual":          row.actual_ref_count,
                    "delta":           row.actual_ref_count - row.ref_count as i64,
                    "size":            row.size,
                    "affected_files":  affected,
                });

                // Repair pass — content-safe corrective UPDATE. Sets
                // `stored` to the value the auditor formula would
                // compute at UPDATE time (subquery matches
                // `chunk_page_sql`'s `actual_ref_count`), so a
                // concurrent write between our page fetch and this
                // UPDATE can't leave a stale value — the subquery
                // re-reads inside the same statement. The `<>`
                // guard makes the UPDATE a no-op if the value is
                // already correct, so this is idempotent under retry.
                //
                // `self.chunk_repair_sql` is built once at construction
                // from the same `BlobReferenceRegistry` as the page
                // query — detection and repair use identical formulas
                // by construction. See `chunk_repair_sql`.
                //
                // Attempt repair FIRST, then record the finding with
                // severity/kind reflecting the final state:
                //   * repair succeeded  → severity "info",  kind "refcount_repaired"
                //   * repair no-op      → severity "info",  kind "refcount_resolved"
                //   * repair failed     → severity "inconsistent", kind "refcount_mismatch"
                //   * no repair requested → severity "inconsistent", kind "refcount_mismatch"
                //
                // Parallels the WARN-then-INFO sequence in logs: an
                // unresolved drift raises attention ("inconsistent"),
                // a repaired one records the fix at info level without
                // inflating the "needs action" tally the outcome UI
                // shows. The detail JSON still carries `stored/actual/
                // delta/affected_files` so the audit trail is complete
                // either way.
                let (kind, severity) = if repair {
                    match sqlx::query(&self.chunk_repair_sql)
                        .bind(&row.hash)
                        .execute(self.pool.as_ref())
                        .await
                    {
                        Ok(res) if res.rows_affected() > 0 => {
                            repaired_count += 1;
                            tracing::info!(
                                target: "audit",
                                event = "blobs_consistency.repaired",
                                run_id = %store.run_id(),
                                hash = %row.hash,
                                stored_was = row.ref_count,
                                actual = row.actual_ref_count,
                                "🩹 blob ref_count repaired"
                            );
                            ("refcount_repaired", "info")
                        }
                        Ok(_) => {
                            // Row not touched — either another
                            // concurrent repair fixed it first, or
                            // drift healed between page fetch and
                            // UPDATE. Current state correct — info.
                            ("refcount_resolved", "info")
                        }
                        Err(e) => {
                            tracing::warn!(
                                target: "oxicloud::consistency",
                                event = "blobs_consistency.repair_failed",
                                run_id = %store.run_id(),
                                hash = %row.hash,
                                error = %e,
                                "blob ref_count repair UPDATE failed — finding stays"
                            );
                            ("refcount_mismatch", "inconsistent")
                        }
                    }
                } else {
                    ("refcount_mismatch", "inconsistent")
                };

                record_or_log(
                    store,
                    BLOBS_CONSISTENCY_JOB_NAME,
                    kind,
                    severity,
                    None, // hash isn't a UUID; resource identifier lives in detail
                    detail,
                )
                .await;
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
                    repaired_count = repaired_count,
                    repair_requested = repair,
                    "blobs_consistency completed with {} finding(s), {} repaired",
                    finding_count,
                    repaired_count
                );
                return RunOutcome::completed_with(serde_json::json!({
                    "repair_requested": repair,
                    "repaired_count":   repaired_count,
                }));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn default_registry() -> BlobReferenceRegistry {
        let pool = Arc::new(
            sqlx::pool::PoolOptions::<sqlx::Postgres>::new()
                .connect_lazy("postgres://invalid/invalid")
                .expect("lazy pool never connects"),
        );
        crate::infrastructure::repositories::pg::blob_reference_sources::built_in_registry(pool)
    }

    /// Golden test for the chunk-level recompute. Pins the statement
    /// byte-for-byte because it is assembled from the registry rather than
    /// written as a literal — the reviewer should read the SQL here.
    ///
    /// This expression must stay equal to what the query computed before the
    /// registry existed: the legacy-files term guarded by `NOT EXISTS`, plus
    /// the manifests-citing-this-chunk term. If a change makes those two
    /// overlap, every single-chunk CDC file is counted twice and the whole
    /// table reports `refcount_mismatch`.
    #[tokio::test]
    async fn chunk_page_statement_is_stable() {
        let sql = chunk_page_sql(&default_registry());
        let expected = r#"SELECT
     b.hash       AS hash,
     b.size       AS size,
     b.ref_count  AS ref_count,
     ((SELECT COUNT(*) FROM storage.files cnt_f
               WHERE cnt_f.blob_hash = b.hash
                 AND NOT EXISTS (
                     SELECT 1 FROM storage.chunk_manifests cnt_m
                      WHERE cnt_m.file_hash = cnt_f.blob_hash
                 ))
 + (SELECT COUNT(*) FROM storage.chunk_manifests cnt_m
                   WHERE b.hash = ANY(cnt_m.chunk_hashes)))::bigint AS actual_ref_count
   FROM storage.blobs b
  WHERE ($1::text IS NULL OR b.hash > $1)
  ORDER BY b.hash
  LIMIT $2"#;
        assert_eq!(sql, expected, "chunk page statement changed:\n{sql}");
    }

    /// With no chunk-level source every blob would look unreferenced and the
    /// sweep would report the entire table as `refcount_mismatch`. Refuse to
    /// build the statement instead.
    #[test]
    #[should_panic(expected = "no chunk-level blob reference source")]
    fn empty_registry_refuses_to_build_page_statement() {
        let _ = chunk_page_sql(&BlobReferenceRegistry::new());
    }
}
