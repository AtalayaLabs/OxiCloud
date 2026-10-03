//! Turns a completed run into out-of-band alerts, by diffing it against
//! the previous completed run of the same job.
//!
//! **Why a diff and not "notify on findings".** Once detectors are
//! scheduled, a persisting problem is reported every single run. Fifty-two
//! weekly alerts about the same 29 orphaned blobs is an alarm that rings
//! until someone disconnects it, and a disconnected channel is worse than
//! no channel — it is a channel an operator believes in.
//!
//! So the unit of news is a **transition**: a finding kind that was not
//! reported last time and is now, or was and is not. The baseline comes
//! from `jobs.run_findings` itself rather than from a separate
//! alert-state table — the history already records what was true last
//! time, and a second store would be a second thing to keep in step.

use std::sync::Arc;

use uuid::Uuid;

use crate::application::ports::notification_sink_ports::{
    AlertClass, FindingAlert, NotificationSink, NotifyThreshold, Transition,
};

use super::recoverable::JobStoreProvider;

pub struct FindingNotifier {
    sinks: Vec<Arc<dyn NotificationSink>>,
    threshold: NotifyThreshold,
}

impl FindingNotifier {
    pub fn new(sinks: Vec<Arc<dyn NotificationSink>>, threshold: NotifyThreshold) -> Self {
        Self { sinks, threshold }
    }

    /// Nothing configured, or everything gated off. Checked before the
    /// diff so a deployment with no webhook pays no queries for it.
    pub fn is_silent(&self) -> bool {
        self.sinks.is_empty() || self.threshold == NotifyThreshold::None
    }

    /// Names of the configured sinks, for the admin panel.
    pub fn sink_names(&self) -> Vec<&'static str> {
        self.sinks.iter().map(|s| s.name()).collect()
    }

    /// The synthetic alert `POST /api/admin/webhook/test` sends.
    ///
    /// Lives here so the shape a test delivers is the shape real alerts
    /// use — a test that exercised a different payload could pass while
    /// the real one failed.
    ///
    /// The caller delivers it directly, **bypassing the severity
    /// threshold**: the gate exists to keep real traffic quiet, and
    /// applying it to a test would silently deliver nothing to an operator
    /// on the default `data_loss` setting, reading as a broken webhook.
    pub fn test_alert() -> FindingAlert {
        FindingAlert {
            job: "webhook_test".to_string(),
            run_id: "00000000-0000-0000-0000-000000000000".to_string(),
            // `AlertClass::Test` below makes the human-facing text say
            // so outright, while these fields stay meaningful for a
            // machine receiver reading the `generic` payload. The kind is
            // named so nobody mistakes the message for a real incident
            // either way.
            kind: "test_alert".to_string(),
            severity: "anomaly".to_string(),
            transition: Transition::Appeared,
            count: 1,
            scanned: 0,
            detail: None,
            class: AlertClass::Test,
        }
    }

    /// Severity for a run that stopped on an error.
    ///
    /// `anomaly`, because the scale grades *findings* — how bad the data
    /// is — and a stopped run makes no claim about data at all. It is
    /// deliberately not `data_loss`: labelling "I could not check" as
    /// "bytes are gone" would corrupt the one signal that has to stay
    /// trustworthy. The consequence, accepted knowingly, is that an
    /// operator on the shipped `data_loss` default hears nothing about a
    /// stalled sweep and opts in by widening the floor.
    const RUN_ERROR_SEVERITY: &'static str = "anomaly";

    /// A run stopped with `error_reason` set — alert once per reason, per
    /// run.
    ///
    /// Delivered directly rather than through [`Self::diff`]: there is no
    /// previous-run baseline to compare against, and the finding history
    /// must never be asked about a run that did not finish. Returns the
    /// number of failed deliveries, like its sibling.
    ///
    /// De-duplicated on [`NOTIFIED_ERROR_PARAM`]: a resume that stops
    /// again for the same reason is silent, a different reason alerts,
    /// and a fresh run alerts because its params start empty. A read or
    /// write failure on the marker errs toward *sending* — a duplicate
    /// alert is a nuisance, a missing one is the defect this exists to
    /// prevent.
    ///
    /// `resumable` — does this run continue from where it stopped? A
    /// retryable pause does; a terminal failure does not, and the
    /// message must not promise a resume that will never happen.
    #[allow(clippy::too_many_arguments)]
    pub async fn notify_run_error(
        &self,
        store: &dyn super::recoverable::JobStore,
        job: &str,
        run_id: Uuid,
        reason: &str,
        detail: &str,
        scanned: u64,
        resumable: bool,
    ) -> u64 {
        if self.is_silent() || !self.threshold.admits(Self::RUN_ERROR_SEVERITY) {
            return 0;
        }

        if store
            .get_string_param(super::recoverable::NOTIFIED_ERROR_PARAM)
            .await
            .ok()
            .flatten()
            .as_deref()
            == Some(reason)
        {
            tracing::debug!(
                target: "oxicloud::scheduler",
                event = "notify.run_error_deduped",
                job, run_id = %run_id, reason,
                "already alerted on this reason for this run; staying quiet"
            );
            return 0;
        }

        let alert = FindingAlert {
            job: job.to_string(),
            run_id: run_id.to_string(),
            kind: reason.to_string(),
            severity: Self::RUN_ERROR_SEVERITY.to_string(),
            transition: Transition::Appeared,
            // One event, not a count of resources.
            count: 1,
            scanned,
            detail: Some(detail.to_string()),
            class: AlertClass::RunHealth { resumable },
        };
        let failures = self.deliver(&alert, "run_error").await;

        // Stamped after delivery, so a send that panicked or a process
        // that died mid-flight re-alerts next time rather than going
        // quiet about a problem nobody heard about.
        if let Err(e) = store
            .set_string_param(super::recoverable::NOTIFIED_ERROR_PARAM, reason)
            .await
        {
            tracing::warn!(
                target: "oxicloud::scheduler",
                event = "notify.run_error_mark_failed",
                job, run_id = %run_id, reason, error = %e,
                "could not record that this reason was alerted; a resume \
                 that stops the same way will alert again"
            );
        }
        failures
    }

    /// A run that had stopped on an error has now completed — send the
    /// all-clear, once.
    ///
    /// Reads the same marker the error alert wrote, so a run that never
    /// failed says nothing and no extra query is needed. The marker is
    /// not cleared: the run is terminal, and the next run starts with
    /// empty params anyway.
    pub async fn notify_run_recovered(
        &self,
        store: &dyn super::recoverable::JobStore,
        job: &str,
        run_id: Uuid,
        scanned: u64,
    ) -> u64 {
        if self.is_silent() || !self.threshold.admits(Self::RUN_ERROR_SEVERITY) {
            return 0;
        }

        let Ok(Some(reason)) = store
            .get_string_param(super::recoverable::NOTIFIED_ERROR_PARAM)
            .await
        else {
            return 0;
        };

        let alert = FindingAlert {
            job: job.to_string(),
            run_id: run_id.to_string(),
            kind: reason,
            severity: Self::RUN_ERROR_SEVERITY.to_string(),
            transition: Transition::Cleared,
            count: 0,
            scanned,
            detail: None,
            // `resumable` is moot on the way out — the run finished —
            // and the cleared wording does not consult it.
            class: AlertClass::RunHealth { resumable: true },
        };
        self.deliver(&alert, "run_recovered").await
    }

    /// Push one alert to every sink, auditing each outcome. Returns the
    /// number that failed.
    ///
    /// Shared by the finding diff and the run-health paths so all three
    /// audit identically — a delivery failure has to look the same in the
    /// log whichever alert produced it.
    async fn deliver(&self, alert: &FindingAlert, source: &'static str) -> u64 {
        let mut failures = 0u64;
        for sink in &self.sinks {
            if let Err(e) = sink.deliver(alert).await {
                failures += 1;
                tracing::warn!(
                    target: "audit",
                    event = "notify.delivery_failed",
                    sink = sink.name(),
                    source,
                    job = %alert.job,
                    run_id = %alert.run_id,
                    kind = %alert.kind,
                    transition = alert.transition.as_str(),
                    error = %e,
                    "👮🏻‍♂️ could not deliver a job alert via {}: {e}",
                    sink.name(),
                );
            } else {
                tracing::info!(
                    target: "audit",
                    event = "notify.delivered",
                    sink = sink.name(),
                    source,
                    job = %alert.job,
                    run_id = %alert.run_id,
                    kind = %alert.kind,
                    severity = %alert.severity,
                    transition = alert.transition.as_str(),
                    count = alert.count,
                    "📣 {}", alert.summary(),
                );
            }
        }
        failures
    }

    /// Diff `run_id` against the previous completed run of `job` and
    /// deliver what changed. Returns the number of failed deliveries.
    ///
    /// Never returns an error: the finding rows are the truth and this is
    /// best-effort delivery, so a dead webhook must not fail a run that
    /// did its work. Everything that goes wrong is audited instead, and
    /// the count is surfaced on the run so a silently broken channel is
    /// visible without reading logs.
    pub async fn notify_completed_run(
        &self,
        provider: &dyn JobStoreProvider,
        job: &str,
        run_id: Uuid,
        scanned: u64,
    ) -> u64 {
        if self.is_silent() {
            return 0;
        }

        let current = match provider.finding_kind_counts(run_id).await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(
                    target: "oxicloud::scheduler",
                    event = "notify.current_kinds_failed",
                    job, run_id = %run_id, error = %e,
                    "could not read this run's findings; no alerts sent"
                );
                return 0;
            }
        };
        let previous = match provider.previous_completed_finding_kinds(job, run_id).await {
            Ok(v) => v,
            Err(e) => {
                // Deliberately silent rather than treating "no baseline"
                // as "everything is new": a DB hiccup would otherwise
                // re-announce every standing finding as a fresh problem.
                tracing::warn!(
                    target: "oxicloud::scheduler",
                    event = "notify.baseline_failed",
                    job, run_id = %run_id, error = %e,
                    "could not read the previous run's findings; no alerts sent"
                );
                return 0;
            }
        };

        let alerts = Self::diff(job, &run_id.to_string(), scanned, &current, &previous)
            .into_iter()
            .filter(|a| self.threshold.admits(&a.severity))
            .collect::<Vec<_>>();

        let mut failures = 0u64;
        for alert in &alerts {
            failures += self.deliver(alert, "finding").await;
        }
        failures
    }

    /// The transition set between two runs, before severity gating.
    ///
    /// Pure, so the interesting cases are testable without a provider.
    fn diff(
        job: &str,
        run_id: &str,
        scanned: u64,
        current: &[(String, String, u64)],
        previous: &[(String, String, u64)],
    ) -> Vec<FindingAlert> {
        let mut out = Vec::new();

        for (kind, severity, count) in current {
            if !previous.iter().any(|(k, _, _)| k == kind) {
                out.push(FindingAlert {
                    job: job.to_string(),
                    run_id: run_id.to_string(),
                    kind: kind.clone(),
                    severity: severity.clone(),
                    transition: Transition::Appeared,
                    count: *count,
                    scanned,
                    // A detector's per-resource detail lives in the
                    // findings drawer; repeating it in every message
                    // would make the alert longer without making it
                    // more actionable.
                    detail: None,
                    class: AlertClass::Finding,
                });
            }
        }

        for (kind, severity, _) in previous {
            if !current.iter().any(|(k, _, _)| k == kind) {
                out.push(FindingAlert {
                    job: job.to_string(),
                    run_id: run_id.to_string(),
                    kind: kind.clone(),
                    // The severity it HAD. A resolution is gated by the
                    // same threshold as its appearance, so an operator
                    // only gets an all-clear for something they were told
                    // about in the first place.
                    severity: severity.clone(),
                    transition: Transition::Cleared,
                    count: 0,
                    scanned,
                    // A detector's per-resource detail lives in the
                    // findings drawer; repeating it in every message
                    // would make the alert longer without making it
                    // more actionable.
                    detail: None,
                    class: AlertClass::Finding,
                });
            }
        }

        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::ports::notification_sink_ports::Severity;

    fn kinds(v: &[(&str, &str, u64)]) -> Vec<(String, String, u64)> {
        v.iter()
            .map(|(k, s, n)| (k.to_string(), s.to_string(), *n))
            .collect()
    }

    fn diff(current: &[(&str, &str, u64)], previous: &[(&str, &str, u64)]) -> Vec<FindingAlert> {
        FindingNotifier::diff("j", "r", 100, &kinds(current), &kinds(previous))
    }

    /// The defect the whole diff exists to prevent: a standing finding
    /// must not re-alert on every scheduled run.
    #[test]
    fn a_finding_present_in_both_runs_is_not_news() {
        let same = [("orphan_blob", "inconsistent", 29)];
        assert!(diff(&same, &same).is_empty());
    }

    /// Even when the count moves. An operator already knows about
    /// orphan_blob; 29 becoming 31 is not a new thing to act on, and
    /// treating it as one brings back the per-run alarm.
    #[test]
    fn a_changed_count_is_not_a_transition() {
        assert!(
            diff(
                &[("orphan_blob", "inconsistent", 31)],
                &[("orphan_blob", "inconsistent", 29)]
            )
            .is_empty()
        );
    }

    #[test]
    fn a_new_kind_appears_with_its_count() {
        let alerts = diff(&[("blob_unreadable", "data_loss", 3)], &[]);
        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0].transition, Transition::Appeared);
        assert_eq!(alerts[0].kind, "blob_unreadable");
        assert_eq!(alerts[0].count, 3);
    }

    #[test]
    fn a_kind_that_stopped_being_reported_clears() {
        let alerts = diff(&[], &[("orphan_blob", "inconsistent", 29)]);
        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0].transition, Transition::Cleared);
        assert_eq!(alerts[0].count, 0, "a resolution has no findings to count");
        assert_eq!(
            alerts[0].severity, "inconsistent",
            "a cleared alert keeps the severity it had, so the gate treats \
             it the same way it treated the appearance"
        );
    }

    #[test]
    fn both_directions_can_happen_in_one_run() {
        let alerts = diff(
            &[("blob_unreadable", "data_loss", 1)],
            &[("orphan_blob", "inconsistent", 29)],
        );
        assert_eq!(alerts.len(), 2);
        assert!(alerts.iter().any(|a| a.transition == Transition::Appeared));
        assert!(alerts.iter().any(|a| a.transition == Transition::Cleared));
    }

    /// With no sinks there is nothing to deliver to, and the notifier must
    /// not spend two queries per run discovering that.
    #[test]
    fn a_notifier_with_no_sinks_is_silent() {
        assert!(
            FindingNotifier::new(vec![], NotifyThreshold::AtLeast(Severity::DataLoss)).is_silent()
        );
    }

    // ─── Run-health alerts ──────────────────────────────────────────────
    //
    // The path that exists because a run which stopped on an error used
    // to tell nobody: the notifier was only ever called from the
    // Completed arm, so an unreachable backend produced a paused row, a
    // log line, and silence on every configured channel.

    /// Collects what it was asked to deliver.
    struct RecordingSink {
        got: std::sync::Mutex<Vec<FindingAlert>>,
        fail: bool,
    }

    impl RecordingSink {
        fn new(fail: bool) -> Arc<Self> {
            Arc::new(Self {
                got: std::sync::Mutex::new(Vec::new()),
                fail,
            })
        }
        fn alerts(&self) -> Vec<FindingAlert> {
            self.got.lock().expect("sink lock").clone()
        }
    }

    #[async_trait::async_trait]
    impl NotificationSink for RecordingSink {
        fn name(&self) -> &'static str {
            "recording"
        }
        async fn deliver(
            &self,
            alert: &FindingAlert,
        ) -> Result<(), crate::common::errors::DomainError> {
            self.got.lock().expect("sink lock").push(alert.clone());
            if self.fail {
                return Err(crate::common::errors::DomainError::internal_error(
                    "Notify",
                    "sink is down".to_string(),
                ));
            }
            Ok(())
        }
    }

    /// Minimal `JobStore` that only has to remember string params — the
    /// de-duplication marker is the only state these paths touch.
    struct ParamStore {
        params: std::sync::Mutex<std::collections::HashMap<String, String>>,
        run_id: Uuid,
    }

    impl ParamStore {
        fn new() -> Self {
            Self {
                params: std::sync::Mutex::new(std::collections::HashMap::new()),
                run_id: Uuid::nil(),
            }
        }
    }

    #[async_trait::async_trait]
    impl super::super::recoverable::JobStore for ParamStore {
        fn run_id(&self) -> Uuid {
            self.run_id
        }
        async fn status(
            &self,
        ) -> Result<super::super::RunStatus, crate::common::errors::DomainError> {
            Ok(super::super::RunStatus::Running)
        }
        async fn checkpoint(
            &self,
            _cursor: Vec<u8>,
            _delta: u64,
        ) -> Result<(), crate::common::errors::DomainError> {
            Ok(())
        }
        fn started_at(&self) -> chrono::DateTime<chrono::Utc> {
            chrono::DateTime::UNIX_EPOCH
        }
        async fn seed_progress_params(
            &self,
            _total: u64,
            _kind: super::super::recoverable::ProgressKind,
        ) -> Result<(), crate::common::errors::DomainError> {
            Ok(())
        }
        async fn set_string_param(
            &self,
            key: &str,
            value: &str,
        ) -> Result<(), crate::common::errors::DomainError> {
            self.params
                .lock()
                .expect("param lock")
                .insert(key.to_string(), value.to_string());
            Ok(())
        }
        async fn get_string_param(
            &self,
            key: &str,
        ) -> Result<Option<String>, crate::common::errors::DomainError> {
            Ok(self.params.lock().expect("param lock").get(key).cloned())
        }
        async fn stat_u64(&self, _key: &str) -> Result<u64, crate::common::errors::DomainError> {
            Ok(0)
        }
        async fn record_finding(
            &self,
            _kind: &str,
            _severity: &str,
            _resource_id: Option<Uuid>,
            _detail: serde_json::Value,
        ) -> Result<(), crate::common::errors::DomainError> {
            Ok(())
        }
        async fn merge_stats(
            &self,
            _extras: &serde_json::Map<String, serde_json::Value>,
        ) -> Result<(), crate::common::errors::DomainError> {
            Ok(())
        }
        async fn mark_completed(&self) -> Result<(), crate::common::errors::DomainError> {
            Ok(())
        }
        async fn mark_paused(
            &self,
            _cursor: Option<Vec<u8>>,
        ) -> Result<(), crate::common::errors::DomainError> {
            Ok(())
        }
        async fn mark_paused_retryable(
            &self,
            _cursor: Option<Vec<u8>>,
            _reason: &str,
            _detail: &str,
        ) -> Result<(), crate::common::errors::DomainError> {
            Ok(())
        }
        async fn mark_failed(
            &self,
            _message: &str,
        ) -> Result<(), crate::common::errors::DomainError> {
            Ok(())
        }
        async fn mark_cancelled(
            &self,
            _cursor: Option<Vec<u8>>,
        ) -> Result<(), crate::common::errors::DomainError> {
            Ok(())
        }
    }

    fn notifier(sink: Arc<RecordingSink>, threshold: NotifyThreshold) -> FindingNotifier {
        FindingNotifier::new(vec![sink], threshold)
    }

    fn anomaly() -> NotifyThreshold {
        NotifyThreshold::AtLeast(Severity::Anomaly)
    }

    /// The case from the live sandbox: DNS died, the run paused, and
    /// nothing was sent. It must send now, carrying the reason as the
    /// alert's kind and the error chain as its detail.
    #[tokio::test]
    async fn a_run_that_stopped_on_an_error_alerts_once() {
        let sink = RecordingSink::new(false);
        let n = notifier(sink.clone(), anomaly());
        let store = ParamStore::new();

        let failures = n
            .notify_run_error(
                &store,
                "backend_consistency",
                Uuid::nil(),
                "backend_unavailable",
                "deep verify read: dns error",
                7,
                true,
            )
            .await;

        assert_eq!(failures, 0);
        let got = sink.alerts();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].kind, "backend_unavailable");
        assert_eq!(got[0].severity, "anomaly");
        assert_eq!(got[0].transition, Transition::Appeared);
        assert_eq!(got[0].scanned, 7);
        assert!(
            got[0].detail.as_deref().unwrap_or_default().contains("dns"),
            "the operator needs the cause, not just the key: {:?}",
            got[0].detail
        );
        // And it must not describe itself as a finding. The first live
        // alert read "1 anomaly finding(s) of kind backend_unavailable",
        // which sends an operator looking for a finding that does not
        // exist.
        let text = got[0].summary();
        assert!(
            !text.contains("finding"),
            "a stopped run is not a finding: {text}"
        );
        assert!(text.contains("stopped early"), "{text}");
        assert!(
            text.contains("resumes from where it stopped"),
            "a retryable pause should say it will continue: {text}"
        );
    }

    /// A terminal failure must not promise a resume that will never
    /// come — the most misleading thing the message could say.
    #[tokio::test]
    async fn a_terminal_failure_does_not_promise_a_resume() {
        let sink = RecordingSink::new(false);
        let n = notifier(sink.clone(), anomaly());
        let store = ParamStore::new();

        n.notify_run_error(&store, "j", Uuid::nil(), "job_failed", "bad data", 3, false)
            .await;

        let text = sink.alerts()[0].summary();
        assert!(text.contains("will not resume"), "{text}");
    }

    /// A backend down all week must not mail on every auto-resume — the
    /// same muting risk the finding diff exists to avoid.
    #[tokio::test]
    async fn the_same_reason_on_a_resumed_run_is_silent() {
        let sink = RecordingSink::new(false);
        let n = notifier(sink.clone(), anomaly());
        let store = ParamStore::new();

        for _ in 0..3 {
            n.notify_run_error(
                &store,
                "backend_consistency",
                Uuid::nil(),
                "backend_unavailable",
                "dns error",
                0,
                true,
            )
            .await;
        }

        assert_eq!(sink.alerts().len(), 1, "re-alerted on a standing stall");
    }

    /// A different cause is different news — a timeout after a DNS
    /// failure means something else is wrong now.
    #[tokio::test]
    async fn a_different_reason_alerts_again() {
        let sink = RecordingSink::new(false);
        let n = notifier(sink.clone(), anomaly());
        let store = ParamStore::new();

        n.notify_run_error(
            &store,
            "j",
            Uuid::nil(),
            "backend_unavailable",
            "dns",
            0,
            true,
        )
        .await;
        n.notify_run_error(&store, "j", Uuid::nil(), "backend_timeout", "slow", 0, true)
            .await;

        let kinds: Vec<String> = sink.alerts().into_iter().map(|a| a.kind).collect();
        assert_eq!(kinds, vec!["backend_unavailable", "backend_timeout"]);
    }

    /// Whoever was told about the stall is owed the all-clear.
    #[tokio::test]
    async fn a_run_that_recovers_sends_the_all_clear() {
        let sink = RecordingSink::new(false);
        let n = notifier(sink.clone(), anomaly());
        let store = ParamStore::new();

        n.notify_run_error(
            &store,
            "j",
            Uuid::nil(),
            "backend_unavailable",
            "dns",
            0,
            true,
        )
        .await;
        n.notify_run_recovered(&store, "j", Uuid::nil(), 2026).await;

        let got = sink.alerts();
        assert_eq!(got.len(), 2);
        assert_eq!(got[1].transition, Transition::Cleared);
        assert_eq!(
            got[1].kind, "backend_unavailable",
            "the all-clear names what it clears"
        );
    }

    /// A run that never stopped on an error has nothing to clear, and
    /// must not announce a recovery from a problem nobody heard about.
    #[tokio::test]
    async fn a_run_that_never_failed_sends_no_all_clear() {
        let sink = RecordingSink::new(false);
        let n = notifier(sink.clone(), anomaly());
        let store = ParamStore::new();

        n.notify_run_recovered(&store, "j", Uuid::nil(), 1).await;

        assert!(sink.alerts().is_empty());
    }

    /// The accepted consequence of grading these on the findings scale:
    /// the shipped default admits only `data_loss`, so an operator opts
    /// in to run-health alerts by widening the floor. Pinned so the
    /// trade-off is a decision rather than a surprise.
    #[tokio::test]
    async fn the_default_threshold_does_not_admit_run_health_alerts() {
        let sink = RecordingSink::new(false);
        let n = notifier(sink.clone(), NotifyThreshold::AtLeast(Severity::DataLoss));
        let store = ParamStore::new();

        n.notify_run_error(
            &store,
            "j",
            Uuid::nil(),
            "backend_unavailable",
            "dns",
            0,
            true,
        )
        .await;

        assert!(sink.alerts().is_empty());
    }

    /// A dead channel is counted, not swallowed — the run outcome
    /// surfaces it as `notify_failures`.
    #[tokio::test]
    async fn a_failing_sink_is_counted_not_swallowed() {
        let sink = RecordingSink::new(true);
        let n = notifier(sink.clone(), anomaly());
        let store = ParamStore::new();

        let failures = n
            .notify_run_error(
                &store,
                "j",
                Uuid::nil(),
                "backend_unavailable",
                "dns",
                0,
                true,
            )
            .await;

        assert_eq!(failures, 1);
    }
}
