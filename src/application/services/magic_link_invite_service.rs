//! Invite-by-email orchestration for `POST /api/grants` with
//! `subject.type = "email"`.
//!
//! Two-step API (kept separate so the handler can interleave the standard
//! grant-creation step in between):
//!
//!   1. [`resolve_or_create_recipient`] — normalise the email, apply the
//!      allowlist + kill-switch checks, then look up or lazily provision
//!      an external user. Returns the resolved [`User`] entity.
//!   2. [`issue_invitation`] — mint a magic-link token targeting the
//!      shared resource, build the `/magic/v1/{token}` URL, and send the
//!      invitation email through the wired `EmailSender`.
//!
//! Step 2 is gated by [`magic_link_eligibility`] — OIDC users are
//! unconditionally rejected (audit `oidc_user`); password users are
//! rejected by default (`has_password`) but allowed when
//! `OXICLOUD_MAGIC_LINK_OPEN_TO_PASSWORD_USERS=true`. Rejected
//! invitations still result in the grant being created — the recipient
//! sees the shared resource in their normal "Shared with me" view —
//! only the courtesy notification mail is suppressed.
//!
//! # Enumeration defense
//!
//! v1 awaits the SMTP send synchronously. A malicious caller can in
//! theory measure response times to distinguish "new external user
//! provisioned + mail sent" from "existing internal user, no mail" —
//! a single-bit oracle. The plan defers full constant-time defense
//! (fire-and-forget spawn, dummy SMTP latency on no-op paths) to PR 12.

use std::sync::Arc;

use askama::Template;

use crate::application::ports::email_sender::{EmailMessage, EmailSender};
use crate::application::services::i18n_application_service::I18nApplicationService;
use crate::application::services::user_lifecycle_service::UserLifecycleService;
use crate::common::config::MagicLinkConfig;
use crate::common::errors::{DomainError, ErrorKind};
use crate::common::locale::Locale;
use crate::domain::entities::magic_link_token::{
    MagicLinkResourceKind, MagicLinkStatus, MagicLinkToken,
};
use crate::domain::entities::user::{User, UserRole};
use crate::domain::repositories::magic_link_token_repository::MagicLinkTokenRepository;
use crate::domain::repositories::user_repository::{UserRepository, UserRepositoryError};
use crate::domain::services::authorization::{Resource, ResourceKind};
use crate::domain::services::email_normalize::normalize_email;
use crate::infrastructure::repositories::pg::UserPgRepository;

/// Eligibility decision for a user to receive a magic-link.
///
/// Returned by [`magic_link_eligibility`]. The `Reject` arm carries a
/// **stable** audit-reason key (`"oidc_user"`, `"has_password"`,
/// `"account_deactivated"`) — log aggregators key off this, do not
/// repurpose existing values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Eligibility {
    Allow,
    Reject(&'static str),
}

/// Decide whether to mint a magic-link for the given user.
///
/// Precedence ladder (PR 19):
///
/// 1. **OIDC linked** → always reject with `"oidc_user"`. The IdP is the
///    security boundary and may enforce MFA that magic-link would
///    bypass. The `open_to_password_users` flag has **no effect**.
/// 2. **Has a password configured** → reject with `"has_password"` by
///    default. Allow when `open_to_password_users` is `true` (lenient
///    mode — operator opt-in via env, accepting that mailbox compromise
///    becomes equivalent to password compromise).
/// 3. **No credential at all** (the typical external user or
///    fresh email-only signup) → allow.
///
/// Account-deactivation is **not** checked here — `send_login_link` /
/// `issue_invitation` handle it separately because the rejection reason
/// (`"account_deactivated"`) is unrelated to credential state.
pub fn magic_link_eligibility(user: &User, open_to_password_users: bool) -> Eligibility {
    if user.is_oidc_user() {
        return Eligibility::Reject("oidc_user");
    }
    if user.has_password() {
        return if open_to_password_users {
            Eligibility::Allow
        } else {
            Eligibility::Reject("has_password")
        };
    }
    Eligibility::Allow
}

pub struct MagicLinkInviteService {
    user_storage: Arc<UserPgRepository>,
    magic_link_repo: Arc<dyn MagicLinkTokenRepository>,
    email_sender: Arc<dyn EmailSender>,
    user_lifecycle: Arc<UserLifecycleService>,
    i18n: Arc<I18nApplicationService>,
    /// Used to validate a stored `preferred_locale` at render time —
    /// a code that's no longer in the registry (e.g. operator removed
    /// `pl.json`) falls back to the server default instead of raising
    /// a translation error.
    locale_registry: Arc<crate::common::locale::LocaleRegistry>,
    magic_link_cfg: MagicLinkConfig,
    /// Public base URL of this OxiCloud instance — used to build the
    /// `/magic/v1/{token}` invitation link. Sourced from
    /// `AppConfig::base_url()` at DI time.
    public_base_url: String,
}

impl MagicLinkInviteService {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        user_storage: Arc<UserPgRepository>,
        magic_link_repo: Arc<dyn MagicLinkTokenRepository>,
        email_sender: Arc<dyn EmailSender>,
        user_lifecycle: Arc<UserLifecycleService>,
        i18n: Arc<I18nApplicationService>,
        locale_registry: Arc<crate::common::locale::LocaleRegistry>,
        magic_link_cfg: MagicLinkConfig,
        public_base_url: String,
    ) -> Self {
        Self {
            user_storage,
            magic_link_repo,
            email_sender,
            user_lifecycle,
            i18n,
            locale_registry,
            magic_link_cfg,
            public_base_url,
        }
    }

    /// Resolve the recipient's preferred locale into a usable `Locale`.
    /// Returns the server default when:
    ///   - `preferred_locale` is `None` (the common case for pre-PR-C
    ///     users and recipients who never picked a language),
    ///   - the stored code no longer resolves against the registry
    ///     (e.g. operator removed a locale file after the row was
    ///     written, or a future schema migration relaxed the CHECK).
    fn locale_for(&self, user: &User) -> Locale {
        user.preferred_locale()
            .and_then(|code| self.locale_registry.parse(code))
            .unwrap_or_else(|| self.locale_registry.default_locale().clone())
    }

    /// Resolve the email to an existing user, or lazily provision a new
    /// external user. Returns the resolved [`User`].
    ///
    /// Errors:
    /// - `InvalidInput` — email failed normalisation (malformed / too long).
    /// - `AccessDenied` — email-grant kill switch is off
    ///   (`OXICLOUD_ALLOW_EXTERNAL_USERS=false`) and no matching user
    ///   exists, OR the email's domain isn't in the allowlist.
    /// - any propagated repo error.
    pub async fn resolve_or_create_recipient(
        &self,
        raw_email: &str,
        inviter_id: Option<uuid::Uuid>,
    ) -> Result<User, DomainError> {
        let normalised = normalize_email(raw_email).map_err(|e| {
            DomainError::new(ErrorKind::InvalidInput, "MagicLinkInvite", format!("{}", e))
        })?;

        // Fast path: existing user with this email — works for both
        // internal (was previously created via normal registration) and
        // external (previous invitation re-sharing) cases. We do NOT
        // touch `preferred_locale` on an existing row; the recipient's
        // own choice (or a previously-inherited value) wins.
        match UserRepository::get_user_by_email(&*self.user_storage, &normalised).await {
            Ok(user) => Ok(user),
            Err(UserRepositoryError::NotFound(_)) => {
                // Best-effort inviter locale lookup. A failure here
                // (deleted inviter row, transient DB blip) is non-fatal
                // — the recipient is created with NULL locale and
                // resolves to the server default like any pre-PR-C row.
                let inviter_locale = if let Some(uid) = inviter_id {
                    match UserRepository::get_user_by_id(&*self.user_storage, uid).await {
                        Ok(u) => u.preferred_locale().map(str::to_string),
                        Err(_) => None,
                    }
                } else {
                    None
                };
                self.create_external_user(&normalised, inviter_locale).await
            }
            Err(e) => Err(DomainError::from(e)),
        }
    }

    /// Lazy provisioning path. Runs the two policy guards (kill switch
    /// and per-domain allowlist) before touching the DB.
    ///
    /// `inviter_locale` is the inviter's `preferred_locale` if any —
    /// PR C inherits it into the new external user's row so the
    /// invitation mail (and any subsequent emails to the recipient)
    /// arrive in a language the inviter likely shares with them. The
    /// recipient can override later via the language switcher.
    async fn create_external_user(
        &self,
        normalised_email: &str,
        inviter_locale: Option<String>,
    ) -> Result<User, DomainError> {
        if !self.magic_link_cfg.allow_external_users {
            return Err(DomainError::new(
                ErrorKind::AccessDenied,
                "MagicLinkInvite",
                "Creating external users is disabled on this server \
                 (OXICLOUD_ALLOW_EXTERNAL_USERS=false)"
                    .to_string(),
            ));
        }
        if !self.magic_link_cfg.is_email_allowed(normalised_email) {
            return Err(DomainError::new(
                ErrorKind::AccessDenied,
                "MagicLinkInvite",
                format!(
                    "Email domain is not in the allowlist (OXICLOUD_EXTERNAL_EMAIL_DOMAINS); \
                     refusing to invite {}",
                    normalised_email,
                ),
            ));
        }

        // External users are created without a username or password.
        // `password_hash IS NULL` is the canonical no-password marker.
        //
        // federation_kind stays None in Phase A of the federation-identity
        // rename — magic-link externals get their `federation_kind` stamp
        // in a future PR when the invite handler is refactored to opt into
        // the composable federation model. Behaviour is unchanged today.
        let mut user = User::new(
            normalised_email.to_string(),
            None,
            None,
            None, // federation_kind
            None, // federation_issuer
            None, // federation_subject
            UserRole::User,
            0,
            true,
        )
        .map_err(|e| {
            DomainError::new(
                ErrorKind::InvalidInput,
                "MagicLinkInvite",
                format!("invalid external user data: {}", e),
            )
        })?;
        // PR C: inherit the inviter's preferred locale at row creation
        // (decision 6 in the plan). Treated as advisory — frequently
        // wrong, but the recipient can override via the language
        // switcher, and the bilingual email partial ships English
        // alongside any non-English copy as a safety net.
        if let Some(locale) = inviter_locale {
            user.set_preferred_locale(Some(locale));
        }

        let saved = UserRepository::create_user(&*self.user_storage, user.clone())
            .await
            .map_err(DomainError::from)?;

        // Fire the user-lifecycle dispatcher — `on_user_created` lights
        // up audit + future external-identity provenance bookkeeping.
        // Errors are logged-and-continued by the dispatcher's
        // `dispatch_created` per the lifecycle contract.
        self.user_lifecycle.dispatch_created(&saved).await;

        Ok(saved)
    }

    /// Mint a magic-link token targeting the resource and email the
    /// invitation link. Caller is expected to have already created the
    /// grant rows.
    ///
    /// `inviter` is interpolated into the subject line ("Alice shared
    /// with you on OxiCloud") and body ("Alice <alice@x.com> shared a
    /// folder with you. Open it by..."). Two forms are computed via
    /// [`User::display_full`] — the short form goes into the subject
    /// (keeps inbox-row width sane), the email-decorated form goes
    /// into the body where the extra identifier helps the recipient
    /// place who's reaching out.
    pub async fn issue_invitation(
        &self,
        recipient: &User,
        inviter: &User,
        resource: Resource,
    ) -> Result<(), DomainError> {
        // The grant is in place either way; only mint a magic link when
        // the recipient is magic-link-eligible. OIDC-linked users never
        // get one (IdP is the security boundary); password users get
        // one only when the operator opted into lenient mode via
        // `OXICLOUD_MAGIC_LINK_OPEN_TO_PASSWORD_USERS=true`. Either way
        // they see the grant in their normal "Shared with me" view —
        // the mail is purely a notification convenience.
        if let Eligibility::Reject(reason) =
            magic_link_eligibility(recipient, self.magic_link_cfg.open_to_password_users)
        {
            tracing::info!(
                target: "audit",
                event = "magic_link.invitation_suppressed",
                reason = reason,
                user_id = %recipient.id(),
                username = %recipient.display_for_audit(),
                "📭 invitation mail suppressed: '{}' is not magic-link-eligible ({})",
                recipient.display_for_audit(),
                reason,
            );
            return Ok(());
        }

        let (kind, resource_id) = match resource {
            Resource::Folder(id) => (MagicLinkResourceKind::Folder, id),
            Resource::File(id) => (MagicLinkResourceKind::File, id),
            // Drive / Calendar / AddressBook / Playlist sharing is
            // out-of-band for the magic-link flow. Drive shares land
            // through `/api/drives/{id}/members`; Calendar /
            // AddressBook shares through the Round-3
            // `/api/(calendars|address-books)/{id}/shares` endpoints;
            // Playlist shares through `/api/playlists/{id}/share`.
            // The DTOs accept every `Resource` variant on the wire
            // (see `ResourceTypeDto`) but only file/folder grants
            // trigger an invitation email. Treating the other arms
            // as audit-logged suppressed no-ops keeps the grant in
            // place while matching the ineligible-recipient branch
            // above.
            Resource::Drive(_)
            | Resource::Calendar(_)
            | Resource::AddressBook(_)
            | Resource::Playlist(_) => {
                tracing::info!(
                    target: "audit",
                    event = "magic_link.invitation_suppressed",
                    reason = "resource_kind_unsupported",
                    user_id = %recipient.id(),
                    resource_kind = %resource.type_str(),
                    "📭 magic-link invitation suppressed: {} resources aren't invitable via email",
                    resource.type_str(),
                );
                return Ok(());
            }
        };
        // Invitation tokens are cross-device by design (recipient has
        // no prior browser context with the server) — no challenge
        // cookie. Long TTL (default 24h) because recipients may not
        // check their email for a while.
        let token = MagicLinkToken::new(
            recipient.id(),
            chrono::Duration::hours(self.magic_link_cfg.invite_ttl_hours as i64),
            Some((kind, resource_id)),
            None,
        );
        self.magic_link_repo.create(&token).await?;

        let link = format!(
            "{}/magic/v1/{}",
            self.public_base_url.trim_end_matches('/'),
            token.token(),
        );

        let kind_key = match resource {
            Resource::Folder(_) => "server.magic_link.email.kind_folder",
            Resource::File(_) => "server.magic_link.email.kind_file",
            // Unreachable — the early-return above exits before we get
            // here for Drive / Calendar / AddressBook / Playlist
            // resources. The arms exist only to satisfy exhaustiveness;
            // if you find any firing, the early-return was bypassed.
            Resource::Drive(_)
            | Resource::Calendar(_)
            | Resource::AddressBook(_)
            | Resource::Playlist(_) => "server.magic_link.email.kind_folder",
        };
        // PR C: render in the recipient's preferred locale (set by UI
        // switcher, OIDC JIT claim, or inviter inheritance at row
        // creation). The bilingual partial appends English below when
        // the resolved locale isn't English, so a wrong guess still
        // produces a readable mail.
        let locale = self.locale_for(recipient);
        let kind_label = self.i18n_or(kind_key, &locale, &[]).await;
        let ttl_hours = self.magic_link_cfg.invite_ttl_hours.to_string();
        // Two display forms: `inviter` (short, no email) flows into the
        // subject line; `inviter_full` (with email decoration) flows
        // into the body. Templates pick whichever placeholder they
        // want — see static/locales/en.json `server.magic_link.email.
        // invitation.*` for the canonical references.
        let inviter_short = inviter.display_full(false);
        let inviter_full = inviter.display_full(true);
        let invite_args: Vec<(&str, &str)> = vec![
            ("inviter", inviter_short.as_str()),
            ("inviter_full", inviter_full.as_str()),
            ("kind", &kind_label),
            ("link", &link),
            ("ttl_hours", &ttl_hours),
        ];

        let subject = self
            .i18n_or(
                "server.magic_link.email.invitation.subject",
                &locale,
                &invite_args,
            )
            .await;
        let text_body = self
            .render_bilingual(
                "server.magic_link.email.invitation.body",
                &locale,
                &invite_args,
            )
            .await;

        let message = EmailMessage {
            to: recipient.email().to_string(),
            subject,
            text_body,
            html_body: None,
        };

        // Synchronous send — see module docs for the enumeration-defense
        // trade-off. PR 12 promotes this to fire-and-forget when the
        // hardening pass lands.
        match self.email_sender.send(message).await {
            Ok(outcome) => {
                tracing::info!(
                    target: "audit",
                    event = "magic_link.invitation_sent",
                    recipient_user_id = %recipient.id(),
                    recipient_email = %recipient.email(),
                    resource = ?resource,
                    smtp_code = outcome.code,
                    smtp_message = %outcome.message,
                );
                Ok(())
            }
            Err(e) => {
                // The grant already exists — log the SMTP failure but
                // don't propagate it as a fatal error, so the API client
                // still gets `201 Created` with the GrantDto. Recipient
                // can re-trigger via the future `POST /api/auth/magic-link/send`
                // endpoint once login-via-email lands.
                tracing::warn!(
                    target: "audit",
                    event = "magic_link.invitation_send_failed",
                    recipient_user_id = %recipient.id(),
                    recipient_email = %recipient.email(),
                    error = %e.message,
                );
                Ok(())
            }
        }
    }

    /// Login-via-email flow (PR 10). Caller submits an email at
    /// `/login`; we look it up — **never lazy-create** here, that path
    /// is reserved for `resolve_or_create_recipient` — and if the
    /// matched user has no other login credential, mint a NULL-resource
    /// magic-link token and email a sign-in link. The redemption
    /// endpoint lands a NULL-resource token on `/shared-with-me`
    /// (external users) or `/files` (internal users).
    ///
    /// Always returns `Ok(())` so the caller can emit a uniform
    /// response shape (`"If an account exists, a link will be sent."`)
    /// that doesn't reveal whether the email maps to an account.
    ///
    /// Audit log distinguishes three real outcomes — `sent`,
    /// `no_account`, `oidc_user`, `has_password` — so operators can see the truth
    /// while the API stays anti-enumeration-safe. A fourth outcome
    /// `send_failed` is logged at `warn` level when SMTP errors.
    ///
    /// `request_challenge` is the per-request random value the handler
    /// already set as the `oxicloud_magic_request` cookie on the
    /// originating browser. The service mirrors it into the token row;
    /// the redemption endpoint compares it against the inbound cookie
    /// to bind the magic-link to the device that requested it.
    /// Anti-enumeration: the handler passes the same challenge whether
    /// or not the user exists / is eligible — the token row is just
    /// not created in those branches, so nothing is leaked by the
    /// presence or absence of the cookie.
    pub async fn send_login_link(
        &self,
        raw_email: &str,
        request_challenge: &str,
    ) -> Result<(), DomainError> {
        let normalised = match normalize_email(raw_email) {
            Ok(n) => n,
            Err(e) => {
                // Malformed input is treated the same as "no account"
                // — uniform response, no oracle from validation errors.
                tracing::info!(
                    target: "audit",
                    event = "auth.magic_link_send",
                    reason = "malformed_email",
                    error = %e,
                    "🔗 login-link suppressed: malformed email",
                );
                return Ok(());
            }
        };

        let user = match UserRepository::get_user_by_email(&*self.user_storage, &normalised).await {
            Ok(u) => u,
            Err(UserRepositoryError::NotFound(_)) => {
                tracing::info!(
                    target: "audit",
                    event = "auth.magic_link_send",
                    reason = "no_account",
                    email = %normalised,
                    "🔗 login-link suppressed: no account for '{}'",
                    normalised,
                );
                return Ok(());
            }
            Err(e) => return Err(DomainError::from(e)),
        };

        if let Eligibility::Reject(reason) =
            magic_link_eligibility(&user, self.magic_link_cfg.open_to_password_users)
        {
            // Refuse the magic-link path for users who have a stronger
            // credential configured. OIDC is unconditional — the IdP is
            // the security boundary and we must not bypass any MFA it
            // enforces. Password is gated by `open_to_password_users`:
            // strict mode refuses (default — magic-link would weaken the
            // password to mailbox-strength); lenient mode allows.
            tracing::info!(
                target: "audit",
                event = "auth.magic_link_send",
                reason = reason,
                user_id = %user.id(),
                username = %user.display_for_audit(),
                email = %normalised,
                "🔗 login-link suppressed: '{}' rejected ({})",
                user.display_for_audit(),
                reason,
            );
            return Ok(());
        }

        if !user.is_active() {
            tracing::info!(
                target: "audit",
                event = "auth.magic_link_send",
                reason = "account_deactivated",
                user_id = %user.id(),
                username = %user.display_for_audit(),
                email = %normalised,
                "🔗 login-link suppressed: account deactivated for '{}'",
                user.display_for_audit(),
            );
            return Ok(());
        }

        // Mint a NULL-resource token bound to the requesting browser
        // via `request_challenge` (PR 22). Short TTL (default 10 min)
        // — the user just clicked the button, so a slow click is
        // almost certainly someone else with access to the inbox.
        let token = MagicLinkToken::new(
            user.id(),
            chrono::Duration::minutes(self.magic_link_cfg.login_ttl_minutes as i64),
            None,
            Some(request_challenge.to_string()),
        );
        self.magic_link_repo.create(&token).await?;

        let link = format!(
            "{}/magic/v1/{}",
            self.public_base_url.trim_end_matches('/'),
            token.token(),
        );
        // PR C: render in the user's preferred locale. Same bilingual
        // safety net as the invitation path — see `issue_invitation`.
        let locale = self.locale_for(&user);
        let ttl_minutes = self.magic_link_cfg.login_ttl_minutes.to_string();
        let login_args: Vec<(&str, &str)> = vec![("link", &link), ("ttl_minutes", &ttl_minutes)];

        let subject = self
            .i18n_or(
                "server.magic_link.email.login.subject",
                &locale,
                &login_args,
            )
            .await;
        let text_body = self
            .render_bilingual("server.magic_link.email.login.body", &locale, &login_args)
            .await;

        let message = EmailMessage {
            to: user.email().to_string(),
            subject,
            text_body,
            html_body: None,
        };

        match self.email_sender.send(message).await {
            Ok(outcome) => {
                tracing::info!(
                    target: "audit",
                    event = "auth.magic_link_send",
                    reason = "sent",
                    user_id = %user.id(),
                    username = %user.display_for_audit(),
                    email = %normalised,
                    smtp_code = outcome.code,
                    smtp_message = %outcome.message,
                    "🔗 login-link sent to '{}'",
                    normalised,
                );
            }
            Err(e) => {
                tracing::warn!(
                    target: "audit",
                    event = "auth.magic_link_send_failed",
                    user_id = %user.id(),
                    email = %normalised,
                    error = %e.message,
                    "🔗 login-link SMTP send failed for '{}'",
                    normalised,
                );
            }
        }

        Ok(())
    }

    /// Mint + email a magic-link for **email verification**, called
    /// only after another authentication factor has already proven the
    /// caller's identity (currently: the login handler after a
    /// successful password check).
    ///
    /// Contract: the caller MUST have validated the user's identity via
    /// an independent factor before invoking this. The method does NOT
    /// re-verify credentials — it exists specifically to bypass the
    /// `has_password` eligibility gate, which would otherwise deadlock
    /// the `OXICLOUD_REQUIRE_VERIFIED_EMAIL` flow (login rejected as
    /// unverified → user asks for a verification link → refused
    /// because they have a password).
    ///
    /// Rejected: OIDC-linked users, deactivated users. Everything else
    /// gets a token — including the "has password" case that
    /// `send_login_link` refuses.
    pub async fn send_verification_link_authenticated(
        &self,
        user: &User,
        request_challenge: &str,
    ) -> Result<(), DomainError> {
        // OIDC boundary is unconditional even here — the IdP owns the
        // identity contract and we must not mint a session-primitive
        // for a user it manages.
        if user.is_oidc_user() {
            tracing::info!(
                target: "audit",
                event = "auth.magic_link_send",
                reason = "oidc_user",
                user_id = %user.id(),
                username = %user.display_for_audit(),
                "🔗 verify-link suppressed: OIDC user",
            );
            return Ok(());
        }
        if !user.is_active() {
            tracing::info!(
                target: "audit",
                event = "auth.magic_link_send",
                reason = "account_deactivated",
                user_id = %user.id(),
                username = %user.display_for_audit(),
                "🔗 verify-link suppressed: account deactivated",
            );
            return Ok(());
        }

        let token = MagicLinkToken::new(
            user.id(),
            chrono::Duration::minutes(self.magic_link_cfg.login_ttl_minutes as i64),
            None,
            Some(request_challenge.to_string()),
        );
        self.magic_link_repo.create(&token).await?;

        let link = format!(
            "{}/magic/v1/{}",
            self.public_base_url.trim_end_matches('/'),
            token.token(),
        );
        // Reuses the login email template for now — same call to
        // action (click the link), same TTL, same challenge binding.
        // A dedicated "verify your email" template can land later
        // without wire changes.
        let locale = self.locale_for(user);
        let ttl_minutes = self.magic_link_cfg.login_ttl_minutes.to_string();
        let login_args: Vec<(&str, &str)> = vec![("link", &link), ("ttl_minutes", &ttl_minutes)];

        let subject = self
            .i18n_or(
                "server.magic_link.email.login.subject",
                &locale,
                &login_args,
            )
            .await;
        let text_body = self
            .render_bilingual("server.magic_link.email.login.body", &locale, &login_args)
            .await;

        let message = EmailMessage {
            to: user.email().to_string(),
            subject,
            text_body,
            html_body: None,
        };

        match self.email_sender.send(message).await {
            Ok(outcome) => {
                tracing::info!(
                    target: "audit",
                    event = "auth.magic_link_send",
                    reason = "sent_verification",
                    user_id = %user.id(),
                    username = %user.display_for_audit(),
                    email = %user.email(),
                    smtp_code = outcome.code,
                    smtp_message = %outcome.message,
                    "🔗 verify-link sent to '{}'",
                    user.email(),
                );
            }
            Err(e) => {
                tracing::warn!(
                    target: "audit",
                    event = "auth.magic_link_send_failed",
                    user_id = %user.id(),
                    email = %user.email(),
                    error = %e.message,
                    "🔗 verify-link SMTP send failed for '{}'",
                    user.email(),
                );
            }
        }

        Ok(())
    }

    /// Fire-and-forget security notification: a password change just
    /// completed on this account. Sends a bilingual email with the
    /// timestamp and originating IP so a compromised-account victim
    /// notices out-of-band and can contact their admin. Fired by
    /// `change_password` after a successful update.
    ///
    /// NOT a magic-link — no token minted, no link in the body, no
    /// TTL to reason about. Pure informational. Distinct audit event
    /// (`auth.password_changed_notification_*`) so operators can
    /// spot delivery issues separately from magic-link sends.
    ///
    /// Deactivated accounts skip — no point mailing someone the
    /// operator just locked out.
    ///
    /// NOTE: we intentionally do NOT skip OIDC-linked users. Hybrid
    /// accounts (SSO + local password) are a real posture: users who
    /// sign in daily via SSO but ALSO keep a local password as a
    /// fallback. When they rotate that local password, they DO need
    /// the notification — the fact that they also have an SSO
    /// linkage doesn't change the "someone touched my local
    /// credential" signal. If `change_password` reached success and
    /// we're here, there was a password worth notifying about (the
    /// upstream `is_oidc_user() && !has_password()` refusal ensured
    /// pure-OIDC users never reach this point).
    ///
    /// `client_ip` is the string the request-scope span already
    /// stamped (via `trusted_proxy::client_ip_from_parts`). We do NOT
    /// re-derive it here; caller passes exactly what the audit log
    /// sees, so the recipient can cross-reference with support.
    pub async fn send_password_changed_notification(
        &self,
        user: &User,
        client_ip: &str,
    ) -> Result<(), DomainError> {
        if !user.is_active() {
            tracing::info!(
                target: "audit",
                event = "auth.password_changed_notification_skipped",
                reason = "account_deactivated",
                user_id = %user.id(),
                "🔔 password-change notification skipped: account deactivated",
            );
            return Ok(());
        }

        let locale = self.locale_for(user);
        // ISO-8601 UTC — machine-parseable, unambiguous across time
        // zones. Human-friendly formatting is a translator concern
        // for a future iteration; for a security email the exact
        // timestamp matters more than the pretty rendering.
        let timestamp = chrono::Utc::now()
            .format("%Y-%m-%d %H:%M:%S UTC")
            .to_string();
        let args: Vec<(&str, &str)> = vec![("ip", client_ip), ("timestamp", &timestamp)];

        let subject = self
            .i18n_or("server.security.password_changed.subject", &locale, &args)
            .await;
        let text_body = self
            .render_bilingual("server.security.password_changed.body", &locale, &args)
            .await;

        let message = EmailMessage {
            to: user.email().to_string(),
            subject,
            text_body,
            html_body: None,
        };

        match self.email_sender.send(message).await {
            Ok(outcome) => {
                tracing::info!(
                    target: "audit",
                    event = "auth.password_changed_notification_sent",
                    user_id = %user.id(),
                    email = %user.email(),
                    client_ip = %client_ip,
                    smtp_code = outcome.code,
                    smtp_message = %outcome.message,
                    "🔔 password-change notification sent to '{}'",
                    user.email(),
                );
            }
            Err(e) => {
                // Non-fatal: change_password already succeeded; a
                // delivery failure here just means the victim of a
                // hypothetical compromise won't be notified out-of-
                // band. Log at warn so ops sees consistent SMTP issues.
                tracing::warn!(
                    target: "audit",
                    event = "auth.password_changed_notification_failed",
                    user_id = %user.id(),
                    email = %user.email(),
                    error = %e.message,
                    "🔔 password-change notification SMTP send failed for '{}'",
                    user.email(),
                );
            }
        }

        Ok(())
    }

    /// Resolve a translation, falling back to the literal key on any
    /// lookup error. Identical to the handler-side helper — kept inline
    /// here because the service layer can't pull in a UI util module
    /// without a circular dependency.
    async fn i18n_or(&self, key: &str, locale: &Locale, args: &[(&str, &str)]) -> String {
        self.i18n
            .translate_args(key, Some(locale.clone()), args)
            .await
            .unwrap_or_else(|_| key.to_string())
    }

    /// Render an email body and, when the resolved locale isn't
    /// English, append the English translation below a divider. This
    /// is the "always readable" safety net: when locale inheritance
    /// guesses wrong (PR 9 invitation flow) or `preferred_locale` is
    /// stale, the recipient still has the English text to fall back
    /// on. English-locale recipients get a single block — the partial
    /// emits no divider in that case.
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
            // Resolve the English copy through the same interpolation
            // path so placeholder values are substituted identically.
            // PR-A's English-fallback inside the I18nService means the
            // resolution is reliable even if a translator hasn't
            // populated the English copy yet — defensive default in
            // both layers.
            Some(self.i18n_or(body_key, &Locale::english(), args).await)
        };
        let divider = self
            .i18n_or(
                "server.magic_link.email.english_fallback_divider",
                locale,
                &[],
            )
            .await;
        let template = BilingualEmailBody {
            body: body.clone(),
            divider,
            english_fallback,
        };
        // `.render()` only fails on programmer error (template field
        // out of sync). Fall back to the raw body so we still send
        // *something* if the divider partial breaks.
        template.render().unwrap_or(body)
    }

    /// Look up the resend-recipient hint for a token whose redemption
    /// just failed. Returns `Some` exactly when:
    ///
    /// 1. the token row exists,
    /// 2. its status is `Expired` (TTL elapsed) or `Used` (already
    ///    redeemed once — recipient may be re-clicking on a different
    ///    device), and
    /// 3. the owning user account is still active.
    ///
    /// Returns `None` (no resend offered) for `Pending` tokens, unknown
    /// tokens, and deactivated accounts. The `None` branches deliberately
    /// look identical to the caller — anyone who can present a valid
    /// token already has its access semantics, so the only "oracle"
    /// surface is "did this token exist in some non-pending state",
    /// which is moot.
    pub async fn lookup_resend_recipient(
        &self,
        token: &str,
    ) -> Result<Option<ResendRecipientHint>, DomainError> {
        let Some(mlt) = self.magic_link_repo.find_by_token(token).await? else {
            return Ok(None);
        };
        // Pending tokens are still redeemable — no reason to offer a
        // resend. The user should just click the original link again.
        if !matches!(
            mlt.status(),
            MagicLinkStatus::Expired | MagicLinkStatus::Used
        ) {
            return Ok(None);
        }
        let user = match UserRepository::get_user_by_id(&*self.user_storage, mlt.user_id()).await {
            Ok(u) => u,
            Err(UserRepositoryError::NotFound(_)) => return Ok(None),
            Err(e) => return Err(DomainError::from(e)),
        };
        if !user.is_active() {
            return Ok(None);
        }
        let email = user.email().to_string();
        let masked_email = mask_email(&email);
        Ok(Some(ResendRecipientHint {
            email,
            masked_email,
        }))
    }
}

/// Plain-text email body wrapper: emits the localized text, then —
/// only when the resolved locale isn't English — a divider plus the
/// English copy. Lives next to the service rather than under
/// `templates/` because the partial is just a few lines and being
/// co-located keeps the relationship between rendering code and
/// template obvious.
#[derive(Template)]
#[template(path = "magic_link/email_body.txt")]
struct BilingualEmailBody {
    body: String,
    divider: String,
    english_fallback: Option<String>,
}

/// Hint surfaced by the 410-Gone page to offer a one-click "send me a
/// fresh link" affordance to a recipient whose magic-link is no longer
/// usable. Carries the recipient's email twice: the raw form (used by
/// the resend handler to dispatch the new mail) and a masked form
/// (rendered into the HTML page so the user can confirm the destination
/// without the full address being plastered in the URL or address bar).
#[derive(Debug, Clone)]
pub struct ResendRecipientHint {
    pub email: String,
    pub masked_email: String,
}

/// Mask an email for display: keep the first character of the local
/// part, then `…`, then the full domain. `alice@example.com` →
/// `a…@example.com`. Short local parts (1 char) collapse to just
/// `…@domain`. Malformed input (no `@`) is masked entirely as `…`.
pub fn mask_email(email: &str) -> String {
    match email.rsplit_once('@') {
        Some((local, domain)) if !local.is_empty() => {
            let mut chars = local.chars();
            let first = chars.next().unwrap_or('?');
            format!("{first}…@{domain}")
        }
        Some((_, domain)) => format!("…@{domain}"),
        None => "…".to_string(),
    }
}

/// Lightweight conversion so the grant handler can derive a
/// [`MagicLinkResourceKind`] from the already-parsed [`ResourceKind`]
/// without re-importing match arms.
impl From<ResourceKind> for MagicLinkResourceKind {
    fn from(kind: ResourceKind) -> Self {
        match kind {
            ResourceKind::Folder => Self::Folder,
            ResourceKind::File => Self::File,
            // Drives aren't a magic-link invite target in D0. The
            // grant DTO surface accepts drive resources, but the
            // grant_handler doesn't issue magic-links for them
            // (drive sharing lands in D2). Mapping Drive → Folder
            // gives a non-panicking fallback that would still emit a
            // valid token shape if the path were ever reached; the
            // runtime branches above suppress drive invitations
            // before reaching this conversion.
            ResourceKind::Drive => Self::Folder,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::entities::user::{User, UserRole};

    fn user(password: Option<&str>, oidc: Option<(&str, &str)>) -> User {
        use crate::domain::entities::user::FederationKind;
        let (kind, issuer, subject) = match oidc {
            Some((p, s)) => (
                Some(FederationKind::Oidc),
                Some(p.to_string()),
                Some(s.to_string()),
            ),
            None => (None, None, None),
        };
        User::new(
            "test@example.com".to_string(),
            None,
            password.map(str::to_string),
            kind,
            issuer,
            subject,
            UserRole::User,
            0,
            true,
        )
        .expect("test user")
    }

    #[test]
    fn oidc_always_rejected_regardless_of_flag() {
        let u = user(None, Some(("google", "sub-123")));
        assert_eq!(
            magic_link_eligibility(&u, false),
            Eligibility::Reject("oidc_user")
        );
        assert_eq!(
            magic_link_eligibility(&u, true),
            Eligibility::Reject("oidc_user")
        );
    }

    #[test]
    fn mask_email_keeps_one_char_of_local() {
        assert_eq!(mask_email("alice@example.com"), "a…@example.com");
        assert_eq!(mask_email("very-long-name@example.com"), "v…@example.com");
    }

    #[test]
    fn mask_email_handles_edge_cases() {
        // Single-char local part still leaks just the first char, by
        // design — same masking rule applies uniformly so the output
        // shape itself doesn't disclose local-part length.
        assert_eq!(mask_email("a@b.co"), "a…@b.co");
        // Malformed (no `@`) is masked entirely.
        assert_eq!(mask_email("not-an-email"), "…");
        // Pathological (starts with `@`) collapses the empty local.
        assert_eq!(mask_email("@example.com"), "…@example.com");
    }

    #[test]
    fn password_user_strict_then_lenient() {
        let u = user(Some("$argon2id$..."), None);
        assert_eq!(
            magic_link_eligibility(&u, false),
            Eligibility::Reject("has_password")
        );
        assert_eq!(magic_link_eligibility(&u, true), Eligibility::Allow);
    }

    #[test]
    fn no_credential_always_allowed() {
        let u = user(None, None);
        assert_eq!(magic_link_eligibility(&u, false), Eligibility::Allow);
        assert_eq!(magic_link_eligibility(&u, true), Eligibility::Allow);
    }

    #[test]
    fn oidc_dominates_password_when_both_set() {
        // Edge case: user has password AND OIDC linked. The ladder
        // checks OIDC first, so the rejection reason is "oidc_user"
        // (not "has_password"). The flag doesn't matter here either.
        let u = user(Some("hash"), Some(("google", "sub-123")));
        assert_eq!(
            magic_link_eligibility(&u, false),
            Eligibility::Reject("oidc_user")
        );
        assert_eq!(
            magic_link_eligibility(&u, true),
            Eligibility::Reject("oidc_user")
        );
    }
}
