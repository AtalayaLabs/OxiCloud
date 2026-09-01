//! The implementor-facing contract for a Part 1 (periodic) job.
//!
//! Everything a native service needs to write to plug into the periodic
//! scheduler is on this page. See `docs/plan/job-registry.md` Part 1
//! for the design rationale and migration criterion (the "operator
//! trigger" question — if an operator would never
//! `POST /api/admin/jobs/{name}/trigger` for this loop, it doesn't
//! belong here; keep it as a core worker).

use async_trait::async_trait;

use super::types::{JobOutcome, JobRunArgs, Mutates};

/// Implemented by every service that wants to run on a fixed interval
/// through the periodic scheduler.
///
/// # Design shape
///
/// A single method, `run()`. One tick = one call. The supervisor:
/// - fires it at the registered interval,
/// - catches panics (bad handlers crash their own run, not the scheduler),
/// - enforces the configured wall-clock timeout (if any),
/// - enforces exclusivity — a second tick that fires while a previous
///   run is still executing is **skipped, not queued**, with a
///   `job.tick_skipped` warning emitted (the operator signal that the
///   job is chronically slower than its cadence).
///
/// Implementors write the body of `run()`. Everything else — logging,
/// timing, panic containment, exclusivity — is the supervisor's job.
///
/// # `name()` guidance
///
/// Return a stable, unique snake_case identifier. Log lines
/// (`job = %name`), admin listing, admin trigger URLs
/// (`POST /api/admin/jobs/{name}/trigger`) and env vars
/// (`OXICLOUD_JOB_<NAME>_INTERVAL_HOURS`) all key on this. Renaming
/// after release is a breaking change to operator scripts and log
/// dashboards.
///
/// # `run()` guidance
///
/// Return [`JobOutcome::Ok`] with a `count` scalar the operator finds
/// meaningful (rows swept, blobs GC'd, bytes reclaimed) plus optional
/// `extra` JSON. Return [`JobOutcome::Err`] on failure — the
/// supervisor logs it under `outcome=err, cause=handler` and moves
/// on; the next tick fires normally.
///
/// **Do not** catch panics inside `run()` — the supervisor does it,
/// and hiding one loses the `cause=panicked` diagnostic signal.
///
/// **Do not** call `tokio::time::sleep` for long durations inside
/// `run()` if you have a `timeout` configured — the timeout fires
/// mid-sleep and kills the run with `cause=timeout`. Use short polling
/// intervals or restructure the work.
///
/// # Reference implementation
///
/// See `TrashCleanupService::run` (once migrated) as the canonical
/// example: reads its own configuration, runs bounded work, returns
/// a count. Everything else is boilerplate the scheduler owns.
#[async_trait]
pub trait JobHandler: Send + Sync {
    /// Stable, unique snake_case identifier. Must be unique across
    /// the process; the registry rejects duplicate registration.
    fn name(&self) -> &str;

    /// One execution. Called at the registered interval and (optionally)
    /// on admin trigger.
    ///
    /// `args` carries per-dispatch parameters (`force: bool` today).
    /// Periodic ticks pass [`JobRunArgs::default()`]; admin triggers
    /// forward query params such as `?force=true`. Handlers that don't
    /// understand a given arg silently ignore it — the arg exists to
    /// give per-job acceleration semantics without spreading per-job
    /// knowledge into every caller.
    ///
    /// See trait-level docs for guidance on when to return Ok vs Err.
    async fn run(&self, args: &JobRunArgs) -> JobOutcome;

    /// `true` iff this handler persists per-run rows to
    /// `jobs.recoverable_runs` (cursor + findings + resume). Surfaced
    /// on [`crate::infrastructure::scheduler::registry::JobSummary`]
    /// so the admin UI can decide whether the row is expandable to
    /// show a run history + findings drawer, without hardcoding a
    /// name-based allowlist.
    ///
    /// Default is `false` — Part 1 periodic handlers (`TrashCleanup`,
    /// `StorageReconcile`, `GrantCleanup`, `DedupGc`) don't have runs
    /// or findings. `RecoverableAdapter` overrides to `true` so every
    /// tenant registered via `register_recoverable_job` flips the flag
    /// automatically at registration time.
    fn is_recoverable(&self) -> bool {
        false
    }

    /// What this job does, in one or two sentences, for the admin UI.
    ///
    /// English, in the trait, beside the behaviour it describes — not in
    /// `locales/*.json`. A description that lives away from the code rots
    /// the moment a job changes, invisibly, and a translator cannot know
    /// what `manifests_consistency` reconciles. i18n can layer on later
    /// keyed by job name with this as the fallback, so a missing
    /// translation degrades to English rather than to a blank panel.
    ///
    /// Defaulted to `""` so adding it to the existing jobs is incremental
    /// rather than one breaking change; the UI omits the line when empty.
    fn description(&self) -> &'static str {
        ""
    }

    /// Whether a run changes state, and under what conditions. See
    /// [`Mutates`] for why this is not a boolean.
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
    /// Independent of [`Self::mutates`], not derived from it — the thumbnail
    /// import jobs are [`Mutates::Always`] *and* repair-capable, inserting
    /// rows on a plain run and additionally unlinking sidecars under repair.
    fn repair_description(&self) -> Option<&'static str> {
        None
    }
}
