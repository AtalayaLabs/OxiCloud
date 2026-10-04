//! Expiring chunked-upload sessions, as a job rather than a detached
//! task.
//!
//! **Why this was promoted.** The sweep used to be a `tokio::spawn` at
//! `ChunkedUploadService` construction, ticking hourly on a hand-rolled
//! `tokio::time::interval`. That gave it no admin trigger, no run
//! history, no findings and no visible progress — the exact situation
//! `spawn_legacy_rechunk` was promoted out of when it became
//! `backend_rechunk`, and the argument transfers without modification.
//!
//! It arguably matters more here. The failure this guards is the one
//! `storage_cleanup_check.sh` names outright — *"which under sustained
//! sync workloads is the classic 'disk fills up over the weekend'
//! failure mode"* — and an operator watching that happen had no way to
//! ask how many sessions were reaped, how many unlinks failed, or
//! whether the loop was running at all. Now all three are on the run.
//!
//! **Why not a recoverable job.** `backend_rechunk` is recoverable
//! because it walks a database cursor that survives a restart. This
//! walks an in-memory `DashMap` and a directory listing: there is no
//! cursor to persist, and a pass that stopped halfway is simply redone
//! an hour later at no cost. A plain [`JobHandler`] with counts in the
//! outcome is the honest shape, and it answers the three questions
//! above.
//!
//! One deviation from `docs/plan/failure-classification-and-satellite-lifecycles.md`
//! Part C, which asked for the per-session failures as *findings*:
//! findings belong to runs in `jobs.run_findings`, which only the
//! recoverable engine writes. Rather than make the job recoverable for
//! the sake of a finding row, the failures are an audited event with a
//! stable `reason` plus a count on the run — the same information,
//! reachable the same two ways (audit log, run history).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use dashmap::DashMap;

use crate::infrastructure::scheduler::{JobHandler, JobOutcome, JobRegistry, JobRunArgs, Mutates};
use crate::infrastructure::services::chunked_upload_service::{
    ChunkedUploadService, UploadSession,
};

pub struct UploadsCleanup {
    /// The live map, shared with the service rather than copied — the
    /// job must see sessions created after it was registered.
    sessions: Arc<DashMap<String, UploadSession>>,
    temp_base_dir: PathBuf,
}

impl UploadsCleanup {
    pub const JOB_NAME: &'static str = "uploads_cleanup";

    pub fn new(sessions: Arc<DashMap<String, UploadSession>>, temp_base_dir: PathBuf) -> Self {
        Self {
            sessions,
            temp_base_dir,
        }
    }

    /// Hourly, matching the interval the detached task used. Sessions
    /// expire after 24 h, so the cadence only bounds how long a dead
    /// session's bytes sit on disk — an hour of slack on a 24 h window.
    fn interval() -> Duration {
        Duration::from_secs(3600)
    }

    pub async fn register(self: Arc<Self>, registry: &JobRegistry) -> Arc<Self> {
        registry
            .register(self.clone(), Some(Self::interval()), None)
            .await;
        self
    }
}

#[async_trait]
impl JobHandler for UploadsCleanup {
    fn name(&self) -> &str {
        Self::JOB_NAME
    }

    fn description(&self) -> &'static str {
        "Removes chunked-upload sessions past their 24 h expiry window and \
         their chunk directories, plus orphaned directories under the \
         upload root that no live session claims. Without it, abandoned \
         uploads accumulate on disk — the classic 'disk fills up over the \
         weekend' failure under sustained sync load. Replaces an hourly \
         background task that had no run history and no way to report \
         what it had done."
    }

    fn mutates(&self) -> Mutates {
        // Always: removing expired sessions IS the job, and there is no
        // discovery-only mode worth having — the same reasoning
        // `job_runs_cleanup` and `backend_reclaim` carry.
        Mutates::Always
    }

    async fn run(&self, _args: &JobRunArgs) -> JobOutcome {
        let report = ChunkedUploadService::cleanup_once(&self.sessions, &self.temp_base_dir).await;

        // `count` is what was reclaimed; the breakdown goes in `extra`.
        // A pass that failed every unlink still reports `ok` — the job
        // ran and did what it could — but `failures` is non-zero and the
        // audit lines name each path, which is the difference between
        // "nothing to do" and "nothing worked".
        JobOutcome::ok_with(
            report.sessions_expired + report.orphan_dirs_removed,
            serde_json::json!({
                "sessions_expired":    report.sessions_expired,
                "orphan_dirs_removed": report.orphan_dirs_removed,
                "failures":            report.failures,
                "sessions_live":       self.sessions.len(),
            }),
        )
    }
}
