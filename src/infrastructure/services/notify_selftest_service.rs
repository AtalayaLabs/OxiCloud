//! A job that records a synthetic finding, so the alerting chain can be
//! exercised end to end: run → `jobs.run_findings` → transition diff →
//! severity threshold → every configured sink.
//!
//! **Why this exists rather than a test fixture.** Everything between a
//! finding and a delivered message is configuration an operator owns —
//! the threshold, the recipient list, the webhook format — and none of it
//! is exercised by the per-channel test buttons, which inject an alert
//! *past* the diff and the threshold. The one failure this feature cannot
//! afford is a channel that looks configured and says nothing, and that
//! failure lives precisely in the part the buttons skip.
//!
//! **Not gated behind a test flag.** It was tempting
//! (`OXICLOUD_ENABLE_SELFTEST_JOBS`), and rejected: a path that only
//! exists in CI is not the path production runs, which is the whole thing
//! being verified here. It is read-only, on-demand only, and the finding
//! it writes is named `selftest_finding` with a detail blob that says so,
//! so an operator reading their history can tell it apart from a real
//! one at a glance.
//!
//! Also the honest way to answer "will I actually be told?" — press it,
//! and either mail arrives or it does not.

use std::sync::Arc;

use async_trait::async_trait;

use crate::infrastructure::scheduler::{
    JobParam, JobRegistry, JobRunArgs, JobStore, JobStoreProvider, Mutates, RecoverableJobHandler,
    RunOutcome,
};

/// Default severity, and deliberately the worst one.
///
/// The shipped threshold admits only `data_loss`, so anything milder
/// would make the default self-test deliver nothing on a correctly
/// configured instance — which is the exact reading ("my channel is
/// broken") that the self-test is supposed to rule out. An operator
/// testing a widened threshold overrides it.
const DEFAULT_SEVERITY: &str = "data_loss";

/// Default finding kind. Named so it cannot be mistaken for a detector's.
const DEFAULT_KIND: &str = "selftest_finding";

static PARAMETERS: [JobParam; 4] = [
    JobParam::string(
        "severity",
        "Severity to record: data_loss (default), inconsistent or anomaly. \
         Findings below OXICLOUD_JOBS_NOTIFY_MIN_SEVERITY are recorded but \
         not delivered — which is itself worth testing.",
    ),
    JobParam::string(
        "kind",
        "Finding kind to record. Defaults to selftest_finding. Alerts are \
         per kind and only on a change, so re-running with the same kind \
         correctly sends nothing the second time.",
    ),
    JobParam::number(
        "findings",
        1,
        "How many findings to record. 0 records none, which makes the next \
         run's diff report the previous kind as cleared — the way to \
         exercise the resolution alert.",
    ),
    JobParam::number(
        "stall",
        0,
        "Stop with a retryable error on this many attempts before \
         completing, as a backend outage would. Each subsequent trigger \
         resumes the same run: with stall=2 the first attempt alerts, the \
         second is silent (same reason, already reported) and the third \
         completes and sends the all-clear.",
    ),
];

/// Per-run attempt counter, so `stall` can mean "the first N attempts"
/// rather than "forever".
///
/// It has to be a counter rather than a flag because a resumed run reads
/// back the args it *started* with — passing `stall=0` on the resuming
/// trigger is ignored by design, which would otherwise make a stalled
/// self-test unresumable and leave the job with a permanently
/// non-terminal run.
const ATTEMPTS_PARAM: &str = "selftest_attempts";

pub struct NotifySelftestJob;

impl NotifySelftestJob {
    pub const JOB_NAME: &'static str = "notify_selftest";

    pub fn new() -> Self {
        Self
    }

    /// On-demand only — `None` cadence. A job whose entire purpose is to
    /// generate an alert must never be on a timer.
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

impl Default for NotifySelftestJob {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl RecoverableJobHandler for NotifySelftestJob {
    fn name(&self) -> &str {
        Self::JOB_NAME
    }

    fn description(&self) -> &'static str {
        "Records one synthetic finding so out-of-band alerting can be \
         verified end to end — the transition diff, the severity threshold \
         and every configured channel, none of which the per-channel test \
         buttons exercise. Read-only: it touches no files, blobs or \
         folders, and writes nothing but its own run and finding rows. \
         Alerts fire on a CHANGE, so a second identical run is silent by \
         design; trigger it with findings=0 to produce the cleared alert. \
         `stall=N` additionally makes the run stop with a retryable error \
         for its first N attempts, which is how the 'a job stopped and \
         nobody was told' path gets exercised without taking a backend \
         away."
    }

    fn mutates(&self) -> Mutates {
        // Nothing a user can see. Its own run row and a finding are the
        // same writes every read-only detector makes.
        Mutates::Never
    }

    fn parameters(&self) -> &'static [JobParam] {
        &PARAMETERS
    }

    /// One subject: the synthetic finding. Reported so the alert reads
    /// "1 ... (scanned 1)" rather than the "scanned 0" that made the
    /// first live webhook test look broken.
    async fn count_total(&self) -> Option<u64> {
        Some(1)
    }

    async fn run_resumable(
        &self,
        store: &dyn JobStore,
        args: &JobRunArgs,
        _resume_cursor: Option<Vec<u8>>,
    ) -> RunOutcome {
        use crate::application::ports::notification_sink_ports::Severity;

        let severity = args.get_str("severity").unwrap_or(DEFAULT_SEVERITY);
        // Validated against the same enum the threshold uses, so a typo
        // fails the run loudly instead of recording a finding that
        // silently sits below every threshold — the failure this job is
        // meant to detect, arriving from the job itself.
        if Severity::parse(severity).is_none() {
            return RunOutcome::Failed {
                message: format!(
                    "`{severity}` is not a severity (accepted: data_loss, \
                     inconsistent, anomaly)"
                ),
            };
        }
        let kind = args.get_str("kind").unwrap_or(DEFAULT_KIND);
        let findings = args.get_number("findings", 1).max(0);

        // Stall before recording anything, so a stalled attempt leaves no
        // findings behind — the state a real detector is in when the
        // backend disappears mid-walk.
        let stall_for = args.get_number("stall", 0).max(0);
        if stall_for > 0 {
            let attempt = store
                .get_string_param(ATTEMPTS_PARAM)
                .await
                .ok()
                .flatten()
                .and_then(|v| v.parse::<i64>().ok())
                .unwrap_or(0)
                + 1;
            if let Err(e) = store
                .set_string_param(ATTEMPTS_PARAM, &attempt.to_string())
                .await
            {
                return RunOutcome::Failed {
                    message: format!("could not record the attempt count: {e}"),
                };
            }
            if attempt <= stall_for {
                return RunOutcome::PausedRetryable {
                    cursor: Vec::new(),
                    // A key of its own rather than borrowing
                    // `backend_unavailable`: an operator reading their
                    // inbox must be able to tell a self-test from the
                    // real thing, and so must a mailbox rule.
                    reason: "selftest_stall".to_string(),
                    detail: format!(
                        "Synthetic stall {attempt} of {stall_for} from the \
                         notify_selftest job. No backend was contacted and \
                         nothing is wrong with this instance."
                    ),
                };
            }
        }

        for i in 0..findings {
            if let Err(e) = store
                .record_finding(
                    kind,
                    severity,
                    // Run-wide: there is no real resource behind this.
                    None,
                    serde_json::json!({
                        "synthetic": true,
                        "index":     i,
                        "note":      "Recorded by the notify_selftest job. \
                                      No data is affected.",
                    }),
                )
                .await
            {
                return RunOutcome::Failed {
                    message: format!("could not record the synthetic finding: {e}"),
                };
            }
        }

        // One subject walked. Without this the run's `scanned_count`
        // stays 0 and the alert reads "(scanned 0)" — the phrasing that
        // made the first live webhook test unreadable.
        if let Err(e) = store.checkpoint(Vec::new(), 1).await {
            return RunOutcome::Failed {
                message: format!("could not checkpoint the run: {e}"),
            };
        }

        tracing::info!(
            target: "audit",
            event = "notify.selftest_run",
            run_id = %store.run_id(),
            kind,
            severity,
            findings,
            "🧪 notify_selftest recorded {findings} synthetic {severity} finding(s) of kind {kind}",
        );

        RunOutcome::Completed {
            extra_stats: serde_json::Map::from_iter([
                ("synthetic_findings".to_string(), findings.into()),
                ("kind".to_string(), kind.into()),
                ("severity".to_string(), severity.into()),
            ]),
        }
    }
}
