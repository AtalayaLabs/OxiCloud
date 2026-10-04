//! Job findings by mail — the second [`NotificationSink`], over the
//! `EmailSender` port the magic-link flow already uses.
//!
//! **Why a sink and not a webhook "format".** The tempting shortcut was
//! an `smtp-gateway` entry in [`WebhookFormat`] with the address in
//! `OXICLOUD_WEBHOOK_TARGET`. It does not survive contact: a format
//! returns a JSON body, so it cannot produce a subject line — and the
//! subject is most of what makes mail readable at 3am. `OXICLOUD_WEBHOOK_*`
//! is also a single destination, so email-as-a-format would mean email
//! *instead of* Telegram rather than alongside it, which is exactly the
//! pairing an operator wants: a push to wake them, mail for the record.
//!
//! **One message per alert, not a digest per run.** Alerts are per
//! finding *kind* and per job run, so a run produces one or two and a
//! first-ever run on a messy instance maybe five — and the transition
//! diff guarantees it does not repeat next week. Against that volume a
//! digest buys little and costs the thing mail is good at: a subject
//! naming one severity and one kind can be filtered and threaded, while
//! "3 findings changed" cannot.
//!
//! [`WebhookFormat`]: super::webhook_notification_sink::WebhookFormat

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;

use crate::application::ports::email_sender::{EmailMessage, EmailSender};
use crate::application::ports::notification_sink_ports::{
    AlertClass, DeliveryReport, FindingAlert, NotificationSink, Transition,
};
use crate::common::errors::{DomainError, ErrorKind};

pub struct EmailNotificationSink {
    sender: Arc<dyn EmailSender>,
    /// Validated at config parse — see `parse_notify_emails`. Non-empty,
    /// or the sink is not built at all.
    recipients: Vec<String>,
    /// First backoff step. A field only so the retry tests can set it to
    /// zero: `tokio::time::pause` needs tokio's `test-util` feature, and
    /// one dependency feature to avoid three seconds of real sleeping in
    /// one test is a poor trade.
    base_backoff: Duration,
}

impl EmailNotificationSink {
    /// Cap on one SMTP exchange.
    ///
    /// `SmtpEmailSender` sets no timeout of its own, and a sink runs on a
    /// job's terminal path: without this, a relay that accepts the
    /// connection and then stalls would hold the run's permit for as long
    /// as it felt like stalling. Twenty seconds rather than the webhook's
    /// ten because an SMTP delivery is a multi-round-trip conversation
    /// with a TLS handshake in front of it.
    const TIMEOUT: Duration = Duration::from_secs(20);

    /// Attempts per recipient, including the first. Same reasoning as the
    /// webhook's: alerting fires precisely when the instance is unhealthy,
    /// which is when the network is likeliest to be having its own
    /// problem, and one refused connection is a poor reason to lose the
    /// message that says bytes may be gone.
    const MAX_ATTEMPTS: u32 = 3;

    /// First backoff step; doubles — 1s, 2s. Worst case per recipient is
    /// three timeouts plus that 3s of waiting, ~63s, which is deliberately
    /// in the same range as the webhook's ~65s: the run is already marked
    /// Completed, so the only cost is holding the job's permit, and both
    /// paths should hold it for a comparable bounded time.
    const BASE_BACKOFF: Duration = Duration::from_secs(1);

    pub fn new(sender: Arc<dyn EmailSender>, recipients: Vec<String>) -> Result<Self, DomainError> {
        if recipients.is_empty() {
            return Err(DomainError::internal_error(
                "Notify",
                "email alerting needs at least one recipient in \
                 OXICLOUD_JOBS_NOTIFY_EMAIL_TO"
                    .to_string(),
            ));
        }
        Ok(Self {
            sender,
            recipients,
            base_backoff: Self::BASE_BACKOFF,
        })
    }

    /// Collapse the backoff, for tests that assert on attempt counts.
    #[cfg(test)]
    fn without_backoff(mut self) -> Self {
        self.base_backoff = Duration::ZERO;
        self
    }

    /// Subject line. Severity and kind lead, so a mailbox rule can sort on
    /// them and a phone notification is legible without opening anything.
    fn subject(alert: &FindingAlert) -> String {
        match (alert.class, alert.transition) {
            (AlertClass::Test, _) => "[OxiCloud] notification test".to_string(),
            // No severity and no count in the lead: a stopped run has no
            // severity of its own (it is graded `anomaly` so the existing
            // threshold can gate it, which is a mechanism, not a
            // description), and "(1)" would read as one finding.
            (AlertClass::RunHealth { .. }, Transition::Appeared) => {
                format!("[OxiCloud] stopped: {} in {}", alert.kind, alert.job)
            }
            (AlertClass::RunHealth { .. }, Transition::Cleared) => {
                format!("[OxiCloud] recovered: {} in {}", alert.kind, alert.job)
            }
            (AlertClass::Finding, Transition::Appeared) => format!(
                "[OxiCloud] {}: {} ({}) in {}",
                alert.severity, alert.kind, alert.count, alert.job
            ),
            (AlertClass::Finding, Transition::Cleared) => {
                format!("[OxiCloud] resolved: {} in {}", alert.kind, alert.job)
            }
        }
    }

    /// Body: the shared one-line summary every transport uses, then the
    /// same fields the `generic` webhook payload carries.
    ///
    /// Self-contained on purpose — mail is read where the panel is not
    /// open, and often by someone deciding whether to go and open it.
    /// Plain text, no HTML part: [`FindingAlert::summary`] is already
    /// written to render anywhere, and a second representation would be a
    /// second thing to keep saying the same thing.
    fn body(alert: &FindingAlert) -> String {
        let mut body = String::with_capacity(512);
        body.push_str(&alert.summary());
        body.push_str("\n\n");
        if alert.class != AlertClass::Test {
            body.push_str(&format!("Job:        {}\n", alert.job));
            body.push_str(&format!("Finding:    {}\n", alert.kind));
            body.push_str(&format!("Severity:   {}\n", alert.severity));
            body.push_str(&format!("Transition: {}\n", alert.transition.as_str()));
            if alert.transition == Transition::Appeared {
                body.push_str(&format!("Count:      {}\n", alert.count));
            }
            body.push_str(&format!("Scanned:    {}\n", alert.scanned));
            body.push_str(&format!("Run:        {}\n", alert.run_id));
            // The whole error chain, for a run that stopped. `Finding:`
            // above says `backend_unavailable`; this is the part that
            // says whether to look at DNS, credentials or the bucket.
            if let Some(d) = alert.detail_trimmed() {
                body.push_str(&format!("\nDetail:\n{d}\n"));
            }
            body.push_str(
                "\nThe affected items are listed under Admin → Jobs in the \
                 OxiCloud web panel. This message is a notification only — \
                 nothing has been repaired or deleted on account of it.\n",
            );
        }
        body
    }

    fn message(&self, alert: &FindingAlert, to: &str) -> EmailMessage {
        EmailMessage {
            to: to.to_string(),
            subject: Self::subject(alert),
            text_body: Self::body(alert),
            html_body: None,
        }
    }

    /// One send, bounded by [`Self::TIMEOUT`].
    async fn send_once(&self, message: EmailMessage) -> Result<u16, DomainError> {
        let to = message.to.clone();
        match tokio::time::timeout(Self::TIMEOUT, self.sender.send(message)).await {
            Ok(Ok(outcome)) => Ok(outcome.code),
            Ok(Err(e)) => Err(e),
            Err(_elapsed) => Err(DomainError::timeout(
                "Notify",
                format!(
                    "SMTP delivery to {to} did not finish within {}s",
                    Self::TIMEOUT.as_secs()
                ),
            )),
        }
    }

    /// Is this failure worth a second attempt?
    ///
    /// The port collapses every SMTP rejection into one
    /// `ErrorKind::InternalError` carrying lettre's message, so a
    /// permanent 550 is indistinguishable here from a refused connection.
    /// Retrying anyway is the right way round: over-retrying a permanent
    /// rejection costs 3s of waiting on a run that has already completed,
    /// while under-retrying a transient one loses the alert.
    ///
    /// The one case that is knowably permanent is a malformed recipient,
    /// which the port reports as `InvalidInput`. Unreachable in practice —
    /// `OXICLOUD_JOBS_NOTIFY_EMAIL_TO` is validated at boot — but a typo
    /// that got past that check should fail fast rather than three times.
    fn retryable(e: &DomainError) -> bool {
        e.kind != ErrorKind::InvalidInput
    }
}

#[async_trait]
impl NotificationSink for EmailNotificationSink {
    fn name(&self) -> &'static str {
        "email"
    }

    async fn deliver(&self, alert: &FindingAlert) -> Result<(), DomainError> {
        // One message per recipient rather than one with several To:
        // addresses. The port is single-recipient by design, and it is
        // also the better behaviour: a shared operations list and a
        // personal address do not need to learn about each other, and one
        // address the relay rejects does not take the others down with it.
        let mut failures: Vec<String> = Vec::new();

        for to in &self.recipients {
            let mut backoff = self.base_backoff;
            let mut last_error = None;

            for attempt in 1..=Self::MAX_ATTEMPTS {
                match self.send_once(self.message(alert, to)).await {
                    Ok(_code) => {
                        last_error = None;
                        break;
                    }
                    Err(e) => {
                        let retry = Self::retryable(&e) && attempt < Self::MAX_ATTEMPTS;
                        tracing::warn!(
                            target: "oxicloud::scheduler",
                            event = "notify.email_attempt_failed",
                            to = %to,
                            job = %alert.job,
                            kind = %alert.kind,
                            attempt,
                            will_retry = retry,
                            error = %e,
                            "could not mail a finding alert to {to}: {e}"
                        );
                        last_error = Some(e);
                        if !retry {
                            break;
                        }
                        tokio::time::sleep(backoff).await;
                        backoff *= 2;
                    }
                }
            }

            if let Some(e) = last_error {
                failures.push(format!("{to}: {e}"));
            }
        }

        if failures.is_empty() {
            Ok(())
        } else {
            // A partial success is reported as a failure: the caller
            // audits and counts it, and "two of three operators were
            // told" is a fact worth the same attention as none of them
            // being told.
            Err(DomainError::internal_error(
                "Notify",
                format!(
                    "{} of {} alert recipient(s) failed — {}",
                    failures.len(),
                    self.recipients.len(),
                    failures.join("; ")
                ),
            ))
        }
    }

    /// One attempt per recipient, no retry, reporting what the server
    /// said. An operator pressing "test" wants an answer now rather than
    /// a minute of backoff against a relay that is plainly down.
    async fn deliver_reporting(&self, alert: &FindingAlert) -> DeliveryReport {
        let mut first_code = None;
        let mut failures: Vec<String> = Vec::new();

        for to in &self.recipients {
            match self.send_once(self.message(alert, to)).await {
                Ok(code) => first_code = first_code.or(Some(code)),
                Err(e) => failures.push(format!("{to}: {e}")),
            }
        }

        if failures.is_empty() {
            DeliveryReport {
                success: true,
                // The SMTP response code, in the slot the webhook sink
                // puts an HTTP status in — the admin panel renders one
                // `code` + `message` pair for both transports.
                code: first_code,
                message: Some(format!("accepted for {}", self.recipients.join(", "))),
            }
        } else {
            DeliveryReport {
                success: false,
                code: None,
                message: Some(failures.join("; ")),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::application::ports::email_sender::EmailSendOutcome;
    use crate::infrastructure::scheduler::FindingNotifier;

    /// Records what it was asked to send, and fails for any recipient in
    /// `fail_for`.
    struct StubSender {
        sent: Mutex<Vec<EmailMessage>>,
        fail_for: Vec<String>,
        /// Reported as `InvalidInput` rather than `InternalError`, to
        /// exercise the no-retry path.
        permanent: bool,
    }

    impl StubSender {
        fn ok() -> Arc<Self> {
            Arc::new(Self {
                sent: Mutex::new(Vec::new()),
                fail_for: Vec::new(),
                permanent: false,
            })
        }

        fn failing_for(addr: &str, permanent: bool) -> Arc<Self> {
            Arc::new(Self {
                sent: Mutex::new(Vec::new()),
                fail_for: vec![addr.to_string()],
                permanent,
            })
        }

        fn sent(&self) -> Vec<EmailMessage> {
            self.sent.lock().expect("stub lock").clone()
        }
    }

    #[async_trait]
    impl EmailSender for StubSender {
        async fn send(&self, message: EmailMessage) -> Result<EmailSendOutcome, DomainError> {
            let to = message.to.clone();
            self.sent.lock().expect("stub lock").push(message);
            if self.fail_for.contains(&to) {
                return Err(if self.permanent {
                    DomainError::new(ErrorKind::InvalidInput, "Notify", "bad mailbox")
                } else {
                    DomainError::internal_error("Notify", "connection refused".to_string())
                });
            }
            Ok(EmailSendOutcome {
                code: 250,
                message: "2.0.0 OK".to_string(),
            })
        }
    }

    fn sink(sender: Arc<StubSender>, recipients: &[&str]) -> EmailNotificationSink {
        EmailNotificationSink::new(sender, recipients.iter().map(|s| s.to_string()).collect())
            .expect("a non-empty recipient list builds")
            .without_backoff()
    }

    fn appeared() -> FindingAlert {
        FindingAlert {
            job: "blobs_consistency".into(),
            run_id: "11111111-1111-1111-1111-111111111111".into(),
            kind: "blob_unreadable".into(),
            severity: "data_loss".into(),
            transition: Transition::Appeared,
            count: 3,
            scanned: 2026,
            detail: None,
            class: AlertClass::Finding,
        }
    }

    /// The sink must not exist without somewhere to deliver — otherwise
    /// `FindingNotifier` counts a configured channel that silently drops
    /// every alert.
    #[test]
    fn an_empty_recipient_list_is_refused() {
        assert!(EmailNotificationSink::new(StubSender::ok(), vec![]).is_err());
    }

    /// Severity and kind lead the subject so a mailbox rule can sort on
    /// them and a phone preview is legible unopened.
    #[test]
    fn the_subject_leads_with_severity_and_kind() {
        let s = EmailNotificationSink::subject(&appeared());
        assert_eq!(
            s,
            "[OxiCloud] data_loss: blob_unreadable (3) in blobs_consistency"
        );
    }

    #[test]
    fn a_cleared_alert_reads_as_resolved_in_the_subject() {
        let mut alert = appeared();
        alert.transition = Transition::Cleared;
        alert.count = 0;
        let s = EmailNotificationSink::subject(&alert);
        assert!(s.contains("resolved"), "{s}");
        assert!(
            !s.contains("(0)"),
            "a resolution must not read as a count: {s}"
        );
    }

    /// Same defect the shared summary was fixed for: someone pressing
    /// "test" must not receive what looks like a real incident.
    #[test]
    fn a_test_alert_says_so_in_both_subject_and_body() {
        let alert = FindingNotifier::test_alert();
        let subject = EmailNotificationSink::subject(&alert);
        let body = EmailNotificationSink::body(&alert);
        assert_eq!(subject, "[OxiCloud] notification test");
        assert!(body.contains("test message from OxiCloud"), "{body}");
        assert!(
            !body.contains("Severity:") && !body.contains("Run:"),
            "a test has no findings to describe: {body}"
        );
    }

    /// Mail is read where the panel is not open, so the body carries the
    /// same fields the `generic` webhook payload does.
    #[test]
    fn the_body_is_self_contained() {
        let body = EmailNotificationSink::body(&appeared());
        for expected in [
            "blobs_consistency",
            "blob_unreadable",
            "data_loss",
            "appeared",
            "2026",
            "11111111-1111-1111-1111-111111111111",
        ] {
            assert!(body.contains(expected), "body is missing {expected}");
        }
        assert!(
            body.contains("nothing has been repaired or deleted"),
            "an alert must not read as a report of action taken: {body}"
        );
    }

    /// One message each, not one with several `To:` addresses — a shared
    /// list and a personal address do not need to learn about each other.
    #[tokio::test]
    async fn every_recipient_gets_their_own_message() {
        let sender = StubSender::ok();
        let sink = sink(sender.clone(), &["ops@example.com", "ed@example.org"]);

        sink.deliver(&appeared()).await.expect("both accepted");

        let sent = sender.sent();
        assert_eq!(sent.len(), 2);
        assert_eq!(sent[0].to, "ops@example.com");
        assert_eq!(sent[1].to, "ed@example.org");
        assert!(sent.iter().all(|m| m.html_body.is_none()));
    }

    /// One bad address must not cost the other recipients their alert…
    #[tokio::test]
    async fn a_rejected_recipient_does_not_stop_the_others() {
        let sender = StubSender::failing_for("broken@example.com", true);
        let sink = sink(sender.clone(), &["broken@example.com", "ops@example.com"]);

        let err = sink
            .deliver(&appeared())
            .await
            .expect_err("a partial delivery is reported as a failure");

        // …and the good one was still attempted.
        assert!(
            sender.sent().iter().any(|m| m.to == "ops@example.com"),
            "the second recipient was skipped"
        );
        // …while the operator is told which one failed and how many.
        assert!(err.to_string().contains("broken@example.com"), "{err}");
        assert!(err.to_string().contains("1 of 2"), "{err}");
    }

    /// A transient failure is retried; the alert is the one message that
    /// says bytes may be gone.
    #[tokio::test]
    async fn a_transient_failure_is_retried() {
        let sender = StubSender::failing_for("ops@example.com", false);
        let sink = sink(sender.clone(), &["ops@example.com"]);

        assert!(sink.deliver(&appeared()).await.is_err());
        assert_eq!(
            sender.sent().len(),
            EmailNotificationSink::MAX_ATTEMPTS as usize
        );
    }

    /// A malformed mailbox will not become valid in two seconds, and the
    /// run is waiting.
    #[tokio::test]
    async fn a_permanent_rejection_is_not_retried() {
        let sender = StubSender::failing_for("ops@example.com", true);
        let sink = sink(sender.clone(), &["ops@example.com"]);

        assert!(sink.deliver(&appeared()).await.is_err());
        assert_eq!(sender.sent().len(), 1, "a 5xx-class rejection was retried");
    }

    /// The admin test path reports the server's verdict instead of
    /// collapsing it, and makes exactly one attempt per recipient.
    #[tokio::test]
    async fn the_test_path_reports_the_smtp_code_without_retrying() {
        let sender = StubSender::ok();
        let sink = sink(sender.clone(), &["ops@example.com"]);

        let report = sink.deliver_reporting(&FindingNotifier::test_alert()).await;

        assert!(report.success);
        assert_eq!(report.code, Some(250));
        assert_eq!(sender.sent().len(), 1);
    }

    #[tokio::test]
    async fn a_failed_test_names_the_recipient_and_leaves_the_code_unset() {
        let sender = StubSender::failing_for("ops@example.com", false);
        let sink = sink(sender.clone(), &["ops@example.com"]);

        let report = sink.deliver_reporting(&FindingNotifier::test_alert()).await;

        assert!(!report.success);
        assert_eq!(report.code, None, "nothing accepted the message");
        assert!(
            report
                .message
                .as_deref()
                .unwrap_or_default()
                .contains("ops@example.com"),
            "{report:?}"
        );
        assert_eq!(sender.sent().len(), 1, "the test path must not back off");
    }
}
