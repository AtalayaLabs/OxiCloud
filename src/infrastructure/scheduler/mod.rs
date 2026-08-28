//! Periodic job scheduler (Part 1 of the job-registry plan).
//!
//! In-memory registration + one-supervisor-task dispatch. Fires
//! registered [`JobHandler`] implementations at their configured
//! intervals with panic containment, timeout enforcement, and
//! same-name exclusivity.
//!
//! # For future implementors
//!
//! - **You write a `JobHandler`.** Implement [`JobHandler::name`] +
//!   [`JobHandler::run`] on your service. Nothing else. See
//!   [`handler`] for the guidance doc-comment.
//! - **DI wires the registration.** In `common/di.rs` (or wherever the
//!   composition root lives), build an `Arc<JobRegistry>` once, register
//!   every service that opts in, then call [`SchedulerEngine::start`].
//! - **Should this loop actually be a scheduler job?** See the migration
//!   criterion in `docs/plan/job-registry.md` — the primary question
//!   is "would an operator plausibly trigger this manually?". Continuous
//!   drains and event-reactive workers stay as their own loops.
//!
//! Part 2 (recoverable-run engine, DB-backed cursor + resume) is
//! designed but not yet implemented. When it lands it will slot in
//! as a sibling module without changing anything here.

mod engine;
mod handler;
mod pg_job_store;
mod recoverable;
mod registry;
mod types;

pub use engine::SchedulerEngine;
pub use handler::JobHandler;
pub use pg_job_store::{PgJobStore, PgJobStoreProvider};
pub use recoverable::{
    Finding, JobStore, JobStoreProvider, OpenedRun, ProgressKind, RecoverableAdapter,
    RecoverableJobHandler, RunOutcome, RunProgress, RunStatus, RunSummary, derive_progress,
    record_or_log, run_or_resume,
};
pub use registry::{JobEntry, JobRegistry, JobSummary, PausedRunBrief, RegisterError};
pub use types::{ErrCause, JobOutcome, JobRunArgs, Mutates};
