//! `job_runs_cleanup` scheduled job — daily retention sweep over the
//! scheduler's own history.
//!
//! Deletes terminal rows from `jobs.recoverable_runs` older than the
//! retention window; their findings go with them through the CASCADE FK.
//! Non-terminal runs are preserved unconditionally — a `Paused` run is
//! resumable work, not history, however long it has been sitting there.
//!
//! **Why this exists.** The purge has been implemented since the
//! recoverable engine landed, but reachable only from
//! `POST /api/admin/jobs/runs/purge` — "not periodic; admins fire this
//! when they want to reclaim space". That is the `dedup_gc` shape, and
//! `dedup_gc` is how 29 orphaned blobs accumulated: a capability nobody
//! triggers does not run. Scheduling `consistency_batch` made it urgent
//! rather than theoretical, since a weekly sweep writes a run row per
//! detector and a finding row per problem, for as long as the problem
//! persists.
//!
//! Retention comes from the declared `retention_days` parameter
//! (default 30, matching what the admin endpoint has always used). No
//! dedicated env var: `OXICLOUD_SCHEDULED_JOBS` already carries
//! parameters, so `job_runs_cleanup=24h?retention_days=90` is the
//! override, and there is no second spelling of the same setting to
//! disagree with the first.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tracing::info;

use crate::infrastructure::scheduler::{
    JobHandler, JobOutcome, JobParam, JobRegistry, JobRunArgs, JobStoreProvider, Mutates,
};

/// Kept at module scope so `parameters()` can return a `'static` slice.
static PARAMETERS: [JobParam; 1] = [JobParam::number(
    "retention_days",
    DEFAULT_RETENTION_DAYS,
    "Delete completed, failed and cancelled runs older than this many \
     days, along with their findings. Paused and running runs are never \
     touched.",
)];

/// The same 30 the admin purge endpoint has always defaulted to. One
/// literal, so the button and the schedule cannot disagree about what
/// "the default retention" means.
const DEFAULT_RETENTION_DAYS: i64 = 30;

pub struct JobRunsCleanupService {
    provider: Arc<dyn JobStoreProvider>,
}

impl JobRunsCleanupService {
    pub const JOB_NAME: &'static str = "job_runs_cleanup";

    pub fn new(provider: Arc<dyn JobStoreProvider>) -> Self {
        Self { provider }
    }

    /// Daily. Retention is a "days" concept, so a finer cadence buys
    /// nothing — same tier as `trash_cleanup` and `notifications_cleanup`.
    fn interval() -> Duration {
        Duration::from_secs(24 * 3600)
    }

    pub async fn register(self: Arc<Self>, registry: &JobRegistry) -> Arc<Self> {
        registry
            .register(self.clone(), Some(Self::interval()), None)
            .await;
        self
    }
}

#[async_trait]
impl JobHandler for JobRunsCleanupService {
    fn name(&self) -> &str {
        Self::JOB_NAME
    }

    fn description(&self) -> &'static str {
        "Deletes completed, failed and cancelled job runs older than the \
         retention window (default 30 days), along with their findings. \
         Paused and running jobs are preserved — a paused run is work \
         waiting to resume, not history. Without this the scheduler's own \
         tables grow without bound, which the weekly consistency sweep \
         would otherwise do a little of every week, forever."
    }

    fn mutates(&self) -> Mutates {
        // Always: deleting old history IS the job, there is no
        // discovery-only mode worth having. Not gated behind `repair`
        // for the same reason `backend_reclaim` is not — a read-only run
        // would do nothing at all.
        Mutates::Always
    }

    fn parameters(&self) -> &'static [JobParam] {
        &PARAMETERS
    }

    async fn run(&self, args: &JobRunArgs) -> JobOutcome {
        // Clamped to at least 1: a zero window would delete runs that
        // finished seconds ago, including the batch run an operator is
        // currently reading the findings of.
        let retention_days = args
            .get_number("retention_days", DEFAULT_RETENTION_DAYS)
            .max(1);

        match self
            .provider
            .purge_terminal_runs(retention_days as i32)
            .await
        {
            Ok(purged) => {
                // Audited rather than merely logged, and deliberately
                // the SAME event name the admin endpoint emits: "where
                // did that run history go?" should have one answer to
                // grep for, whether a human or the schedule removed it.
                info!(
                    target: "audit",
                    event = "jobs.runs_purged",
                    reason = "retention_sweep",
                    retention_days,
                    purged,
                    "🧹 job-run retention sweep: {purged} run(s) purged (retention {retention_days} d)"
                );
                JobOutcome::ok_with(
                    purged,
                    serde_json::json!({
                        "retention_days": retention_days,
                        "purged":         purged,
                    }),
                )
            }
            Err(e) => JobOutcome::err(format!("job runs cleanup failed: {e}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    use crate::common::errors::DomainError;
    use crate::infrastructure::scheduler::{Finding, OpenedRun, RunSummary};
    use uuid::Uuid;

    /// Records the retention window it was asked for; every other method
    /// is unreachable from this job.
    struct SpyProvider {
        asked: Mutex<Option<i32>>,
    }

    #[async_trait]
    impl JobStoreProvider for SpyProvider {
        async fn purge_terminal_runs(&self, retention_days: i32) -> Result<u64, DomainError> {
            *self.asked.lock().unwrap() = Some(retention_days);
            Ok(7)
        }
        async fn open_or_start(
            &self,
            _job_name: &str,
            _unattended: bool,
        ) -> Result<OpenedRun, DomainError> {
            unreachable!("cleanup is a plain JobHandler — it opens no run")
        }
        async fn boot_recovery_sweep(&self) -> Result<u64, DomainError> {
            unreachable!()
        }
        async fn list_runs(
            &self,
            _job_name: &str,
            _limit: u32,
        ) -> Result<Vec<RunSummary>, DomainError> {
            unreachable!()
        }
        async fn get_run_by_id(&self, _run_id: Uuid) -> Result<Option<RunSummary>, DomainError> {
            unreachable!()
        }
        async fn request_cancel(&self, _job_name: &str) -> Result<Option<Uuid>, DomainError> {
            unreachable!()
        }
        async fn request_terminal_cancel(
            &self,
            _job_name: &str,
        ) -> Result<Option<Uuid>, DomainError> {
            unreachable!()
        }
        async fn list_findings(
            &self,
            _run_id: Uuid,
            _limit: u32,
            _offset: u32,
        ) -> Result<Vec<Finding>, DomainError> {
            unreachable!()
        }
        async fn finding_severity_counts(
            &self,
            _run_id: Uuid,
        ) -> Result<Vec<(String, u64)>, DomainError> {
            unreachable!()
        }
    }

    async fn run_with(args: JobRunArgs) -> (JobOutcome, Option<i32>) {
        let spy = Arc::new(SpyProvider {
            asked: Mutex::new(None),
        });
        let job = JobRunsCleanupService::new(spy.clone());
        let outcome = job.run(&args.normalized_for(job.parameters())).await;
        let asked = *spy.asked.lock().unwrap();
        (outcome, asked)
    }

    /// A scheduled tick passes no parameters, so the declared default is
    /// what actually runs every night.
    #[tokio::test]
    async fn a_bare_tick_uses_the_thirty_day_default() {
        let (outcome, asked) = run_with(JobRunArgs::default()).await;
        assert_eq!(asked, Some(30));
        assert!(outcome.is_ok());
    }

    #[tokio::test]
    async fn an_explicit_window_is_honoured() {
        let args = JobRunArgs::from_declared(&PARAMETERS, [("retention_days", "90")])
            .expect("declared param");
        assert_eq!(run_with(args).await.1, Some(90));
    }

    /// Zero would delete runs that finished seconds ago — including the
    /// batch run whose findings an operator is reading right now. The
    /// floor is the difference between a retention sweep and wiping the
    /// history every night.
    #[tokio::test]
    async fn a_zero_window_is_clamped_rather_than_obeyed() {
        let args =
            JobRunArgs::from_declared(&PARAMETERS, [("retention_days", "0")]).expect("declared");
        assert_eq!(run_with(args).await.1, Some(1));

        let args =
            JobRunArgs::from_declared(&PARAMETERS, [("retention_days", "-5")]).expect("declared");
        assert_eq!(run_with(args).await.1, Some(1));
    }

    /// `consistency_batch` enrols children by name. This job deletes run
    /// history and is `Mutates::Always`, so joining the discovery-only
    /// batch would be wrong twice over — and the only thing keeping it
    /// out is the suffix.
    #[test]
    fn the_name_keeps_it_out_of_the_consistency_batch() {
        assert!(
            !crate::infrastructure::services::consistency_batch_service::is_batch_child(
                JobRunsCleanupService::JOB_NAME
            )
        );
    }
}
