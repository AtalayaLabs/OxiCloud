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
use super::types::{JobOutcome, JobParam, JobRunArgs, Mutates};

// ─── Run status ─────────────────────────────────────────────────────────────

/// Mirror of the `TEXT` values allowed in `jobs.recoverable_runs.status`.
///
/// Terminal set = `{Completed, Failed, Cancelled}`. Non-terminal set
/// (the one the exclusivity partial unique index scopes) =
/// `{Running, Paused, CancelRequested}`.
///
/// `CancelRequested` IS non-terminal — the run is still shutting down.
/// A second trigger arriving during cancel MUST NOT spawn a parallel
/// run; the trigger endpoint returns the surviving row instead.
///
/// `Cancelled` IS terminal — admin explicitly abandoned the run. Distinct
/// from `Failed` because it's user-driven, not a handler error. Distinct
/// from `Paused` because it's not resumable. Runs land in `Cancelled` via
/// two paths: (1) admin cancel on a Running row (sets
/// `params.cancel_intent = "terminate"` alongside the CancelRequested
/// flip; engine post-processes handler's Paused return → Cancelled), or
/// (2) admin cancel on an already-Paused row (direct DB flip).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum RunStatus {
    Running,
    Paused,
    CancelRequested,
    Completed,
    Failed,
    Cancelled,
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
            RunStatus::Cancelled => "Cancelled",
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
            "Cancelled" => Some(RunStatus::Cancelled),
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

/// Value written to `params.cancel_intent` to tell the engine's
/// terminal-write wrap how to interpret a subsequent
/// [`RunOutcome::Paused`] return. Absent → treat as ordinary pause
/// (write `Paused`). Present with this value → the admin asked to
/// abandon, not just yield, so write `Cancelled` instead.
pub const CANCEL_INTENT_PARAM: &str = "cancel_intent";
pub const CANCEL_INTENT_TERMINATE: &str = "terminate";

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
    /// The ENVIRONMENT failed after a bounded number of attempts, and
    /// this is worth trying again later.
    ///
    /// Lands as `Paused` in the row, so resume works unchanged. What
    /// differs is `error_message`: an operator has to be able to tell "I
    /// paused this" from "the provider went down", and a paused run with
    /// no explanation is an unexplained one.
    ///
    /// Distinct from both neighbours, and the distinction is the point:
    ///
    /// | outcome | meaning | resumes? |
    /// |---|---|---|
    /// | `Failed` | the data or the request is wrong | no — terminal |
    /// | `Paused` | an operator asked it to stop | yes |
    /// | `PausedRetryable` | the environment failed | yes, and says why |
    ///
    /// Reached only after the handler has already retried — see
    /// `retry_transient` — because a single transient error is not news.
    /// The cap exists because no status-based taxonomy can tell a
    /// deterministic 5xx from a passing one (Azurite answers 500 to a
    /// CRC64 ranged GET, every time), so the policy is deliberately
    /// "retry as if transient, then hand the decision to a human".
    PausedRetryable {
        cursor: Vec<u8>,
        reason: String,
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

    /// Turn a failed operation into the right outcome:
    /// [`RunOutcome::PausedRetryable`] when the error is transient,
    /// [`RunOutcome::Failed`] otherwise.
    ///
    /// **This is where step 1's classification pays off.** Handlers
    /// should route every backend error through here rather than
    /// reaching for `Failed` directly, so "the provider is down" stops a
    /// long scan at its cursor instead of discarding it.
    ///
    /// `cursor` is the resume position — normally the same value the
    /// handler last checkpointed. Pass `None` only when nothing has been
    /// settled yet; the run then resumes from the beginning.
    ///
    /// # Why the engine does not add its own retry loop
    ///
    /// The plan sketched bounded backoff *here*. Measuring first showed
    /// two layers already exist below: the AWS SDK retries internally,
    /// and `RetryBlobBackend` wraps every remote backend with its own
    /// exponential backoff (defaults: 3 retries, 100 ms, ×2, 10 s cap —
    /// all env-tunable). A third layer would multiply, not add: one
    /// logical operation could span SDK × decorator × engine attempts,
    /// turning a brief outage into minutes of held `migration_readonly`.
    ///
    /// The plan anticipated exactly this — "do not double-retry … the
    /// AWS SDK already retries internally, so a second layer above it
    /// multiplies" — so the retrying stays where it already is, at the
    /// operation, and the engine supplies the part that was genuinely
    /// missing: converting an exhausted-retry failure into a resumable
    /// pause with a reason instead of a terminal `Failed`.
    ///
    /// Retrying at this level would also mean re-running a scan, not an
    /// operation. Tuning attempts belongs in
    /// `OXICLOUD_STORAGE_RETRY_*`, where it applies per request.
    pub fn from_domain_error(
        cursor: Option<&[u8]>,
        context: &str,
        err: &crate::domain::errors::DomainError,
    ) -> Self {
        if err.is_transient() {
            RunOutcome::PausedRetryable {
                cursor: cursor.map(<[u8]>::to_vec).unwrap_or_default(),
                reason: format!("{context}: {err}"),
            }
        } else {
            RunOutcome::Failed {
                message: format!("{context}: {err}"),
            }
        }
    }
}

/// Write `JobRunArgs` to `params` on a Fresh run, or read them back on a
/// Resumed one.
///
/// Returns the args the handler should actually use. On resume that is
/// whatever the original run recorded, NOT what the resuming caller
/// passed — see the call site in [`run_or_resume`] for why changing mode
/// mid-run is refused.
///
/// Every value is stored as a string, matching the `params` convention the
/// progress fields already use, and each is read back independently: a run
/// paused before its job declared a parameter simply has no key for it, and
/// the declared default applies. That is the safe direction — a resumed
/// legacy run under-acts rather than deleting under a flag nobody gave it.
///
/// **Driven by `declared`, not by a hardcoded list.** The previous version
/// carried `const FLAGS = ["force", "deep", "repair"]` plus a special case
/// for `storage`, so a job growing a parameter had to remember to edit this
/// function — and forgetting meant the parameter was silently dropped on
/// resume, turning a `?repair=true` migration back into a discovery run
/// after a restart. Iterating the declaration makes that unrepresentable.
async fn persist_or_restore_args(
    store: &dyn JobStore,
    declared: &[JobParam],
    args: &JobRunArgs,
    is_fresh: bool,
) -> Result<JobRunArgs, String> {
    if is_fresh {
        // Filter to what THIS job declares rather than persisting whatever
        // the caller handed over. `consistency_batch` forwards its own args
        // verbatim to each sub-job, so without this a tenant's `params`
        // would grow the coordinator's keys — `deep` on a job that has no
        // deep mode — and the run-detail view would claim a mode the job
        // never had.
        let mut effective = std::collections::BTreeMap::new();
        for p in declared {
            let value = args
                .iter()
                .find(|(k, _)| *k == p.name)
                .map(|(_, v)| v.clone())
                .unwrap_or_else(|| p.default.to_value());
            // A `None` string is absent rather than empty, so a run that
            // did not scope itself does not grow a key claiming it did.
            if let Some(v) = value.to_param_string() {
                store
                    .set_string_param(p.name, &v)
                    .await
                    .map_err(|e| format!("persist `{}` to params: {e}", p.name))?;
            }
            effective.insert(p.name.to_string(), value);
        }
        return Ok(JobRunArgs::new(effective));
    }

    let mut restored = std::collections::BTreeMap::new();
    for p in declared {
        let stored = store
            .get_string_param(p.name)
            .await
            .map_err(|e| format!("read `{}` from params: {e}", p.name))?;
        let value = match stored {
            // A value this job wrote itself, so a parse failure means the
            // row was hand-edited or the parameter changed type between
            // releases. Fall back to the default rather than failing the
            // resume — losing the flag is recoverable, refusing to resume a
            // half-finished migration is not.
            Some(raw) => p.parse_value(&raw).unwrap_or_else(|_| {
                tracing::warn!(
                    target: "oxicloud::scheduler",
                    param = p.name,
                    raw = %raw,
                    "stored job parameter does not parse as its declared type; using the default"
                );
                p.default.to_value()
            }),
            None => p.default.to_value(),
        };
        restored.insert(p.name.to_string(), value);
    }
    Ok(JobRunArgs::new(restored))
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

    /// What this job does, for the admin UI.
    ///
    /// English, in the trait, beside the behaviour it describes — not in
    /// `locales/*.json`. A description that lives away from the code rots the
    /// moment a job changes, invisibly, and a translator cannot know what
    /// `manifests_consistency` reconciles. i18n can layer on later keyed by
    /// job name, with this as the fallback, so a missing translation degrades
    /// to English rather than a blank panel.
    ///
    /// Defaulted so adding it to ~15 existing jobs is incremental rather than
    /// one breaking change.
    fn description(&self) -> &'static str {
        ""
    }

    /// Whether a run changes state, and under what conditions.
    ///
    /// Three values rather than a boolean because there are three cases, and
    /// the interesting one is conditional: a job can be read-only by default
    /// and destructive under `?repair=true`. A boolean forces that job to
    /// answer wrongly for one of its two modes — `false` on something that
    /// can delete files is actively misleading.
    fn mutates(&self) -> Mutates {
        Mutates::Never
    }

    /// `Some(..)` when `?repair=true` does something beyond a default run,
    /// describing what it ADDS; `None` when the flag is inert.
    ///
    /// One method rather than a `supports_repair` boolean plus prose: its
    /// presence drives whether the UI offers the toggle, its content drives
    /// the confirmation text. A boolean would leave the frontend to invent
    /// wording for a destructive action it does not understand.
    ///
    /// Independent of [`Self::mutates`], not derived from it — the import
    /// jobs are [`Mutates::Always`] *and* repair-capable, inserting rows on a
    /// plain run and additionally unlinking files under repair.
    fn repair_description(&self) -> Option<&'static str> {
        None
    }

    /// The run parameters this job accepts. See
    /// [`JobHandler::parameters`](super::handler::JobHandler::parameters).
    ///
    /// Matters more here than for a plain job: `run_or_resume` persists
    /// these so a Paused run resumes with the same parameters it started
    /// under. The engine iterates this declaration to do it, so an
    /// undeclared parameter is not merely ignored — it is lost across a
    /// resume, which is how a `?repair=true` migration could come back
    /// as discovery-only after a restart.
    fn parameters(&self) -> &'static [JobParam] {
        &[]
    }

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
    /// `severity` — one of:
    /// - `"data_loss"` — bytes / rows unreachable or gone.
    /// - `"inconsistent"` — counters or materialised values wrong,
    ///   content intact.
    /// - `"anomaly"` — surprising state worth surfacing, no known impact.
    ///   This is the level the admin panel labels "notices"; there is no
    ///   separate `notice` severity, and a job that acted on what it found
    ///   says so in `detail` rather than in a fourth severity that would
    ///   render identically.
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
    /// [`RunOutcome::PausedRetryable`]. Handler code MUST NOT call this.
    ///
    /// Writes `status = Paused` — so resume is the same operation — plus
    /// `error_message = reason`. The reason is the whole point: without
    /// it the panel cannot distinguish an operator pause from a provider
    /// outage, and a paused migration holding `migration_readonly` looks
    /// like someone forgot about it.
    ///
    /// Separate method rather than an extra argument on
    /// [`Self::mark_paused`] because the two carry different meaning and
    /// only one of them writes `error_message`. A `reason: Option<&str>`
    /// parameter would let a caller write a Paused row with an
    /// error message and no error, which is the state this exists to
    /// distinguish from.
    async fn mark_paused_retryable(
        &self,
        cursor: Option<Vec<u8>>,
        reason: &str,
    ) -> Result<(), DomainError>;

    /// Engine-only. Called by [`run_or_resume`] on
    /// [`RunOutcome::Failed`]. Handler code MUST NOT call this.
    async fn mark_failed(&self, message: &str) -> Result<(), DomainError>;

    /// Engine-only. Called by [`run_or_resume`] when the handler
    /// returns [`RunOutcome::Paused`] AND
    /// `params.cancel_intent = "terminate"` — the admin asked to
    /// abandon the run, not just yield. Writes `status = 'Cancelled'`
    /// + `completed_at = NOW()`. Preserves the cursor for post-mortem
    /// (an operator can see how far it got before being killed).
    /// Handler code MUST NOT call this.
    async fn mark_cancelled(&self, cursor: Option<Vec<u8>>) -> Result<(), DomainError>;
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

    /// Request TERMINAL cancellation — admin abandons the run rather
    /// than yielding it for later resume. Two paths depending on the
    /// current row's status:
    ///
    /// - **`Running` / `CancelRequested`** — same DB flip as
    ///   [`Self::request_cancel`] (Running → CancelRequested) BUT
    ///   also stamps `params.cancel_intent = "terminate"`. When the
    ///   handler yields and the engine wraps `RunOutcome::Paused`, it
    ///   reads the intent and calls
    ///   [`JobStore::mark_cancelled`] instead of `mark_paused`.
    /// - **`Paused`** — no handler is running, so the engine wrap
    ///   never fires. Direct DB flip `Paused → Cancelled +
    ///   completed_at = NOW()`.
    /// - **Terminal or absent** — no-op (`Ok(None)`).
    ///
    /// Returns the affected run's id when any transition happened,
    /// `None` otherwise.
    async fn request_terminal_cancel(&self, job_name: &str) -> Result<Option<Uuid>, DomainError>;

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

    // Bind the run to the flags it started with.
    //
    // A Fresh run records its `JobRunArgs` in `params`; a Resumed run reads
    // them back and runs with THOSE, ignoring whatever the resuming caller
    // passed. Two reasons, and the engine is the only place both are
    // guaranteed:
    //
    // **A resumed run must not change mode.** Handlers read `args` on every
    // call, so a paused `?repair=true` import resumed by a plain trigger
    // silently continued as import-only — the deletion half never finished
    // and nothing said so. The same held for `?deep=true`: a paused bit-rot
    // scan resumed shallow while still reporting as the run that started
    // deep. Fixing it per-handler meant every job remembering, and three of
    // them did not.
    //
    // **The run row should say what it did.** For a destructive job, "did
    // this run delete anything?" is answerable only from `params`, and that
    // is what an operator reads afterwards.
    //
    // Deliberately NOT overridable on resume. Adding `?repair=true` to a
    // resume would apply it to the remaining entries only, producing a run
    // that half-deleted — the honest way to change your mind is to cancel
    // and start fresh.
    let args = match persist_or_restore_args(&*store, job.parameters(), args, is_fresh).await {
        Ok(effective) => effective,
        Err(e) => {
            // Fail the run rather than guess. Proceeding would mean acting
            // under flags nothing recorded, which for the jobs that delete
            // is the one thing worth refusing.
            log_terminal_write_err("mark_failed", run_id, store.mark_failed(&e).await);
            return JobOutcome::err(e);
        }
    };

    // Dispatch. Terminal writes to `jobs.recoverable_runs` happen
    // here (NOT in the handler) so the row always ends in a state
    // that matches what the handler returned.
    let outcome = job.run_resumable(&*store, &args, resume_cursor).await;

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
            // Read the intent stamped by `/api/admin/jobs/{name}/cancel`
            // (terminal cancel path). Absent → ordinary pause. Present
            // with `terminate` → admin asked to abandon; write
            // Cancelled instead of Paused. Any read error falls
            // through to Paused — errs on preserving-progress side.
            let terminate = store
                .get_string_param(CANCEL_INTENT_PARAM)
                .await
                .ok()
                .flatten()
                .as_deref()
                == Some(CANCEL_INTENT_TERMINATE);
            let cursor_hex = hex::encode(&cursor);
            if terminate {
                log_terminal_write_err(
                    "mark_cancelled",
                    run_id,
                    store.mark_cancelled(Some(cursor)).await,
                );
                JobOutcome::ok_with(
                    stats.finding_count,
                    serde_json::json!({
                        "cancelled":         true,
                        "run_id":            run_id.to_string(),
                        "cursor_hex":        cursor_hex,
                        "finding_count":     stats.finding_count,
                        "scanned_count":     stats.scanned_count,
                        "severity_counts":   stats.by_severity,
                    }),
                )
            } else {
                log_terminal_write_err(
                    "mark_paused",
                    run_id,
                    store.mark_paused(Some(cursor)).await,
                );
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
        }
        RunOutcome::PausedRetryable { cursor, reason } => {
            let cursor_hex = hex::encode(&cursor);
            log_terminal_write_err(
                "mark_paused_retryable",
                run_id,
                store.mark_paused_retryable(Some(cursor), &reason).await,
            );
            // Audited, not merely logged. Writes are refused app-wide
            // while `backend_migration` holds `migration_readonly`, so a
            // run that stopped on a provider outage is an operational
            // event someone has to act on — and "why is the app
            // read-only" must be answerable afterwards.
            tracing::info!(
                target: "audit",
                event = "job.paused_retryable",
                reason = "backend_unavailable",
                job = %job.name(),
                run_id = %run_id,
                cursor_hex = %cursor_hex,
                detail = %reason,
                "👮🏻‍♂️ `{}` paused after exhausting retries: {reason}",
                job.name(),
            );
            // `ok`, not `err`: the run did not fail, it stopped and can
            // be resumed. Reporting it as an error would put a red job
            // in the panel that a Resume click fixes, which reads as a
            // bug rather than as a decision waiting to be made.
            JobOutcome::ok_with(
                stats.finding_count,
                serde_json::json!({
                    "paused":            true,
                    "retryable":         true,
                    "reason":            reason,
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

    // The registry only ever sees `dyn JobHandler`, so the tenant's own
    // metadata has to be forwarded through the wrapper or it is invisible
    // to `GET /api/admin/jobs`. Silently returning the JobHandler defaults
    // here would leave every recoverable job undescribed and reported as
    // read-only — including ones that delete files.
    //
    // EVERY metadata method the tenant can declare belongs here. Adding
    // one to `RecoverableJobHandler` without adding it below compiles
    // cleanly — both traits have defaults — and the tenant's value is
    // then simply lost. `parameters` shipped that way for exactly one
    // boot: the default `&[]` made the trigger endpoint reject
    // `?repair=true` on the very jobs that declare it, and
    // `OXICLOUD_STARTUP_JOBS` panicked at startup with "this job accepts
    // none". Pinned by `adapter_forwards_tenant_metadata`.
    fn description(&self) -> &'static str {
        self.inner.description()
    }
    fn mutates(&self) -> Mutates {
        self.inner.mutates()
    }
    fn repair_description(&self) -> Option<&'static str> {
        self.inner.repair_description()
    }
    fn parameters(&self) -> &'static [JobParam] {
        self.inner.parameters()
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
        async fn mark_paused_retryable(
            &self,
            cursor: Option<Vec<u8>>,
            reason: &str,
        ) -> Result<(), DomainError> {
            let mut s = self.state.lock().unwrap();
            s.status = RunStatus::Paused;
            // Both, deliberately: Paused so resume works, `error_message`
            // so a test can assert the two pause shapes are
            // distinguishable — which is the whole reason the variant
            // exists.
            s.error_message = Some(reason.to_string());
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
        async fn mark_cancelled(&self, cursor: Option<Vec<u8>>) -> Result<(), DomainError> {
            let mut s = self.state.lock().unwrap();
            s.status = RunStatus::Cancelled;
            if let Some(c) = cursor {
                s.cursor = Some(c);
            }
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

        /// Test-only read — last-created run's `error_message`. What
        /// separates an operator pause from a provider outage: both are
        /// `Paused`, only one carries a reason.
        fn last_error_message(&self) -> Option<String> {
            let stores = self.stores.lock().unwrap();
            stores
                .last()
                .and_then(|s| s.state.lock().unwrap().error_message.clone())
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
                !matches!(
                    state.status,
                    RunStatus::Completed | RunStatus::Failed | RunStatus::Cancelled
                )
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

        async fn request_terminal_cancel(
            &self,
            _job_name: &str,
        ) -> Result<Option<Uuid>, DomainError> {
            let stores = self.stores.lock().unwrap();
            if let Some(s) = stores.last() {
                let mut state = s.state.lock().unwrap();
                match state.status {
                    RunStatus::Paused => {
                        state.status = RunStatus::Cancelled;
                        return Ok(Some(s.run_id));
                    }
                    RunStatus::Running | RunStatus::CancelRequested => {
                        state.status = RunStatus::CancelRequested;
                        state.string_params.insert(
                            CANCEL_INTENT_PARAM.to_string(),
                            CANCEL_INTENT_TERMINATE.to_string(),
                        );
                        return Ok(Some(s.run_id));
                    }
                    _ => {}
                }
            }
            Ok(None)
        }
    }

    // ─── Handlers ──────────────────────────────────────────────────────────

    /// Hits a transient backend error partway through, exactly as a
    /// remote backend does once its own retry decorator has given up.
    struct TransientlyFailingHandler;
    #[async_trait]
    impl RecoverableJobHandler for TransientlyFailingHandler {
        fn name(&self) -> &str {
            "transient_failer"
        }
        async fn run_resumable(
            &self,
            store: &dyn JobStore,
            _args: &JobRunArgs,
            _resume_cursor: Option<Vec<u8>>,
        ) -> RunOutcome {
            store.checkpoint(vec![9, 9], 3).await.unwrap();
            RunOutcome::from_domain_error(
                Some(&[9, 9]),
                "backend enumeration failed on s3",
                &crate::domain::errors::DomainError::transient_backend("S3", "503 SlowDown"),
            )
        }
    }

    /// Same shape, but a permanent fault — the control that proves the
    /// classification is doing the work rather than everything pausing.
    struct PermanentlyFailingHandler;
    #[async_trait]
    impl RecoverableJobHandler for PermanentlyFailingHandler {
        fn name(&self) -> &str {
            "permanent_failer"
        }
        async fn run_resumable(
            &self,
            _store: &dyn JobStore,
            _args: &JobRunArgs,
            _resume_cursor: Option<Vec<u8>>,
        ) -> RunOutcome {
            RunOutcome::from_domain_error(
                Some(&[9, 9]),
                "backend enumeration failed on s3",
                &crate::domain::errors::DomainError::internal_error("S3", "403 AccessDenied"),
            )
        }
    }

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

    /// The registry only ever sees `dyn JobHandler`, so a recoverable
    /// tenant's metadata reaches `GET /api/admin/jobs` only if the adapter
    /// forwards it. Falling back to the `JobHandler` defaults here would
    /// report every recoverable job as undescribed and read-only —
    /// including the imports, which delete files under repair.
    #[tokio::test]
    async fn adapter_forwards_job_metadata_from_inner_handler() {
        struct Annotated;
        #[async_trait]
        impl RecoverableJobHandler for Annotated {
            fn name(&self) -> &str {
                "annotated"
            }
            async fn run_resumable(
                &self,
                _store: &dyn JobStore,
                _args: &JobRunArgs,
                _resume_cursor: Option<Vec<u8>>,
            ) -> RunOutcome {
                RunOutcome::completed()
            }
            fn description(&self) -> &'static str {
                "walks a thing"
            }
            fn mutates(&self) -> Mutates {
                Mutates::OnRepairOnly
            }
            fn repair_description(&self) -> Option<&'static str> {
                Some("fixes the thing")
            }
            fn parameters(&self) -> &'static [JobParam] {
                const PARAMS: &[JobParam] = &[JobParam::boolean("repair", false, "fix the thing")];
                PARAMS
            }
        }

        let provider: Arc<dyn JobStoreProvider> = Arc::new(MemProvider::new());
        let adapter = RecoverableAdapter::new(Arc::new(Annotated), provider);
        let as_handler: &dyn JobHandler = &adapter;

        assert_eq!(as_handler.description(), "walks a thing");
        assert_eq!(as_handler.mutates(), Mutates::OnRepairOnly);
        assert_eq!(as_handler.repair_description(), Some("fixes the thing"));

        // Regression: this one was NOT forwarded when `parameters` was
        // added, and both traits having defaults meant it compiled
        // silently. The registry then saw `&[]`, so the trigger endpoint
        // rejected `?repair=true` on the jobs that declare it and
        // `OXICLOUD_STARTUP_JOBS=thumb_derived_import?repair=true`
        // panicked at boot with "this job accepts none".
        assert_eq!(
            as_handler.parameters().len(),
            1,
            "tenant parameters must reach the registry through the adapter"
        );
        assert_eq!(as_handler.parameters()[0].name, "repair");
    }

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

    /// A transient backend failure must PAUSE with a reason, not fail.
    ///
    /// This is the whole point of the plan: `Failed` is terminal, so an
    /// outage used to discard a partially-complete migration. The run has
    /// to keep its cursor and stay resumable, and it has to say why it
    /// stopped — a paused `backend_migration` still holds
    /// `migration_readonly`, refusing writes application-wide, so
    /// "someone paused this" and "the provider went down" cannot look
    /// alike.
    #[tokio::test]
    async fn transient_failure_pauses_with_a_reason_and_keeps_the_cursor() {
        let provider = Arc::new(MemProvider::new());
        let provider_trait: Arc<dyn JobStoreProvider> = provider.clone();

        let outcome = run_or_resume(
            Arc::new(TransientlyFailingHandler),
            provider_trait,
            &JobRunArgs::default(),
        )
        .await;

        // Reported Ok, not Err: the run did not fail, it stopped and can
        // be resumed. A red job that a Resume click fixes reads as a bug
        // rather than a decision waiting to be made.
        assert!(outcome.is_ok(), "expected Ok, got {outcome:?}");
        if let JobOutcome::Ok { extra, .. } = outcome {
            assert_eq!(extra["paused"], true);
            assert_eq!(extra["retryable"], true);
            assert!(
                extra["reason"].as_str().unwrap().contains("503"),
                "the reason must reach the panel: {extra:?}"
            );
        }

        assert_eq!(provider.last_status(), Some(RunStatus::Paused));
        assert_eq!(
            provider.last_cursor(),
            Some(vec![9, 9]),
            "resume position must survive, or the outage costs the whole scan"
        );
        let msg = provider.last_error_message().expect("reason recorded");
        assert!(msg.contains("503"), "error_message names the cause: {msg}");
    }

    /// The control: a permanent fault still fails terminally. Without
    /// this the classification could be doing nothing and everything
    /// would simply pause, which looks like success in the test above.
    #[tokio::test]
    async fn permanent_failure_still_fails_terminally() {
        let provider = Arc::new(MemProvider::new());
        let provider_trait: Arc<dyn JobStoreProvider> = provider.clone();

        let outcome = run_or_resume(
            Arc::new(PermanentlyFailingHandler),
            provider_trait,
            &JobRunArgs::default(),
        )
        .await;

        assert!(!outcome.is_ok(), "a 403 must not be retried forever");
        assert_eq!(provider.last_status(), Some(RunStatus::Failed));
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
