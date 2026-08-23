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
//! * `blob_unreadable` (severity `data_loss`, deep mode only) —
//!   `blob_exists` returned true but the read pipeline errored (can't
//!   decrypt, network glitch, permission error, etc.). Distinct from
//!   `blob_corrupted` (which requires successful read + hash mismatch);
//!   here we can't get bytes out at all. Same operator impact — any
//!   file referencing this hash is inaccessible — but the remedy
//!   differs (key recovery, retry, or blob replacement, depending on
//!   the recorded `error` field).
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

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use sqlx::PgPool;

use crate::application::ports::blob_reference_ports::{BlobReferenceRegistry, RefLevel};
use crate::application::ports::blob_storage_ports::BlobStorageBackend;
use crate::common::config::NamedStorageEntry;
use crate::infrastructure::scheduler::{
    JobRegistry, JobRunArgs, JobStore, JobStoreProvider, RecoverableJobHandler, RunOutcome,
    RunStatus, record_or_log,
};
use crate::infrastructure::services::entry_backend::build_entry_backend;

pub const BLOBS_CONSISTENCY_JOB_NAME: &str = "blobs_consistency";

/// `params` JSONB key under which the entry name being probed is
/// stashed on a Fresh run (matches `TARGET_NAME_PARAM` on
/// `backend_migration`). Resumed runs re-read it so a paused audit
/// survives restart without the admin re-specifying the target.
pub const PROBED_STORAGE_PARAM: &str = "probed_storage";

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
    /// The default backend to probe when `args.storage` is `None` —
    /// the currently-active LIVE backend, injected at DI time. Runs
    /// with `?storage=<name>` build a fresh backend for the named
    /// entry instead (via [`build_entry_backend`]).
    backend: Arc<dyn BlobStorageBackend>,
    /// Snapshot of `AppConfig.storage_entries` used to resolve
    /// `args.storage` to a `NamedStorageEntry`. Empty for the
    /// legacy zero-entries path — `?storage=<name>` runs then
    /// fail-fast with a clear "no entries declared" message.
    storage_entries: Vec<NamedStorageEntry>,
    /// Ambient `AppConfig.storage_path` — used as the `root_dir`
    /// fallback for a Local target entry with no `_ROOT_DIR`. Same
    /// fallback rule the boot path uses.
    storage_path_fallback: PathBuf,
    /// The chunk-level page query, assembled once from the blob-reference
    /// registry so this recompute and `dedup_gc` agree on what "referenced"
    /// means. Built at construction rather than per page so the sweep runs a
    /// fixed statement — same reasoning as `DedupService::manifest_reap_sql`.
    /// See `docs/plan/derived-blobs.md`.
    chunk_page_sql: String,
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
     b.created_at AS created_at,
     ({expected})::bigint AS actual_ref_count
   FROM storage.blobs b
  WHERE ($1::text IS NULL OR b.hash > $1)
  ORDER BY b.hash
  LIMIT $2"
    )
}

impl BlobsConsistencyCheck {
    pub fn new(
        pool: Arc<PgPool>,
        backend: Arc<dyn BlobStorageBackend>,
        storage_entries: Vec<NamedStorageEntry>,
        storage_path_fallback: PathBuf,
        reference_registry: Arc<BlobReferenceRegistry>,
    ) -> Self {
        Self {
            pool,
            backend,
            storage_entries,
            storage_path_fallback,
            chunk_page_sql: chunk_page_sql(&reference_registry),
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
        // Resolve the backend to probe. Two paths, mirroring the
        // Fresh/Resumed split the backend_migration handler uses:
        //
        // * Fresh + args.storage=Some — probe that named entry
        //   instead of the live backend. Stamp probed_storage in
        //   params so a mid-audit restart resumes against the same
        //   entry without re-input.
        // * Fresh + args.storage=None — probe the live backend
        //   (today's default; audit of what the app is actually
        //   using).
        // * Resumed — read probed_storage from params; None means
        //   the original run was against the live backend.
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
                Some(entry) => build_entry_backend(entry, &self.storage_path_fallback),
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
                event = "blobs_consistency.probe_scoped",
                run_id = %store.run_id(),
                probed_storage = %name,
                "blobs_consistency probing entry `{name}` (via ?storage=<name>) instead of \
                 live backend"
            );
        }

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

        // Deep mode is a per-run flag with two consumers:
        //  1. This handler — decides whether to re-hash bytes.
        //  2. The admin panel — needs to display whether the run
        //     was deep so operators know what the scan actually
        //     verified.
        //
        // On a Fresh run we take it from `deep` (the trigger
        // endpoint stamps `?deep=true` onto the args) and stash it
        // in `params.deep` so:
        //   * Resume picks up the same mode (would previously become
        //     non-deep on Resume — a Paused deep scan silently lost
        //     its `deep` intent).
        //   * The admin panel run-detail view can render
        //     `params.deep = "true"` alongside `target_name`,
        //     `progress_kind`, etc.
        //
        // Persist BEFORE the walk so a mid-fresh-batch crash still
        // leaves a Paused row with the right mode marker.
        let deep = if is_fresh {
            let deep = args.deep;
            let v = if deep { "true" } else { "false" };
            if let Err(e) = store.set_string_param("deep", v).await {
                return RunOutcome::Failed {
                    message: format!("failed to persist deep flag to params: {e}"),
                };
            }
            deep
        } else {
            // Resumed run — read the persisted flag. Default to
            // false (fast mode) if the row is a pre-K3.5 Paused
            // scan without the param stashed.
            match store.get_string_param("deep").await {
                Ok(Some(v)) => v == "true",
                Ok(None) => false,
                Err(e) => {
                    return RunOutcome::Failed {
                        message: format!("read `deep` from params: {e}"),
                    };
                }
            }
        };

        if deep {
            tracing::info!(
                target: "oxicloud::consistency",
                event = "blobs_consistency.deep_mode_active",
                run_id = %store.run_id(),
                "deep mode: re-reading + re-hashing every blob (bit-rot detection)"
            );
        }

        // Repair mode: same shape as `deep` above so the admin run-
        // detail view can display `params.repair = "true"` alongside
        // `params.deep`. Fresh persists what the trigger asked for;
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
                    deep = deep,
                    "blobs_consistency completed with {} finding(s), {} repaired",
                    finding_count,
                    repaired_count
                );
                return RunOutcome::completed_with(serde_json::json!({
                    "repair_requested": repair,
                    "repaired_count":   repaired_count,
                }));
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

                    // Repair pass — content-safe corrective UPDATE. Sets
                    // `stored` to the value the auditor's two-term formula
                    // would compute at UPDATE time (subquery mirrors
                    // `chunk_page_sql`'s `actual_ref_count`), so a
                    // concurrent write between our page fetch and this
                    // UPDATE can't leave a stale value — the subquery
                    // re-reads inside the same statement. The
                    // `<> (subquery)` guard makes the UPDATE a no-op if
                    // the drift has healed, making this idempotent under
                    // retry.
                    if repair {
                        let expected = "( \
                            (SELECT COUNT(*) FROM storage.files f \
                              WHERE f.blob_hash = b.hash \
                                AND NOT EXISTS ( \
                                    SELECT 1 FROM storage.chunk_manifests m \
                                     WHERE m.file_hash = f.blob_hash \
                                )) \
                          + (SELECT COUNT(*) FROM storage.chunk_manifests m \
                              WHERE b.hash = ANY(m.chunk_hashes)) \
                        )";
                        let update_sql = format!(
                            "UPDATE storage.blobs b \
                                SET ref_count = {expected} \
                              WHERE b.hash = $1 \
                                AND b.ref_count <> {expected}",
                        );
                        match sqlx::query(&update_sql)
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
                            }
                            Ok(_) => {
                                // No row touched — concurrent repair or
                                // self-healing drift. Silent no-op.
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
                            }
                        }
                    }
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
                let exists = match backend.blob_exists(&row.hash).await {
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
                //
                // Finding fields:
                //   * `hash` — expected hash (the key the blob is
                //     indexed under in `storage.blobs`).
                //   * `computed_hash` — what BLAKE3 of the current
                //     bytes actually produces. Diagnostic: a
                //     one-bit flip vs a truncation vs a whole-file
                //     swap all leave distinctive signatures.
                //   `expected_hash` was NOT reused as a name to
                //   avoid mistaking it for "the hash we expect to
                //   see on disk (i.e. what will fix this)".
                if deep {
                    match recompute_hash(backend.as_ref(), &row.hash).await {
                        Ok(computed_hash) if computed_hash == row.hash => {}
                        Ok(computed_hash) => {
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
                                    "computed_hash":   computed_hash,
                                    "size":            row.size,
                                    "ref_count":       row.ref_count,
                                    "affected_files":  affected,
                                }),
                            )
                            .await;
                        }
                        Err(e) => {
                            // Blob can't be read at all — record as
                            // `blob_unreadable`. Distinct from
                            // `blob_corrupted` (hash mismatch = we
                            // can read but content differs): here
                            // we can't get bytes out to hash. Common
                            // causes: decrypt failure (missing key),
                            // network glitch on S3/Azure, missing
                            // file on Local, permission error.
                            //
                            // Recorded as `data_loss` because from
                            // the file's perspective the outcome is
                            // the same as corruption: content is
                            // inaccessible. Admins triage the error
                            // string to distinguish transient
                            // (retry-safe) from permanent (needs
                            // key recovery or blob replacement).
                            finding_count += 1;
                            let affected = affected_files(self.pool.as_ref(), &row.hash).await;
                            record_or_log(
                                store,
                                BLOBS_CONSISTENCY_JOB_NAME,
                                "blob_unreadable",
                                "data_loss",
                                None,
                                serde_json::json!({
                                    "hash":           row.hash,
                                    "size":           row.size,
                                    "ref_count":      row.ref_count,
                                    "affected_files": affected,
                                    "error":          e.to_string(),
                                }),
                            )
                            .await;
                            tracing::warn!(
                                target: "oxicloud::consistency",
                                event = "blobs_consistency.blob_unreadable",
                                run_id = %store.run_id(),
                                hash = %row.hash,
                                error = %e,
                                "🚨 blob unreadable in deep mode — recorded finding, continuing"
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
                    repaired_count = repaired_count,
                    repair_requested = repair,
                    deep = deep,
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
/// Deep-mode helper — read the blob from the backend and recompute
/// its BLAKE3 hash. Returns the recomputed hex string; callers
/// compare against the expected hash themselves. Returning the
/// actual hash (not just a bool) lets the finding surface WHAT the
/// bytes now hash to, which is diagnostic gold: a specific one-bit
/// flip has a very different signature from a chunk-boundary
/// corruption or a truncated read. `Err(_)` on backend-side error
/// (network blip, permission issue) — callers log-and-skip since
/// transient failure isn't a corruption signal.
async fn recompute_hash(
    backend: &dyn BlobStorageBackend,
    expected_hash: &str,
) -> Result<String, crate::common::errors::DomainError> {
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

    Ok(hasher.finalize().to_hex().to_string())
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
     b.created_at AS created_at,
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
