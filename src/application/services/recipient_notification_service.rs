//! Unified entry point for share-related notification emails.
//!
//! Single service called by both `POST /api/grants` (initial invitation
//! when a grant lands) and `POST /api/grants/{id}/notify` (manual resend
//! from the My Shares menu). Replaces the prior arrangement where
//! `create_grant` directly invoked
//! [`MagicLinkInviteService::issue_invitation`] and internal users got
//! no email at all.
//!
//! # Behaviour ladder
//!
//! Per resolved recipient member:
//!
//! 1. **Eligibility** decides the dispatch arm:
//!    - `magic_link_eligibility(recipient) == Allow` →
//!      `NotifyKind::MagicLink` (mints a token and emails the
//!      invitation by delegating to
//!      [`MagicLinkInviteService::issue_invitation`]).
//!    - Otherwise (password user, OIDC user, OIDC-linked external) →
//!      `NotifyKind::PlainNotification` — provided the recipient has
//!      not opted out (`auth.users.notify_on_share = false`) and the
//!      operator-level kill switch
//!      `OXICLOUD_NOTIFY_INTERNAL_USERS_ON_SHARE` is `true`.
//!    - Otherwise → `NotifyOutcome::NotApplicable` with a structured
//!      reason.
//! 2. **Coalesce check** keyed by `(granter_id, recipient_email)`. If
//!    the last send for this pair was less than the window ago, return
//!    `Coalesced` without dispatching. Magic-link first-invitations
//!    are NOT coalesced — they're the only way the recipient can claim
//!    the share.
//! 3. **Hard rate limit** keyed by recipient email. Reuses
//!    `magic_link_send_per_email_rate_limiter` so an attacker can't
//!    alternate between `/notify` and `/magic/v1/{token}/resend` to
//!    double the cap.
//! 4. **Dispatch** via the magic-link arm or the plain-notification
//!    arm. On successful SMTP send, update the coalesce timestamp.
//! 5. **Audit**: one `grant.notify_sent` or `grant.notify_skipped` per
//!    member; for group sends, one `grant.notify_group_expanded`
//!    summary line carrying `group_id` and `member_count`.
//!
//! # Forward-compatibility
//!
//! The entry takes `(granter, subject, resource, trigger)` — NOT a
//! pre-resolved `&User` — so [`Subject::Group`] is a real arm in
//! [`Self::resolve_subject_members`] and not a future refactor. The
//! infrastructure (group repository, transitive expansion with 30s
//! Moka cache) already ships from earlier work; we just plug in.

use std::sync::Arc;
use std::time::Duration;

use askama::Template;
use chrono::{DateTime, Utc};
use moka::sync::Cache;
use uuid::Uuid;

use crate::application::dtos::grant_dto::{NotifyOutcomeDto, NotifyOutcomeSetDto};
use crate::application::ports::email_sender::{EmailMessage, EmailSender};
use crate::application::services::i18n_application_service::I18nApplicationService;
use crate::application::services::magic_link_invite_service::{
    Eligibility, MagicLinkInviteService, magic_link_eligibility,
};
use crate::application::services::subject_group_service::SubjectGroupService;
use crate::common::config::MagicLinkConfig;
use crate::common::errors::DomainError;
use crate::common::locale::{Locale, LocaleRegistry};
use crate::domain::entities::user::User;
use crate::domain::repositories::user_repository::UserRepository;
use crate::domain::services::authorization::{Resource, Subject};
use crate::infrastructure::repositories::pg::UserPgRepository;
use crate::interfaces::middleware::rate_limit::RateLimiter;

/// Concurrent per-recipient dispatches in flight during a group fan-out.
/// High enough to collapse a 30-member group's serial SMTP latency,
/// low enough not to flood the relay (most reject >10 parallel sessions).
const NOTIFY_DISPATCH_CONCURRENCY: usize = 6;

/// What triggered the notification — purely an audit discriminator.
/// `GrantCreated` → fired implicitly when a grant lands; `ManualResend`
/// → granter explicitly clicked "Notify by email" in My Shares.
#[derive(Debug, Clone, Copy)]
pub enum NotifyTrigger {
    GrantCreated,
    ManualResend,
}

impl NotifyTrigger {
    fn audit_str(self) -> &'static str {
        match self {
            NotifyTrigger::GrantCreated => "grant_created",
            NotifyTrigger::ManualResend => "manual_resend",
        }
    }
}

/// Which email arm dispatched. `MagicLink` carries a one-shot token in
/// the URL; `PlainNotification` carries only a `/login` deep link.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotifyKind {
    MagicLink,
    PlainNotification,
}

impl NotifyKind {
    fn audit_str(self) -> &'static str {
        match self {
            NotifyKind::MagicLink => "magic_link",
            NotifyKind::PlainNotification => "plain_notification",
        }
    }
}

/// One per resolved recipient. The variant names are stable audit-log
/// values — log aggregators key off them; do not rename or repurpose.
#[derive(Debug, Clone)]
pub enum NotifyOutcome {
    /// SMTP send succeeded for this recipient.
    Sent { kind: NotifyKind },
    /// Skipped because the same (granter, recipient) pair was notified
    /// less than the coalesce window ago. The grant is recorded; the
    /// recipient sees it at next login. Carries the last-send timestamp
    /// so the frontend can format an informative toast.
    Coalesced { last_sent_at: DateTime<Utc> },
    /// Per-recipient hard cap reached. Caller may retry after the
    /// returned number of seconds.
    RateLimited { retry_after_secs: u32 },
    /// No mail dispatched. `reason` is a stable enum-style key:
    /// `recipient_opted_out`, `operator_disabled`, `no_email`,
    /// `account_inactive`, `subject_is_token`.
    NotApplicable { reason: &'static str },
}

impl NotifyOutcome {
    fn to_dto(&self) -> NotifyOutcomeDto {
        match self {
            NotifyOutcome::Sent { kind } => NotifyOutcomeDto::Sent {
                detail: kind.audit_str().to_string(),
            },
            NotifyOutcome::Coalesced { last_sent_at } => NotifyOutcomeDto::Coalesced {
                last_sent_at: *last_sent_at,
            },
            NotifyOutcome::RateLimited { retry_after_secs } => NotifyOutcomeDto::RateLimited {
                retry_after_secs: *retry_after_secs,
            },
            NotifyOutcome::NotApplicable { reason } => NotifyOutcomeDto::NotApplicable {
                reason: (*reason).to_string(),
            },
        }
    }
}

/// Aggregated result for one share-notification action. Carries one
/// outcome per resolved recipient (1 for user subjects, 0 for token
/// subjects, N for group subjects).
#[derive(Debug, Clone)]
pub struct NotifyOutcomeSet {
    pub outcomes: Vec<NotifyOutcome>,
}

impl NotifyOutcomeSet {
    pub fn empty() -> Self {
        Self {
            outcomes: Vec::new(),
        }
    }

    pub fn total_recipients(&self) -> usize {
        self.outcomes.len()
    }

    pub fn to_dto(&self) -> NotifyOutcomeSetDto {
        NotifyOutcomeSetDto::from_outcomes(
            self.outcomes.iter().map(NotifyOutcome::to_dto).collect(),
        )
    }
}

/// Default coalesce window (10 minutes). Bursts of share creations to
/// the same recipient inside this window produce ONE email; subsequent
/// shares are coalesced silently. Recipient still sees every share at
/// next login.
const COALESCE_WINDOW_SECS: u64 = 10 * 60;

/// Maximum keys held by the coalesce cache. Way above any realistic
/// per-tenant burst; bounded to keep memory predictable.
const COALESCE_CACHE_MAX_ENTRIES: u64 = 100_000;

pub struct RecipientNotificationService {
    user_storage: Arc<UserPgRepository>,
    magic_link_service: Arc<MagicLinkInviteService>,
    email_sender: Arc<dyn EmailSender>,
    i18n: Arc<I18nApplicationService>,
    locale_registry: Arc<LocaleRegistry>,
    subject_groups: Arc<SubjectGroupService>,
    /// Per-(granter, recipient_email) timestamp of last successful send.
    /// Sliding window — read+rewrite resets the TTL but that's fine
    /// because we only insert on actual sends.
    coalesce_cache: Cache<(Uuid, String), DateTime<Utc>>,
    /// Shared with the public `/magic/v1/{token}/resend` channel so an
    /// attacker can't alternate between channels to double the cap.
    per_email_limiter: Arc<RateLimiter>,
    magic_link_cfg: MagicLinkConfig,
    public_base_url: String,
}

impl RecipientNotificationService {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        user_storage: Arc<UserPgRepository>,
        magic_link_service: Arc<MagicLinkInviteService>,
        email_sender: Arc<dyn EmailSender>,
        i18n: Arc<I18nApplicationService>,
        locale_registry: Arc<LocaleRegistry>,
        subject_groups: Arc<SubjectGroupService>,
        per_email_limiter: Arc<RateLimiter>,
        magic_link_cfg: MagicLinkConfig,
        public_base_url: String,
    ) -> Self {
        let coalesce_cache = Cache::builder()
            .time_to_live(Duration::from_secs(COALESCE_WINDOW_SECS))
            .max_capacity(COALESCE_CACHE_MAX_ENTRIES)
            .build();
        Self {
            user_storage,
            magic_link_service,
            email_sender,
            i18n,
            locale_registry,
            subject_groups,
            coalesce_cache,
            per_email_limiter,
            magic_link_cfg,
            public_base_url,
        }
    }

    /// Single entry point. Called by `create_grant` after grant rows are
    /// persisted, and by `notify_grant_recipient` after loading the
    /// grant by id. Returns one outcome per resolved recipient.
    ///
    /// Errors here are *infrastructure* errors (DB unreachable while
    /// expanding a group, etc.). Per-recipient failures (SMTP, etc.)
    /// are captured as outcomes, never as `Err`.
    pub async fn send_share_notification(
        &self,
        granter: &User,
        subject: Subject,
        resource: Resource,
        trigger: NotifyTrigger,
    ) -> Result<NotifyOutcomeSet, DomainError> {
        // Resolve subject → Vec<User>. Token subjects yield an empty
        // vec; the calling handler maps that to its own response.
        let members = self.resolve_subject_members(subject).await?;
        if members.is_empty() {
            return Ok(NotifyOutcomeSet::empty());
        }

        // Audit summary line for group expansions — operators tracing
        // a single grant action want to see "this fanned out to N
        // recipients" without combing per-member lines.
        if let Subject::Group(group_id) = subject {
            tracing::info!(
                target: "audit",
                event = "grant.notify_group_expanded",
                granter_id = %granter.id(),
                group_id = %group_id,
                member_count = members.len(),
                resource = ?resource,
                trigger = %trigger.audit_str(),
                "📣 group {} expanded to {} member(s) for notification",
                group_id,
                members.len(),
            );
        }

        // SMTP dispatch dominates each iteration (hundreds of ms per
        // recipient) and the iterations are independent — coalescing and
        // rate-limiting key on (granter, recipient), which is distinct per
        // member. Bounded concurrency keeps a 30-member group grant from
        // holding the HTTP response for 15+ s of serial sends while still
        // capping the pressure on the SMTP relay. `buffered` (not
        // `buffer_unordered`) preserves the member order of the outcomes.
        use futures::stream::{self, StreamExt};
        let outcomes: Vec<NotifyOutcome> = stream::iter(members)
            .map(|member| async move {
                self.dispatch_to_one_user(granter, &member, resource, trigger)
                    .await
            })
            .buffered(NOTIFY_DISPATCH_CONCURRENCY)
            .collect()
            .await;
        Ok(NotifyOutcomeSet { outcomes })
    }

    /// User subjects → single-element vec; Token subjects → empty;
    /// Group subjects → transitively expanded member list.
    async fn resolve_subject_members(&self, subject: Subject) -> Result<Vec<User>, DomainError> {
        match subject {
            Subject::User(id) => {
                match UserRepository::get_user_by_id(&*self.user_storage, id).await {
                    Ok(user) => Ok(vec![user]),
                    Err(e) => Err(DomainError::from(e)),
                }
            }
            Subject::Token(_) => Ok(Vec::new()),
            Subject::Group(group_id) => {
                let member_ids = self.subject_groups.list_transitive_users(group_id).await?;
                if member_ids.is_empty() {
                    return Ok(Vec::new());
                }
                UserRepository::get_users_by_ids(&*self.user_storage, member_ids)
                    .await
                    .map_err(DomainError::from)
            }
        }
    }

    /// THE last function sending email. Per-recipient: eligibility
    /// match → coalesce → rate-limit → dispatch → audit. No SMTP send
    /// happens outside this function.
    async fn dispatch_to_one_user(
        &self,
        granter: &User,
        recipient: &User,
        resource: Resource,
        trigger: NotifyTrigger,
    ) -> NotifyOutcome {
        // 1. Account state — deactivated users get no mail regardless.
        if !recipient.is_active() {
            self.audit_skipped(granter, recipient, resource, trigger, "account_inactive");
            return NotifyOutcome::NotApplicable {
                reason: "account_inactive",
            };
        }

        // 2. Choose the dispatch arm.
        let kind = match magic_link_eligibility(
            recipient,
            self.magic_link_cfg.open_to_password_users,
            self.magic_link_cfg.open_to_oidc_users,
        ) {
            Eligibility::Allow => NotifyKind::MagicLink,
            Eligibility::Reject(_) => {
                // Plain-notification arm. Check the two gates.
                if !self.magic_link_cfg.notify_internal_users_on_share {
                    self.audit_skipped(granter, recipient, resource, trigger, "operator_disabled");
                    return NotifyOutcome::NotApplicable {
                        reason: "operator_disabled",
                    };
                }
                if !recipient.notify_on_share() {
                    self.audit_skipped(
                        granter,
                        recipient,
                        resource,
                        trigger,
                        "recipient_opted_out",
                    );
                    return NotifyOutcome::NotApplicable {
                        reason: "recipient_opted_out",
                    };
                }
                if recipient.email().is_empty() {
                    self.audit_skipped(granter, recipient, resource, trigger, "no_email");
                    return NotifyOutcome::NotApplicable { reason: "no_email" };
                }
                NotifyKind::PlainNotification
            }
        };

        // 3. Coalesce check — only meaningful when we'd actually send.
        // Per-pair: `(granter_id, recipient_email)`.
        let coalesce_key = (granter.id(), recipient.email().to_string());
        if let Some(last) = self.coalesce_cache.get(&coalesce_key) {
            self.audit_skipped(granter, recipient, resource, trigger, "coalesced");
            return NotifyOutcome::Coalesced { last_sent_at: last };
        }

        // 4. Hard rate limit on the recipient email.
        if self
            .per_email_limiter
            .check_and_increment(recipient.email())
            .is_err()
        {
            self.audit_skipped(granter, recipient, resource, trigger, "rate_limited");
            return NotifyOutcome::RateLimited {
                retry_after_secs: self.per_email_limiter.retry_after() as u32,
            };
        }

        // 5. Dispatch + audit.
        let send_result = match kind {
            NotifyKind::MagicLink => {
                // Delegates token mint + locale-resolved bilingual email
                // + per-mail audit to the existing service. Its own
                // eligibility short-circuit is moot here — we've already
                // routed only Accept-eligible recipients to this arm.
                // Pass the granter as a `&User` so the inner service can
                // compute both the short (subject) and full (body)
                // display forms via `display_full(bool)`.
                self.magic_link_service
                    .issue_invitation(recipient, granter, resource)
                    .await
                    .map_err(|e| e.message)
            }
            NotifyKind::PlainNotification => {
                self.send_plain_notification(granter, recipient, resource)
                    .await
            }
        };

        match send_result {
            Ok(()) => {
                // Update coalesce timestamp ONLY on successful send.
                // Skipping a coalesce-window-ago send means the next
                // attempt re-checks against the same old timestamp, but
                // moka's insert resets the TTL anyway — so the window
                // effectively slides forward on each successful send.
                self.coalesce_cache.insert(coalesce_key, Utc::now());
                tracing::info!(
                    target: "audit",
                    event = "grant.notify_sent",
                    kind = %kind.audit_str(),
                    granter_id = %granter.id(),
                    recipient_id = %recipient.id(),
                    recipient_email = %recipient.email(),
                    resource = ?resource,
                    trigger = %trigger.audit_str(),
                    "📨 notify sent ({}) to {}",
                    kind.audit_str(),
                    recipient.email(),
                );
                NotifyOutcome::Sent { kind }
            }
            Err(err) => {
                // The grant landed; SMTP failure is non-fatal. Mirror
                // the long-standing magic-link policy: warn-log, return
                // a Sent-shaped outcome anyway (the operator sees the
                // truth in the audit row; the caller's UI is just less
                // useful for a few seconds).
                tracing::warn!(
                    target: "audit",
                    event = "grant.notify_send_failed",
                    kind = %kind.audit_str(),
                    granter_id = %granter.id(),
                    recipient_id = %recipient.id(),
                    recipient_email = %recipient.email(),
                    error = %err,
                    "📭 notify send failed ({}): {}",
                    kind.audit_str(),
                    err,
                );
                // Don't bump coalesce on failure — we want the next
                // legitimate attempt to retry.
                NotifyOutcome::Sent { kind }
            }
        }
    }

    /// Render and send the plain-notification email ("Hey, you got a
    /// new grant"). No magic link; recipient must sign in normally.
    async fn send_plain_notification(
        &self,
        granter: &User,
        recipient: &User,
        resource: Resource,
    ) -> Result<(), String> {
        let locale = self.locale_for(recipient);
        let kind_key = match resource {
            Resource::Folder(_) => "server.magic_link.email.kind_folder",
            Resource::File(_) => "server.magic_link.email.kind_file",
            // Drive / Calendar / AddressBook / Playlist shares don't
            // produce email notifications through this path. Fall
            // back to the folder label so any code that does reach
            // here still produces a readable (if generic) mail body.
            Resource::Drive(_)
            | Resource::Calendar(_)
            | Resource::AddressBook(_)
            | Resource::Playlist(_) => "server.magic_link.email.kind_folder",
        };
        let kind_label = self.i18n_or(kind_key, &locale, &[]).await;
        // Short form for the subject, long form (with email) for the
        // body — same pattern as `MagicLinkInviteService::issue_invitation`.
        let inviter_short = granter.display_full(false);
        let inviter_full = granter.display_full(true);
        let login_link = format!("{}/login", self.public_base_url.trim_end_matches('/'),);

        let args: Vec<(&str, &str)> = vec![
            ("inviter", inviter_short.as_str()),
            ("inviter_full", inviter_full.as_str()),
            ("kind", &kind_label),
            ("login_link", &login_link),
        ];

        let subject = self
            .i18n_or("server.notification.share.subject", &locale, &args)
            .await;
        let body = self
            .render_bilingual("server.notification.share.body", &locale, &args)
            .await;

        let message = EmailMessage {
            to: recipient.email().to_string(),
            subject,
            text_body: body,
            html_body: None,
        };

        self.email_sender
            .send(message)
            .await
            .map(|_| ())
            .map_err(|e| e.message)
    }

    /// Resolve a recipient's stored locale → `Locale`. Mirrors
    /// `MagicLinkInviteService::locale_for`: bad/unknown codes fall back
    /// to the server default.
    fn locale_for(&self, user: &User) -> Locale {
        user.preferred_locale()
            .and_then(|code| self.locale_registry.parse(code))
            .unwrap_or_else(|| self.locale_registry.default_locale().clone())
    }

    /// Translate with arg substitution, falling back to the literal key
    /// if the i18n lookup errors (defensive — shouldn't happen with the
    /// English-fallback layer in place).
    async fn i18n_or(&self, key: &str, locale: &Locale, args: &[(&str, &str)]) -> String {
        self.i18n
            .translate_args(key, Some(locale.clone()), args)
            .await
            .unwrap_or_else(|_| key.to_string())
    }

    /// Body + English-fallback partial. Same shape as
    /// `MagicLinkInviteService::render_bilingual`. Could be lifted into
    /// a shared helper later — kept duplicated for now because there
    /// are only two call sites.
    async fn render_bilingual(
        &self,
        body_key: &str,
        locale: &Locale,
        args: &[(&str, &str)],
    ) -> String {
        let body = self.i18n_or(body_key, locale, args).await;
        let english_fallback = if locale.is_english() {
            None
        } else {
            Some(self.i18n_or(body_key, &Locale::english(), args).await)
        };
        let divider = self
            .i18n_or(
                "server.magic_link.email.english_fallback_divider",
                locale,
                &[],
            )
            .await;
        let template = BilingualBody {
            body: body.clone(),
            divider,
            english_fallback,
        };
        template.render().unwrap_or(body)
    }

    fn audit_skipped(
        &self,
        granter: &User,
        recipient: &User,
        resource: Resource,
        trigger: NotifyTrigger,
        reason: &'static str,
    ) {
        tracing::info!(
            target: "audit",
            event = "grant.notify_skipped",
            reason = reason,
            granter_id = %granter.id(),
            recipient_id = %recipient.id(),
            recipient_email = %recipient.email(),
            resource = ?resource,
            trigger = %trigger.audit_str(),
            "🤫 notify skipped ({}) for {}",
            reason,
            recipient.email(),
        );
    }
}

/// Reuses the same partial template as `MagicLinkInviteService`. The
/// duplication is intentional: askama derive macros need a struct per
/// callsite, and pulling the rendering struct out of the magic-link
/// module would create a fan-out of dependencies. Two ~10-line copies
/// is cheaper than the abstraction.
#[derive(Template)]
#[template(path = "magic_link/email_body.txt")]
struct BilingualBody {
    body: String,
    divider: String,
    english_fallback: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notify_outcome_to_dto_sent_variants() {
        let dto_ml = NotifyOutcome::Sent {
            kind: NotifyKind::MagicLink,
        }
        .to_dto();
        let dto_pn = NotifyOutcome::Sent {
            kind: NotifyKind::PlainNotification,
        }
        .to_dto();
        match dto_ml {
            NotifyOutcomeDto::Sent { detail } => assert_eq!(detail, "magic_link"),
            _ => panic!("expected Sent"),
        }
        match dto_pn {
            NotifyOutcomeDto::Sent { detail } => assert_eq!(detail, "plain_notification"),
            _ => panic!("expected Sent"),
        }
    }

    #[test]
    fn notify_outcome_to_dto_skip_variants() {
        let now = Utc::now();
        match (NotifyOutcome::Coalesced { last_sent_at: now }).to_dto() {
            NotifyOutcomeDto::Coalesced { last_sent_at } => assert_eq!(last_sent_at, now),
            _ => panic!("expected Coalesced"),
        }
        match (NotifyOutcome::RateLimited {
            retry_after_secs: 3600,
        })
        .to_dto()
        {
            NotifyOutcomeDto::RateLimited { retry_after_secs } => {
                assert_eq!(retry_after_secs, 3600)
            }
            _ => panic!("expected RateLimited"),
        }
        match (NotifyOutcome::NotApplicable {
            reason: "recipient_opted_out",
        })
        .to_dto()
        {
            NotifyOutcomeDto::NotApplicable { reason } => {
                assert_eq!(reason, "recipient_opted_out")
            }
            _ => panic!("expected NotApplicable"),
        }
    }

    #[test]
    fn notify_outcome_set_total_recipients_matches_outcomes_len() {
        let set = NotifyOutcomeSet {
            outcomes: vec![
                NotifyOutcome::Sent {
                    kind: NotifyKind::PlainNotification,
                },
                NotifyOutcome::Coalesced {
                    last_sent_at: Utc::now(),
                },
                NotifyOutcome::NotApplicable {
                    reason: "recipient_opted_out",
                },
            ],
        };
        assert_eq!(set.total_recipients(), 3);
        let dto = set.to_dto();
        assert_eq!(dto.total_recipients, 3);
        assert_eq!(dto.outcomes.len(), 3);
    }

    #[test]
    fn empty_outcome_set() {
        let set = NotifyOutcomeSet::empty();
        assert_eq!(set.total_recipients(), 0);
        let dto = set.to_dto();
        assert_eq!(dto.total_recipients, 0);
        assert!(dto.outcomes.is_empty());
    }

    #[test]
    fn audit_strs_are_stable() {
        // These string values appear in operator-facing audit logs and
        // log aggregators key off them. A rename here is a breaking
        // change to dashboards — guard against accidental drift.
        assert_eq!(NotifyTrigger::GrantCreated.audit_str(), "grant_created");
        assert_eq!(NotifyTrigger::ManualResend.audit_str(), "manual_resend");
        assert_eq!(NotifyKind::MagicLink.audit_str(), "magic_link");
        assert_eq!(
            NotifyKind::PlainNotification.audit_str(),
            "plain_notification"
        );
    }
}
