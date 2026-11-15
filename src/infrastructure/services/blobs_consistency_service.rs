//! Fourth tenant of Part 2 (recoverable-run engine).
//!
//! Iterates `storage.blobs` — the content-addressable registry — and
//! checks the reference-counting invariants `dedup_gc` relies on.
//!
//! **Database only.** It opens no backend and makes no network call;
//! `?storage=<name>` and `?deep=true` are both inert here.
//!
//! Two per-row checks share the same walk — one page fetch already
//! has every column both need:
//!
//! * `refcount_mismatch` (severity `inconsistent`, repairable) —
//!   `storage.blobs.ref_count` disagrees with the actual reference
//!   count computed from `storage.files.blob_hash` +
//!   `storage.chunk_manifests.chunk_hashes[]`. Under-count means
//!   dedup GC could prematurely reap a live blob; over-count means
//!   a blob is being pinned longer than needed. Content-safe either
//!   way (the storage.blobs row is fine, the counter is wrong).
//!
//! * `blob_orphan_stalled` (severity `anomaly`, discovery-only) —
//!   the row satisfies every reap predicate `dedup_gc` uses
//!   (`ref_count <= 0`, no chunk-level referrer) AND has been sitting
//!   past a margin derived from how often GC actually runs — see
//!   [`StallGrace`], which reads the live schedule rather than trusting
//!   a constant. Signal that the
//!   GC pipeline itself is stuck — the job stopped running, is
//!   failing on the same hash every tick, or a ghost row keeps
//!   pinning the same set. No `?repair=true` path: per
//!   [[feedback_no_silent_auto_repair]], papering over the symptom
//!   here would hide the root cause (a wedged worker, a hanging
//!   backend delete, a ghost referrer being recreated) — the
//!   operator diagnoses first, then runs
//!   `POST /api/admin/jobs/dedup_gc/trigger?force=true` themselves.
//!   The two checks are orthogonal in-loop: a row with drift is NOT
//!   also flagged as stalled — the drift IS why the GC hasn't taken
//!   it, so fixing the counter is the whole story.
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

use std::sync::{Arc, Weak};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
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

/// The `blob_orphan_stalled` threshold, derived per run from the cadence
/// of whatever actually drives garbage collection on this instance.
///
/// This used to be a constant, `GC_ORPHAN_GRACE_SECS × 24` (= 24h), and
/// it was coupled to the wrong quantity. `GC_ORPHAN_GRACE_SECS` is how
/// long a blob must sit before GC will *consider* it; it says nothing
/// about how often GC *runs*. The two numbers happened to multiply out
/// near a day, which hid the mistake.
///
/// The arithmetic that matters:
///
/// * a blob becomes GC-eligible `GC_ORPHAN_GRACE_SECS` after it is
///   orphaned (1h), and
/// * the next GC-bearing sweep lands up to one full driver interval
///   later (24h by default),
///
/// so the worst-case legitimate latency is ~25h — *above* the old 24h
/// threshold. A blob orphaned shortly after a sweep was therefore
/// guaranteed to be reported as stalled before it could possibly have
/// been collected. Structural false positives, not bad luck.
///
/// Which job is the driver is not obvious. `dedup_gc` registers itself
/// with `interval = None` **on purpose** — the periodic role belongs to
/// `trash_cleanup`, which ends each sweep with an inline
/// `garbage_collect()` (and `clear_trash_in` does the same on the
/// interactive path). So reading `dedup_gc`'s own schedule and concluding
/// "never scheduled, nothing will reap this" would be wrong on a
/// perfectly healthy instance. We take the soonest of the two: whichever
/// reaps first bounds the wait.
///
/// `backend_reclaim` is deliberately NOT in this sum, though it is the
/// other half of the deletion pipeline. `dedup_gc` enqueues each reaped
/// hash into `storage.pending_actions` **in the same transaction as the
/// row delete** (`blob_handler.rs`, "Division of labour"), and this check
/// fires on a surviving *row*. Reclaim latency governs stranded backend
/// *objects* — a different finding with a different owner. Adding it here
/// would inflate the threshold for a stage that cannot hold the row.
struct StallGrace {
    /// The threshold itself.
    secs: i64,
    /// Soonest driver interval found, for the finding payload. `None` =
    /// nothing scheduled drives GC on this instance.
    driver_secs: Option<i64>,
    /// Which job supplied `driver_secs`, for the remediation hint.
    driver: Option<&'static str>,
}

impl StallGrace {
    /// Jobs that end up invoking `garbage_collect()`. Order is
    /// irrelevant — the soonest wins.
    const GC_DRIVERS: [&'static str; 2] = [
        crate::infrastructure::services::trash_cleanup_service::TrashCleanupService::JOB_NAME,
        crate::infrastructure::services::blob_handler::DEDUP_GC_JOB_NAME,
    ];

    /// Eligibility delay before GC will even look at an orphan. The
    /// other half of the threshold, and already a constant maintained
    /// next to the sweep that honours it — not restated here.
    const ELIGIBILITY_SECS: i64 =
        crate::infrastructure::services::blob_handler::BlobHandler::GC_ORPHAN_GRACE_SECS;

    /// Read the live schedule and derive the threshold. Called once per
    /// run, not per row.
    ///
    /// With a driver: `eligibility + 2 x interval`. The first interval is
    /// the wait for the next sweep; the second is a sampling bound, not a
    /// comfort margin. This check observes a periodic process, so by
    /// Nyquist–Shannon it cannot distinguish "the sweep has not come round
    /// yet" from "the sweep is not happening" over anything less than two
    /// periods. At `1 x` the threshold sits exactly on the worst case and
    /// trips on sweep duration or a single deferred tick — reporting a
    /// phase difference as a fault.
    ///
    /// Without one (nothing scheduled drives GC, or the registry is gone
    /// because we are shutting down): just the eligibility delay. Nothing
    /// is going to reap the row, so the finding is correct however long
    /// we wait, and there is no cadence to derive a margin from.
    ///
    /// Both arms are built from values that already exist — the live
    /// interval and `GC_ORPHAN_GRACE_SECS` — so this adds no tunable of
    /// its own, and no second knob for an operator to keep in sync.
    async fn resolve(registry: &Weak<JobRegistry>) -> Self {
        let Some(registry) = registry.upgrade() else {
            return Self {
                secs: Self::ELIGIBILITY_SECS,
                driver_secs: None,
                driver: None,
            };
        };

        let mut soonest: Option<(&'static str, i64)> = None;
        for name in Self::GC_DRIVERS {
            // `Some(None)` = registered, on-demand only → contributes no
            // cadence. `None` = not registered on this build.
            if let Some(Some(interval)) = registry.schedule_of(name).await {
                let secs = interval.as_secs() as i64;
                if soonest.is_none_or(|(_, best)| secs < best) {
                    soonest = Some((name, secs));
                }
            }
        }

        match soonest {
            // One interval to reach the next sweep, one more because two
            // periods is the minimum needed to tell a missed sweep from
            // an unlucky phase (see the doc above).
            Some((driver, driver_secs)) => Self {
                secs: Self::ELIGIBILITY_SECS + driver_secs * 2,
                driver_secs: Some(driver_secs),
                driver: Some(driver),
            },
            None => Self {
                secs: Self::ELIGIBILITY_SECS,
                driver_secs: None,
                driver: None,
            },
        }
    }

    /// Remediation text for a row that is past the threshold. Names the
    /// cause we can actually infer, rather than sending the operator to
    /// audit dedup_gc's worker health in every case.
    fn remediation_hint(&self) -> String {
        match (self.driver, self.driver_secs) {
            // A driver is scheduled, yet the row outlived two of its
            // cycles. Either the driver is not firing (the common cause
            // on an instance that restarts or suspends — `next_run_at` is
            // computed as `now + interval` at registration, so a 24h
            // timer never completes on a host that reboots or sleeps more
            // often than daily) or the reap itself is wedged.
            (Some(driver), Some(secs)) => format!(
                "`{driver}` drives GC every {secs}s and this row outlived \
                 two of its cycles. Check GET /api/admin/jobs: if \
                 `{driver}` shows no recent run, the schedule is not firing — its \
                 next_run_at is set to now+interval at startup, so a long interval \
                 never elapses on a host that restarts or suspends more often than \
                 that. If it IS running, look for a wedged reap (advisory-lock \
                 contention, ghost chunk_manifests/storage.files row). Either way, \
                 POST /api/admin/jobs/dedup_gc/trigger?force=true drains the backlog now."
            ),
            // Nothing scheduled drives GC: orphans accumulate until
            // someone deletes from trash or triggers by hand. This is a
            // configuration state, not a stalled pipeline.
            _ => format!(
                "No scheduled job drives garbage collection on this instance, so this \
                 row will not be reaped automatically. Add a GC driver to \
                 OXICLOUD_JOBS_SCHEDULED (e.g. `dedup_gc=1h`, or ensure \
                 `{}` is scheduled), then \
                 POST /api/admin/jobs/dedup_gc/trigger?force=true to drain the backlog.",
                crate::infrastructure::services::trash_cleanup_service::TrashCleanupService::JOB_NAME,
            ),
        }
    }
}

/// Orphans found still INSIDE the grace window, accumulated across the
/// sweep and reported as one finding at the end.
///
/// Aggregated deliberately. A busy install can orphan thousands of blobs
/// inside a 49h window, and a finding per row would bury the `anomaly`
/// rows that actually need attention — the same "a gate that always
/// prints is a gate nobody reads" problem. The sample is capped so the
/// payload stays bounded whatever the count.
#[derive(Default)]
struct PendingGc {
    count: u64,
    oldest_secs: i64,
    /// Hashes carried in the finding. Enough to start investigating,
    /// bounded enough that a million-orphan install still writes a small
    /// row.
    sample: Vec<String>,
}

impl PendingGc {
    const SAMPLE_MAX: usize = 10;

    fn record(&mut self, hash: &str, waited_secs: i64) {
        self.count += 1;
        self.oldest_secs = self.oldest_secs.max(waited_secs);
        if self.sample.len() < Self::SAMPLE_MAX {
            self.sample.push(hash.to_string());
        }
    }

    fn sample_truncated(&self) -> bool {
        self.count as usize > self.sample.len()
    }
}

/// The sweep's single completion path: emit the pending-GC aggregate,
/// log, and build the outcome.
///
/// It exists because the scan can finish in TWO places — an empty page
/// (total rows an exact multiple of [`BATCH_SIZE`]) and a short final
/// page (every other time) — and they must report identically. They did
/// not. The short-page path omitted `stalled_count` from both its log
/// line and its outcome, and never emitted the pending-GC aggregate at
/// all, so on a 3452-row instance (17 × 200 + 52) the orphan summary
/// silently never appeared and only a 1-in-200 row count would have
/// revealed it. One function with two call sites makes that class of
/// divergence impossible rather than merely unlikely.
async fn finish(
    store: &dyn JobStore,
    repair: bool,
    finding_count: u64,
    repaired_count: u64,
    stalled_count: u64,
    pending: &PendingGc,
    grace: &StallGrace,
) -> RunOutcome {
    // Only when there is something to say — a healthy idle instance
    // stays silent.
    if pending.count > 0 {
        record_or_log(
            store,
            BLOBS_CONSISTENCY_JOB_NAME,
            // Distinct kind from `blob_orphan_stalled`: same subject,
            // opposite verdict. A dashboard filtering for problems must
            // not pick this up, and an operator reading history should be
            // able to tell "queue is draining normally" from "queue is
            // stuck" without parsing numbers.
            "blob_orphan_pending_gc",
            // "info", not "anomaly" — `Severity::Anomaly` is the lowest
            // NOTIFYING tier, and a blob waiting its turn is not worth
            // waking anyone. Visible in the panel, never alerts.
            "info",
            None,
            serde_json::json!({
                "pending_count":           pending.count,
                "oldest_wait_secs":        pending.oldest_secs,
                "stall_grace_secs":        grace.secs,
                "gc_driver":               grace.driver,
                "gc_driver_interval_secs": grace.driver_secs,
                "sample_hashes":           pending.sample,
                "sample_truncated":        pending.sample_truncated(),
                "remediation_hint":        "No action needed — these blobs are unreferenced \
                 and waiting for the next garbage-collection sweep, still inside the \
                 expected window. They become `blob_orphan_stalled` findings only if \
                 they outlive it. To reclaim the space sooner, \
                 POST /api/admin/jobs/dedup_gc/trigger?force=true.",
            }),
        )
        .await;
    }

    tracing::info!(
        target: "oxicloud::consistency",
        event = "blobs_consistency.completed",
        run_id = %store.run_id(),
        finding_count = finding_count,
        repaired_count = repaired_count,
        stalled_count = stalled_count,
        pending_gc_count = pending.count,
        stall_grace_secs = grace.secs,
        gc_driver = grace.driver.unwrap_or("none"),
        repair_requested = repair,
        "blobs_consistency completed with {} refcount finding(s), \
         {} repaired, {} stalled, {} awaiting GC (grace {}s)",
        finding_count,
        repaired_count,
        stalled_count,
        pending.count,
        grace.secs,
    );

    RunOutcome::completed_with(serde_json::json!({
        "repair_requested": repair,
        "repaired_count":   repaired_count,
        "stalled_count":    stalled_count,
        // Separated from `stalled_count` on purpose: "N orphans exist"
        // and "N orphans are overdue" are different operational facts,
        // and collapsing them is what made the old 24h constant look fine.
        "pending_gc_count": pending.count,
        "stall_grace_secs": grace.secs,
        "gc_driver":        grace.driver,
    }))
}

pub struct BlobsConsistencyCheck {
    pool: Arc<PgPool>,
    /// The chunk-level page query, assembled once from the blob-reference
    /// registry so this recompute and `dedup_gc` agree on what "referenced"
    /// means. Built at construction rather than per page so the sweep runs a
    /// fixed statement — same reasoning as `BlobHandler::manifest_reap_sql`.
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
    /// Job registry, consulted once per run to derive the
    /// `blob_orphan_stalled` threshold from the live GC cadence — see
    /// [`StallGrace`].
    ///
    /// `Weak`, for the reason `ConsistencyBatch` is: the registry owns
    /// this handler as an `Arc<dyn RecoverableJobHandler>`, so a strong
    /// handle back would close an `Arc` cycle and leak both for the
    /// process lifetime.
    job_registry: Weak<JobRegistry>,
}

/// The chunk-level page query, with `actual_ref_count` summed from the
/// registered reference sources.
///
/// `storage.blobs.ref_count` semantics — the invariant `blob_handler`
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
     b.hash        AS hash,
     b.size        AS size,
     b.ref_count   AS ref_count,
     b.orphaned_at AS orphaned_at,
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
    pub fn new(
        pool: Arc<PgPool>,
        reference_registry: Arc<BlobReferenceRegistry>,
        job_registry: &Arc<JobRegistry>,
    ) -> Self {
        Self {
            pool,
            chunk_page_sql: chunk_page_sql(&reference_registry),
            chunk_repair_sql: chunk_repair_sql(&reference_registry),
            job_registry: Arc::downgrade(job_registry),
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
    /// Wall-clock instant this row hit `ref_count = 0` and became
    /// eligible for GC. `NULL` for pre-migration rows or write-paths
    /// that never stamped it — those the GC treats as immediately
    /// reap-able (see `blob_handler.rs` phase-2 predicate), so the
    /// stall check ignores them too: without a stamp we cannot say
    /// how long a row has been sitting.
    orphaned_at: Option<DateTime<Utc>>,
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
        "Walks storage.blobs and checks two ref-counting invariants \
         dedup_gc relies on. First: refcount_mismatch — the stored \
         ref_count disagrees with the references that actually exist \
         (under-count lets GC reap a live blob, over-count pins a dead \
         one); repairable via ?repair=true. Second: blob_orphan_stalled \
         — the row satisfies every reap predicate GC uses but is still \
         present past 24× GC's grace, meaning the GC pipeline itself is \
         stuck (worker crashed, backend delete hanging, ghost referrer \
         being recreated); discovery-only, because a one-click repair \
         would hide the root cause the operator needs to fix — after \
         diagnosis, POST /api/admin/jobs/dedup_gc/trigger?force=true \
         drains the backlog. Database only — cheap and safe to run at \
         any time. Missing, orphaned or corrupted bytes are \
         backend_consistency's job."
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

    fn parameters(&self) -> &'static [crate::infrastructure::scheduler::JobParam] {
        use crate::infrastructure::scheduler::JobParam;
        // No `deep` — re-reading bytes is backend work and moved to
        // `backend_consistency`. Declaring it here would put a knob in
        // the panel that this job ignores, which is the thing the
        // declaration exists to stop.
        const PARAMS: &[JobParam] = &[JobParam::boolean(
            "repair",
            false,
            "Rewrite drifted ref_count values to the recomputed truth. \
             Without this the run only reports them.",
        )];
        PARAMS
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
        // Stall-check finding counter. Reported alongside
        // `finding_count` (which covers refcount findings) so the
        // completion line separates the two invariant classes.
        let mut stalled_count = 0u64;

        // Orphans still INSIDE the grace window: correct, expected,
        // transient state — the reap pipeline simply hasn't come round
        // yet. Reported, because an operator watching disk usage wants to
        // know bytes are queued for reclamation, but as one aggregated
        // `info` finding rather than one per row. See [`PendingGc`].
        let mut pending = PendingGc::default();

        // Threshold for this run, read from the live schedule once here
        // rather than per row — the cadence cannot change mid-run, and
        // `schedule_of` takes a lock per call.
        let grace = StallGrace::resolve(&self.job_registry).await;

        // `?deep=true` is not handled here. Re-reading and re-hashing
        // bytes is backend work end to end, so it moved to
        // `backend_consistency`, where the merge-join already holds the
        // matched key pairs worth verifying. A deep flag on this tenant
        // would be a flag with nothing to do.

        // Repair mode is persisted to `params.repair` so the admin
        // run-detail view can display it, and restored on resume so a
        // paused repair scan stays a repair scan — a mid-scan crash must
        // not silently downgrade the remaining rows to discovery-only.
        //
        // Both happen in `run_or_resume`, for every declared parameter,
        // under this same key. This job used to do it itself; that
        // duplication is what the parameter declaration removes.
        let repair = args.get_bool("repair");

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
                        stalled_count = stalled_count,
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
                // Single completion path — see `finish`. The other one is
                // the short-final-page return below; both must report
                // identically, which is why neither builds its own.
                return finish(
                    store,
                    repair,
                    finding_count,
                    repaired_count,
                    stalled_count,
                    &pending,
                    &grace,
                )
                .await;
            }

            // No grace window here any more. It existed to keep the
            // physical probe from flagging a blob whose bytes had landed
            // but whose row hadn't — a write-path race this tenant no
            // longer looks at. The refcount comparison reads one
            // consistent DB snapshot, so there is nothing to wait for.
            for row in &rows {
                if row.ref_count as i64 == row.actual_ref_count {
                    // No drift. Check for stall — orthogonal condition,
                    // only meaningful when the counter is CORRECT: if
                    // drift existed, the drift IS the reason the GC
                    // hasn't taken this row, and firing stall on top
                    // would mislead the operator into hunting a
                    // GC-pipeline issue that isn't there. Fix the
                    // counter → the row becomes eligible on the next
                    // sweep. Only when counter == actual == 0 AND the
                    // row has been sitting past the resolved grace (see
                    // `StallGrace`) is this a genuine "the GC should
                    // have taken this and hasn't" signal.
                    if row.actual_ref_count == 0
                        && let Some(orphaned_at) = row.orphaned_at
                    {
                        let stalled_secs = (Utc::now() - orphaned_at).num_seconds();
                        if stalled_secs <= grace.secs {
                            // Inside the window: nothing is wrong. Record
                            // it for the aggregate and move on.
                            pending.record(&row.hash, stalled_secs);
                        } else {
                            stalled_count += 1;
                            let affected = affected_files(self.pool.as_ref(), &row.hash).await;
                            let detail = serde_json::json!({
                                "hash":              row.hash,
                                "size":              row.size,
                                "ref_count":         row.ref_count,
                                "orphaned_at":       orphaned_at,
                                "stalled_for_secs":  stalled_secs,
                                "stall_grace_secs":  grace.secs,
                                // The arithmetic behind the threshold, so
                                // the operator can see WHY this row
                                // qualified instead of trusting a bare
                                // constant. Derived from the live
                                // schedule — see `StallGrace`.
                                "gc_driver":         grace.driver,
                                "gc_driver_interval_secs": grace.driver_secs,
                                "gc_eligibility_grace_secs": StallGrace::ELIGIBILITY_SECS,
                                "affected_files":    affected,
                                // Inline hint the admin UI can render on
                                // click. Not repaired here (see module
                                // doc). Built from the resolved schedule
                                // so it names the cause we can infer —
                                // the old text sent the operator to audit
                                // dedup_gc's worker health even when the
                                // real answer was "no GC driver is
                                // scheduled" or "the 24h timer never
                                // fires on this host".
                                "remediation_hint":  grace.remediation_hint(),
                            });
                            record_or_log(
                                store,
                                BLOBS_CONSISTENCY_JOB_NAME,
                                // Stable machine-readable kind — the
                                // admin UI and log-aggregator queries
                                // key off this string. Do not rename.
                                "blob_orphan_stalled",
                                // "anomaly" — surprising state worth
                                // surfacing, no direct data impact.
                                // The bytes are safe; their persistence
                                // past grace means the reap pipeline
                                // needs attention.
                                "anomaly",
                                None, // hash isn't a UUID; identifier lives in detail
                                detail,
                            )
                            .await;
                        }
                    }
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
                // Normal end of the scan: the last page was short. This
                // is the path that fires on almost every run (any row
                // count that is not an exact multiple of BATCH_SIZE), so
                // it MUST report what the empty-page path above reports —
                // hence the shared `finish`. It previously built its own
                // outcome, omitted `stalled_count`, and never emitted the
                // pending-GC aggregate at all.
                return finish(
                    store,
                    repair,
                    finding_count,
                    repaired_count,
                    stalled_count,
                    &pending,
                    &grace,
                )
                .await;
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

    // ── `blob_orphan_stalled` grace derivation ──────────────────────────
    //
    // These pin the bug that prompted the change. The threshold used to be
    // the constant `GC_ORPHAN_GRACE_SECS * 24` (= 24h), coupled to GC's
    // ELIGIBILITY delay rather than to how often GC RUNS. With the default
    // 24h `trash_cleanup` cadence the worst-case legitimate wait is
    // 24h + 1h = 25h, so any blob orphaned shortly after a sweep was
    // reported as stalled before it could possibly have been collected.
    // `grace_exceeds_worst_case_reap_latency` is the one that would have
    // failed; the others pin the parts a refactor could quietly collapse.

    struct NoopHandler(&'static str);

    #[async_trait]
    impl crate::infrastructure::scheduler::JobHandler for NoopHandler {
        fn name(&self) -> &str {
            self.0
        }
        async fn run(&self, _args: &JobRunArgs) -> crate::infrastructure::scheduler::JobOutcome {
            crate::infrastructure::scheduler::JobOutcome::ok(0)
        }
    }

    /// Registry with the named jobs registered at the given cadences.
    async fn registry_with(jobs: &[(&'static str, Option<u64>)]) -> Arc<JobRegistry> {
        let reg = Arc::new(JobRegistry::new());
        for (name, secs) in jobs {
            reg.register(
                Arc::new(NoopHandler(name))
                    as Arc<dyn crate::infrastructure::scheduler::JobHandler>,
                secs.map(std::time::Duration::from_secs),
                None,
            )
            .await;
        }
        reg
    }

    const TRASH_CLEANUP: &str =
        crate::infrastructure::services::trash_cleanup_service::TrashCleanupService::JOB_NAME;
    const DEDUP_GC: &str = crate::infrastructure::services::blob_handler::DEDUP_GC_JOB_NAME;
    /// Alias the constant UNDER TEST rather than recomputing it from
    /// `GC_ORPHAN_GRACE_SECS`. These tests pin the relationship between
    /// the eligibility delay and the driver cadence, not a hardcoded
    /// multiple of an hour — so temporarily scaling `ELIGIBILITY_SECS`
    /// to experiment on a sandbox must not turn them red.
    const ELIGIBILITY: i64 = StallGrace::ELIGIBILITY_SECS;

    /// THE regression. A blob cannot be reaped sooner than one full driver
    /// interval plus the eligibility delay, so the threshold must exceed
    /// that sum or it fires on healthy instances. The old constant did not:
    /// 24h grace vs a 25h floor.
    #[tokio::test]
    async fn grace_exceeds_worst_case_reap_latency() {
        let day = 24 * 3600;
        let reg = registry_with(&[(TRASH_CLEANUP, Some(day as u64))]).await;
        let grace = StallGrace::resolve(&Arc::downgrade(&reg)).await;

        let worst_case = day + ELIGIBILITY;
        assert!(
            grace.secs > worst_case,
            "grace {} must exceed worst-case reap latency {} (driver {}s + eligibility {}s); \
             the old GC_ORPHAN_GRACE_SECS*24 constant gave {} and flagged healthy instances",
            grace.secs,
            worst_case,
            day,
            ELIGIBILITY,
            ELIGIBILITY * 24,
        );
    }

    /// `dedup_gc` is registered on-demand (`interval = None`) by design,
    /// because `trash_cleanup` drives GC inline. Reading dedup_gc's own
    /// schedule and concluding "nothing reaps this" would slander a healthy
    /// instance, so an on-demand registration must contribute no cadence
    /// while still letting the other driver supply one.
    #[tokio::test]
    async fn on_demand_dedup_gc_does_not_suppress_the_real_driver() {
        let reg = registry_with(&[(DEDUP_GC, None), (TRASH_CLEANUP, Some(24 * 3600))]).await;
        let grace = StallGrace::resolve(&Arc::downgrade(&reg)).await;

        assert_eq!(grace.driver, Some(TRASH_CLEANUP));
        assert_eq!(grace.driver_secs, Some(24 * 3600));
    }

    /// Whichever driver reaps soonest bounds the wait, so an operator who
    /// schedules `dedup_gc=1h` gets a tight threshold — fast honest signal
    /// instead of a two-day silence.
    #[tokio::test]
    async fn soonest_driver_wins() {
        let reg = registry_with(&[(DEDUP_GC, Some(3600)), (TRASH_CLEANUP, Some(24 * 3600))]).await;
        let grace = StallGrace::resolve(&Arc::downgrade(&reg)).await;

        assert_eq!(grace.driver, Some(DEDUP_GC));
        assert_eq!(grace.secs, ELIGIBILITY + 3600 * 2);
    }

    /// Nothing scheduled drives GC: fall back to the eligibility delay
    /// alone and report no driver, which switches the remediation hint to
    /// "configure one" rather than "audit the worker".
    #[tokio::test]
    async fn no_driver_falls_back_to_eligibility_and_says_so() {
        let reg = registry_with(&[(DEDUP_GC, None)]).await;
        let grace = StallGrace::resolve(&Arc::downgrade(&reg)).await;

        assert_eq!(grace.secs, ELIGIBILITY);
        assert_eq!(grace.driver, None);
        assert!(
            grace.remediation_hint().contains("No scheduled job drives"),
            "hint should name the configuration cause, got: {}",
            grace.remediation_hint()
        );
    }

    /// A dropped registry (shutdown) must not panic the sweep.
    #[tokio::test]
    async fn dropped_registry_falls_back_to_eligibility() {
        let weak = {
            let reg = registry_with(&[(TRASH_CLEANUP, Some(24 * 3600))]).await;
            Arc::downgrade(&reg)
        };
        let grace = StallGrace::resolve(&weak).await;
        assert_eq!(grace.secs, ELIGIBILITY);
        assert_eq!(grace.driver, None);
    }

    /// With a driver present the hint must point at the schedule, naming
    /// the restart/suspend failure mode — that is the actual cause on a
    /// host that reboots or hibernates more often than the interval, and
    /// the old text sent operators hunting advisory locks instead.
    #[tokio::test]
    async fn hint_with_driver_names_the_schedule_not_the_worker() {
        let reg = registry_with(&[(TRASH_CLEANUP, Some(24 * 3600))]).await;
        let grace = StallGrace::resolve(&Arc::downgrade(&reg)).await;
        let hint = grace.remediation_hint();

        assert!(
            hint.contains(TRASH_CLEANUP),
            "hint should name the driver: {hint}"
        );
        assert!(
            hint.contains("restarts or suspends"),
            "hint should explain why a long interval never elapses: {hint}"
        );
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
     b.hash        AS hash,
     b.size        AS size,
     b.ref_count   AS ref_count,
     b.orphaned_at AS orphaned_at,
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

    /// Golden test — the repair statement is assembled from the same
    /// registry as `chunk_page_sql`, so pin it byte-for-byte too. If
    /// the registry ever changes what it produces at
    /// `RefLevel::Chunk`, BOTH this test and
    /// `chunk_page_statement_is_stable` above break together — an
    /// operator using `?repair=true` shouldn't see the detection
    /// formula report drift the repair formula can't clear.
    ///
    /// Ships the two-term formula (`storage.files` legacy-path count +
    /// `storage.chunk_manifests` chunk-membership count) twice — once
    /// in SET, once in the `<>` guard. Both must stay identical so the
    /// guard is meaningful.
    #[tokio::test]
    async fn chunk_repair_statement_is_stable() {
        let sql = chunk_repair_sql(&default_registry());
        let expected = r#"UPDATE storage.blobs b
            SET ref_count = ((SELECT COUNT(*) FROM storage.files cnt_f
               WHERE cnt_f.blob_hash = b.hash
                 AND NOT EXISTS (
                     SELECT 1 FROM storage.chunk_manifests cnt_m
                      WHERE cnt_m.file_hash = cnt_f.blob_hash
                 ))
 + (SELECT COUNT(*) FROM storage.chunk_manifests cnt_m
                   WHERE b.hash = ANY(cnt_m.chunk_hashes)))::bigint
          WHERE b.hash = $1
            AND b.ref_count <> ((SELECT COUNT(*) FROM storage.files cnt_f
               WHERE cnt_f.blob_hash = b.hash
                 AND NOT EXISTS (
                     SELECT 1 FROM storage.chunk_manifests cnt_m
                      WHERE cnt_m.file_hash = cnt_f.blob_hash
                 ))
 + (SELECT COUNT(*) FROM storage.chunk_manifests cnt_m
                   WHERE b.hash = ANY(cnt_m.chunk_hashes)))::bigint"#;
        assert_eq!(sql, expected, "chunk repair statement changed:\n{sql}");
    }
}
