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
    Completed,
    Paused { cursor: Vec<u8> },
    Failed { message: String },
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
    match job.run_resumable(&*store, args, resume_cursor).await {
        RunOutcome::Completed => {
            log_terminal_write_err("mark_completed", run_id, store.mark_completed().await);
            JobOutcome::ok_with(
                0,
                serde_json::json!({
                    "completed": true,
                    "run_id": run_id.to_string(),
                }),
            )
        }
        RunOutcome::Paused { cursor } => {
            let cursor_hex = hex::encode(&cursor);
            log_terminal_write_err("mark_paused", run_id, store.mark_paused(Some(cursor)).await);
            JobOutcome::ok_with(
                0,
                serde_json::json!({
                    "paused": true,
                    "run_id": run_id.to_string(),
                    "cursor_hex": cursor_hex,
                }),
            )
        }
        RunOutcome::Failed { message } => {
            log_terminal_write_err("mark_failed", run_id, store.mark_failed(&message).await);
            JobOutcome::err(format!("{message} (run_id={run_id})"))
        }
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
            RunOutcome::Completed
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
            RunOutcome::Completed
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
