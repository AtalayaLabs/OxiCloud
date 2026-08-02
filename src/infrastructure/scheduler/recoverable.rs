//! Part 2 of `docs/plan/job-registry.md` — the recoverable-run engine.
//!
//! Sibling to Part 1's [`JobHandler`](super::handler::JobHandler): where
//! `JobHandler` covers one-shot periodic jobs whose outcome is a
//! `JobOutcome`, this module covers long-running iteration that must
//! survive process restarts. State lives in `jobs.recoverable_runs`
//! and is threaded to the handler via a [`JobStore`].
//!
//! # Layering
//!
//! A [`RecoverableJobHandler`] is wrapped by [`RecoverableAdapter`]
//! to expose a `JobHandler` face; the wrapper is what registers with
//! the existing [`JobRegistry`](super::registry::JobRegistry). Part 1
//! knows nothing about cursors — every recoverable job appears to the
//! supervisor as a normal `JobHandler` whose `run()` calls
//! [`run_or_resume`] under the hood.
//!
//! # Persistence contract
//!
//! - [`JobStoreProvider::open_or_start`] is the sole entry into
//!   `jobs.recoverable_runs`. It enforces the "one non-terminal run
//!   per `job_name`" invariant via the DB's partial unique index.
//! - [`JobStoreProvider::boot_recovery_sweep`] runs once at server
//!   startup to flip `Running`/`CancelRequested` rows abandoned by a
//!   previous process to `Paused`, so an operator can resume them
//!   explicitly.
//!
//! # For future implementors
//!
//! - Implement [`RecoverableJobHandler`] on your service. Write a
//!   cursor-based scan loop that polls [`JobStore::status`] between
//!   batches for cooperative cancellation and calls
//!   [`JobStore::checkpoint`] every ~30 s or ~1 000 rows.
//! - Register via `svc.register_recoverable_job(&registry, &provider).await`
//!   (see the ergonomic helper on the service — same shape as
//!   Part 1's `register_job`).
//! - `docs/architecture/jobs.md` will cover this in operator-facing
//!   detail once Slice 2 (admin endpoints) lands.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::Serialize;
use uuid::Uuid;

use crate::common::errors::DomainError;

use super::handler::JobHandler;
use super::types::{JobOutcome, JobRunArgs};

// ─── Run status ─────────────────────────────────────────────────────────────

/// Mirror of the `TEXT` values allowed in `jobs.recoverable_runs.status`.
///
/// Terminal set = `{Completed, Failed}`. Non-terminal set (the one the
/// exclusivity partial unique index scopes) =
/// `{Running, Paused, CancelRequested}`.
///
/// `CancelRequested` IS non-terminal — the run is still shutting down.
/// A second trigger arriving during cancel MUST NOT spawn a parallel
/// run; the trigger endpoint returns the surviving row instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum RunStatus {
    Running,
    Paused,
    CancelRequested,
    Completed,
    Failed,
}

impl RunStatus {
    /// Stable label matching the SQL storage form.
    pub fn as_str(self) -> &'static str {
        match self {
            RunStatus::Running => "Running",
            RunStatus::Paused => "Paused",
            RunStatus::CancelRequested => "CancelRequested",
            RunStatus::Completed => "Completed",
            RunStatus::Failed => "Failed",
        }
    }

    /// Parse from the SQL `status` column value; returns `None` for
    /// unknown strings (schema drift signal).
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "Running" => Some(RunStatus::Running),
            "Paused" => Some(RunStatus::Paused),
            "CancelRequested" => Some(RunStatus::CancelRequested),
            "Completed" => Some(RunStatus::Completed),
            "Failed" => Some(RunStatus::Failed),
            _ => None,
        }
    }

    /// The set the exclusivity partial index scopes. Read: "a run in
    /// this state blocks a fresh dispatch."
    pub fn is_non_terminal(self) -> bool {
        matches!(
            self,
            RunStatus::Running | RunStatus::Paused | RunStatus::CancelRequested
        )
    }
}

// ─── Run outcome (handler → engine) ─────────────────────────────────────────

/// What a [`RecoverableJobHandler`] returns from `run_resumable`.
/// Translated by [`run_or_resume`] into a [`JobOutcome`] for uniform
/// supervisor logging + last-outcome storage.
///
/// - `Completed` — walked the whole space; engine writes `status = Completed`.
/// - `Paused` — cooperative pause (cancel poll or graceful shutdown);
///   engine persists cursor + writes `status = Paused` so a future
///   resume picks up from here.
/// - `Failed` — irrecoverable error; cursor NOT advanced; engine
///   writes `status = Failed` with the message.
#[derive(Debug, Clone)]
pub enum RunOutcome {
    /// The run walked the whole subject space.
    ///
    /// `extra_stats` is merged into the run row's `stats` JSONB
    /// alongside the engine-owned `scanned_count` + `finding_count`
    /// / `severity_counts`. Handlers use it to surface per-run
    /// summary counters (e.g. `backend_rotate` reports
    /// `{"rewritten": N, "skipped": M, "failed": K}`) — the outcome
    /// message in `JobOutcome.extra` and every downstream reader
    /// of `RunSummary.stats` see the merged fields.
    ///
    /// Empty map = "no tenant-specific extras" — same shape as the
    /// pre-K3 bare `Completed` variant. Handlers that don't
    /// summarise their work call [`Self::completed`].
    Completed {
        extra_stats: serde_json::Map<String, serde_json::Value>,
    },
    Paused {
        cursor: Vec<u8>,
    },
    Failed {
        message: String,
    },
}

impl RunOutcome {
    /// Convenience for the common case: handler has nothing to add
    /// to `stats` beyond what the engine already tracks (finding /
    /// scanned counters). Equivalent to
    /// `Completed { extra_stats: Map::new() }`.
    pub fn completed() -> Self {
        RunOutcome::Completed {
            extra_stats: serde_json::Map::new(),
        }
    }

    /// Convenience for handlers that want to surface per-run
    /// summary counters. Takes any JSON object literal produced by
    /// `serde_json::json!({...})`; panics if the top-level value
    /// isn't an Object (programmer bug — the contract is
    /// object-shaped).
    ///
    /// Example — a rotate handler at run-complete:
    ///
    /// ```ignore
    /// return RunOutcome::completed_with(serde_json::json!({
    ///     "rewritten": rewritten_count,
    ///     "skipped":   skipped_count,
    ///     "failed":    failed_count,
    /// }));
    /// ```
    pub fn completed_with(extras: serde_json::Value) -> Self {
        match extras {
            serde_json::Value::Object(map) => RunOutcome::Completed { extra_stats: map },
            other => panic!(
                "RunOutcome::completed_with expected a JSON object, got {}",
                other
            ),
        }
    }
}

// ─── Traits — implementor + port ────────────────────────────────────────────

/// The implementor-facing contract for a long-running, restart-tolerant
/// job. Sibling of [`JobHandler`]; NOT a subtrait — a stateless job
/// that only implements `JobHandler` never needs to know Part 2 exists.
///
/// # Contract
///
/// - **`name()` must be stable.** Appears in `jobs.recoverable_runs.job_name`,
///   log lines, and admin URLs (`POST /api/admin/jobs/{name}/trigger`).
///   Renaming after release is a breaking change.
/// - **Poll `store.status()` between batches** — the operator-cancel
///   path sets `status = CancelRequested`, and the handler MUST
///   observe that and return `RunOutcome::Paused { cursor }` at the
///   next safe boundary. Failing to poll means cancel doesn't work.
/// - **Checkpoint periodically.** Every ~30 s OR ~1 000 rows,
///   whichever comes first. Cheaper thresholds waste DB traffic;
///   coarser thresholds leak more work on crash.
/// - **Do NOT catch panics inside `run_resumable`.** The Part 1
///   supervisor's `tokio::spawn` + `catch_unwind` boundary covers
///   panics uniformly — masking one loses the `cause=panicked`
///   diagnostic.
/// - **Do NOT accept a wall-clock timeout.** Part 1's `timeout`
///   knob is applied by the supervisor only for `JobHandler`
///   dispatches. A `tokio::time::timeout` fired mid-scan aborts the
///   task without letting the handler persist the cursor — the
///   cooperative `status()` poll is the ONLY safe cancel path for
///   recoverable jobs.
/// - **Do NOT call the terminal-write methods** (`mark_completed`,
///   `mark_paused`, `mark_failed`) on the store — [`run_or_resume`]
///   owns those, driven by your `RunOutcome` return value. Calling
///   them yourself risks leaving the row in a state that disagrees
///   with what you return.
#[async_trait]
pub trait RecoverableJobHandler: Send + Sync {
    /// Stable snake_case identifier. Must match the eventual admin
    /// URL fragment: `POST /api/admin/jobs/{name}/trigger`.
    fn name(&self) -> &str;

    /// Long-running scan. See trait-level doc for the contract.
    ///
    /// `store` — bound to THIS run (a single row in
    /// `jobs.recoverable_runs`). Use it for cancel polling +
    /// checkpointing + finding recording.
    /// `args` — per-dispatch parameters forwarded from the trigger
    /// endpoint (`?force=true` maps to `args.force`).
    /// `resume_cursor` — the cursor persisted by a prior Paused run,
    /// or `None` for a fresh run. Decode into your own key type
    /// (blob hash, file_id UUID, ltree path, …).
    async fn run_resumable(
        &self,
        store: &dyn JobStore,
        args: &JobRunArgs,
        resume_cursor: Option<Vec<u8>>,
    ) -> RunOutcome;

    /// **Optional** — override to enable progress estimation on the
    /// admin UI. Called ONCE at fresh-run start by [`run_or_resume`];
    /// the returned count is stashed in `params.total_rows` and paired
    /// with `stats.scanned_count` at serialisation time to produce a
    /// `RunProgress` fraction on `RunSummary`.
    ///
    /// Return `None` (the default) when the tenant cannot count its
    /// subject — an external crawler, a streaming source, or any
    /// unbounded workload. The UI then hides the bar and falls back
    /// to raw `scanned_count`.
    ///
    /// **Not called on resume.** A Paused run keeps the `total_rows`
    /// stamped at its original start — mid-scan re-counts would make
    /// the fraction jump around every time the operator resumed.
    async fn count_total(&self) -> Option<u64> {
        None
    }

    /// Confidence level of the count returned by [`count_total`].
    /// Default is [`ProgressKind::Count`] — assume the count is
    /// authoritative unless the tenant overrides. Tenants whose
    /// `count_total` is a proxy (backend enumeration counting DB
    /// blobs instead of backend objects) return
    /// [`ProgressKind::Approximate`].
    fn progress_kind(&self) -> ProgressKind {
        ProgressKind::Count
    }
}

/// Bound-to-a-run handle. The handler polls status + writes
/// checkpoints; [`run_or_resume`] alone drives the terminal
/// transitions (marked in the trait doc as engine-only).
///
/// Terminal writes are ON this trait (not a separate one) to keep
/// the concrete impl monolithic — but handler code must not call
/// them. See the `RecoverableJobHandler` trait doc.
#[async_trait]
pub trait JobStore: Send + Sync {
    /// UUID identifying this specific run (`jobs.recoverable_runs.id`).
    fn run_id(&self) -> Uuid;

    /// Fixed at run start. Long-running consistency scans use this
    /// as their grace-window reference — NOT `chrono::Utc::now()`,
    /// which would drift across a multi-hour scan.
    fn started_at(&self) -> DateTime<Utc>;

    /// Current status of the run's row. Between batches the handler
    /// polls this; if it returns [`RunStatus::CancelRequested`], the
    /// handler MUST return [`RunOutcome::Paused`] at the next safe
    /// boundary.
    async fn status(&self) -> Result<RunStatus, DomainError>;

    /// Advance cursor + accumulate `delta_count` into
    /// `stats.scanned_count`, bump `last_progress_at`. Called between
    /// batches — the run's heartbeat.
    async fn checkpoint(&self, cursor: Vec<u8>, delta_count: u64) -> Result<(), DomainError>;

    /// **Engine-only.** Called by [`run_or_resume`] on a Fresh run
    /// after the tenant's [`RecoverableJobHandler::count_total`]
    /// reports a countable subject. Stamps `params.total_rows` +
    /// `params.progress_kind` on the row so subsequent `RunSummary`
    /// projections can derive `progress` without asking the tenant
    /// again. Handler code MUST NOT call this.
    async fn seed_progress_params(&self, total: u64, kind: ProgressKind)
    -> Result<(), DomainError>;

    /// Set an arbitrary string field on `params` (JSONB). Used by
    /// handlers on a Fresh run to persist per-run configuration that
    /// must survive a mid-run restart — e.g. `backend_migration`
    /// stamping `params.target_name` at run start so a resume can
    /// pick up the same target without the admin re-specifying it.
    ///
    /// Handler-callable (unlike `seed_progress_params`, which is
    /// engine-only). Idempotent: re-writing the same value is a
    /// no-op UPDATE.
    async fn set_string_param(&self, key: &str, value: &str) -> Result<(), DomainError>;

    /// Read a string field from `params` (JSONB). Returns `None` when
    /// the key is absent or its value isn't a JSON string. Paired
    /// with [`Self::set_string_param`] — handlers on a Resumed run
    /// use this to recover per-run config that a prior Fresh open
    /// stamped.
    async fn get_string_param(&self, key: &str) -> Result<Option<String>, DomainError>;

    /// Current `stats.scanned_count` for this run. Used by handlers
    /// on a Resume path to reconstruct progress state that isn't
    /// persisted in `params` — e.g. `backend_migration` seeds its
    /// user-facing `MigrationProgress` counter with this so the
    /// admin banner shows continued progress across a restart
    /// instead of resetting to 0.
    ///
    /// Returns `0` if the key is absent (fresh row) or not a
    /// number. Callers on a Fresh run can safely skip this — the
    /// answer is trivially 0 and the write path starts fresh.
    async fn scanned_count(&self) -> Result<u64, DomainError>;

    /// Persist one finding to `jobs.run_findings` and bump
    /// `stats.finding_count` on the parent run. Consistency handlers
    /// call this in place of the transitional
    /// `tracing::warn!(event = "consistency_finding", …)` — see
    /// `docs/plan/job-registry.md` Part 2 §Findings.
    ///
    /// `kind` — stable machine-readable enum-style key (e.g.
    /// `"stale_used_bytes"`, `"missing_blob"`). Never rename across
    /// releases; new failure modes get new values.
    ///
    /// `severity` — one of `"data_loss"`, `"inconsistent"`, `"anomaly"`.
    ///
    /// `resource_id` — the file / folder / drive / blob the finding
    /// pertains to. `None` for run-wide findings (e.g. "backend
    /// enumeration truncated at 1M keys").
    ///
    /// `detail` — per-tenant per-kind JSON blob. Consumers key off
    /// `kind` to know the shape (cached/actual/delta for
    /// `stale_used_bytes`, blob_hash for `missing_blob`, etc.).
    ///
    /// Failure surfaces to the caller as `Err`. Handlers should
    /// log-and-continue rather than fail the whole run — a lost
    /// finding is bad but not worse than aborting the walk.
    async fn record_finding(
        &self,
        kind: &str,
        severity: &str,
        resource_id: Option<Uuid>,
        detail: serde_json::Value,
    ) -> Result<(), DomainError>;

    /// **Engine-only.** Merge `extras` into the run row's `stats`
    /// JSONB (SQL `stats = stats || $1`). Called by [`run_or_resume`]
    /// on [`RunOutcome::Completed`] to persist the handler's
    /// per-run summary counters alongside the engine-owned
    /// `scanned_count` / `finding_count`. Handler code MUST NOT
    /// call this directly — return an `extra_stats` map on
    /// `Completed` and the engine handles the write.
    ///
    /// Idempotent: merging the same map twice yields the same row.
    /// A stats key that already exists is OVERWRITTEN by the
    /// merge (last-write-wins) — a handler that emits e.g.
    /// `"rewritten": 300` at run end always displaces any prior
    /// per-batch write of the same key.
    async fn merge_stats(
        &self,
        extras: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<(), DomainError>;

    // ─── Terminal writes — engine-only. Do not call from handler code.

    /// Engine-only. Called by [`run_or_resume`] on
    /// [`RunOutcome::Completed`]. Handler code MUST NOT call this.
    async fn mark_completed(&self) -> Result<(), DomainError>;

    /// Engine-only. Called by [`run_or_resume`] on
    /// [`RunOutcome::Paused`]. `cursor` = the resume key the handler
    /// returned. Handler code MUST NOT call this.
    async fn mark_paused(&self, cursor: Option<Vec<u8>>) -> Result<(), DomainError>;

    /// Engine-only. Called by [`run_or_resume`] on
    /// [`RunOutcome::Failed`]. Handler code MUST NOT call this.
    async fn mark_failed(&self, message: &str) -> Result<(), DomainError>;
}

/// Registry-level operations on `jobs.recoverable_runs` — NOT bound
/// to a specific run. Provides the entry point [`run_or_resume`] uses
/// to look up / create a run, and the boot-time crash-recovery sweep.
#[async_trait]
pub trait JobStoreProvider: Send + Sync {
    /// Called by [`run_or_resume`]. Behaviour:
    ///
    /// - No non-terminal row for `job_name`: INSERT a fresh Running
    ///   row (`cursor = NULL`, `started_at = NOW()`), return
    ///   [`OpenedRun::Fresh`].
    /// - Latest non-terminal row is `Paused`: UPDATE to Running,
    ///   return [`OpenedRun::Resumed`] with the persisted cursor.
    /// - Latest non-terminal row is `Running` or `CancelRequested`:
    ///   return [`OpenedRun::AlreadyActive`] — caller MUST NOT
    ///   dispatch a parallel run.
    ///
    /// A concurrent INSERT race is handled internally via the DB's
    /// partial unique index — the losing INSERT falls back to reading
    /// the winning row.
    async fn open_or_start(&self, job_name: &str) -> Result<OpenedRun, DomainError>;

    /// Boot-time crash recovery. Any row abandoned in `Running` or
    /// `CancelRequested` when the previous process died gets flipped
    /// to `Paused` with `error_message = 'server restart mid-run'`.
    /// Returns the number of rows updated.
    ///
    /// Does NOT auto-resume — the bug that killed the previous run
    /// may still be present. Operators trigger the resume explicitly
    /// via `POST /api/admin/jobs/{name}/trigger`, which calls
    /// `open_or_start` and picks up the Paused cursor.
    async fn boot_recovery_sweep(&self) -> Result<u64, DomainError>;

    /// Latest N runs for `job_name`, newest first, terminal + non-terminal
    /// both included. Powers `GET /api/admin/jobs/{name}/runs`. `limit`
    /// caps the return size; the API layer clamps it too.
    async fn list_runs(&self, job_name: &str, limit: u32) -> Result<Vec<RunSummary>, DomainError>;

    /// Fetch one run by id. Powers `GET /api/admin/jobs/{name}/runs/{id}`.
    /// Returns `None` when the id doesn't exist (unknown or pruned).
    async fn get_run_by_id(&self, run_id: Uuid) -> Result<Option<RunSummary>, DomainError>;

    /// Request cancellation of the CURRENT active run for `job_name`
    /// by flipping its status from `Running` → `CancelRequested`.
    /// Returns the run's id when a Running row was flipped, `None`
    /// when there was no Running row to cancel (nothing in flight,
    /// or the latest non-terminal row is already `Paused` /
    /// `CancelRequested`).
    ///
    /// Cooperative — the handler still needs to poll `store.status()`
    /// and return `RunOutcome::Paused` at the next safe boundary. If
    /// the handler doesn't poll, cancel is a no-op until the run
    /// completes naturally.
    async fn request_cancel(&self, job_name: &str) -> Result<Option<Uuid>, DomainError>;

    /// Findings for a specific run, newest-last, paginated.
    /// Powers `GET /api/admin/jobs/{name}/runs/{id}/findings`.
    /// `limit` caps rows; the API layer clamps it too. `offset` is
    /// simple integer paging — findings-per-run is typically small
    /// enough that cursor pagination is overkill.
    async fn list_findings(
        &self,
        run_id: Uuid,
        limit: u32,
        offset: u32,
    ) -> Result<Vec<Finding>, DomainError>;

    /// Aggregate finding count grouped by severity for a specific
    /// run. Used by [`run_or_resume`] to fold per-severity counts
    /// into the outer `JobOutcome::extra` so the admin UI can
    /// distinguish `data_loss`/`inconsistent` findings (which turn
    /// the outer outcome pill amber/red — actionable) from
    /// `anomaly` findings (which render as a neutral notice —
    /// informational). Runs one grouped SQL query; O(number of
    /// distinct severities on the run) rows returned.
    async fn finding_severity_counts(
        &self,
        run_id: Uuid,
    ) -> Result<Vec<(String, u64)>, DomainError>;

    /// Operator-triggered retention cleanup. DELETEs every
    /// TERMINAL run (`Completed`, `Failed`) whose `completed_at`
    /// is older than `retention_days` days ago. Findings drop
    /// alongside via the `ON DELETE CASCADE` FK on
    /// `jobs.run_findings.run_id`.
    ///
    /// Non-terminal rows (`Running`, `Paused`, `CancelRequested`)
    /// are ALWAYS preserved regardless of age — an in-flight or
    /// paused run must not be reaped by retention.
    ///
    /// `retention_days` is treated as `max(1, retention_days)` at
    /// the impl layer to defend against a zero/negative value
    /// eating just-completed runs.
    ///
    /// Returns the number of run rows deleted (which equals
    /// the number of finding rows deleted *transitively* via
    /// CASCADE; callers wanting the finding count separately
    /// should query it BEFORE calling this).
    ///
    /// Powers `POST /api/admin/jobs/runs/purge`. Not periodic — the
    /// operator decides when to reclaim space.
    async fn purge_terminal_runs(&self, retention_days: i32) -> Result<u64, DomainError>;
}

/// How a `RunProgress` fraction was derived. Lets the UI communicate
/// confidence to the operator — a `count`-derived 47% is authoritative,
/// an `approximate`-derived 47% is a proxy (e.g. `storage_consistency`
/// using DB blob count as a stand-in for backend object count).
///
/// A future `cursor` variant will cover UUID-cursor-position-derived
/// fractions (`cursor_position / 2^128`) — useful when `COUNT(*)` on
/// the subject table is too expensive to run at start. Not implemented
/// yet; all shipped tenants override [`RecoverableJobHandler::count_total`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ProgressKind {
    /// `scanned_count / total_rows` where `total_rows` came from a
    /// definitive `COUNT(*)` on the tenant's subject table.
    Count,
    /// `scanned_count / total_rows` where `total_rows` is a proxy
    /// (e.g. DB blob count for a backend enumeration). The fraction
    /// deviating from 1.0 at run end IS informative — it quantifies
    /// the drift the check is looking for.
    Approximate,
}

impl ProgressKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ProgressKind::Count => "count",
            ProgressKind::Approximate => "approximate",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "count" => Some(ProgressKind::Count),
            "approximate" => Some(ProgressKind::Approximate),
            _ => None,
        }
    }
}

/// Progress estimate on a recoverable run. Populated on `RunSummary`
/// only when the tenant's [`RecoverableJobHandler::count_total`]
/// returned `Some(n)` at run start — a tenant that cannot count its
/// subject (external crawler, streaming source) leaves this `None` and
/// the UI hides the progress bar.
///
/// `fraction` CAN exceed 1.0 at the end of an
/// [`ProgressKind::Approximate`] run — the deviation IS the finding.
/// The UI should clamp for the bar width but surface the raw fraction
/// in the tooltip.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct RunProgress {
    pub fraction: f32,
    pub kind: ProgressKind,
    /// Included so the UI can render "347 / 1200" alongside the bar
    /// without recomputing from `stats.scanned_count`.
    pub scanned: u64,
    pub total: u64,
}

/// Build a `RunProgress` from the persisted scanned / total / kind.
/// `None` when `total` is absent (tenant didn't count) OR zero (avoid
/// dividing by zero and rendering a bar for an empty-subject run).
pub fn derive_progress(
    scanned: u64,
    total: Option<u64>,
    kind: Option<ProgressKind>,
) -> Option<RunProgress> {
    let total = total?;
    if total == 0 {
        return None;
    }
    let kind = kind.unwrap_or(ProgressKind::Count);
    // We deliberately DON'T clamp — an approximate-kind run can
    // legitimately exceed 1.0 (backend has orphans), and that
    // deviation is informative signal. The UI clamps for bar width
    // but shows raw fraction in the tooltip.
    let fraction = scanned as f32 / total as f32;
    Some(RunProgress {
        fraction,
        kind,
        scanned,
        total,
    })
}

/// Serialisable snapshot of one `jobs.run_findings` row, returned by
/// `GET /api/admin/jobs/{name}/runs/{id}/findings`. Consumers key off
/// `kind` to know the shape of `detail`.
#[derive(Debug, Clone, Serialize)]
pub struct Finding {
    pub id: Uuid,
    pub run_id: Uuid,
    pub kind: String,
    pub severity: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resource_id: Option<Uuid>,
    pub detail: serde_json::Value,
    pub created_at: DateTime<Utc>,
}

/// Serialisable snapshot of one `jobs.recoverable_runs` row, returned
/// by the admin listing + get-run endpoints.
#[derive(Debug, Clone, Serialize)]
pub struct RunSummary {
    pub id: Uuid,
    pub job_name: String,
    pub status: RunStatus,
    pub started_at: DateTime<Utc>,
    pub last_progress_at: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<DateTime<Utc>>,
    /// `stats` JSONB dump — job-specific counters (scanned_count,
    /// migrated_blobs, findings_this_run, …).
    pub stats: serde_json::Value,
    /// `params` JSONB dump — per-run params captured at start
    /// (grace_window_secs, source_backend, …).
    pub params: serde_json::Value,
    /// Cursor as hex — omitted when null. Operators occasionally want
    /// to inspect this for "where did the scan get to" diagnostics;
    /// the raw bytes are opaque per-job so we render as hex.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor_hex: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
    /// Present when the tenant reported a countable subject at run
    /// start (see [`RecoverableJobHandler::count_total`]). `None`
    /// tells the UI "hide the progress bar, show scanned_count as a
    /// raw number instead."
    #[serde(skip_serializing_if = "Option::is_none")]
    pub progress: Option<RunProgress>,
}

/// Result of [`JobStoreProvider::open_or_start`].
pub enum OpenedRun {
    /// Fresh run — new row inserted, cursor is None (start from scratch).
    Fresh { store: Arc<dyn JobStore> },
    /// Existing Paused run resumed. `cursor` is the last-persisted
    /// resume key; the handler decodes it into its own type.
    Resumed {
        store: Arc<dyn JobStore>,
        cursor: Vec<u8>,
    },
    /// A non-terminal run is already active; the caller must NOT
    /// spawn a parallel dispatch. Returned to admin/trigger callers
    /// as `Ok { count: 0, extra: {"skipped": "already_running", …} }`.
    AlreadyActive { run_id: Uuid, status: RunStatus },
}

// ─── Engine glue ────────────────────────────────────────────────────────────

/// The single entry point for running a `RecoverableJobHandler`
/// outside test code. Coordinates row lookup/creation, dispatches
/// the handler, translates `RunOutcome` → `JobOutcome`, writes the
/// terminal status.
///
/// Called by [`RecoverableAdapter::run`] (the Part 1 JobHandler face)
/// so recoverable jobs slot into the existing scheduler unchanged.
pub async fn run_or_resume(
    job: Arc<dyn RecoverableJobHandler>,
    provider: Arc<dyn JobStoreProvider>,
    args: &JobRunArgs,
) -> JobOutcome {
    let opened = match provider.open_or_start(job.name()).await {
        Ok(o) => o,
        Err(e) => return JobOutcome::err(format!("open_or_start failed: {e}")),
    };
    let (store, resume_cursor, is_fresh) = match opened {
        OpenedRun::AlreadyActive { run_id, status } => {
            return JobOutcome::ok_with(
                0,
                serde_json::json!({
                    "skipped": "already_running",
                    "run_id": run_id.to_string(),
                    "status": status.as_str(),
                }),
            );
        }
        OpenedRun::Fresh { store } => (store, None, true),
        OpenedRun::Resumed { store, cursor } => (store, Some(cursor), false),
    };
    let run_id = store.run_id();

    // Seed progress params on a Fresh run only — a resumed run keeps
    // the total_rows stamped when it originally started, otherwise
    // the fraction would jump every time the operator resumed. A
    // failed count is not fatal; the progress block just stays None
    // on the summary (UI falls back to raw scanned_count).
    if is_fresh && let Some(total) = job.count_total().await {
        let kind = job.progress_kind();
        if let Err(e) = store.seed_progress_params(total, kind).await {
            tracing::warn!(
                target: "oxicloud::scheduler",
                event = "recoverable.seed_progress_failed",
                job = job.name(),
                run_id = %run_id,
                error = %e,
                "failed to seed progress params; run continues without a bar"
            );
        }
    }

    // Dispatch. Terminal writes to `jobs.recoverable_runs` happen
    // here (NOT in the handler) so the row always ends in a state
    // that matches what the handler returned.
    let outcome = job.run_resumable(&*store, args, resume_cursor).await;

    // Fetch the terminal run summary so we can surface aggregate
    // stats (finding_count, scanned_count) on the outer JobOutcome
    // extras. The outer admin listing (`GET /api/admin/jobs`) reads
    // `last_outcome.extra` — without this the UI can't badge a job
    // as "has findings" without also fetching the run history.
    // Called AFTER the handler returns but BEFORE the terminal write,
    // so stats are the ones accumulated during the run.
    let stats = fetch_outcome_stats(&*provider, run_id).await;

    match outcome {
        RunOutcome::Completed { extra_stats } => {
            // Merge tenant-supplied extras into the run's stats
            // JSONB BEFORE the terminal mark, so downstream readers
            // see the merged view atomically. `fetch_outcome_stats`
            // (a few lines up) already ran and reflects the state
            // WITHOUT the merge — re-fetch so the outer JobOutcome
            // includes the tenant counters too.
            if !extra_stats.is_empty() {
                log_terminal_write_err(
                    "merge_stats",
                    run_id,
                    store.merge_stats(&extra_stats).await,
                );
            }
            log_terminal_write_err("mark_completed", run_id, store.mark_completed().await);
            let stats = fetch_outcome_stats(&*provider, run_id).await;
            JobOutcome::ok_with(
                stats.finding_count,
                serde_json::json!({
                    "completed":         true,
                    "run_id":            run_id.to_string(),
                    "finding_count":     stats.finding_count,
                    "scanned_count":     stats.scanned_count,
                    "severity_counts":   stats.by_severity,
                    "extra_stats":       serde_json::Value::Object(extra_stats),
                }),
            )
        }
        RunOutcome::Paused { cursor } => {
            let cursor_hex = hex::encode(&cursor);
            log_terminal_write_err("mark_paused", run_id, store.mark_paused(Some(cursor)).await);
            JobOutcome::ok_with(
                stats.finding_count,
                serde_json::json!({
                    "paused":            true,
                    "run_id":            run_id.to_string(),
                    "cursor_hex":        cursor_hex,
                    "finding_count":     stats.finding_count,
                    "scanned_count":     stats.scanned_count,
                    "severity_counts":   stats.by_severity,
                }),
            )
        }
        RunOutcome::Failed { message } => {
            log_terminal_write_err("mark_failed", run_id, store.mark_failed(&message).await);
            JobOutcome::err(format!("{message} (run_id={run_id})"))
        }
    }
}

/// Aggregate summary of a just-completed run, folded into the
/// outer `JobOutcome::extra`. Missing / failed queries default to
/// zeros so the outer outcome stays quiet instead of erroring.
struct OutcomeStats {
    finding_count: u64,
    scanned_count: u64,
    /// Per-severity counts as a JSON map (`{"data_loss": N,
    /// "inconsistent": M, "anomaly": K}`). The frontend uses this
    /// to render the outer outcome pill: amber/red when
    /// `data_loss + inconsistent > 0` (actionable), neutral notice
    /// when only `anomaly > 0` (informational).
    by_severity: serde_json::Value,
}

async fn fetch_outcome_stats(provider: &dyn JobStoreProvider, run_id: Uuid) -> OutcomeStats {
    let (finding_count, scanned_count) = match provider.get_run_by_id(run_id).await {
        Ok(Some(summary)) => (
            summary
                .stats
                .get("finding_count")
                .and_then(|v| v.as_u64())
                .unwrap_or(0),
            summary
                .stats
                .get("scanned_count")
                .and_then(|v| v.as_u64())
                .unwrap_or(0),
        ),
        _ => (0, 0),
    };

    // Per-severity breakdown. Only queried when there are findings
    // to break down — a clean run doesn't need the extra round-trip.
    let by_severity = if finding_count > 0 {
        match provider.finding_severity_counts(run_id).await {
            Ok(rows) => {
                let mut map = serde_json::Map::new();
                for (severity, count) in rows {
                    map.insert(severity, serde_json::Value::Number(count.into()));
                }
                serde_json::Value::Object(map)
            }
            Err(_) => serde_json::Value::Object(Default::default()),
        }
    } else {
        serde_json::Value::Object(Default::default())
    };

    OutcomeStats {
        finding_count,
        scanned_count,
        by_severity,
    }
}

fn log_terminal_write_err(op: &str, run_id: Uuid, res: Result<(), DomainError>) {
    if let Err(e) = res {
        tracing::warn!(
            target: "oxicloud::scheduler",
            event = "recoverable.terminal_write_failed",
            op = op,
            run_id = %run_id,
            error = %e,
            "failed to write terminal status for recoverable run"
        );
    }
}

// ─── Recording helper — used by every consistency tenant ───────────────────

/// Persist a finding via `store.record_finding` and, if the write
/// fails, drop a `record_finding.failed` line to the tenant's
/// tracing target so operators don't lose the event silently.
///
/// Exists because every consistency tenant needs the same
/// log-and-continue shape — extracting it here keeps each tenant's
/// per-row branch a single call.
pub async fn record_or_log(
    store: &dyn JobStore,
    job: &str,
    kind: &str,
    severity: &str,
    resource_id: Option<Uuid>,
    detail: serde_json::Value,
) {
    if let Err(e) = store
        .record_finding(kind, severity, resource_id, detail)
        .await
    {
        tracing::warn!(
            target: "oxicloud::consistency",
            event = "record_finding.failed",
            run_id = %store.run_id(),
            job = job,
            kind = kind,
            resource_id = ?resource_id,
            error = %e,
            "failed to persist finding; dropped (walk continues)"
        );
    }
}

// ─── Adapter — bridge to Part 1's JobHandler ────────────────────────────────

/// Wraps a `RecoverableJobHandler` behind a `JobHandler` face so it
/// registers with the existing `JobRegistry` unchanged. The Part 1
/// supervisor's dispatch loop calls the adapter's `run()`, which
/// delegates to `run_or_resume(inner, provider, args)`.
///
/// Constructed by `service.register_recoverable_job(&registry,
/// &provider)` — see the ergonomic helper on each recoverable
/// service.
pub struct RecoverableAdapter {
    inner: Arc<dyn RecoverableJobHandler>,
    provider: Arc<dyn JobStoreProvider>,
    name: String,
}

impl RecoverableAdapter {
    pub fn new(inner: Arc<dyn RecoverableJobHandler>, provider: Arc<dyn JobStoreProvider>) -> Self {
        let name = inner.name().to_string();
        Self {
            inner,
            provider,
            name,
        }
    }
}

#[async_trait]
impl JobHandler for RecoverableAdapter {
    fn name(&self) -> &str {
        &self.name
    }
    async fn run(&self, args: &JobRunArgs) -> JobOutcome {
        run_or_resume(self.inner.clone(), self.provider.clone(), args).await
    }
    fn is_recoverable(&self) -> bool {
        // Every tenant registered through `register_recoverable_job` is
        // wrapped by this adapter, so this flag flips true for exactly
        // the set of jobs whose runs + findings the admin UI should
        // let operators drill into. No name-based allowlists needed
        // downstream.
        true
    }
}

// ─── Ergonomics: JobRegistry extension for recoverable jobs ─────────────────

impl super::registry::JobRegistry {
    /// Register a recoverable job. Wraps the handler in a
    /// [`RecoverableAdapter`] and delegates to the standard
    /// [`register`](super::registry::JobRegistry::register) — so a
    /// recoverable job appears to the supervisor as a normal
    /// `JobHandler` at `name`.
    ///
    /// `interval` follows the same semantic as periodic jobs:
    /// - `Some(dur)` — supervisor fires it periodically (and admin
    ///   triggers land on the same `run_or_resume` dispatch).
    /// - `None` — admin-triggered only. Typical for long-running
    ///   tenants (storage migration, reextract, consistency checks).
    ///
    /// Timeout is force-None — recoverable jobs use cooperative
    /// cancellation via `store.status()` polling, NOT wall-clock
    /// timeouts. See `RecoverableJobHandler` trait doc.
    pub async fn register_recoverable_job(
        &self,
        handler: Arc<dyn RecoverableJobHandler>,
        provider: Arc<dyn JobStoreProvider>,
        interval: Option<Duration>,
    ) {
        let adapter = Arc::new(RecoverableAdapter::new(handler, provider));
        self.register(adapter, interval, None).await;
    }
}

// ─── Tests — in-memory JobStore mock + run_or_resume paths ──────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // ─── In-memory JobStore ────────────────────────────────────────────────

    struct MemStore {
        run_id: Uuid,
        started_at: DateTime<Utc>,
        state: Mutex<MemStoreState>,
    }

    struct MemStoreState {
        status: RunStatus,
        cursor: Option<Vec<u8>>,
        scanned_count: u64,
        error_message: Option<String>,
        findings: Vec<Finding>,
        progress_total: Option<u64>,
        progress_kind: Option<ProgressKind>,
        string_params: std::collections::HashMap<String, String>,
        /// K3+: extras merged into the run's stats JSONB via
        /// `merge_stats` at Completed time. Tests observe the merged
        /// view by reading this map alongside `scanned_count`.
        extra_stats: serde_json::Map<String, serde_json::Value>,
    }

    #[async_trait]
    impl JobStore for MemStore {
        fn run_id(&self) -> Uuid {
            self.run_id
        }
        fn started_at(&self) -> DateTime<Utc> {
            self.started_at
        }
        async fn status(&self) -> Result<RunStatus, DomainError> {
            Ok(self.state.lock().unwrap().status)
        }
        async fn checkpoint(&self, cursor: Vec<u8>, delta_count: u64) -> Result<(), DomainError> {
            let mut s = self.state.lock().unwrap();
            s.cursor = Some(cursor);
            s.scanned_count += delta_count;
            Ok(())
        }
        async fn record_finding(
            &self,
            kind: &str,
            severity: &str,
            resource_id: Option<Uuid>,
            detail: serde_json::Value,
        ) -> Result<(), DomainError> {
            let mut s = self.state.lock().unwrap();
            s.findings.push(Finding {
                id: Uuid::new_v4(),
                run_id: self.run_id,
                kind: kind.to_string(),
                severity: severity.to_string(),
                resource_id,
                detail,
                created_at: Utc::now(),
            });
            Ok(())
        }
        async fn seed_progress_params(
            &self,
            total: u64,
            kind: ProgressKind,
        ) -> Result<(), DomainError> {
            let mut s = self.state.lock().unwrap();
            s.progress_total = Some(total);
            s.progress_kind = Some(kind);
            Ok(())
        }
        async fn set_string_param(&self, key: &str, value: &str) -> Result<(), DomainError> {
            self.state
                .lock()
                .unwrap()
                .string_params
                .insert(key.to_string(), value.to_string());
            Ok(())
        }
        async fn get_string_param(&self, key: &str) -> Result<Option<String>, DomainError> {
            Ok(self.state.lock().unwrap().string_params.get(key).cloned())
        }
        async fn scanned_count(&self) -> Result<u64, DomainError> {
            Ok(self.state.lock().unwrap().scanned_count)
        }
        async fn merge_stats(
            &self,
            extras: &serde_json::Map<String, serde_json::Value>,
        ) -> Result<(), DomainError> {
            let mut s = self.state.lock().unwrap();
            for (k, v) in extras {
                s.extra_stats.insert(k.clone(), v.clone());
            }
            Ok(())
        }
        async fn mark_completed(&self) -> Result<(), DomainError> {
            self.state.lock().unwrap().status = RunStatus::Completed;
            Ok(())
        }
        async fn mark_paused(&self, cursor: Option<Vec<u8>>) -> Result<(), DomainError> {
            let mut s = self.state.lock().unwrap();
            s.status = RunStatus::Paused;
            if let Some(c) = cursor {
                s.cursor = Some(c);
            }
            Ok(())
        }
        async fn mark_failed(&self, message: &str) -> Result<(), DomainError> {
            let mut s = self.state.lock().unwrap();
            s.status = RunStatus::Failed;
            s.error_message = Some(message.to_string());
            Ok(())
        }
    }

    // ─── In-memory JobStoreProvider ────────────────────────────────────────
    //
    // Simplified: one job_name at a time, no cross-job isolation. Enough
    // to exercise the run_or_resume control flow.

    struct MemProvider {
        stores: Mutex<Vec<Arc<MemStore>>>,
    }

    impl MemProvider {
        fn new() -> Self {
            Self {
                stores: Mutex::new(Vec::new()),
            }
        }

        /// Test-only helper — seed a Running row without going through
        /// `open_or_start`. Lets tests set up the "concurrent trigger
        /// hits already-active" scenario without racing.
        fn seed_running(&self) -> Uuid {
            let store = Arc::new(MemStore {
                run_id: Uuid::new_v4(),
                started_at: Utc::now(),
                state: Mutex::new(MemStoreState {
                    status: RunStatus::Running,
                    cursor: None,
                    scanned_count: 0,
                    error_message: None,
                    findings: Vec::new(),
                    progress_total: None,
                    progress_kind: None,
                    string_params: std::collections::HashMap::new(),
                    extra_stats: serde_json::Map::new(),
                }),
            });
            let id = store.run_id;
            self.stores.lock().unwrap().push(store);
            id
        }

        /// Test-only read — last-created run's status, for post-hoc
        /// assertions.
        fn last_status(&self) -> Option<RunStatus> {
            let stores = self.stores.lock().unwrap();
            stores.last().map(|s| s.state.lock().unwrap().status)
        }

        /// Test-only read — last-created run's cursor.
        fn last_cursor(&self) -> Option<Vec<u8>> {
            let stores = self.stores.lock().unwrap();
            stores
                .last()
                .and_then(|s| s.state.lock().unwrap().cursor.clone())
        }
    }

    #[async_trait]
    impl JobStoreProvider for MemProvider {
        async fn open_or_start(&self, _job_name: &str) -> Result<OpenedRun, DomainError> {
            let mut stores = self.stores.lock().unwrap();
            if let Some(store) = stores.last() {
                let state = store.state.lock().unwrap();
                if state.status.is_non_terminal() {
                    return match state.status {
                        RunStatus::Paused => {
                            let cursor = state.cursor.clone().unwrap_or_default();
                            drop(state);
                            store.state.lock().unwrap().status = RunStatus::Running;
                            Ok(OpenedRun::Resumed {
                                store: store.clone(),
                                cursor,
                            })
                        }
                        _ => Ok(OpenedRun::AlreadyActive {
                            run_id: store.run_id,
                            status: state.status,
                        }),
                    };
                }
            }
            let store = Arc::new(MemStore {
                run_id: Uuid::new_v4(),
                started_at: Utc::now(),
                state: Mutex::new(MemStoreState {
                    status: RunStatus::Running,
                    cursor: None,
                    scanned_count: 0,
                    error_message: None,
                    findings: Vec::new(),
                    progress_total: None,
                    progress_kind: None,
                    string_params: std::collections::HashMap::new(),
                    extra_stats: serde_json::Map::new(),
                }),
            });
            stores.push(store.clone());
            Ok(OpenedRun::Fresh { store })
        }

        async fn boot_recovery_sweep(&self) -> Result<u64, DomainError> {
            let stores = self.stores.lock().unwrap();
            let mut n = 0u64;
            for s in stores.iter() {
                let mut state = s.state.lock().unwrap();
                if matches!(
                    state.status,
                    RunStatus::Running | RunStatus::CancelRequested
                ) {
                    state.status = RunStatus::Paused;
                    state.error_message = Some("server restart mid-run".into());
                    n += 1;
                }
            }
            Ok(n)
        }

        async fn list_runs(
            &self,
            job_name: &str,
            limit: u32,
        ) -> Result<Vec<RunSummary>, DomainError> {
            let stores = self.stores.lock().unwrap();
            let now = Utc::now();
            let out: Vec<RunSummary> = stores
                .iter()
                .rev() // newest first — MemProvider stores in insertion order
                .take(limit as usize)
                .map(|s| {
                    let state = s.state.lock().unwrap();
                    let progress = derive_progress(
                        state.scanned_count,
                        state.progress_total,
                        state.progress_kind,
                    );
                    RunSummary {
                        id: s.run_id,
                        job_name: job_name.to_string(),
                        status: state.status,
                        started_at: s.started_at,
                        last_progress_at: now,
                        completed_at: None,
                        stats: serde_json::json!({ "scanned_count": state.scanned_count }),
                        params: serde_json::json!({}),
                        cursor_hex: state.cursor.as_ref().map(hex::encode),
                        error_message: state.error_message.clone(),
                        progress,
                    }
                })
                .collect();
            Ok(out)
        }

        async fn get_run_by_id(&self, run_id: Uuid) -> Result<Option<RunSummary>, DomainError> {
            let stores = self.stores.lock().unwrap();
            let now = Utc::now();
            Ok(stores.iter().find(|s| s.run_id == run_id).map(|s| {
                let state = s.state.lock().unwrap();
                let progress = derive_progress(
                    state.scanned_count,
                    state.progress_total,
                    state.progress_kind,
                );
                RunSummary {
                    id: s.run_id,
                    job_name: "mem".to_string(),
                    status: state.status,
                    started_at: s.started_at,
                    last_progress_at: now,
                    completed_at: None,
                    stats: serde_json::json!({ "scanned_count": state.scanned_count }),
                    params: serde_json::json!({}),
                    cursor_hex: state.cursor.as_ref().map(hex::encode),
                    error_message: state.error_message.clone(),
                    progress,
                }
            }))
        }

        async fn list_findings(
            &self,
            run_id: Uuid,
            limit: u32,
            offset: u32,
        ) -> Result<Vec<Finding>, DomainError> {
            let stores = self.stores.lock().unwrap();
            let Some(store) = stores.iter().find(|s| s.run_id == run_id) else {
                return Ok(Vec::new());
            };
            let state = store.state.lock().unwrap();
            Ok(state
                .findings
                .iter()
                .skip(offset as usize)
                .take(limit as usize)
                .cloned()
                .collect())
        }

        async fn finding_severity_counts(
            &self,
            run_id: Uuid,
        ) -> Result<Vec<(String, u64)>, DomainError> {
            let stores = self.stores.lock().unwrap();
            let Some(store) = stores.iter().find(|s| s.run_id == run_id) else {
                return Ok(Vec::new());
            };
            let state = store.state.lock().unwrap();
            let mut counts: std::collections::HashMap<String, u64> =
                std::collections::HashMap::new();
            for f in state.findings.iter() {
                *counts.entry(f.severity.clone()).or_default() += 1;
            }
            Ok(counts.into_iter().collect())
        }

        async fn purge_terminal_runs(&self, retention_days: i32) -> Result<u64, DomainError> {
            // Test-double: no `completed_at` to compare against, so
            // just drop every terminal-state store when
            // `retention_days` > 0. Sufficient for the trait
            // contract check; PG impl exercises the real
            // `completed_at < NOW() - days` filter.
            let days = retention_days.max(1);
            if days == 0 {
                return Ok(0);
            }
            let mut stores = self.stores.lock().unwrap();
            let before = stores.len();
            stores.retain(|s| {
                let state = s.state.lock().unwrap();
                !matches!(state.status, RunStatus::Completed | RunStatus::Failed)
            });
            Ok((before - stores.len()) as u64)
        }

        async fn request_cancel(&self, _job_name: &str) -> Result<Option<Uuid>, DomainError> {
            let stores = self.stores.lock().unwrap();
            if let Some(s) = stores.last() {
                let mut state = s.state.lock().unwrap();
                if state.status == RunStatus::Running {
                    state.status = RunStatus::CancelRequested;
                    return Ok(Some(s.run_id));
                }
            }
            Ok(None)
        }
    }

    // ─── Handlers ──────────────────────────────────────────────────────────

    struct CompletingHandler;
    #[async_trait]
    impl RecoverableJobHandler for CompletingHandler {
        fn name(&self) -> &str {
            "completer"
        }
        async fn run_resumable(
            &self,
            store: &dyn JobStore,
            _args: &JobRunArgs,
            _resume_cursor: Option<Vec<u8>>,
        ) -> RunOutcome {
            store.checkpoint(vec![1, 2, 3], 5).await.unwrap();
            RunOutcome::completed()
        }
    }

    struct PausingHandler;
    #[async_trait]
    impl RecoverableJobHandler for PausingHandler {
        fn name(&self) -> &str {
            "pauser"
        }
        async fn run_resumable(
            &self,
            _store: &dyn JobStore,
            _args: &JobRunArgs,
            _resume_cursor: Option<Vec<u8>>,
        ) -> RunOutcome {
            RunOutcome::Paused {
                cursor: b"halfway".to_vec(),
            }
        }
    }

    struct FailingHandler;
    #[async_trait]
    impl RecoverableJobHandler for FailingHandler {
        fn name(&self) -> &str {
            "failer"
        }
        async fn run_resumable(
            &self,
            _store: &dyn JobStore,
            _args: &JobRunArgs,
            _resume_cursor: Option<Vec<u8>>,
        ) -> RunOutcome {
            RunOutcome::Failed {
                message: "boom".into(),
            }
        }
    }

    struct ResumeInspectHandler {
        saw_cursor: Arc<Mutex<Option<Vec<u8>>>>,
    }
    #[async_trait]
    impl RecoverableJobHandler for ResumeInspectHandler {
        fn name(&self) -> &str {
            "resumer"
        }
        async fn run_resumable(
            &self,
            _store: &dyn JobStore,
            _args: &JobRunArgs,
            resume_cursor: Option<Vec<u8>>,
        ) -> RunOutcome {
            *self.saw_cursor.lock().unwrap() = resume_cursor;
            RunOutcome::completed()
        }
    }

    // ─── Tests ─────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn fresh_run_completes_and_marks_status_completed() {
        let provider = Arc::new(MemProvider::new());
        let provider_trait: Arc<dyn JobStoreProvider> = provider.clone();

        let outcome = run_or_resume(
            Arc::new(CompletingHandler),
            provider_trait,
            &JobRunArgs::default(),
        )
        .await;

        assert!(outcome.is_ok(), "expected Ok, got {outcome:?}");
        if let JobOutcome::Ok { extra, .. } = outcome {
            assert_eq!(extra["completed"], true);
            assert!(extra["run_id"].is_string());
        }
        assert_eq!(provider.last_status(), Some(RunStatus::Completed));
    }

    #[tokio::test]
    async fn paused_run_persists_cursor_and_marks_status_paused() {
        let provider = Arc::new(MemProvider::new());
        let provider_trait: Arc<dyn JobStoreProvider> = provider.clone();

        let outcome = run_or_resume(
            Arc::new(PausingHandler),
            provider_trait,
            &JobRunArgs::default(),
        )
        .await;

        assert!(outcome.is_ok());
        if let JobOutcome::Ok { extra, .. } = outcome {
            assert_eq!(extra["paused"], true);
            assert_eq!(extra["cursor_hex"], hex::encode(b"halfway"));
        }
        assert_eq!(provider.last_status(), Some(RunStatus::Paused));
        assert_eq!(provider.last_cursor(), Some(b"halfway".to_vec()));
    }

    #[tokio::test]
    async fn failed_run_marks_status_failed_and_returns_err() {
        let provider = Arc::new(MemProvider::new());
        let provider_trait: Arc<dyn JobStoreProvider> = provider.clone();

        let outcome = run_or_resume(
            Arc::new(FailingHandler),
            provider_trait,
            &JobRunArgs::default(),
        )
        .await;

        assert!(!outcome.is_ok(), "expected Err, got {outcome:?}");
        if let JobOutcome::Err { message } = outcome {
            assert!(message.starts_with("boom (run_id="));
        }
        assert_eq!(provider.last_status(), Some(RunStatus::Failed));
    }

    #[tokio::test]
    async fn resume_hands_cursor_back_to_handler() {
        let provider = Arc::new(MemProvider::new());
        let provider_trait: Arc<dyn JobStoreProvider> = provider.clone();

        // Run 1 pauses with cursor.
        run_or_resume(
            Arc::new(PausingHandler),
            provider_trait.clone(),
            &JobRunArgs::default(),
        )
        .await;
        assert_eq!(provider.last_status(), Some(RunStatus::Paused));

        // Run 2 must see resume_cursor = the paused cursor.
        let seen = Arc::new(Mutex::new(None));
        run_or_resume(
            Arc::new(ResumeInspectHandler {
                saw_cursor: seen.clone(),
            }),
            provider_trait,
            &JobRunArgs::default(),
        )
        .await;
        assert_eq!(*seen.lock().unwrap(), Some(b"halfway".to_vec()));
    }

    #[tokio::test]
    async fn concurrent_trigger_hits_already_active() {
        let provider = Arc::new(MemProvider::new());
        let provider_trait: Arc<dyn JobStoreProvider> = provider.clone();

        // Seed a Running row (simulates an in-flight prior dispatch).
        let seeded_run_id = provider.seed_running();

        let outcome = run_or_resume(
            Arc::new(CompletingHandler),
            provider_trait,
            &JobRunArgs::default(),
        )
        .await;

        // Must be Ok with skipped=already_running, NOT a fresh dispatch.
        assert!(outcome.is_ok());
        if let JobOutcome::Ok { extra, .. } = &outcome {
            assert_eq!(extra["skipped"], "already_running");
            assert_eq!(extra["run_id"], seeded_run_id.to_string());
            assert_eq!(extra["status"], "Running");
        }
        // Seeded run's status untouched (no parallel dispatch happened).
        assert_eq!(provider.last_status(), Some(RunStatus::Running));
    }

    #[tokio::test]
    async fn boot_recovery_sweep_flips_running_to_paused() {
        let provider = Arc::new(MemProvider::new());
        let provider_trait: Arc<dyn JobStoreProvider> = provider.clone();

        provider.seed_running();
        provider.seed_running();

        let flipped = provider_trait.boot_recovery_sweep().await.unwrap();
        assert_eq!(flipped, 2);
        assert_eq!(provider.last_status(), Some(RunStatus::Paused));
    }

    #[tokio::test]
    async fn runstatus_parse_is_symmetric() {
        for s in [
            RunStatus::Running,
            RunStatus::Paused,
            RunStatus::CancelRequested,
            RunStatus::Completed,
            RunStatus::Failed,
        ] {
            assert_eq!(RunStatus::parse(s.as_str()), Some(s));
        }
        assert!(RunStatus::parse("garbage").is_none());
    }
}
