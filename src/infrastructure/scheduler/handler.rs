//! The implementor-facing contract for a Part 1 (periodic) job.
//!
//! Everything a native service needs to write to plug into the periodic
//! scheduler is on this page. See `docs/plan/job-registry.md` Part 1
//! for the design rationale and migration criterion (the "operator
//! trigger" question — if an operator would never
//! `POST /api/admin/jobs/{name}/trigger` for this loop, it doesn't
//! belong here; keep it as a core worker).

use async_trait::async_trait;

use super::types::{JobOutcome, JobRunArgs};

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
}
