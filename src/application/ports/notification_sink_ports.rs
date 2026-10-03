//! Out-of-band delivery of job findings to whoever operates the
//! instance.
//!
//! **Why this is not `NotificationApplicationService`.** That service
//! writes `notif.notifications`, whose `user_id` is `NOT NULL REFERENCES
//! auth.users(id)` — it is per-user and in-app, and it is the right shape
//! for "someone shared a folder with you". A job finding is addressed to
//! whoever runs the instance, who may have no account, may not be logged
//! in, and must be reachable precisely when the instance is unhealthy.
//! Fanning findings out to every admin's inbox would still fail that
//! case.
//!
//! The principle that service states in its own docs is kept, though, and
//! it is the one that matters here too: **the row is the truth, delivery
//! is best-effort.** A finding lives in `jobs.run_findings` whether or not
//! a webhook answered. A sink that fails must not fail the run — but it
//! must not be silent either, so a failure is audited and counted.

use async_trait::async_trait;

use crate::common::errors::DomainError;

/// How bad a finding is, as an ordered scale.
///
/// The string values are the ones already in `jobs.run_findings.severity`,
/// the admin API and the findings drawer. Deliberately not a second
/// vocabulary (`critical`/`warn`): an operator reading an alert beside a
/// config file should not have to translate between them. Friendly labels
/// are a UI concern.
///
/// The column is TEXT and the set is open, so [`Severity::parse`] returns
/// `None` for anything it does not know and callers treat that as
/// "below any threshold" — a new severity added by a future tenant must
/// not start paging people before someone decides it should.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    /// Informational. A healthy instance doing its job emits these, so
    /// they never notify.
    Anomaly,
    /// Real drift, usually recoverable and often already queued for
    /// reclamation.
    Inconsistent,
    /// Bytes may be gone. The reason this feature exists.
    DataLoss,
}

impl Severity {
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "anomaly" => Some(Self::Anomaly),
            "inconsistent" => Some(Self::Inconsistent),
            "data_loss" => Some(Self::DataLoss),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Anomaly => "anomaly",
            Self::Inconsistent => "inconsistent",
            Self::DataLoss => "data_loss",
        }
    }
}

/// What a notification threshold admits.
///
/// Separate from [`Severity`] because "notify about nothing" is a valid
/// operator choice and is not a severity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotifyThreshold {
    /// Deliver nothing. The channel stays configured but silent.
    None,
    /// Deliver findings at this severity or above.
    AtLeast(Severity),
}

impl NotifyThreshold {
    /// Does a finding of this severity clear the bar?
    ///
    /// An unparseable severity never does — see [`Severity::parse`].
    pub fn admits(self, raw_severity: &str) -> bool {
        match self {
            Self::None => false,
            Self::AtLeast(min) => Severity::parse(raw_severity).is_some_and(|s| s >= min),
        }
    }

    /// Parse the `OXICLOUD_JOBS_NOTIFY_MIN_SEVERITY` value. `none`/`off`
    /// silence the channel; anything else must be a known severity.
    ///
    /// Returns `Err` with the accepted set rather than falling back to a
    /// default: a typo'd threshold that silently became "notify about
    /// everything" would page someone at 3am for an `anomaly`, and one
    /// that silently became "nothing" would be indistinguishable from a
    /// healthy instance.
    pub fn parse(raw: &str) -> Result<Self, String> {
        let v = raw.trim().to_ascii_lowercase();
        match v.as_str() {
            "none" | "off" => Ok(Self::None),
            other => Severity::parse(other).map(Self::AtLeast).ok_or_else(|| {
                format!(
                    "`{raw}` is not a severity (accepted: data_loss, inconsistent, anomaly, none)"
                )
            }),
        }
    }
}

/// What kind of event an alert describes.
///
/// Replaced a `test: bool`, and the reason is a defect that shipped
/// twice. Every transport builds its text from one shared
/// [`FindingAlert::summary`], which was written for detector findings —
/// so a run that *stopped* was announced as "1 anomaly finding(s) of kind
/// backend_unavailable", sending an operator to look for a finding that
/// does not exist. The first time this happened, the admin test message
/// read "1 anomaly finding(s) of kind test_alert (scanned 0)" and got its
/// own sentence; a boolean per special case does not scale past the
/// second one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlertClass {
    /// A detector reported (or stopped reporting) a finding kind. The
    /// count and the scanned total both mean what they say.
    Finding,
    /// A run stopped before finishing, or finished after having stopped.
    /// Nothing was *found*; `count` is 1 because it is one event, and
    /// `kind` is the run's `error_reason`.
    RunHealth {
        /// Does this run continue by itself (or by a Resume) from where
        /// it stopped? `PausedRetryable` yes, a terminal `Failed` no —
        /// and the message must not promise a resume that will never
        /// come.
        resumable: bool,
    },
    /// The admin "test this channel" message. Carries structured fields
    /// for a machine receiver but says outright that it is a test.
    Test,
}

/// Which way a finding kind crossed the line between two runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transition {
    /// Absent from the previous completed run, present now.
    Appeared,
    /// Present in the previous completed run, gone now. Worth sending:
    /// an operator told about a problem is owed the news that it ended,
    /// and without it the only way to learn is to go looking.
    Cleared,
}

impl Transition {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Appeared => "appeared",
            Self::Cleared => "cleared",
        }
    }
}

/// One thing worth telling an operator: a finding kind started or stopped
/// being reported by a job.
///
/// Per *kind*, not per finding. A run that reports 29 orphaned blobs is
/// one alert naming 29, not 29 alerts — the per-resource detail is in the
/// findings drawer, and a channel that emits one message per affected
/// blob is a channel that gets muted.
#[derive(Debug, Clone)]
pub struct FindingAlert {
    pub job: String,
    pub run_id: String,
    pub kind: String,
    pub severity: String,
    pub transition: Transition,
    /// How many findings of this kind the run recorded. `0` on a
    /// `Cleared` alert.
    pub count: u64,
    /// How many subjects the run walked, for context — "3 of 2026" reads
    /// very differently from "3 of 3".
    pub scanned: u64,
    /// Free text explaining this specific alert, when the `kind` alone
    /// does not say enough to act on.
    ///
    /// Carries the error chain for a run-health alert: `kind` is
    /// `backend_unavailable`, and this is the `dns error … nodename nor
    /// servname provided` that tells the operator whether to look at
    /// their network or their credentials. Detectors leave it `None` —
    /// their per-resource detail belongs in the findings drawer, not in
    /// every message.
    ///
    /// Truncated by [`FindingAlert::DETAIL_LIMIT`] when rendered, because
    /// a nested SDK error chain can run to kilobytes and a chat channel
    /// will either reject or badly wrap it.
    pub detail: Option<String>,
    /// What this alert actually describes — see [`AlertClass`]. Decides
    /// the wording, because "1 finding of kind X" is only true for one
    /// of the three.
    pub class: AlertClass,
}

impl FindingAlert {
    /// Cap on the rendered [`Self::detail`]. Generous enough for a full
    /// AWS SDK connector error (the DNS case runs ~300 chars), short of
    /// Telegram's 4096-byte message limit even with the summary in front
    /// of it.
    pub const DETAIL_LIMIT: usize = 600;

    /// [`Self::detail`], trimmed to [`Self::DETAIL_LIMIT`] on a char
    /// boundary.
    pub fn detail_trimmed(&self) -> Option<String> {
        self.detail.as_ref().map(|d| {
            let d = d.trim();
            if d.chars().count() <= Self::DETAIL_LIMIT {
                return d.to_string();
            }
            let mut out: String = d.chars().take(Self::DETAIL_LIMIT).collect();
            out.push('…');
            out
        })
    }

    /// One-line human summary, shared by every transport so they cannot
    /// describe the same event differently.
    ///
    /// **Deliberately plain text — no Markdown.** Job names and finding
    /// kinds are full of underscores (`blobs_consistency`,
    /// `manifest_refcount_mismatch`), and Telegram's `parse_mode=Markdown`
    /// reads an unpaired `_` as an unterminated italic and rejects the whole
    /// message with "can't parse entities". That failure costs the alert
    /// entirely, which is the worst outcome this feature has — so the text
    /// stays renderable everywhere instead of prettier in one place.
    pub fn summary(&self) -> String {
        match (self.class, self.transition) {
            (AlertClass::Test, _) => "This is a test message from OxiCloud. Your webhook is \
                 configured correctly — real alerts will arrive here."
                .to_string(),

            // Nothing was found, so this must not say "finding". What an
            // operator needs is which job stopped, why, how far it got,
            // and whether it will pick itself up.
            (AlertClass::RunHealth { resumable }, Transition::Appeared) => format!(
                "{} stopped early: {}, after scanning {}. {}",
                self.job,
                self.kind,
                self.scanned,
                if resumable {
                    "It resumes from where it stopped once the cause clears."
                } else {
                    "This run will not resume on its own."
                }
            ),
            (AlertClass::RunHealth { .. }, Transition::Cleared) => format!(
                "{} is running again ({} no longer reported).",
                self.job, self.kind
            ),

            (AlertClass::Finding, Transition::Appeared) => format!(
                "{}: {} {} finding(s) of kind {} (scanned {})",
                self.job, self.count, self.severity, self.kind, self.scanned
            ),
            (AlertClass::Finding, Transition::Cleared) => format!(
                "{}: {} no longer reported (was {}; scanned {})",
                self.job, self.kind, self.severity, self.scanned
            ),
        }
    }
}

/// What a receiver said, for the admin test path.
///
/// Shaped like `SmtpTestResultDto`'s `code` + `message` on purpose: the
/// admin panel presents mail and webhook together, and two transports
/// whose diagnostics render differently make that page harder to read
/// than it needs to be.
#[derive(Debug, Clone, Default)]
pub struct DeliveryReport {
    pub success: bool,
    /// Transport status — the HTTP status for a webhook. `None` when the
    /// receiver never answered (DNS failure, refused connection, timeout).
    pub code: Option<u16>,
    /// The receiver's own words: its response body, or the transport error
    /// when there was no response. This is the diagnosis — a webhook 403
    /// is far less useful than the `invalid_token` the body carries.
    pub message: Option<String>,
}

/// A best-effort delivery channel for [`FindingAlert`]s.
///
/// Implementations MUST NOT panic and SHOULD apply their own timeout —
/// a sink is called from a job's terminal path, and a channel that hangs
/// would hold a run open indefinitely.
#[async_trait]
pub trait NotificationSink: Send + Sync {
    /// Short stable name, for the audit line when delivery fails.
    fn name(&self) -> &'static str;

    /// Deliver one alert. Returning `Err` is expected and survivable —
    /// the caller audits and counts it, and the finding row stands
    /// regardless.
    async fn deliver(&self, alert: &FindingAlert) -> Result<(), DomainError>;

    /// Deliver once and report what the receiver said, for an operator
    /// testing their configuration.
    ///
    /// Distinct from [`Self::deliver`] in two ways a transport may care
    /// about: it reports the receiver's verdict instead of collapsing it
    /// to an error, and it should NOT retry — an operator clicking "test"
    /// wants an answer now, not a minute of backoff against a host that is
    /// plainly down.
    ///
    /// The default implementation delegates, reporting only success or the
    /// error text; transports override it to carry status and body.
    async fn deliver_reporting(&self, alert: &FindingAlert) -> DeliveryReport {
        match self.deliver(alert).await {
            Ok(()) => DeliveryReport {
                success: true,
                ..Default::default()
            },
            Err(e) => DeliveryReport {
                success: false,
                code: None,
                message: Some(e.to_string()),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn severity_orders_by_how_bad_it_is() {
        assert!(Severity::DataLoss > Severity::Inconsistent);
        assert!(Severity::Inconsistent > Severity::Anomaly);
    }

    /// The default: data loss notifies, drift does not.
    #[test]
    fn the_strict_threshold_admits_only_data_loss() {
        let t = NotifyThreshold::AtLeast(Severity::DataLoss);
        assert!(t.admits("data_loss"));
        assert!(!t.admits("inconsistent"));
        assert!(!t.admits("anomaly"));
    }

    #[test]
    fn widening_the_threshold_includes_everything_above_it() {
        let t = NotifyThreshold::AtLeast(Severity::Inconsistent);
        assert!(t.admits("data_loss"));
        assert!(t.admits("inconsistent"));
        assert!(!t.admits("anomaly"));
    }

    #[test]
    fn none_silences_every_severity() {
        let t = NotifyThreshold::None;
        assert!(!t.admits("data_loss"));
        assert!(!t.admits("anomaly"));
    }

    /// The severity column is TEXT and the set is open. A severity this
    /// build has never heard of must not page anyone — whoever adds one
    /// decides then whether it should.
    #[test]
    fn an_unknown_severity_is_below_every_threshold() {
        assert!(!NotifyThreshold::AtLeast(Severity::Anomaly).admits("catastrophe"));
        assert!(!NotifyThreshold::AtLeast(Severity::Anomaly).admits(""));
    }

    #[test]
    fn threshold_parsing_accepts_the_names_the_findings_use() {
        assert_eq!(
            NotifyThreshold::parse("data_loss"),
            Ok(NotifyThreshold::AtLeast(Severity::DataLoss))
        );
        assert_eq!(
            NotifyThreshold::parse("  Inconsistent "),
            Ok(NotifyThreshold::AtLeast(Severity::Inconsistent))
        );
        assert_eq!(NotifyThreshold::parse("off"), Ok(NotifyThreshold::None));
        assert_eq!(NotifyThreshold::parse("none"), Ok(NotifyThreshold::None));
    }

    /// A typo must not resolve to a working threshold in either
    /// direction — silently "everything" pages for an anomaly, silently
    /// "nothing" is indistinguishable from a healthy instance.
    #[test]
    fn a_typo_is_rejected_and_names_the_accepted_set() {
        let err = NotifyThreshold::parse("critical").expect_err("not a severity here");
        assert!(err.contains("data_loss"), "unhelpful message: {err}");
    }

    /// The first live test read as `1 anomaly finding(s) of kind test_alert
    /// (scanned 0)` — accurate and useless to the human who pressed the
    /// button. It has to say what it is.
    #[test]
    fn a_test_message_says_it_is_a_test() {
        let alert = FindingAlert {
            job: "webhook_test".into(),
            run_id: "r0".into(),
            kind: "test_alert".into(),
            severity: "anomaly".into(),
            transition: Transition::Appeared,
            count: 1,
            scanned: 0,
            detail: None,
            class: AlertClass::Test,
        };
        let s = alert.summary();
        assert!(s.contains("test message from OxiCloud"), "{s}");
        assert!(
            !s.contains("finding(s)") && !s.contains("scanned"),
            "a test must not read as a finding: {s}"
        );
    }

    /// Job names and finding kinds are full of underscores, and Telegram's
    /// Markdown mode rejects an unpaired `_` with "can't parse entities" —
    /// losing the alert entirely. So the shared text carries no Markdown
    /// markers at all.
    #[test]
    fn the_summary_is_plain_text_so_every_receiver_renders_it() {
        let alert = FindingAlert {
            job: "blobs_consistency".into(),
            run_id: "r1".into(),
            kind: "manifest_refcount_mismatch".into(),
            severity: "inconsistent".into(),
            transition: Transition::Appeared,
            count: 2,
            scanned: 10,
            detail: None,
            class: AlertClass::Finding,
        };
        let s = alert.summary();
        assert!(
            !s.contains('`'),
            "backticks render literally on Telegram: {s}"
        );
        assert!(!s.contains('*'), "{s}");
    }

    #[test]
    fn a_cleared_alert_reads_as_resolution_not_as_zero_findings() {
        let alert = FindingAlert {
            job: "blobs_consistency".into(),
            run_id: "r1".into(),
            kind: "orphan_blob".into(),
            severity: "inconsistent".into(),
            transition: Transition::Cleared,
            class: AlertClass::Finding,
            count: 0,
            scanned: 2026,
            detail: None,
        };
        let s = alert.summary();
        assert!(s.contains("no longer reported"), "{s}");
        assert!(
            !s.contains("0 "),
            "a cleared alert must not read as a count: {s}"
        );
    }
}
