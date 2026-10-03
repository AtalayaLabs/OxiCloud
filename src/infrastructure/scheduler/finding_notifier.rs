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
    FindingAlert, NotificationSink, NotifyThreshold, Transition,
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
            // `test` makes the human-facing text say so outright; these
            // fields stay meaningful for a machine receiver reading the
            // `generic` payload.
            test: true,
            // Named so nobody mistakes the message for a real incident.
            kind: "test_alert".to_string(),
            severity: "anomaly".to_string(),
            transition: Transition::Appeared,
            count: 1,
            scanned: 0,
        }
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
            for sink in &self.sinks {
                if let Err(e) = sink.deliver(alert).await {
                    failures += 1;
                    tracing::warn!(
                        target: "audit",
                        event = "notify.delivery_failed",
                        sink = sink.name(),
                        job,
                        run_id = %run_id,
                        kind = %alert.kind,
                        transition = alert.transition.as_str(),
                        error = %e,
                        "👮🏻‍♂️ could not deliver a job finding alert via {}: {e}",
                        sink.name(),
                    );
                } else {
                    tracing::info!(
                        target: "audit",
                        event = "notify.delivered",
                        sink = sink.name(),
                        job,
                        run_id = %run_id,
                        kind = %alert.kind,
                        severity = %alert.severity,
                        transition = alert.transition.as_str(),
                        count = alert.count,
                        "📣 {}", alert.summary(),
                    );
                }
            }
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
                    test: false,
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
                    test: false,
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
}
