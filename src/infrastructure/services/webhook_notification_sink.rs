//! The outbound webhook transport. Job findings are its first consumer,
//! not its definition — hence `OXICLOUD_WEBHOOK_*` rather than a
//! jobs-scoped name, and an admin test endpoint under `/webhook/test`.
//!
//! **Slack is not a separate integration.** Neither are Discord and
//! Teams: all three accept an HTTP POST with a JSON body and differ only
//! in which key holds the text. So this is one transport with a payload
//! formatter, which is also what keeps the licence clean — a vendor SDK
//! would add a dependency and a coupling to a project that must stay
//! self-hostable under MIT.

use std::time::Duration;

use async_trait::async_trait;
use serde_json::json;

use crate::application::ports::notification_sink_ports::{
    FindingAlert, NotificationSink, Transition,
};
use crate::common::errors::DomainError;

/// Which JSON shape to POST.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WebhookFormat {
    /// The alert's own fields, flat. For an operator's own receiver,
    /// where a structured payload beats a sentence.
    #[default]
    Generic,
    /// `{"text": "..."}` — Slack incoming webhooks.
    Slack,
    /// `{"content": "..."}` — Discord.
    Discord,
    /// `{"text": "..."}` with the legacy MessageCard type — Teams.
    Teams,
    /// `{"chat_id": …, "text": "..."}` — the Telegram Bot API's
    /// `sendMessage`. URL is `https://api.telegram.org/bot<TOKEN>/sendMessage`
    /// and `target` is the chat id: a negative group / supergroup id
    /// (`-100…`), a positive private-chat id, or `@channelusername`.
    Telegram,
    /// `{"topic": "...", "message": "...", …}` — ntfy's JSON publishing
    /// route. URL is the ntfy base (`https://ntfy.sh/`) and `target` is
    /// the topic.
    ///
    /// The topic goes in the body rather than the URL because this sink
    /// speaks JSON: ntfy's `POST /<topic>` form treats the whole body as
    /// the message text, so JSON publishing has to go to the root with
    /// `topic` as a field.
    Ntfy,
}

impl WebhookFormat {
    /// Parse `OXICLOUD_WEBHOOK_FORMAT`. Unknown values are an
    /// error rather than a fallback to `generic`: a typo'd `slak` would
    /// post a body Slack ignores, which looks exactly like a webhook that
    /// was never called.
    pub fn parse(raw: &str) -> Result<Self, String> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "generic" => Ok(Self::Generic),
            "slack" => Ok(Self::Slack),
            "discord" => Ok(Self::Discord),
            "teams" => Ok(Self::Teams),
            "telegram" => Ok(Self::Telegram),
            "ntfy" => Ok(Self::Ntfy),
            other => Err(format!(
                "`{other}` is not a webhook format \
                 (accepted: generic, slack, discord, teams, telegram, ntfy)"
            )),
        }
    }

    /// Does this format carry its recipient in the payload, needing a
    /// `target` alongside the URL? Telegram's chat id and ntfy's topic.
    pub fn needs_target(self) -> bool {
        matches!(self, Self::Telegram | Self::Ntfy)
    }

    /// Render one alert into this format's body. `target` is used only by
    /// the formats where the recipient travels in the payload rather than
    /// in the URL.
    pub fn body(self, alert: &FindingAlert, target: Option<&str>) -> serde_json::Value {
        let text = match alert.transition {
            // The emoji earns its place: these land in a channel beside
            // unrelated traffic, and severity has to be legible without
            // reading the sentence.
            Transition::Appeared => format!("🚨 {}", alert.summary()),
            Transition::Cleared => format!("✅ {}", alert.summary()),
        };
        match self {
            Self::Generic => json!({
                "job":        alert.job,
                "run_id":     alert.run_id,
                "kind":       alert.kind,
                "severity":   alert.severity,
                "transition": alert.transition.as_str(),
                "count":      alert.count,
                "scanned":    alert.scanned,
                "summary":    alert.summary(),
            }),
            Self::Slack => json!({ "text": text }),
            Self::Discord => json!({ "content": text }),
            Self::Teams => json!({
                "@type":    "MessageCard",
                "@context": "https://schema.org/extensions",
                "text":     text,
            }),
            // `target` is validated at construction, so `None` here is
            // unreachable in production. Serialising it as null rather
            // than panicking keeps a misconfiguration a 400 from the
            // receiver instead of a crashed job.
            //
            // A numeric chat id is sent as a JSON NUMBER, not a string.
            // Telegram documents the field as "Integer or String" and does
            // accept a quoted number, but the typed form is what the API
            // actually describes — and group ids are negative
            // (`-1001234567890` for a supergroup), which is exactly the
            // shape most likely to be handled inconsistently by a lenient
            // parser. `@channelusername` stays a string, as it must.
            Self::Telegram => {
                let chat_id = match target.and_then(|t| t.parse::<i64>().ok()) {
                    Some(n) => json!(n),
                    None => json!(target),
                };
                json!({ "chat_id": chat_id, "text": text })
            }
            Self::Ntfy => json!({
                "topic":    target,
                "title":    format!("OxiCloud: {}", alert.job),
                "message":  alert.summary(),
                // ntfy's scale is 1–5. Data loss is the case worth
                // breaking a do-not-disturb rule for; other appearances
                // are high but not urgent; a resolution is informational
                // and must not buzz a phone at 3am.
                "priority": match (alert.transition, alert.severity.as_str()) {
                    (Transition::Appeared, "data_loss") => 5,
                    (Transition::Appeared, _) => 4,
                    (Transition::Cleared, _) => 3,
                },
                "tags": match alert.transition {
                    Transition::Appeared => ["rotating_light"],
                    Transition::Cleared => ["white_check_mark"],
                },
            }),
        }
    }
}

pub struct WebhookNotificationSink {
    client: reqwest::Client,
    url: String,
    format: WebhookFormat,
    /// Recipient for formats that carry it in the payload — a Telegram
    /// chat id, an ntfy topic. Required at construction for those,
    /// unused by the rest.
    target: Option<String>,
}

/// Cap on how much of a receiver's response body the test endpoint echoes
/// back. Enough for `{"ok":false,"description":"chat not found"}`, short of
/// an HTML error page.
const MAX_REPORTED_BODY: usize = 512;

/// Is this status worth trying again?
///
/// Retrying a permanent rejection is not resilience, it is five times the
/// delay for the same answer — and the job's terminal path is waiting. A
/// 404 is the wrong URL, a 401/403 a revoked token, a 410 a Slack hook
/// someone deleted: none of those recover on their own.
///
/// `429` is included because it is explicitly "ask again later", and 5xx
/// because a receiver having a bad minute is the case backoff exists for.
fn retryable_status(status: reqwest::StatusCode) -> bool {
    status == reqwest::StatusCode::TOO_MANY_REQUESTS || status.is_server_error()
}

impl WebhookNotificationSink {
    /// Timeout on the whole request. A sink runs on a job's terminal
    /// path, so an unbounded POST would hold the run open for as long as
    /// the receiver felt like stalling — the shape of the Azure
    /// `put_blob` hang, and not one to repeat deliberately.
    const TIMEOUT: Duration = Duration::from_secs(10);

    /// Attempts per alert, including the first.
    ///
    /// Alerting fires exactly when an instance is unhealthy, which is
    /// also when a network is most likely to be having its own problem —
    /// so a single failed POST is a bad reason to lose the one message
    /// that says bytes may be gone.
    const MAX_ATTEMPTS: u32 = 5;

    /// First backoff step; doubles each time — 1s, 2s, 4s, 8s.
    ///
    /// Worst case per alert is those 15s of waiting plus five 10s
    /// timeouts, so ~65s. Bounded on purpose: the run is already marked
    /// Completed before any of this, so the only cost is holding the
    /// job's permit, and an unbounded retry would hold it forever.
    const BASE_BACKOFF: Duration = Duration::from_secs(1);

    pub fn new(
        url: String,
        format: WebhookFormat,
        target: Option<String>,
    ) -> Result<Self, DomainError> {
        // Caught here rather than at the first alert: a channel that is
        // only discovered to be misconfigured when something goes wrong is
        // a channel that is silent exactly when it matters.
        if format.needs_target() && target.as_deref().unwrap_or("").is_empty() {
            return Err(DomainError::internal_error(
                "Notify",
                "webhook format `telegram` (chat id) and `ntfy` (topic) need \
                 OXICLOUD_WEBHOOK_TARGET"
                    .to_string(),
            ));
        }
        let client = reqwest::Client::builder()
            .timeout(Self::TIMEOUT)
            .build()
            .map_err(|e| {
                DomainError::internal_error("Notify", format!("webhook HTTP client: {e}"))
            })?;
        Ok(Self {
            client,
            url,
            format,
            target,
        })
    }
}

#[async_trait]
impl NotificationSink for WebhookNotificationSink {
    fn name(&self) -> &'static str {
        "webhook"
    }

    /// One attempt, reporting the receiver's status and body.
    ///
    /// No retry, deliberately: an operator clicking "test" wants an answer
    /// now, and the backoff `deliver` uses would make a plainly-dead host
    /// take ~65s to say so. The body is the useful half — a 403 alone does
    /// not distinguish a revoked Slack token from a disabled channel, and
    /// the body says `invalid_token`.
    async fn deliver_reporting(
        &self,
        alert: &FindingAlert,
    ) -> crate::application::ports::notification_sink_ports::DeliveryReport {
        use crate::application::ports::notification_sink_ports::DeliveryReport;

        let body = self.format.body(alert, self.target.as_deref());
        match self.client.post(&self.url).json(&body).send().await {
            Ok(res) => {
                let code = res.status().as_u16();
                let success = res.status().is_success();
                // Truncated: a receiver answering with an HTML error page
                // should not put kilobytes through the admin API.
                let text = res.text().await.unwrap_or_default();
                let mut message = text.trim().to_string();
                if message.len() > MAX_REPORTED_BODY {
                    message.truncate(MAX_REPORTED_BODY);
                    message.push('…');
                }
                DeliveryReport {
                    success,
                    code: Some(code),
                    message: (!message.is_empty()).then_some(message),
                }
            }
            // No response at all — DNS, refused, timeout. `code` stays
            // None, which is itself the diagnosis: nothing answered.
            Err(e) => DeliveryReport {
                success: false,
                code: None,
                message: Some(e.to_string()),
            },
        }
    }

    async fn deliver(&self, alert: &FindingAlert) -> Result<(), DomainError> {
        let body = self.format.body(alert, self.target.as_deref());
        let mut backoff = Self::BASE_BACKOFF;

        for attempt in 1..=Self::MAX_ATTEMPTS {
            // Two failure shapes, and only one of them is worth repeating.
            // `Err` from `send` is transport — DNS, refused connection,
            // timeout — which is exactly the "network fell down" case. A
            // response, by contrast, means the receiver answered, and then
            // the status decides.
            let err: DomainError = match self.client.post(&self.url).json(&body).send().await {
                Ok(res) if res.status().is_success() => return Ok(()),
                Ok(res) => {
                    let status = res.status();
                    // The status is the whole diagnosis for a webhook, so
                    // it travels in the error either way.
                    let err =
                        DomainError::internal_error("Notify", format!("webhook returned {status}"));
                    if !retryable_status(status) {
                        return Err(err);
                    }
                    err
                }
                Err(e) => DomainError::internal_error("Notify", format!("webhook POST: {e}")),
            };

            if attempt == Self::MAX_ATTEMPTS {
                return Err(DomainError::internal_error(
                    "Notify",
                    format!("{err} (after {attempt} attempts)"),
                ));
            }
            tracing::debug!(
                target: "oxicloud::scheduler",
                event = "notify.delivery_retry",
                attempt,
                backoff_ms = backoff.as_millis(),
                error = %err,
                "webhook delivery failed; retrying"
            );
            tokio::time::sleep(backoff).await;
            backoff *= 2;
        }
        // Unreachable: the loop returns on the final attempt. Written as a
        // real error rather than `unreachable!()` so a future edit to the
        // bounds cannot turn a logic slip into a panic on a path whose
        // whole purpose is surviving failure.
        Err(DomainError::internal_error(
            "Notify",
            "webhook retry loop exhausted without a verdict".to_string(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn alert(transition: Transition) -> FindingAlert {
        FindingAlert {
            job: "backend_consistency".into(),
            run_id: "11111111-1111-1111-1111-111111111111".into(),
            kind: "blob_unreadable".into(),
            severity: "data_loss".into(),
            transition,
            count: 3,
            scanned: 2026,
            test: false,
        }
    }

    /// Each chat product reads a different key, and posting the wrong one
    /// is indistinguishable from never calling the webhook at all.
    #[test]
    fn each_format_puts_the_text_where_its_receiver_looks() {
        let a = alert(Transition::Appeared);
        assert!(WebhookFormat::Slack.body(&a, None)["text"].is_string());
        assert!(WebhookFormat::Discord.body(&a, None)["content"].is_string());
        assert_eq!(WebhookFormat::Teams.body(&a, None)["@type"], "MessageCard");
        assert!(WebhookFormat::Teams.body(&a, None)["text"].is_string());
    }

    /// Telegram and ntfy carry the recipient in the body, which no other
    /// format does — bending the Slack shape to fit would post a body
    /// either one rejects.
    #[test]
    fn the_payload_formats_carry_their_recipient_in_the_body() {
        let a = alert(Transition::Appeared);

        // Negative because Telegram group / supergroup ids are, and sent
        // as a NUMBER because that is the typed form the Bot API describes.
        let tg = WebhookFormat::Telegram.body(&a, Some("-1001234567890"));
        assert_eq!(tg["chat_id"], -1001234567890_i64);
        assert!(
            tg["chat_id"].is_i64(),
            "a numeric chat id must not be quoted"
        );
        assert!(tg["text"].as_str().unwrap().contains("blob_unreadable"));

        // A channel username is not a number and has to stay a string.
        let by_name = WebhookFormat::Telegram.body(&a, Some("@oxicloud_alerts"));
        assert_eq!(by_name["chat_id"], "@oxicloud_alerts");

        let ntfy = WebhookFormat::Ntfy.body(&a, Some("oxicloud-alerts"));
        assert_eq!(ntfy["topic"], "oxicloud-alerts");
        assert_eq!(ntfy["title"], "OxiCloud: backend_consistency");
        assert!(
            ntfy["message"]
                .as_str()
                .unwrap()
                .contains("blob_unreadable")
        );
    }

    /// ntfy's 1–5 scale is what decides whether a phone buzzes at 3am.
    /// Data loss earns that; a resolution must not.
    #[test]
    fn ntfy_priority_tracks_how_urgent_the_news_is() {
        let mut data_loss = alert(Transition::Appeared);
        data_loss.severity = "data_loss".into();
        assert_eq!(
            WebhookFormat::Ntfy.body(&data_loss, Some("t"))["priority"],
            5
        );

        let mut drift = alert(Transition::Appeared);
        drift.severity = "inconsistent".into();
        assert_eq!(WebhookFormat::Ntfy.body(&drift, Some("t"))["priority"], 4);

        let cleared = alert(Transition::Cleared);
        assert_eq!(WebhookFormat::Ntfy.body(&cleared, Some("t"))["priority"], 3);
    }

    /// Refused at construction, not at the first alert — a channel
    /// discovered to be misconfigured only when something breaks is silent
    /// exactly when it matters.
    #[test]
    fn a_payload_format_without_a_target_is_refused_up_front() {
        for format in [WebhookFormat::Telegram, WebhookFormat::Ntfy] {
            assert!(
                WebhookNotificationSink::new("https://x/".into(), format, None).is_err(),
                "{format:?} needs a target"
            );
            assert!(
                WebhookNotificationSink::new("https://x/".into(), format, Some(String::new()))
                    .is_err(),
                "an empty target is as unusable as an absent one ({format:?})"
            );
        }
        assert!(
            WebhookNotificationSink::new("https://x/".into(), WebhookFormat::Slack, None).is_ok(),
            "formats that put the recipient in the URL need no target"
        );
    }

    /// The generic shape is for machines, so the fields have to survive
    /// as fields rather than being flattened into prose.
    #[test]
    fn the_generic_format_stays_structured() {
        let body = WebhookFormat::Generic.body(&alert(Transition::Appeared), None);
        assert_eq!(body["job"], "backend_consistency");
        assert_eq!(body["kind"], "blob_unreadable");
        assert_eq!(body["severity"], "data_loss");
        assert_eq!(body["transition"], "appeared");
        assert_eq!(body["count"], 3);
        assert_eq!(body["scanned"], 2026);
    }

    #[test]
    fn a_cleared_alert_is_visibly_good_news() {
        let appeared = WebhookFormat::Slack.body(&alert(Transition::Appeared), None);
        let cleared = WebhookFormat::Slack.body(&alert(Transition::Cleared), None);
        assert!(appeared["text"].as_str().unwrap().starts_with("🚨"));
        assert!(cleared["text"].as_str().unwrap().starts_with("✅"));
    }

    /// Retrying a permanent rejection is five times the delay for the same
    /// answer, on a path the job is waiting on. The split is the point of
    /// the backoff, not the sleeping.
    #[test]
    fn only_failures_that_can_recover_are_retried() {
        use reqwest::StatusCode;
        for s in [
            StatusCode::INTERNAL_SERVER_ERROR,
            StatusCode::BAD_GATEWAY,
            StatusCode::SERVICE_UNAVAILABLE,
            StatusCode::GATEWAY_TIMEOUT,
            StatusCode::TOO_MANY_REQUESTS,
        ] {
            assert!(retryable_status(s), "{s} should be retried");
        }
        for s in [
            StatusCode::BAD_REQUEST,
            StatusCode::UNAUTHORIZED,
            StatusCode::FORBIDDEN,
            StatusCode::NOT_FOUND,
            StatusCode::GONE,
        ] {
            assert!(
                !retryable_status(s),
                "{s} will not fix itself — retrying only delays the job"
            );
        }
    }

    /// Exercises the real loop: a listener that accepts and immediately
    /// drops the connection is the "network fell down" shape, so delivery
    /// must come back as an error naming the attempt count rather than
    /// hanging or panicking.
    ///
    /// Backoff is why this is `#[ignore]` by default — it sleeps 1+2+4+8s.
    /// Run it with `cargo test -- --ignored webhook_retries`.
    #[tokio::test]
    #[ignore = "sleeps ~15s through the real backoff"]
    async fn webhook_retries_then_reports_the_attempt_count() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                // Accept and drop: the client sees a closed connection,
                // which is a transport error rather than a status.
                let _ = listener.accept().await;
            }
        });

        let sink = WebhookNotificationSink::new(
            format!("http://{addr}/hook"),
            WebhookFormat::Generic,
            None,
        )
        .unwrap();
        let err = sink
            .deliver(&alert(Transition::Appeared))
            .await
            .expect_err("a dropped connection cannot succeed");
        assert!(
            err.to_string().contains("after 5 attempts"),
            "the error should say how hard it tried: {err}"
        );
    }

    /// The test path must report what the receiver said, and must not
    /// retry — an operator clicking "test" against a dead host should get
    /// an answer immediately rather than after the full backoff.
    #[tokio::test]
    async fn the_test_report_carries_the_status_and_body_without_retrying() {
        use std::sync::Arc;
        use tokio::io::AsyncWriteExt;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let hits_server = hits.clone();
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                hits_server.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let body = r#"{"ok":false,"description":"chat not found"}"#;
                let res = format!(
                    "HTTP/1.1 403 Forbidden\r\nContent-Length: {}\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(res.as_bytes()).await;
                let _ = stream.flush().await;
            }
        });

        let sink = WebhookNotificationSink::new(
            format!("http://{addr}/hook"),
            WebhookFormat::Generic,
            None,
        )
        .unwrap();
        let report = sink.deliver_reporting(&alert(Transition::Appeared)).await;

        assert!(!report.success);
        assert_eq!(report.code, Some(403), "the status has to survive");
        assert!(
            report
                .message
                .as_deref()
                .unwrap_or("")
                .contains("chat not found"),
            "the body is the diagnosis: {:?}",
            report.message
        );
        assert_eq!(
            hits.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "a test must not retry — 403 is permanent and the operator is waiting"
        );
    }

    #[test]
    fn an_unknown_format_is_rejected_and_names_the_real_ones() {
        let err = WebhookFormat::parse("slak").expect_err("typo must not pass");
        assert!(err.contains("slack"), "unhelpful message: {err}");
        assert_eq!(WebhookFormat::parse("  SLACK "), Ok(WebhookFormat::Slack));
    }
}
