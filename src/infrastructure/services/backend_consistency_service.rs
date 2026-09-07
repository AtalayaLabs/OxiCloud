//! Fifth tenant of Part 2 (recoverable-run engine).
//!
//! **Merge-joins** the backend's blob enumeration against
//! `storage.blobs`, both ordered by hash, so a single pass yields the
//! delta in *both* directions rather than one.
//!
//! It previously walked the backend and probed the DB with
//! `WHERE hash = ANY($1)` over each page, which could only ever see
//! backend-only entries: a row whose bytes are gone never appears in a
//! backend listing, so it was invisible here by construction. That half
//! was left to `blobs_consistency`'s per-row HEAD probe, which does not
//! survive the row counts this plan produces — see
//! `docs/plan/derived-blobs.md`. That probe is now gone: this tenant
//! owns every backend-side check, and `blobs_consistency` is DB-only.
//!
//! ### Per-row checks
//!
//! * `orphan_blob` (severity `inconsistent`) — bytes on disk / S3 /
//!   Azure with no registry row. Not data-loss (nothing broken —
//!   just storage overhead), but points at dedup_gc or
//!   ingest-path drift. Recovery = register-registry-row (if the
//!   bytes are still needed) OR delete the file (if truly orphan).
//! * `blob_missing_from_backend` (severity `data_loss`) — a registry
//!   row whose bytes are absent. The opposite direction and the more
//!   serious one: an orphan wastes space, this loses a file.
//! * `blob_corrupted` (severity `data_loss`, `?deep=true` only) —
//!   the key exists on both sides but the bytes behind it no longer
//!   hash to it. Silent bit-rot.
//! * `blob_unreadable` (severity `data_loss`, `?deep=true` only) —
//!   the key exists but the bytes cannot be read at all: decrypt
//!   failure (missing key), transport error, permissions. Same impact
//!   as corruption from a file's point of view, different remedy,
//!   hence a separate kind. Triage on the recorded `error`.
//!
//! ### Deep mode
//!
//! The last two moved here from `blobs_consistency`, which used to
//! carry a backend solely for them. Re-hashing is backend work end to
//! end — the only DB input is the hash — and this walk already holds
//! the matched key pairs, which is exactly the set worth reading. It
//! costs a full read of every blob, so it is opt-in.
//!
//! ### Why the two orderings agree
//!
//! The merge-join's premise is that the backend's byte order and the
//! database's `ORDER BY hash` rank identically. They do, because hashes
//! are lowercase BLAKE3 hex of fixed length: over `[0-9a-f]` digits
//! precede letters in both, and there is no case to fold. A hash column
//! that ever admitted uppercase or variable length would break this
//! silently and in both directions at once.
//!
//! ### When enumeration fails
//!
//! **The run fails.** There is no degraded mode.
//!
//! There used to be: an error on the first `list_blob_hashes` call
//! emitted a `backend_unenumerable` anomaly and fell back to
//! `probe_each_row`, one `blob_exists` per `storage.blobs` row. That
//! recovered `blob_missing_from_backend` (the direction that loses
//! FILES) but never `orphan_blob`, since bytes no row claims are
//! invisible to anything starting from the database.
//!
//! It was written for two cases, and neither exists:
//!
//! * **Azure** — enumerates since the 256-way shard walk (see
//!   `AzureBlobBackend::list_blob_hashes` for why it needs one to do
//!   what S3 gets from `StartAfter`).
//! * **Mid-migration** — never applied. That justification named a
//!   `MigrationBlobBackend` that does not exist;
//!   `SwappableBlobBackend::list_blob_hashes` forwards to whatever is
//!   currently active, as do the Encrypted, Cached and Retry wrappers.
//!   Do not reintroduce the claim without grepping for the impl.
//!
//! So the only thing still reaching it was a *transient* failure — auth
//! blip, throttle, network — being relabelled as a capability limit and
//! silently costing orphan coverage. A failed run is louder than an
//! anomaly on an otherwise-clean-looking scan, which was the fallback's
//! own stated goal.
//!
//! The trait default still returns `operation_not_supported`, so a
//! future write-only or read-only-mirror backend would fail every run
//! here. **That is when the fallback should come back — with tests.**
//! It had none, which is the other half of why it went.
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

use std::sync::Arc;

use async_trait::async_trait;
use chrono::{Duration, Utc};
use sqlx::PgPool;

use crate::application::ports::blob_storage_ports::BlobStorageBackend;
use crate::infrastructure::scheduler::{
    JobRegistry, JobRunArgs, JobStore, JobStoreProvider, ProgressKind, RecoverableJobHandler,
    RunOutcome, RunStatus, record_or_log,
};
use crate::infrastructure::services::blob_diagnostics::affected_files;

pub const BACKEND_CONSISTENCY_JOB_NAME: &str = "backend_consistency";

/// `params` JSONB key under which the entry name being enumerated is
/// stashed on a Fresh run (matches `TARGET_NAME_PARAM` on
/// `backend_migration`). Resumed runs re-read it so a paused audit
/// survives restart without the admin re-specifying the target.
///
/// Defined here rather than in `blobs_consistency`, which no longer
/// touches a backend and so has no entry to scope.
pub const PROBED_STORAGE_PARAM: &str = "probed_storage";

/// Batch size for backend enumeration + DB probe. 500 amortises the DB
/// round-trip; larger batches on S3 hit ListObjectsV2's per-request
/// limit (1000) with wasted rows filtered client-side, and smaller ones
/// over-poll the DB.
///
/// **This is not the cancel cadence.** It used to be: a shallow batch is
/// one backend list + one DB probe + a Rust set-difference, so polling
/// once per batch kept cancel sub-second. Deep mode then moved into this
/// tenant and added 500 full blob reads per batch — measured at 155 ms
/// each against remote S3, so ~63 s per batch — and a cancel poll that
/// only ran between batches left Pause/Cancel unresponsive for a minute
/// on exactly the run an operator most wants to stop.
///
/// [`DEEP_CANCEL_POLL_EVERY`] decouples the two: cancellation is now
/// checked inside the verify loop, so this constant went back to being
/// purely about I/O batching.
const BATCH_SIZE: usize = 500;

/// How many deep verifications to run between cancel polls.
///
/// A poll is one small indexed DB read (~0.1 ms) against a blob read
/// measured at 155 ms on remote storage, so polling every blob would
/// cost well under 1%. 16 keeps even a local-backend deep run — where a
/// verify is ~0.8 ms and the ratio is far less favourable — under a
/// couple of percent, while capping cancel latency at well under a
/// second on any backend.
///
/// Shallow batches do not need this: they are already fast enough that
/// the per-batch poll bounds latency on its own.
const DEEP_CANCEL_POLL_EVERY: u64 = 16;

/// Grace window — orphans younger than this are skipped, since the
/// write path is durability-before-visibility: bytes hit disk before
/// the `storage.blobs` row is inserted. A scan catching a blob
/// mid-write would false-positive it as orphan. Matches
/// `blobs_consistency` + `dedup_gc`.
const CREATE_GRACE: Duration = Duration::hours(1);

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

    fn description(&self) -> &'static str {
        "Merge-joins the storage backend's blob enumeration against \
         storage.blobs, both ordered by hash, so one pass yields the delta \
         in both directions: bytes on the backend no DB row claims, and \
         rows whose bytes are gone. If the backend cannot be enumerated \
         the run fails rather than reporting partial coverage. \
         Add ?deep=true to also read every matched blob back and re-hash \
         it, catching silent bit-rot — that is a full read of storage and \
         can take hours. Read-only in every mode: nothing is uploaded or \
         deleted."
    }

    fn parameters(&self) -> &'static [crate::infrastructure::scheduler::JobParam] {
        use crate::infrastructure::scheduler::JobParam;
        const PARAMS: &[JobParam] = &[
            JobParam::boolean(
                "deep",
                false,
                "Read every matched blob back and re-hash it, catching \
                 silent bit-rot. A full read of storage — can take hours.",
            ),
            JobParam::string(
                "storage",
                "Name of the storage entry to audit. Absent audits the \
                 active backend; naming an entry is how either side of a \
                 migration gets audited directly.",
            ),
        ];
        PARAMS
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
        // `run_or_resume` persists and restores `storage` for us now, so
        // the normal path is a plain read.
        //
        // The fallback is a MIGRATION concern, not defensiveness. This job
        // used to persist the same value under its own
        // `probed_storage` key; a run paused before this change has that
        // key and no `storage` one. Without the fallback such a run would
        // resume against the ACTIVE backend instead of the entry it was
        // auditing — silently auditing the wrong thing, which is worse
        // than failing. Removable once no pre-upgrade paused runs remain.
        let probed_storage: Option<String> = match args.get_str("storage") {
            Some(name) => Some(name.to_string()),
            None if resume_cursor.is_some() => {
                match store.get_string_param(PROBED_STORAGE_PARAM).await {
                    Ok(legacy) => {
                        if legacy.is_some() {
                            tracing::info!(
                                target: "oxicloud::consistency",
                                event = "backend_consistency.legacy_storage_param",
                                run_id = %store.run_id(),
                                "resumed a run that recorded its target under the pre-declaration \
                                 `probed_storage` key"
                            );
                        }
                        legacy
                    }
                    Err(e) => {
                        return RunOutcome::Failed {
                            message: format!("read {PROBED_STORAGE_PARAM} from params: {e}"),
                        };
                    }
                }
            }
            None => None,
        };
        // The entry name this run audits, recorded in the outcome so a
        // finished run says WHAT it checked.
        //
        // Without it a completed run is silent about its target: findings
        // carry `backend`, but a clean run has none, so after switching
        // the active backend there is no way to tell which storage a
        // previous green run actually verified. Ed hit exactly that —
        // a 1.5s local sweep read as an S3 audit.
        //
        // Read from `admin_settings` rather than a boot-time snapshot,
        // because a migration cutover rewrites it while the process
        // lives; a cached copy would name the pre-cutover entry. Best
        // effort: this is a label, and failing an audit over it would be
        // the wrong trade.
        let audited_entry: Option<String> = match &probed_storage {
            Some(name) => Some(name.clone()),
            None => {
                match crate::infrastructure::services::entry_backend::resolve_active_entry(
                    self.pool.as_ref(),
                    &self.storage_entries,
                )
                .await
                {
                    Ok(crate::infrastructure::services::entry_backend::ActiveEntry::Explicit(
                        e,
                    )) => Some(e.name.clone()),
                    // Unset means the boot fallback picked the first
                    // entry; naming it would be a guess, and a wrong
                    // label is worse than an absent one.
                    _ => None,
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

        // Deep mode — read every matched blob back and re-hash it, rather
        // than trusting that a key present on both sides means the bytes
        // behind it are still the bytes that key names.
        //
        // It lives here rather than in `blobs_consistency` because it is
        // a backend operation end to end: the only DB input is the hash,
        // which this merge-join already holds. Keeping it there forced
        // that tenant to carry a backend for one flag, which is the
        // overlap this split removes.
        //
        // Persisted to `params.deep` and restored on resume by
        // `run_or_resume`, so a Paused deep scan does not silently
        // continue shallow and the run-detail view can show what the scan
        // actually verified.
        let deep = args.get_bool("deep");

        // Verify through storage, never through a read-through cache.
        //
        // A cache answers from its own copy, so re-hashing through one
        // checks the CACHE: rot on the remote is masked by a good cached
        // copy, and rot in the cache is recorded as `blob_corrupted`
        // against a healthy remote — sending an operator to the wrong
        // layer. The finding names `backend.backend_type()`, so that
        // attribution has to be true.
        //
        // Only the cache is peeled; the decryptor stays, because the
        // cache holds plaintext and the content hash is over plaintext.
        // `?storage=<entry>` already builds an uncached stack, so this
        // only changes the live-backend path — which is the one that was
        // silently fast.
        let verify_backend = backend.uncached().unwrap_or_else(|| backend.clone());
        // Counter, not just a flag: a deep run that verified nothing and
        // a deep run that verified everything are otherwise
        // indistinguishable in the outcome, which is exactly the
        // ambiguity that made a 1.5s "deep" sweep over 2022 chunks look
        // plausible.
        let mut verified_count = 0u64;
        if deep {
            tracing::info!(
                target: "oxicloud::consistency",
                event = "backend_consistency.deep_mode_active",
                run_id = %store.run_id(),
                cached_read_bypassed = backend.uncached().is_some(),
                "deep mode: re-reading + re-hashing every matched blob (bit-rot detection)"
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
                    // Fail loudly, first batch or not.
                    //
                    // A first-batch failure used to degrade to
                    // `probe_each_row` instead. That was written for
                    // backends which genuinely cannot enumerate, and none
                    // ship today — see the module docs for why the two it
                    // named do not apply. What was left reaching it was a
                    // transient error relabelled as a capability limit, on
                    // a run that then looked clean while having lost orphan
                    // coverage entirely.
                    //
                    // Whether the enumeration died on page 1 or page 900,
                    // the audit did not complete, and the operator needs to
                    // know that rather than read a green run.
                    return RunOutcome::Failed {
                        message: format!(
                            "backend enumeration failed on {}: {e}",
                            backend.backend_type()
                        ),
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
                    verified = verified_count,
                    "backend_consistency completed with {} finding(s)",
                    finding_count
                );
                return RunOutcome::completed_with(serde_json::json!({
                    "deep":          deep,
                    "verified":      verified_count,
                    "backend":       backend.backend_type(),
                    "storage_entry": audited_entry,
                    "scoped":        probed_storage.is_some(),
                }));
            }

            // ── Merge-join, not a one-sided probe ────────────────
            //
            // Both sides are ordered by hash ascending — the backend by
            // contract (`BlobStorageBackend::list_blob_hashes`), the DB by
            // `ORDER BY hash` — so one pass yields BOTH deltas instead of
            // one:
            //
            //   * present on the backend, absent from the DB → `orphan_blob`
            //   * present in the DB, absent from the backend →
            //     `blob_missing_from_backend` (data loss, not overhead)
            //
            // The old form probed `WHERE hash = ANY($1)` over the backend
            // page, so it could only ever see the first kind: a row whose
            // bytes are gone never appears in a backend listing and was
            // invisible here by construction.
            //
            // Ordering is the whole premise, so it is worth being explicit
            // about why the two agree. Hashes are lowercase BLAKE3 hex of
            // fixed length, and over `[0-9a-f]` the database collation and
            // byte order rank identically (digits before letters in both,
            // no case folding to disagree about). A hash column that ever
            // admitted uppercase or variable length would break this
            // silently, in both directions.
            let db_hashes: Vec<String> = match sqlx::query_as::<_, (String,)>(
                r#"SELECT hash FROM storage.blobs
                    WHERE ($1::text IS NULL OR hash > $1)
                    ORDER BY hash
                    LIMIT $2"#,
            )
            .bind(cursor.as_deref())
            .bind(BATCH_SIZE as i64)
            .fetch_all(self.pool.as_ref())
            .await
            {
                Ok(rows) => rows.into_iter().map(|(h,)| h).collect(),
                Err(e) => {
                    return RunOutcome::Failed {
                        message: format!("db page: {e}"),
                    };
                }
            };

            // The two pages cover different ranges, so only the overlap can
            // be judged. Beyond `horizon` a hash missing from one side may
            // simply be on the next page of the other, and emitting there
            // would invent findings in both directions. When a side is
            // exhausted its entries cannot be "on a later page", so the
            // other side's tail becomes judgeable.
            let backend_last = page.blobs.last().map(|e| e.hash.as_str());
            let db_last = db_hashes.last().map(|s| s.as_str());
            let backend_done = page.next_cursor.is_none();
            let db_done = db_hashes.len() < BATCH_SIZE;

            let horizon: Option<&str> = match (backend_last, db_last) {
                _ if backend_done && db_done => None, // judge everything
                (Some(b), Some(d)) if backend_done => Some(b.max(d)),
                (Some(b), Some(d)) if db_done => Some(b.max(d)),
                (Some(b), Some(d)) => Some(b.min(d)),
                (Some(b), None) => Some(b),
                (None, Some(d)) => Some(d),
                (None, None) => None,
            };
            let in_range = |h: &str| horizon.is_none_or(|limit| h <= limit);

            let mut bi = page.blobs.iter().peekable();
            let mut di = db_hashes.iter().peekable();
            loop {
                match (bi.peek(), di.peek()) {
                    // Present on both sides. Shallow: nothing to say — the
                    // key exists where the registry claims. Deep: the key
                    // matching says nothing about the bytes behind it, so
                    // read them back and re-hash.
                    //
                    // Guarded by `in_range` so a pair past the horizon is
                    // not read twice — the cursor stops at the horizon, so
                    // that pair comes round again next batch and is
                    // verified then.
                    (Some(b), Some(d)) if b.hash == **d => {
                        if deep && in_range(&b.hash) {
                            // Cancel poll INSIDE the verify loop. The
                            // per-batch poll above bounds latency by one
                            // batch, which is ~63 s of remote reads in
                            // deep mode — a minute of an apparently
                            // ignored Cancel on a run that may last
                            // hours. Pausing here is safe and cheap: the
                            // cursor still points at the last completed
                            // batch, so resume re-verifies this batch's
                            // handful of blobs rather than skipping them.
                            if verified_count.is_multiple_of(DEEP_CANCEL_POLL_EVERY)
                                && matches!(store.status().await, Ok(RunStatus::CancelRequested))
                            {
                                tracing::info!(
                                    target: "oxicloud::consistency",
                                    event = "backend_consistency.cancelled_mid_verify",
                                    run_id = %store.run_id(),
                                    verified = verified_count,
                                    "deep verify cancelled mid-batch, pausing at the last checkpoint"
                                );
                                return RunOutcome::Paused {
                                    cursor: cursor
                                        .as_ref()
                                        .map(|s| s.as_bytes().to_vec())
                                        .unwrap_or_default(),
                                };
                            }
                            verified_count += 1;
                            finding_count += self
                                .verify_bytes(store, verify_backend.as_ref(), &b.hash)
                                .await;
                        }
                        bi.next();
                        di.next();
                    }
                    // Backend-only: bytes with no registry row.
                    (Some(b), d_opt)
                        if d_opt.is_none_or(|d| b.hash.as_str() < d.as_str())
                            && in_range(&b.hash) =>
                    {
                        // Grace window: the write path is
                        // durability-before-visibility, so bytes exist
                        // briefly before their row does. Without this every
                        // in-flight upload reads as an orphan.
                        if !matches!(b.mtime, Some(m) if m > grace_cutoff) {
                            finding_count += 1;
                            record_or_log(
                                store,
                                BACKEND_CONSISTENCY_JOB_NAME,
                                "orphan_blob",
                                "inconsistent",
                                None,
                                serde_json::json!({
                                    "hash":    b.hash,
                                    "mtime":   b.mtime.map(|t| t.to_rfc3339()),
                                    "backend": backend.backend_type(),
                                }),
                            )
                            .await;
                        }
                        bi.next();
                    }
                    // DB-only: a row whose bytes are gone. Severity is
                    // `data_loss`, not `inconsistent` — an orphan wastes
                    // space, this loses a file.
                    (b_opt, Some(d))
                        if b_opt.is_none_or(|b| d.as_str() < b.hash.as_str()) && in_range(d) =>
                    {
                        finding_count += 1;
                        record_or_log(
                            store,
                            BACKEND_CONSISTENCY_JOB_NAME,
                            "blob_missing_from_backend",
                            "data_loss",
                            None,
                            serde_json::json!({
                                "hash":    d,
                                "backend": backend.backend_type(),
                                "note":    "registry row with no bytes on the backend",
                            }),
                        )
                        .await;
                        di.next();
                    }
                    // Past the horizon on both sides, or both exhausted.
                    _ => break,
                }
            }

            // Advance to the horizon, not the backend's own cursor.
            //
            // One hash serves both sides: they share an ordering, so "resume
            // after H" means `start_after(H)` on the backend and
            // `WHERE hash > H` in the DB. Advancing past the horizon would
            // skip the un-judged tail of whichever side reached further.
            //
            // Scanned count covers blobs and unknowns, since both were
            // walked.
            let batch_len = (page.blobs.len() + page.unknowns.len()) as u64;
            let exhausted = backend_done && db_done;
            cursor = if exhausted {
                None
            } else {
                horizon.map(|h| h.to_string())
            };

            // Neither side exhausted yet no horizon means neither returned a
            // row — nothing left to compare, and continuing would spin on the
            // same empty pages forever.
            if cursor.is_none() && !exhausted && horizon.is_none() {
                tracing::debug!(
                    target: "oxicloud::consistency",
                    event = "backend_consistency.no_horizon",
                    run_id = %store.run_id(),
                    "both sides returned no rows before exhaustion; ending the sweep"
                );
            }

            let cursor_bytes = cursor
                .as_ref()
                .map(|s| s.as_bytes().to_vec())
                .unwrap_or_default();
            if let Err(e) = store.checkpoint(cursor_bytes, batch_len).await {
                return RunOutcome::Failed {
                    message: format!("checkpoint: {e}"),
                };
            }

            // Both sides drained → the sweep is complete.
            if cursor.is_none() {
                tracing::info!(
                    target: "oxicloud::consistency",
                    event = "backend_consistency.completed",
                    run_id = %store.run_id(),
                    finding_count = finding_count,
                    verified = verified_count,
                    "backend_consistency completed with {} finding(s)",
                    finding_count
                );
                // `verified` is reported on EVERY run, zero included:
                // absent-vs-zero is exactly the distinction an operator
                // needs, and omitting it on a shallow run would make
                // "deep verified nothing" look like "this was shallow".
                return RunOutcome::completed_with(serde_json::json!({
                    "deep":          deep,
                    "verified":      verified_count,
                    "backend":       backend.backend_type(),
                    "storage_entry": audited_entry,
                    "scoped":        probed_storage.is_some(),
                }));
            }
        }
    }
}

impl BackendConsistencyCheck {
    /// Deep-mode per-blob verification. Reads the blob back, re-hashes it,
    /// and records what it finds. Returns the number of findings recorded
    /// (0 or 1) so the caller's counter stays the single tally.
    ///
    /// Moved here from `blobs_consistency` along with the rest of the
    /// backend-touching work: the merge-join already holds a verified
    /// key pair, which is exactly the set worth reading.
    async fn verify_bytes(
        &self,
        store: &dyn JobStore,
        backend: &dyn BlobStorageBackend,
        hash: &str,
    ) -> u64 {
        match recompute_hash(backend, hash).await {
            // The bytes still hash to the key they are filed under.
            Ok(computed) if computed == hash => 0,
            // Silent bit-rot. `computed_hash` is reported rather than a
            // bare "mismatch" because the value is diagnostic: a one-bit
            // flip, a truncation and a whole-object swap leave distinct
            // signatures.
            Ok(computed) => {
                let affected = affected_files(self.pool.as_ref(), hash).await;
                record_or_log(
                    store,
                    BACKEND_CONSISTENCY_JOB_NAME,
                    "blob_corrupted",
                    "data_loss",
                    None,
                    serde_json::json!({
                        "hash":           hash,
                        "computed_hash":  computed,
                        "backend":        backend.backend_type(),
                        "affected_files": affected,
                    }),
                )
                .await;
                1
            }
            // Bytes are there by key but cannot be read at all: decrypt
            // failure (missing key), transport error, permissions. Same
            // impact as corruption from a file's point of view — the
            // content is inaccessible — but a different remedy, which is
            // why it is a separate kind rather than folded into
            // `blob_corrupted`. Operators triage on `error`.
            Err(e) => {
                let affected = affected_files(self.pool.as_ref(), hash).await;
                record_or_log(
                    store,
                    BACKEND_CONSISTENCY_JOB_NAME,
                    "blob_unreadable",
                    "data_loss",
                    None,
                    serde_json::json!({
                        "hash":           hash,
                        "backend":        backend.backend_type(),
                        "affected_files": affected,
                        "error":          e.to_string(),
                    }),
                )
                .await;
                tracing::warn!(
                    target: "oxicloud::consistency",
                    event = "backend_consistency.blob_unreadable",
                    run_id = %store.run_id(),
                    hash = %hash,
                    error = %e,
                    "🚨 blob unreadable in deep mode — recorded finding, continuing"
                );
                1
            }
        }
    }
}

/// Deep-mode helper — read the blob from the backend and recompute its
/// BLAKE3 hash. Returns the recomputed hex string; callers compare it
/// against the expected hash themselves. Returning the actual hash (not
/// a bool) lets the finding surface WHAT the bytes now hash to, which is
/// diagnostic gold: a one-bit flip has a very different signature from a
/// chunk-boundary corruption or a truncated read. `Err(_)` on any
/// backend-side error — the caller records that as `blob_unreadable`
/// rather than as corruption.
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
            DomainError::internal_error("BackendConsistency", format!("stream read: {e}"))
        })?;
        hasher.update(&bytes);
    }

    Ok(hasher.finalize().to_hex().to_string())
}
