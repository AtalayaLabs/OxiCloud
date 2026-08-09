use crate::application::dtos::user_dto::{
    AdminUserSummaryDto, AuthResponseDto, ChangePasswordDto, LoginDto, RefreshTokenDto,
    RegisterDto, UpgradeToInternalDto, UserDto,
};
use crate::application::ports::auth_ports::{
    OidcIdClaims, OidcServicePort, PasswordHasherPort, SessionStoragePort, TokenServicePort,
    UserStoragePort,
};
use crate::application::ports::authorization_ports::AuthorizationEngine;
use crate::application::ports::user_lifecycle::{DeletionMode, LogoutReason};
use crate::application::services::user_lifecycle_service::UserLifecycleService;
use crate::common::config::{AuthMethod, AuthPolicy, OidcConfig};
use crate::common::errors::{DomainError, ErrorKind};
use crate::domain::entities::magic_link_token::{MagicLinkResourceKind, MagicLinkStatus};
use crate::domain::entities::session::Session;
use crate::domain::entities::user::{User, UserFlags, UserRole};
use crate::domain::repositories::magic_link_token_repository::MagicLinkTokenRepository;
use crate::domain::services::authorization::Subject;
use crate::infrastructure::repositories::pg::SessionPgRepository;
use crate::infrastructure::repositories::pg::UserPgRepository;
use crate::infrastructure::services::jwt_service::JwtTokenService;
use crate::infrastructure::services::oidc_service::OidcService;
use crate::infrastructure::services::password_hasher::Argon2PasswordHasher;
use moka::sync::Cache;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::RwLock;
use std::time::Duration;
use uuid::Uuid;

/// Validate a client-supplied DPoP JWK thumbprint. RFC 7638 §3 produces
/// a base64url-encoded SHA-256 (32 bytes → 43 base64url chars, no
/// padding). We accept exactly that shape; anything else is a client
/// bug or forgery attempt and gets rejected at the login boundary.
///
/// Returned string is the exact input on success — we don't
/// canonicalise the thumbprint further (it IS the canonical form).
fn validate_dpop_jkt(raw: &str) -> Result<String, &'static str> {
    if raw.len() != 43 {
        return Err("DPoP thumbprint must be 43 characters (base64url SHA-256)");
    }
    if !raw
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err("DPoP thumbprint contains non-base64url characters");
    }
    Ok(raw.to_string())
}

#[cfg(test)]
mod dpop_jkt_tests {
    use super::validate_dpop_jkt;

    #[test]
    fn accepts_well_formed_thumbprint() {
        // 43 base64url chars — a real SHA-256 output shape
        let jkt = "AbCdEfGhIjKlMnOpQrStUvWxYz0123456789-_ABCDE";
        assert_eq!(validate_dpop_jkt(jkt).unwrap(), jkt);
    }

    #[test]
    fn rejects_wrong_length() {
        assert!(validate_dpop_jkt("").is_err());
        assert!(validate_dpop_jkt("too-short").is_err());
        assert!(
            validate_dpop_jkt(&"a".repeat(44)).is_err(),
            "44 chars must be rejected"
        );
    }

    #[test]
    fn rejects_padding() {
        // 43-char string ending in `=` is still 43 chars but invalid
        // base64url (padding never appears in URL_SAFE_NO_PAD).
        let with_pad = format!("{}{}", "a".repeat(42), "=");
        assert!(validate_dpop_jkt(&with_pad).is_err());
    }

    #[test]
    fn rejects_standard_base64_alphabet() {
        // `+` and `/` are standard base64 — url-safe uses `-` and `_`
        let with_plus = format!("{}+", "a".repeat(42));
        let with_slash = format!("{}/", "a".repeat(42));
        assert!(validate_dpop_jkt(&with_plus).is_err());
        assert!(validate_dpop_jkt(&with_slash).is_err());
    }
}

/// Result of a successful OIDC callback. The handler layer inspects this to
/// decide whether to redirect to the regular frontend or complete a Nextcloud
/// Login Flow v2 session.
pub enum OidcCallbackResult {
    /// Regular web login — contains a one-time exchange code for the frontend.
    WebLogin { exchange_code: String },
    /// Nextcloud Login Flow v2 — the user authenticated via OIDC but the flow
    /// was initiated from the Nextcloud login page. The handler must create an
    /// app password and complete the NC login flow.
    NextcloudLogin {
        nc_flow_token: String,
        user_id: Uuid,
        username: String,
    },
    /// Self-service link flow completed — the OIDC identity was
    /// attached to the already-authenticated user. Handler redirects
    /// the browser to `/profile?linked=1` (or `?link_error=<reason>`
    /// on the `LinkRefused` variant below).
    ///
    /// The user's existing session cookies remain valid (no new session
    /// is minted for the link flow — the user was already logged in
    /// when they started).
    LinkCompleted { user_id: Uuid },
    /// Self-service link refused by a safety check. `reason` is the
    /// stable enum-shaped key the handler surfaces on the
    /// `/profile?link_error=<reason>` redirect. See
    /// docs/plan/oidc-account-linking.md § Safety checks.
    LinkRefused { reason: &'static str },
    /// Auto-link decision refused during the OIDC LOGIN callback path
    /// (existing local user matched by email but the decision tree
    /// rejected). `reason` is one of `auto_link_disabled`,
    /// `auto_link_email_not_verified`, `already_linked_elsewhere`;
    /// the handler maps each to a distinct CamelCase `error_type`
    /// on the 409 response so the login page can switch on it.
    /// See docs/plan/oidc-account-linking.md § Auto-link.
    AutoLinkRefused { reason: &'static str },
}

/// Outcome of a successful magic-link redemption. The auth tokens are
/// the same shape as a password login; the optional resource fields tell
/// the handler whether to deep-link to the invited resource or fall back
/// to the generic `/shared-with-me` landing.
/// Outcome of a `register` call. The handler maps this to either an
/// anti-enumerated uniform 200 (when SMTP is available — there's a
/// "check your email" cover story for the user) or the classic
/// 201/409 split (when SMTP is unavailable — without the cover story,
/// uniform responses would just be misleading UX with no security
/// benefit). Either way the service emits the same audit-log entries.
#[derive(Debug, Clone)]
pub enum RegisterResult {
    /// Boxed to avoid the `large_enum_variant` clippy warning —
    /// `UserDto` is ~250 bytes, the other variants are zero-sized,
    /// so a heap-pointer indirection keeps the enum's stack size
    /// small. `register` is called once per request; the
    /// allocation cost is negligible.
    Created(Box<UserDto>),
    UsernameTaken,
    EmailTaken,
}

/// Outcome of a `redeem_magic_link` call (PR 22).
///
/// - `Allowed(redemption)` — the token is valid and the browser
///   binding either matched or was overridden via the user's
///   explicit cross-browser confirmation. The token has been
///   atomically marked used.
/// - `NeedsCrossBrowserConfirm` — the token carries a
///   `request_challenge` but the incoming cookie didn't match.
///   The handler should render a confirmation page; the user
///   clicks Continue and we re-redeem with `cross_browser_confirmed = true`.
///   The token is NOT marked used yet — it stays redeemable.
#[derive(Debug)]
pub enum MagicLinkRedeemResult {
    /// Boxed to keep the enum's stack size small — `MagicLinkRedemption`
    /// is ~350 bytes while `NeedsCrossBrowserConfirm` is zero-sized.
    /// One redemption per request; the heap indirection is negligible.
    Allowed(Box<MagicLinkRedemption>),
    NeedsCrossBrowserConfirm,
}

#[derive(Debug, Clone)]
pub struct MagicLinkRedemption {
    pub auth: AuthResponseDto,
    pub resource_kind: Option<MagicLinkResourceKind>,
    pub resource_id: Option<Uuid>,
}

/// Why an OIDC flow was initiated — dispatched on at callback time.
///
/// `Login` (default) → normal login: JIT-provision or match existing
/// user, mint OxiCloud session.
///
/// `Link { user_id }` → self-service identity link
/// (`POST /api/auth/oidc/link/start`). The callback runs safety checks
/// and, on success, UPDATEs `federation_*` on the ALREADY-LOGGED-IN
/// user's row. See docs/plan/oidc-account-linking.md.
#[derive(Clone)]
enum FlowIntent {
    Login,
    Link { user_id: Uuid },
}

/// Tracks a pending OIDC authorization flow (CSRF + PKCE + nonce)
#[derive(Clone)]
struct PendingOidcFlow {
    pkce_verifier: String,
    nonce: String,
    /// When set, this OIDC flow was initiated from the Nextcloud Login Flow v2
    /// page. On successful callback the flow will mint an app-password and
    /// complete the Nextcloud login flow instead of issuing internal JWTs.
    nc_flow_token: Option<String>,
    /// What the callback should DO with a successful IdP response.
    /// Defaults to `Login` for every existing flow-mint call site;
    /// self-service linking sets `Link { user_id }`.
    intent: FlowIntent,
}

/// Tracks a pending one-time token exchange after successful OIDC callback
#[derive(Clone)]
struct PendingOidcToken {
    auth_response: AuthResponseDto,
}

/// Interior state for OIDC — protected by RwLock for hot-reload.
struct OidcState {
    service: Option<Arc<OidcService>>,
    config: Option<OidcConfig>,
}

/// Default quota: 100 GB
const DEFAULT_ADMIN_QUOTA: i64 = 107_374_182_400;
const DEFAULT_USER_QUOTA: i64 = 1_073_741_824; // 1 GB

pub struct AuthApplicationService {
    user_storage: Arc<UserPgRepository>,
    session_storage: Arc<SessionPgRepository>,
    password_hasher: Arc<Argon2PasswordHasher>,
    token_service: Arc<JwtTokenService>,
    /// Dispatcher for user-lifecycle events. `None` only in tests that don't
    /// exercise the lifecycle path; production DI always wires this.
    /// PersonalDriveLifecycleHook (registered on this dispatcher) owns the
    /// per-user folder provisioning that AuthApplicationService used to do
    /// inline pre-PR 3.
    user_lifecycle: Option<Arc<UserLifecycleService>>,
    /// Path to the storage directory, used for disk-space–aware quota calculation
    storage_path: PathBuf,
    oidc: RwLock<OidcState>,
    /// Pending OIDC authorization flows keyed by state token (CSRF + PKCE + nonce).
    /// Auto-expires after 10 minutes via moka TTL; max 10 000 entries for DoS protection.
    pending_oidc_flows: Cache<String, PendingOidcFlow>,
    /// Pending one-time token codes for secure token delivery after OIDC callback.
    /// Auto-expires after 60 seconds via moka TTL; max 10 000 entries for DoS protection.
    pending_oidc_tokens: Cache<String, PendingOidcToken>,
    completed_oidc_logins: Cache<String, String>,
    /// Back-Channel Logout replay guard — dedupes logout_tokens by their
    /// `jti` claim within the token's freshness window (5 min per BCL §2.6).
    /// A cooperative IdP will not re-send a logout_token, but the endpoint
    /// is public and unauthenticated so a rogue caller could try to; we
    /// short-circuit repeats to avoid burning DB writes on duplicates.
    /// Note: tokens without a jti bypass this guard — the validator has
    /// already enforced signature + freshness + subject-presence, so at
    /// worst a legitimate re-notification runs the (idempotent) revoke path
    /// a second time and returns "no rows changed".
    backchannel_logout_jti_seen: Cache<String, ()>,
    /// Magic-link token repository — populated when the magic-link feature
    /// is enabled (PR 8+). `None` means redemption endpoints return 503.
    magic_link_repo: Option<Arc<dyn MagicLinkTokenRepository>>,
    /// Per-user authorization flags (`role` / `is_external` / `active`),
    /// consulted by middleware guards on every WebDAV / CalDAV / CardDAV
    /// request. The short TTL keeps the "role changes apply without token
    /// rotation" property within seconds while removing one DB round-trip
    /// per request; the known mutation paths (`change_user_role`,
    /// `set_user_active`) also invalidate eagerly. `moka::future` so
    /// concurrent misses for one user coalesce into a single DB lookup
    /// (`try_get_with` single-flight) — every authenticated request
    /// calls this, so each 30 s TTL expiry used to fan out one SELECT
    /// per in-flight request of that user.
    user_flags_cache: moka::future::Cache<Uuid, UserFlags>,
    /// Self-service auth-method allowlist (mirrors
    /// `AuthConfig::allowed_auth_methods`). Empty = both methods
    /// allowed. Consulted by login / register / magic-link handlers via
    /// `is_password_login_allowed()` / `is_magic_link_login_allowed()`
    /// so callers don't have to reach for the app config.
    allowed_auth_methods: Vec<AuthMethod>,
    /// Additive auth-policy switches (mirrors `AuthConfig::auth_policies`).
    /// Consulted by handlers / providers-info endpoint to compose the
    /// login-page UX hints (e.g. `AutoRedirectIfStandaloneOidc`) without
    /// reaching into the app config on every call.
    auth_policies: Vec<AuthPolicy>,
    /// Whether `POST /api/auth/login` refuses accounts whose
    /// `email_verified_at IS NULL`. Mirrors
    /// `AuthConfig::require_verified_email`.
    require_verified_email: bool,
    /// OPAQUE envelope repo — populated when the OPAQUE substrate is
    /// wired (`OXICLOUD_AUTH_OPAQUE_MODE != off`). `login()` consults it to
    /// enforce the Phase 4 gate: once a user has completed at least
    /// one successful OPAQUE handshake (`opaque_migrated_at IS NOT
    /// NULL`), legacy `POST /api/auth/login` is refused for that
    /// account. `None` = substrate off, no gate applies.
    opaque_repo: Option<Arc<dyn crate::application::ports::opaque_ports::OpaqueRepositoryPort>>,
}

/// TTL for [`AuthApplicationService::user_flags_cache`]. Upper bound on how
/// long a role / external / active change can take to be observed by the
/// per-request guards when it bypasses the eager invalidation paths.
const USER_FLAGS_CACHE_TTL: Duration = Duration::from_secs(30);

impl AuthApplicationService {
    pub fn new(
        user_storage: Arc<UserPgRepository>,
        session_storage: Arc<SessionPgRepository>,
        password_hasher: Arc<Argon2PasswordHasher>,
        token_service: Arc<JwtTokenService>,
        storage_path: PathBuf,
    ) -> Self {
        Self {
            user_storage,
            session_storage,
            password_hasher,
            token_service,
            user_lifecycle: None,
            storage_path,
            oidc: RwLock::new(OidcState {
                service: None,
                config: None,
            }),
            pending_oidc_flows: Cache::builder()
                .max_capacity(10_000)
                .time_to_live(Duration::from_secs(600))
                .build(),
            pending_oidc_tokens: Cache::builder()
                .max_capacity(10_000)
                .time_to_live(Duration::from_secs(60))
                .build(),
            completed_oidc_logins: Cache::builder()
                .max_capacity(10_000)
                .time_to_live(Duration::from_secs(120))
                .build(),
            backchannel_logout_jti_seen: Cache::builder()
                .max_capacity(10_000)
                // Matches OidcService::validate_logout_token freshness clamp
                // (5 min). Any token older than that fails validation before
                // reaching the jti check, so no need to remember jtis longer.
                .time_to_live(Duration::from_secs(300))
                .build(),
            magic_link_repo: None,
            user_flags_cache: moka::future::Cache::builder()
                .max_capacity(10_000)
                .time_to_live(USER_FLAGS_CACHE_TTL)
                .build(),
            allowed_auth_methods: vec![AuthMethod::Password, AuthMethod::MagicLink],
            auth_policies: Vec::new(),
            require_verified_email: false,
            opaque_repo: None,
        }
    }

    /// Populates the auth-method allowlist + policy vector +
    /// `require_verified_email` snapshot from the loaded config.
    /// Called by the DI factory. If left uncalled (test builds),
    /// defaults are permissive: both self-service methods enabled,
    /// no policies, verified-email not required.
    pub fn with_auth_policy(
        mut self,
        allowed_methods: Vec<AuthMethod>,
        auth_policies: Vec<AuthPolicy>,
        require_verified_email: bool,
    ) -> Self {
        self.allowed_auth_methods = allowed_methods;
        self.auth_policies = auth_policies;
        self.require_verified_email = require_verified_email;
        self
    }

    /// True iff `POST /api/auth/login` is a supported endpoint on this
    /// deployment. Composes the OIDC `disable_password_login` legacy
    /// flag with the newer `OXICLOUD_AUTH_METHODS` allowlist.
    pub fn is_password_login_allowed(&self) -> bool {
        !self.password_login_disabled()
            && (self.allowed_auth_methods.is_empty()
                || self.allowed_auth_methods.contains(&AuthMethod::Password))
    }

    /// True iff `POST /api/auth/magic-link/send` should mint tokens for
    /// end-user login on this deployment.
    ///
    /// Requires ALL of:
    ///   * repo wired (SMTP configured, tokens can actually be minted);
    ///   * allowlist permits `MagicLink` (or is empty = permissive);
    ///   * OIDC is NOT enabled at the deployment level.
    ///
    /// The OIDC guard is a hard rule: when OIDC is enabled it is the
    /// master identity provider — magic-link would bypass any 2FA / step-up
    /// policy that the IdP enforces. An operator running OIDC + local
    /// accounts hybrid must NOT expose magic-link login for the local
    /// accounts either, because a user provisioned via OIDC-JIT could
    /// receive a magic-link on the same mailbox and sidestep MFA. Admin-
    /// mediated invites use OIDC or password bootstrap instead.
    pub fn is_magic_link_login_allowed(&self) -> bool {
        self.magic_link_enabled()
            && !self.oidc_enabled()
            && (self.allowed_auth_methods.is_empty()
                || self.allowed_auth_methods.contains(&AuthMethod::MagicLink))
    }

    /// True iff login should reject accounts with `email_verified_at IS
    /// NULL`. Backed by `OXICLOUD_REQUIRE_VERIFIED_EMAIL`.
    pub fn require_verified_email(&self) -> bool {
        self.require_verified_email
    }

    /// True iff the login SPA should auto-redirect to the OIDC
    /// authorize endpoint on page load (SSO-only, no click needed).
    ///
    /// Composed to be BOTH policy-set AND effectively-standalone:
    ///   * `AutoRedirectIfStandaloneOidc` policy is in the vector, AND
    ///   * OIDC is enabled AND is the only WORKING login method
    ///     (password + magic-link both refused by the composition of
    ///     the allowlist + OIDC-master rule).
    ///
    /// When the policy is set but other methods are also live, this is
    /// a silent no-op — the FE renders the multi-method chooser. If
    /// the policy is NOT set, this is always false regardless.
    pub fn auto_redirect_to_oidc(&self) -> bool {
        self.auth_policies
            .contains(&AuthPolicy::AutoRedirectIfStandaloneOidc)
            && self.oidc_enabled()
            && !self.is_password_login_allowed()
            && !self.is_magic_link_login_allowed()
    }

    /// Resolve a login-identifier (username OR email) to the account's
    /// registered email address. Mirrors the `POST /api/auth/login`
    /// dispatcher (`@` presence → email lookup, else → username
    /// lookup). Returns `None` when the identifier doesn't match any
    /// account — callers that need anti-enumeration semantics MUST
    /// still return their uniform response after logging the reason.
    ///
    /// The username namespace forbids `@` (PR 16), so the two paths
    /// are disjoint — no ambiguity.
    pub async fn resolve_login_identifier_to_email(&self, identifier: &str) -> Option<String> {
        if identifier.contains('@') {
            Some(identifier.to_string())
        } else {
            self.user_storage
                .get_user_by_username(identifier)
                .await
                .ok()
                .map(|u| u.email().to_string())
        }
    }

    /// Direct lookup helpers used by handlers that need the full `User`
    /// entity (not just the email). Mirrors the internal `user_storage`
    /// calls the service already makes in `login`. Currently used by
    /// the login handler to auto-mint a verification magic-link after
    /// a successful password check.
    pub async fn find_user_by_email(&self, email: &str) -> Result<User, DomainError> {
        self.user_storage.get_user_by_email(email).await
    }
    pub async fn find_user_by_username(&self, username: &str) -> Result<User, DomainError> {
        self.user_storage.get_user_by_username(username).await
    }

    /// Wire the magic-link token repository. Called from the DI factory
    /// when the magic-link feature is configured. Mirrors the
    /// `with_oidc` / `with_user_lifecycle` builder pattern.
    pub fn with_magic_link_repo(mut self, repo: Arc<dyn MagicLinkTokenRepository>) -> Self {
        self.magic_link_repo = Some(repo);
        self
    }

    /// Whether magic-link redemption is wired. Handlers should check this
    /// before attempting to redeem a token; `false` → return 503.
    pub fn magic_link_enabled(&self) -> bool {
        self.magic_link_repo.is_some()
    }

    /// Wire the OPAQUE envelope repo. Called by the DI factory when the
    /// OPAQUE substrate is configured (`OXICLOUD_AUTH_OPAQUE_MODE != off`).
    /// Enables the Phase 4 legacy-login gate — see the field docstring.
    pub fn with_opaque_repo(
        mut self,
        repo: Arc<dyn crate::application::ports::opaque_ports::OpaqueRepositoryPort>,
    ) -> Self {
        self.opaque_repo = Some(repo);
        self
    }

    /// Returns the default quota for the given role, capped to the available
    /// disk space on the filesystem that hosts the storage directory.
    fn capped_quota(&self, role: &UserRole) -> i64 {
        let base_quota = match role {
            UserRole::Admin => DEFAULT_ADMIN_QUOTA,
            _ => DEFAULT_USER_QUOTA,
        };

        match Self::available_disk_space(&self.storage_path) {
            Some(avail) => {
                let avail_i64 = avail as i64;
                if avail_i64 < base_quota {
                    tracing::info!(
                        "Available disk space ({} bytes) is less than default {} quota ({} bytes) — capping quota",
                        avail_i64,
                        if *role == UserRole::Admin {
                            "admin"
                        } else {
                            "user"
                        },
                        base_quota,
                    );
                    avail_i64
                } else {
                    base_quota
                }
            }
            None => {
                tracing::warn!("Could not determine available disk space, using default quota");
                base_quota
            }
        }
    }

    /// Query the available space on the filesystem that contains `path`.
    fn available_disk_space(path: &std::path::Path) -> Option<u64> {
        use fs2::available_space;
        match available_space(path) {
            Ok(space) => Some(space),
            Err(e) => {
                tracing::warn!("Failed to query disk space for {:?}: {}", path, e);
                None
            }
        }
    }

    /// Configures the user-lifecycle dispatcher. Wired by the DI factory
    /// after core services are up. PR 1: only AuditLifecycleHook is
    /// registered, so calls without this configured silently no-op.
    pub fn with_user_lifecycle(mut self, lifecycle: Arc<UserLifecycleService>) -> Self {
        self.user_lifecycle = Some(lifecycle);
        self
    }

    /// Configures the OIDC service
    pub fn with_oidc(self, oidc_service: Arc<OidcService>, oidc_config: OidcConfig) -> Self {
        {
            let mut state = self.oidc.write().unwrap();
            state.service = Some(oidc_service);
            state.config = Some(oidc_config);
        }
        self
    }

    /// Hot-reload OIDC configuration at runtime (called from admin settings service)
    pub fn reload_oidc(&self, oidc_service: Arc<OidcService>, oidc_config: OidcConfig) {
        let mut state = self.oidc.write().unwrap();
        state.service = Some(oidc_service);
        state.config = Some(oidc_config);
    }

    /// Disable OIDC at runtime (called from admin settings service)
    pub fn disable_oidc(&self) {
        let mut state = self.oidc.write().unwrap();
        state.service = None;
        state.config = None;
    }

    /// Returns whether OIDC is configured and enabled
    pub fn oidc_enabled(&self) -> bool {
        let state = self.oidc.read().unwrap();
        state.service.is_some() && state.config.as_ref().is_some_and(|c| c.enabled)
    }

    /// Returns whether password login is disabled (OIDC-only mode)
    pub fn password_login_disabled(&self) -> bool {
        let state = self.oidc.read().unwrap();
        state
            .config
            .as_ref()
            .is_some_and(|c| c.disable_password_login)
    }

    /// Returns a clone of the OIDC config if available
    pub fn oidc_config(&self) -> Option<OidcConfig> {
        let state = self.oidc.read().unwrap();
        state.config.clone()
    }

    /// Returns an Arc clone of the OIDC service if available
    pub fn oidc_service(&self) -> Option<Arc<OidcService>> {
        let state = self.oidc.read().unwrap();
        state.service.clone()
    }

    /// Public registration. Returns one of three outcomes:
    /// - `Created(user)` — a user was actually created
    /// - `UsernameTaken` / `EmailTaken` — collision; no DB write
    ///
    /// The handler decides the HTTP shape based on whether SMTP is
    /// available (anti-enumeration uniform 200 vs classic 201/409).
    /// The service emits the same audit-log entries either way — the
    /// audit channel is the source of truth for the actual outcome.
    ///
    /// Real failures (DB error, password too short, etc.) surface as
    /// `Err`.
    pub async fn register(&self, dto: RegisterDto) -> Result<RegisterResult, DomainError> {
        // Username uniqueness (only when a username was supplied — None
        // is the "claim later" path, multiple NULLs are allowed by the
        // UNIQUE index per Postgres semantics).
        if let Some(ref username) = dto.username
            && self
                .user_storage
                .get_user_by_username(username)
                .await
                .is_ok()
        {
            tracing::info!(
                target: "audit",
                event = "auth.register",
                reason = "username_taken",
                attempted_username = %username,
                attempted_email = %dto.email,
                "🛂 register collision: username '{}' already exists",
                username,
            );
            return Ok(RegisterResult::UsernameTaken);
        }

        if self
            .user_storage
            .get_user_by_email(&dto.email)
            .await
            .is_ok()
        {
            tracing::info!(
                target: "audit",
                event = "auth.register",
                reason = "email_taken",
                attempted_email = %dto.email,
                "🛂 register collision: email '{}' is already registered",
                dto.email,
            );
            return Ok(RegisterResult::EmailTaken);
        }

        // SECURITY: Public registration ALWAYS creates regular users.
        // Admin users can only be created via:
        //   1. The one-time /api/setup endpoint (first boot)
        //   2. The admin panel (admin_create_user)
        let role = UserRole::User;
        let quota = self.capped_quota(&role);

        // Validate password length before hashing — only when one is
        // supplied. Omitted password means the user opts into the
        // magic-link bootstrap path.
        let password_hash = match dto.password {
            Some(ref pw) => {
                if pw.len() < 8 {
                    return Err(DomainError::new(
                        ErrorKind::InvalidInput,
                        "User",
                        "Password must be at least 8 characters long",
                    ));
                }
                Some(self.password_hasher.hash_password(pw).await?)
            }
            None => None,
        };

        let user = User::new(
            dto.email.clone(),
            dto.username.clone(),
            password_hash,
            None, // federation_kind: local password registration
            None, // federation_issuer
            None, // federation_subject
            role,
            quota,
            false,
        )
        .map_err(|e| {
            DomainError::new(
                ErrorKind::InvalidInput,
                "User",
                format!("Error creating user: {}", e),
            )
        })?;

        // Save user
        let created_user = self.user_storage.create_user(user).await?;

        // Lifecycle: PersonalDriveLifecycleHook handles personal-folder
        // creation (was inlined here pre-PR 3); audit log + future
        // provisioning steps land here too.
        if let Some(lc) = &self.user_lifecycle {
            lc.dispatch_created(&created_user).await;
        }

        tracing::info!(
            target: "audit",
            event = "auth.register",
            reason = "created",
            user_id = %created_user.id(),
            username = %created_user.display_for_audit(),
            email = %created_user.email(),
            is_external = false,
            "🛂 user registered",
        );
        Ok(RegisterResult::Created(Box::new(UserDto::from(
            created_user,
        ))))
    }

    /// Create the first admin user during initial system setup.
    ///
    /// This is called by the `/api/setup` endpoint after verifying the setup
    /// token. It unconditionally creates an admin user. The caller (handler)
    /// is responsible for:
    ///   1. Verifying the setup token
    ///   2. Checking that the system is not already initialized
    ///   3. Marking the system as initialized after this call succeeds
    pub async fn setup_create_admin(
        &self,
        username: String,
        email: String,
        password: String,
    ) -> Result<UserDto, DomainError> {
        // Validate username
        if username.len() < 3 || username.len() > 254 {
            return Err(DomainError::new(
                ErrorKind::InvalidInput,
                "User",
                "Username must be between 3 and 254 characters".to_string(),
            ));
        }

        // Check for duplicate username
        if self
            .user_storage
            .get_user_by_username(&username)
            .await
            .is_ok()
        {
            return Err(DomainError::new(
                ErrorKind::AlreadyExists,
                "User",
                format!("User '{}' already exists", username),
            ));
        }

        // Check email uniqueness
        if self.user_storage.get_user_by_email(&email).await.is_ok() {
            return Err(DomainError::new(
                ErrorKind::AlreadyExists,
                "User",
                format!("Email '{}' is already registered", email),
            ));
        }

        // Validate password
        if password.len() < 8 {
            return Err(DomainError::new(
                ErrorKind::InvalidInput,
                "User",
                "Password must be at least 8 characters long".to_string(),
            ));
        }

        let role = UserRole::Admin;
        let quota = self.capped_quota(&role);
        let password_hash = self.password_hasher.hash_password(&password).await?;

        let user = User::new(
            email,
            Some(username.clone()),
            Some(password_hash),
            None, // federation_kind: setup admin is local
            None, // federation_issuer
            None, // federation_subject
            role,
            quota,
            false,
        )
        .map_err(|e| {
            DomainError::new(
                ErrorKind::InvalidInput,
                "User",
                format!("Error creating admin user: {}", e),
            )
        })?;

        // First-run admin is authoritative by definition — they set the
        // password themselves, at the console, on a fresh install. Mark
        // verified so `OXICLOUD_REQUIRE_VERIFIED_EMAIL` never locks the
        // sole account with root-level power out of their own instance.
        let mut user = user;
        user.mark_email_verified();

        let created_user = self.user_storage.create_user(user).await?;

        // Lifecycle: notify hooks. PR 3 moves home-folder creation into
        // PersonalDriveLifecycleHook fired here.
        // Lifecycle: PersonalDriveLifecycleHook provisions the admin's
        // home folder. Audit logs the creation event.
        if let Some(lc) = &self.user_lifecycle {
            lc.dispatch_created(&created_user).await;
        }

        tracing::info!(
            "Initial admin created via setup: {} ({})",
            username,
            created_user.id()
        );
        Ok(UserDto::from(created_user))
    }

    pub async fn login(
        &self,
        dto: LoginDto,
        client_ip: Option<String>,
        user_agent: Option<String>,
    ) -> Result<AuthResponseDto, DomainError> {
        // Gate: policy may forbid password logins entirely (either the
        // legacy OIDC-only mode or the newer `OXICLOUD_AUTH_METHODS`
        // allowlist without `password`). Refuse BEFORE the user lookup
        // so we don't leak account existence via timing on a disabled
        // endpoint.
        if !self.is_password_login_allowed() {
            tracing::info!(
                target: "audit",
                event = "auth.login_rejected",
                reason = "password_login_disabled",
                attempted_username = %dto.username,
                "🔐 login rejected: password login disabled by policy",
            );
            return Err(DomainError::new(
                ErrorKind::AccessDenied,
                "Auth",
                "Password login is disabled",
            ));
        }

        // Dispatch on `@` in the input: presence of `@` means an email
        // was typed, absence means a username. The two namespaces are
        // provably disjoint (PR 16 forbids `@` in usernames), so this
        // is unambiguous — one DB lookup, no fallback chain.
        let lookup = if dto.username.contains('@') {
            self.user_storage.get_user_by_email(&dto.username).await
        } else {
            self.user_storage.get_user_by_username(&dto.username).await
        };
        let user = lookup.map_err(|_| {
            // Audit: unknown-identifier login attempt. Reason key kept
            // stable so log search can aggregate without parsing the
            // human-readable message. Caller's client IP + request id
            // are attached automatically by the request-scope span.
            tracing::info!(
                target: "audit",
                event = "auth.login_rejected",
                reason = "unknown_user",
                attempted_username = %dto.username,
                "🔐 login rejected: no such user '{}'",
                dto.username,
            );
            DomainError::new(ErrorKind::AccessDenied, "Auth", "Invalid credentials")
        })?;

        // Check if user is active
        if !user.is_active() {
            tracing::info!(
                target: "audit",
                event = "auth.login_rejected",
                reason = "account_deactivated",
                user_id = %user.id(),
                username = %user.display_for_audit(),
                "🔐 login rejected: account deactivated for '{}'",
                user.display_for_audit(),
            );
            return Err(DomainError::new(
                ErrorKind::AccessDenied,
                "Auth",
                "Account deactivated",
            ));
        }

        // Verify password using the injected hasher. If the user has no
        // password configured (externals, OIDC-only), short-circuit to
        // "invalid credentials" — the password-login path never accepts
        // a NULL hash.
        let Some(hash) = user.password_hash() else {
            tracing::info!(
                target: "audit",
                event = "auth.login_rejected",
                reason = "no_password",
                user_id = %user.id(),
                username = %user.display_for_audit(),
                "🔐 login rejected: user has no password configured for '{}'",
                user.display_for_audit(),
            );
            return Err(DomainError::new(
                ErrorKind::AccessDenied,
                "Auth",
                "Invalid credentials",
            ));
        };
        let is_valid = self
            .password_hasher
            .verify_password(&dto.password, hash)
            .await?;

        if !is_valid {
            tracing::info!(
                target: "audit",
                event = "auth.login_rejected",
                reason = "bad_password",
                user_id = %user.id(),
                username = %user.display_for_audit(),
                "🔐 login rejected: bad password for '{}'",
                user.display_for_audit(),
            );
            return Err(DomainError::new(
                ErrorKind::AccessDenied,
                "Auth",
                "Invalid credentials",
            ));
        }

        // Phase 4 gate: legacy password login is refused for users who
        // have completed at least one OPAQUE handshake
        // (`opaque_migrated_at IS NOT NULL`). A stale client or a
        // downgrade attacker with a stolen password blob is the only
        // caller who lands here — the SPA already probes
        // `POST /api/auth/opaque/login/lookup` and takes the OPAQUE
        // branch when an envelope exists. Admin password reset
        // atomically NULLs `opaque_migrated_at` (see
        // `opaque_pg_repository.rs::clear_registration`), so the
        // state is coherent — no `force_password_change`
        // carve-out is needed here.
        //
        // Checked AFTER password verify so an attacker without the
        // password learns nothing new about a user's OPAQUE status:
        // only a caller who supplied the right password gets the
        // distinguishing "use OPAQUE" signal, and that caller was
        // going to be redirected anyway.
        //
        // Fails OPEN on repo error — a transient DB blip must not
        // lock every migrated user out; the same login path will
        // succeed on the next attempt when the repo recovers, and
        // an operator reading the audit log sees the failure clearly.
        if let Some(opaque) = self.opaque_repo.as_ref() {
            match opaque.is_migrated(user.id()).await {
                Ok(true) => {
                    tracing::info!(
                        target: "audit",
                        event = "auth.login_rejected",
                        reason = "opaque_migrated_use_opaque",
                        user_id = %user.id(),
                        username = %user.display_for_audit(),
                        "🔐 legacy login refused: user is OPAQUE-migrated ('{}')",
                        user.display_for_audit(),
                    );
                    return Err(DomainError::new(
                        ErrorKind::AccessDenied,
                        "Auth",
                        "Password login refused: this account has migrated to OPAQUE",
                    ));
                }
                Ok(false) => {}
                Err(e) => {
                    tracing::warn!(
                        target: "audit",
                        event = "auth.opaque_migration_check_failed",
                        user_id = %user.id(),
                        error = %e,
                        "OPAQUE migration check failed — allowing legacy login as fallback"
                    );
                }
            }
        }

        // Gate: `OXICLOUD_REQUIRE_VERIFIED_EMAIL`. Checked AFTER password
        // validation so an attacker with only a username cannot probe
        // account verification state (the response shape is
        // `Invalid credentials` for bad passwords regardless of whether
        // the email is verified — a wrong-password observer learns
        // nothing).
        //
        // ADMIN EXEMPTION: admins are trusted by fiat and predate this
        // gate. Fresh admin accounts (admin_create_user /
        // setup_create_admin) are stamped verified at creation; the
        // exemption covers pre-existing admin accounts installed before
        // the flag shipped.
        //
        // The auto-send of a verification magic-link when this branch
        // fires is done at the handler layer (login handler triggers
        // `send_verification_link_authenticated`) rather than here —
        // the service returns the distinguished error and the handler
        // orchestrates the side effect. Keeps this method side-effect-
        // free on the audit path.
        if self.require_verified_email
            && !matches!(user.role(), UserRole::Admin)
            && !user.is_email_verified()
        {
            tracing::info!(
                target: "audit",
                event = "auth.login_rejected",
                reason = "email_not_verified",
                user_id = %user.id(),
                username = %user.display_for_audit(),
                "🔐 login rejected: email not verified for '{}' (password OK)",
                user.display_for_audit(),
            );
            return Err(DomainError::new(
                ErrorKind::AccessDenied,
                "Auth",
                "Email not verified",
            ));
        }

        // Mint the session — factored so the OPAQUE login handler
        // can reuse the exact same shape after a successful OPAQUE
        // handshake (Phase 1, `login/ke3`). Both paths converge here
        // so lifecycle + token + session-family semantics stay in
        // one place.
        self.mint_session_for_authenticated_user(
            user,
            dto.dpop_jkt,
            client_ip,
            user_agent,
            crate::domain::entities::session::SessionOrigin::Password,
        )
        .await
    }

    /// Emit a fresh session for a user who has ALREADY been
    /// authenticated by a mechanism the caller trusts (legacy
    /// password verify, OPAQUE KE3 success, magic-link redemption).
    ///
    /// This method does NOT verify any credential — the caller must
    /// have proven identity before invoking it. What it DOES do:
    ///
    ///   * Dispatch `on_user_login` lifecycle (so
    ///     `PersonalDriveLifecycleHook` can safety-net first-login
    ///     provisioning).
    ///   * Update `last_login_at` (in-memory; `create_session`
    ///     persists it as a side effect via its own transaction).
    ///   * Mint access + refresh tokens under a fresh token family.
    ///   * Persist the session row.
    ///   * Return the shared [`AuthResponseDto`] shape.
    ///
    /// Callers: `login()` (after password verify),
    /// `redeem_magic_link()` (after token redemption),
    /// `interfaces::api::handlers::opaque_auth_handler::login_ke3`
    /// (after OPAQUE handshake).
    pub async fn mint_session_for_authenticated_user(
        &self,
        mut user: crate::domain::entities::user::User,
        dpop_jkt: Option<String>,
        client_ip: Option<String>,
        user_agent: Option<String>,
        origin: crate::domain::entities::session::SessionOrigin,
    ) -> Result<AuthResponseDto, DomainError> {
        // Lifecycle: dispatch login BEFORE register_login() so hooks
        // observing `last_login_at().is_none()` see "first ever login"
        // correctly. See tip #1 in user_lifecycle.rs.
        if let Some(lc) = &self.user_lifecycle {
            lc.dispatch_login(&user).await;
        }

        // Update last login (in-memory only — the DTO below carries it).
        // The full-row `update_user` this path used to issue was 100%
        // redundant: `create_session` stamps `last_login_at`/`updated_at`
        // in its own transaction right below, and nothing re-reads the row
        // in between. Dropping it removes one transaction + a 17-column
        // rewrite (incl. the up-to-512 KiB avatar) per password login
        // (benches/ROUND12.md §2, 4.45x).
        user.register_login();

        // Validate DPoP thumbprint FIRST — the same validated value
        // has to flow into both the JWT `cnf.jkt` claim (RFC 9449 §5)
        // and the session row's `dpop_jkt` column. Reject-before-mint
        // avoids issuing a token whose confirmation-key would be
        // rejected by the very next request's DPoP middleware.
        let validated_jkt = match dpop_jkt.as_deref() {
            Some(jkt) => Some(validate_dpop_jkt(jkt).map_err(|e| {
                tracing::info!(
                    target: "audit",
                    event = "auth.dpop_bind_rejected",
                    reason = "malformed_thumbprint",
                    user_id = %user.id(),
                    "🔐 DPoP bind rejected: {}", e,
                );
                DomainError::new(
                    ErrorKind::InvalidInput,
                    "Auth",
                    "dpop_jkt must be a 43-character base64url SHA-256 thumbprint (RFC 7638)",
                )
            })?),
            None => None,
        };

        // Generate tokens using the injected token service. The
        // access token carries the `cnf.jkt` binding when present,
        // so the DPoP middleware can enforce "bound → proof required"
        // straight from the already-validated JWT — no session-row
        // lookup on the hot path.
        let access_token = self
            .token_service
            .generate_access_token(&user, validated_jkt.as_deref())?;

        let refresh_token = self.token_service.generate_refresh_token();

        // Save session — new login starts a new token family. DPoP
        // binding is set at INSERT time and immutable thereafter (see
        // `docs/plan/dpop.md` — a mutable bind would let an attacker
        // downgrade a bound session by re-binding to their own key).
        let mut session = Session::new(
            user.id(),
            refresh_token.clone(),
            client_ip,
            user_agent,
            self.token_service.refresh_token_expiry_days(),
            Uuid::new_v4(),
            origin,
        );
        if let Some(jkt) = validated_jkt {
            session = session.with_dpop_jkt(jkt);
        }

        self.session_storage.create_session(session).await?;

        // Authentication response
        let force_password_change = self.read_force_password_change(user.id()).await;
        Ok(AuthResponseDto {
            user: UserDto::from(user),
            access_token,
            refresh_token,
            token_type: "Bearer".to_string(),
            expires_in: self.token_service.refresh_token_expiry_secs(),
            force_password_change,
        })
    }

    /// Read `force_password_change_at_next_login` for the given user,
    /// with fail-open semantics on repo error (returns `false` and
    /// logs a warn). Every callsite that builds an `AuthResponseDto`
    /// uses this — mint_session (legacy + OPAQUE), magic-link
    /// redemption, refresh, OIDC callback — so the flag surfaces
    /// consistently across all login shapes, and a DB blip doesn't
    /// spam every response with a spurious change-password prompt.
    async fn read_force_password_change(&self, user_id: Uuid) -> bool {
        self.user_storage
            .is_force_password_change(user_id)
            .await
            .unwrap_or_else(|e| {
                tracing::warn!(
                    target: "audit",
                    event = "auth.force_password_change_read_failed",
                    user_id = %user_id,
                    error = %e,
                    "force_password_change lookup failed; treating as false"
                );
                false
            })
    }

    /// Redeem a magic-link token and emit a fresh session in one shot.
    ///
    /// The flow:
    /// 1. Look up the token in the repo. Unknown token → `NotFound`.
    /// 2. Atomically transition `Pending → Used` via the repo's
    ///    `mark_used()` (single SQL UPDATE with `WHERE status='pending'`).
    ///    A second redemption attempt receives `Ok(false)` and is rejected
    ///    as `AccessDenied`.
    /// 3. Load the user, verify they're active.
    /// 4. Dispatch `on_user_login` (so PersonalDriveLifecycleHook can
    ///    safety-net any internal user whose first credential happens
    ///    to be a magic link — externals short-circuit by `is_external()`).
    /// 5. Register login + persist + issue session in the same pipeline
    ///    as password login.
    ///
    /// The returned `MagicLinkRedemption` carries the resource target so
    /// the handler can build the redirect URL.
    ///
    /// Returns `ServiceUnavailable` (mapped from `NotImplemented`) when
    /// the magic-link repo isn't wired — the handler maps that to HTTP 503.
    ///
    /// `incoming_challenge` is the value the handler read from the
    /// browser's `oxicloud_magic_request` cookie (or `None` if absent).
    /// `cross_browser_confirmed` is `true` when the user has clicked
    /// through the cross-browser confirmation page (PR 22).
    pub async fn redeem_magic_link(
        &self,
        token: &str,
        incoming_challenge: Option<&str>,
        cross_browser_confirmed: bool,
        client_ip: Option<String>,
        user_agent: Option<String>,
    ) -> Result<MagicLinkRedeemResult, DomainError> {
        let repo = self.magic_link_repo.as_ref().ok_or_else(|| {
            DomainError::new(
                ErrorKind::NotImplemented,
                "MagicLink",
                "magic-link feature is not configured on this server",
            )
        })?;

        // Defense-in-depth: if magic-link login was minted under an older
        // policy and the operator has since flipped OIDC on (or dropped
        // `MagicLink` from `OXICLOUD_AUTH_METHODS`), we must not honour
        // pre-existing login tokens. Invitation tokens (resource_kind =
        // File / Folder) are checked separately below — they represent
        // an admin-mediated invite, which is a distinct policy question
        // from "self-service login via email".
        //
        // We do the token lookup FIRST so we can classify by
        // `resource_kind()` before applying the gate — invitations
        // survive, plain logins do not.
        let mlt = repo.find_by_token(token).await?.ok_or_else(|| {
            // Audit: unknown / forged magic-link redemption. The first
            // 8 chars of the bogus token are logged so a recurring
            // probe pattern is recognisable without dumping the full
            // secret to the log stream.
            let token_preview: String = token.chars().take(8).collect();
            tracing::info!(
                target: "audit",
                event = "magic_link.redemption_rejected",
                reason = "unknown_token",
                token_prefix = %token_preview,
                "🔗 magic-link rejected: unknown token (prefix='{}…')",
                token_preview,
            );
            DomainError::new(
                ErrorKind::NotFound,
                "MagicLink",
                "unknown or invalid magic link",
            )
        })?;

        // Enforce the login-magic-link policy on stale tokens.
        // resource_kind = None means "plain login-via-email"; anything
        // else is an invite (which follows its own admin-mediated
        // trust chain). Refuse the login case if the current policy
        // forbids magic-link login.
        if mlt.resource_kind().is_none() && !self.is_magic_link_login_allowed() {
            tracing::info!(
                target: "audit",
                event = "magic_link.redemption_rejected",
                reason = "login_disabled_by_policy",
                token_id = %mlt.id(),
                user_id = %mlt.user_id(),
                "🔗 magic-link rejected: login-via-email disabled by policy (OIDC-master or allowlist)",
            );
            return Err(DomainError::new(
                ErrorKind::AccessDenied,
                "MagicLink",
                "magic-link login is disabled",
            ));
        }

        // Friendly early-rejection messages. The atomic `mark_used`
        // below is the canonical single-use guard.
        if mlt.status() == MagicLinkStatus::Used {
            tracing::info!(
                target: "audit",
                event = "magic_link.redemption_rejected",
                reason = "already_used",
                token_id = %mlt.id(),
                user_id = %mlt.user_id(),
                "🔗 magic-link rejected: token already used for user {}",
                mlt.user_id(),
            );
            return Err(DomainError::new(
                ErrorKind::AccessDenied,
                "MagicLink",
                "this magic link has already been used",
            ));
        }
        if mlt.is_expired() {
            tracing::info!(
                target: "audit",
                event = "magic_link.redemption_rejected",
                reason = "expired",
                token_id = %mlt.id(),
                user_id = %mlt.user_id(),
                "🔗 magic-link rejected: token expired for user {}",
                mlt.user_id(),
            );
            return Err(DomainError::new(
                ErrorKind::AccessDenied,
                "MagicLink",
                "this magic link has expired",
            ));
        }

        // PR 22 — browser binding for login-via-email tokens. When the
        // token carries a `request_challenge`, compare it against the
        // cookie the handler extracted. Mismatch surfaces as a
        // cross-browser confirmation page (the handler renders the
        // HTML); the user clicks Continue and we re-enter with
        // `cross_browser_confirmed = true`. Invitation tokens have no
        // challenge — they bypass this check entirely (cross-device by
        // design). The token is NOT marked used on the prompt path —
        // it stays redeemable for the confirm round-trip.
        if let Some(expected) = mlt.request_challenge()
            && !cross_browser_confirmed
            && incoming_challenge != Some(expected)
        {
            tracing::info!(
                target: "audit",
                event = "magic_link.cross_browser_prompt",
                token_id = %mlt.id(),
                user_id = %mlt.user_id(),
                incoming_present = incoming_challenge.is_some(),
                "🔗 magic-link cross-browser: cookie absent or mismatched for user {}",
                mlt.user_id(),
            );
            return Ok(MagicLinkRedeemResult::NeedsCrossBrowserConfirm);
        }

        let consumed = repo.mark_used(mlt.id()).await?;
        if !consumed {
            // Either a concurrent redemption beat us, or the row was
            // marked expired by the sweeper between our find and update.
            tracing::info!(
                target: "audit",
                event = "magic_link.redemption_rejected",
                reason = "race_or_swept",
                token_id = %mlt.id(),
                user_id = %mlt.user_id(),
                "🔗 magic-link rejected: lost race to mark_used (user {})",
                mlt.user_id(),
            );
            return Err(DomainError::new(
                ErrorKind::AccessDenied,
                "MagicLink",
                "this magic link has already been used",
            ));
        }

        let mut user = self.user_storage.get_user_by_id(mlt.user_id()).await?;
        if !user.is_active() {
            tracing::info!(
                target: "audit",
                event = "magic_link.redemption_rejected",
                reason = "account_deactivated",
                token_id = %mlt.id(),
                user_id = %user.id(),
                username = %user.display_for_audit(),
                "🔗 magic-link rejected: account deactivated for '{}'",
                user.display_for_audit(),
            );
            return Err(DomainError::new(
                ErrorKind::AccessDenied,
                "Auth",
                "Account deactivated",
            ));
        }

        // Dispatch BEFORE register_login so hooks observing
        // `last_login_at().is_none()` see "first ever login" correctly.
        if let Some(lc) = &self.user_lifecycle {
            lc.dispatch_login(&user).await;
        }
        user.register_login();
        // PR 23: clicking the magic-link IS proof of email control —
        // stamp the verification (idempotent, preserves the first
        // timestamp). Applies to both invitation and login-via-email
        // tokens. Narrow single-column write: `last_login_at` is stamped
        // by `create_session` below, so the full-row `update_user` this
        // path used to issue only ever contributed the verification
        // timestamp (benches/ROUND12.md §3, 8.9x).
        user.mark_email_verified();
        self.user_storage.mark_email_verified(user.id()).await?;

        // Magic-link redemption is a GET redirect — no way to
        // thread `dpop_jkt` into a GET body. Session is minted
        // unbound; the SPA calls `POST /api/auth/dpop/bind`
        // post-redirect to bind it (see Gate 3). Token accordingly
        // ships without `cnf.jkt`.
        let access_token = self.token_service.generate_access_token(&user, None)?;
        let refresh_token = self.token_service.generate_refresh_token();
        let session = Session::new(
            user.id(),
            refresh_token.clone(),
            client_ip,
            user_agent,
            self.token_service.refresh_token_expiry_days(),
            Uuid::new_v4(),
            crate::domain::entities::session::SessionOrigin::MagicLink,
        );
        self.session_storage.create_session(session).await?;

        tracing::info!(
            target: "audit",
            event = "magic_link.redeemed",
            user_id = %user.id(),
            username = %user.display_for_audit(),
            is_external = user.is_external(),
            resource_kind = ?mlt.resource_kind(),
            resource_id = ?mlt.resource_id(),
            cross_browser_confirmed = cross_browser_confirmed,
        );

        let force_password_change = self.read_force_password_change(user.id()).await;
        let auth = AuthResponseDto {
            user: UserDto::from(user),
            access_token,
            refresh_token,
            token_type: "Bearer".to_string(),
            expires_in: self.token_service.refresh_token_expiry_secs(),
            force_password_change,
        };

        Ok(MagicLinkRedeemResult::Allowed(Box::new(
            MagicLinkRedemption {
                auth,
                resource_kind: mlt.resource_kind(),
                resource_id: mlt.resource_id(),
            },
        )))
    }

    /// Verifies username/password credentials without creating a session.
    pub async fn verify_credentials(
        &self,
        username: &str,
        password: &str,
    ) -> Result<crate::application::dtos::user_dto::CurrentUser, DomainError> {
        let user = self
            .user_storage
            .get_user_by_username(username)
            .await
            .map_err(|_| {
                DomainError::new(ErrorKind::AccessDenied, "Auth", "Invalid credentials")
            })?;

        if !user.is_active() {
            return Err(DomainError::new(
                ErrorKind::AccessDenied,
                "Auth",
                "Account deactivated",
            ));
        }

        let Some(hash) = user.password_hash() else {
            return Err(DomainError::new(
                ErrorKind::AccessDenied,
                "Auth",
                "Invalid credentials",
            ));
        };
        let is_valid = self.password_hasher.verify_password(password, hash).await?;

        if !is_valid {
            return Err(DomainError::new(
                ErrorKind::AccessDenied,
                "Auth",
                "Invalid credentials",
            ));
        }

        Ok(crate::application::dtos::user_dto::CurrentUser {
            id: user.id(),
            username: std::sync::Arc::from(user.username().unwrap_or("")),
            email: std::sync::Arc::from(user.email()),
            role: smol_str::SmolStr::new_static(user.role().as_str()),
            // `verify_credentials` is only called from paths that
            // don't need per-session DPoP context (admin/setup
            // flows); the DPoP middleware never reads CurrentUser
            // populated by this method. Leaving None is safe.
            dpop_jkt: None,
        })
    }

    pub async fn refresh_token(
        &self,
        dto: RefreshTokenDto,
        client_ip: Option<String>,
        user_agent: Option<String>,
    ) -> Result<AuthResponseDto, DomainError> {
        // Get valid session
        let session = self
            .session_storage
            .get_session_by_refresh_token(&dto.refresh_token)
            .await?;

        // Reuse detection: a revoked token being replayed indicates the token was
        // stolen after rotation. Invalidate the entire family to protect all devices.
        if session.is_revoked() {
            tracing::warn!(
                user_id = %session.user_id(),
                family_id = %session.family_id(),
                "Refresh token reuse detected — revoking entire token family"
            );
            self.session_storage
                .revoke_session_family(session.family_id())
                .await?;
            // Lifecycle: TokenReused logout — fired once per logical
            // revoke-family call. PR 4 may refine to per-session firing.
            if let Some(lc) = &self.user_lifecycle
                && let Ok(user) = self.user_storage.get_user_by_id(session.user_id()).await
            {
                lc.dispatch_logout(user, LogoutReason::TokenReused);
            }
            return Err(DomainError::new(
                ErrorKind::AccessDenied,
                "Auth",
                "Session expired or invalid",
            ));
        }

        if session.is_expired() {
            return Err(DomainError::new(
                ErrorKind::AccessDenied,
                "Auth",
                "Session expired or invalid",
            ));
        }

        // Get user
        let user = self.user_storage.get_user_by_id(session.user_id()).await?;

        // Check if user is active
        if !user.is_active() {
            return Err(DomainError::new(
                ErrorKind::AccessDenied,
                "Auth",
                "Account deactivated",
            ));
        }

        // Generate new tokens. Inherit the DPoP binding from the
        // parent session so the refreshed access token carries the
        // same `cnf.jkt` — otherwise every refresh would silently
        // downgrade to unbound and the next request would 401 under
        // Gate 9 enforcement (see Gate 7).
        let access_token = self
            .token_service
            .generate_access_token(&user, session.dpop_jkt())?;
        let new_refresh_token = self.token_service.generate_refresh_token();

        // New session inherits the family_id so reuse of any ancestor triggers
        // full-family revocation. Revoking the old session and inserting the
        // new one happen in ONE transaction (`rotate_session`) — this path
        // used to pay two BEGIN/COMMIT pairs per refresh, and DAV clients
        // rotate constantly (benches/ROUND12.md §4).
        //
        // The DPoP binding travels with the family: if the parent session
        // was bound to a browser-held keypair, the refreshed session MUST
        // be bound to the same one (see `docs/plan/dpop.md` Gate 7). Same
        // browser → same key → same jkt. Skipping this would let a
        // refresh silently downgrade the session to unbound, and every
        // subsequent request would fail DPoP verification once required
        // mode enforces per-session binding.
        let mut new_session = Session::new(
            user.id(),
            new_refresh_token.clone(),
            client_ip,
            user_agent,
            self.token_service.refresh_token_expiry_days(),
            session.family_id(),
            // A rotation doesn't change how the user first authenticated,
            // so origin is inherited from the parent row. Also keeps the
            // admin panel's origin column stable across the natural
            // refresh cycle apiFetch triggers on every 401.
            session.origin(),
        );
        if let Some(jkt) = session.dpop_jkt() {
            new_session = new_session.with_dpop_jkt(jkt.to_string());
        }
        // Carry over the OIDC provenance so RP-initiated logout still
        // works after a refresh. Without this, an OIDC session rotates
        // into a row with `oidc_id_token = NULL` on the very first
        // refresh (which apiFetch triggers transparently on any 401),
        // and the `/api/auth/logout` handler then has no `id_token_hint`
        // to build the IdP's `end_session_endpoint` URL — user gets a
        // local-only logout and stays signed in on the IdP.
        // `oidc_sid` follows the same rule so Back-Channel Logout can
        // still target this device via the IdP's sid claim after refresh.
        if let Some(id_token) = session.oidc_id_token() {
            new_session = new_session.with_oidc_id_token(id_token.to_string());
        }
        if let Some(sid) = session.oidc_sid() {
            new_session = new_session.with_oidc_sid(sid.to_string());
        }

        self.session_storage
            .rotate_session(session.id(), new_session)
            .await?;

        // Refresh re-reads the flag so an admin flip mid-session
        // surfaces on the next refresh even if it wasn't set at
        // initial login. The SPA's post-refresh flow (silent, on
        // its own timer) can then route the user to change-password
        // without waiting for an explicit re-login.
        let force_password_change = self.read_force_password_change(user.id()).await;
        Ok(AuthResponseDto {
            user: UserDto::from(user),
            access_token,
            refresh_token: new_refresh_token,
            token_type: "Bearer".to_string(),
            expires_in: self.token_service.refresh_token_expiry_secs(),
            force_password_change,
        })
    }

    /// Revoke the caller's session and, when the session was minted through
    /// OIDC, build the RP-initiated logout URL so the browser can also end
    /// the IdP's SSO session (fixes shared-computer scenario where local
    /// logout alone would let the next `/login` visit silently re-auth
    /// through a still-valid IdP cookie).
    ///
    /// Returns `Ok(None)` for:
    /// - non-OIDC sessions (password / magic-link) — nothing to propagate;
    /// - OIDC sessions where the IdP's discovery doesn't advertise an
    ///   `end_session_endpoint` — no way to propagate. Callers should still
    ///   clear local cookies; the IdP session will time out on its own.
    ///
    /// `post_logout_redirect_uri` MUST be registered on the OIDC client
    /// (Keycloak: "Valid post logout redirect URIs"), else the IdP refuses
    /// the redirect back and the user is left on the IdP error page.
    pub async fn logout(
        &self,
        user_id: Uuid,
        refresh_token: &str,
        post_logout_redirect_uri: &str,
    ) -> Result<Option<String>, DomainError> {
        // Get session
        let session = match self
            .session_storage
            .get_session_by_refresh_token(refresh_token)
            .await
        {
            Ok(s) => s,
            // If the session doesn't exist, we consider the logout successful
            Err(_) => return Ok(None),
        };

        // Verify that the session belongs to the user
        if session.user_id() != user_id {
            return Err(DomainError::new(
                ErrorKind::AccessDenied,
                "Auth",
                "The session does not belong to the user",
            ));
        }

        // Capture the id_token BEFORE revocation so we can build the
        // RP-initiated logout URL. Revocation only flips a boolean, so the
        // row (and its oidc_id_token column) survives — this order is
        // defensive against a future change that hard-deletes on revoke.
        let id_token_hint = session.oidc_id_token().map(str::to_string);

        // Revoke session
        self.session_storage.revoke_session(session.id()).await?;

        // Lifecycle: notify hooks. One extra DB roundtrip per logout
        // (user load) is acceptable — logout is rare. Failure to load
        // the user is non-fatal: we already revoked the session.
        if let Some(lc) = &self.user_lifecycle
            && let Ok(user) = self.user_storage.get_user_by_id(user_id).await
        {
            lc.dispatch_logout(user, LogoutReason::UserInitiated);
        }

        // If this was an OIDC session AND the IdP advertises an
        // end_session_endpoint, build the RP-initiated logout URL.
        // Otherwise return None — the caller clears local state either way.
        let Some(id_token) = id_token_hint else {
            return Ok(None);
        };
        let oidc = { self.oidc.read().unwrap().service.clone() };
        let Some(oidc) = oidc else {
            return Ok(None);
        };
        oidc.build_end_session_url(&id_token, post_logout_redirect_uri)
            .await
    }

    /// OIDC Back-Channel Logout 1.0 entry point.
    ///
    /// Called by the public BCL handler with an unvalidated logout_token
    /// (as delivered by the IdP over server-to-server HTTP). This method
    /// owns the full flow:
    ///
    ///   1. Validate the token (signature + spec-mandated claims).
    ///   2. Reject replays via the `jti` seen-cache (best-effort — tokens
    ///      without a jti are impossible to dedupe cheaply, so the revoke
    ///      path stays idempotent as a safety net).
    ///   3. Prefer `sid` (per-device revocation) over `sub` (all-device)
    ///      when both are present — matches the intent of the IdP that
    ///      chose to include `sid`.
    ///   4. Dispatch per-user lifecycle hooks so downstream systems
    ///      (websocket subscriptions, etc.) can react.
    ///
    /// Returns the count of session rows actually flipped from
    /// `revoked=false` to `revoked=true` — 0 is a fine outcome (already
    /// logged out or unknown user; both are indistinguishable from the
    /// IdP's viewpoint and both mean "OxiCloud has no live session for
    /// that identity").
    pub async fn backchannel_logout(&self, logout_token: &str) -> Result<u64, DomainError> {
        let oidc = {
            let state = self.oidc.read().unwrap();
            state.service.clone().ok_or_else(|| {
                DomainError::new(
                    ErrorKind::InternalError,
                    "OIDC",
                    "OIDC service not configured — cannot process backchannel logout",
                )
            })?
        };

        let claims = oidc.validate_logout_token(logout_token).await?;

        // Replay guard. Insertion-first-then-check: `get()` + `insert()`
        // is racy across concurrent BCL calls with the same jti (both
        // could observe absent, both would run the revocation), but the
        // revocation is idempotent so at worst we double-audit. If it
        // matters more we can move to `entry().or_insert()` semantics.
        if let Some(jti) = claims.jti.as_ref() {
            if self.backchannel_logout_jti_seen.get(jti).is_some() {
                tracing::info!(
                    target: "audit",
                    event = "oidc.backchannel_logout_replayed",
                    jti = %jti,
                    "👮🏻‍♂️ OIDC backchannel-logout token replayed — ignored"
                );
                return Ok(0);
            }
            self.backchannel_logout_jti_seen.insert(jti.clone(), ());
        }

        // Resolve which sessions to revoke.
        let affected_user_ids: Vec<Uuid> = if let Some(sid) = claims.sid.as_ref() {
            self.session_storage
                .revoke_sessions_by_oidc_sid(sid)
                .await?
        } else if let Some(sub) = claims.sub.as_ref() {
            // Pass claims.iss (the id_token's real issuer URL from the
            // logout_token), NOT the OIDC service's provider_name
            // display label. Post Phase B of the federation-identity
            // rename, `auth.users.federation_issuer` stores the iss
            // URL — matching on the display label misses every row.
            self.session_storage
                .revoke_user_sessions_by_federation_subject(&claims.iss, sub)
                .await?
                .into_iter()
                .collect()
        } else {
            // Validator already enforced sub-or-sid presence; being here
            // means the validator has drifted. Fail loud.
            return Err(DomainError::new(
                ErrorKind::InternalError,
                "OIDC",
                "backchannel_logout: validator returned claims without sub or sid",
            ));
        };

        // Dispatch lifecycle hooks per unique affected user. Best-effort;
        // hook failures don't undo the revocation (which already committed).
        // Deduped because sid-based revocation could theoretically match
        // multiple sessions for the same user if the IdP re-issued sids.
        if let Some(lc) = &self.user_lifecycle {
            let unique: std::collections::HashSet<Uuid> =
                affected_user_ids.iter().copied().collect();
            for uid in unique {
                if let Ok(user) = self.user_storage.get_user_by_id(uid).await {
                    lc.dispatch_logout(user, LogoutReason::IdpNotification);
                }
            }
        }

        Ok(affected_user_ids.len() as u64)
    }

    pub async fn logout_all(&self, user_id: Uuid) -> Result<u64, DomainError> {
        // Revoke all user sessions
        let revoked_count = self
            .session_storage
            .revoke_all_user_sessions(user_id)
            .await?;

        Ok(revoked_count)
    }

    /// External → internal account upgrade.
    ///
    /// Contract:
    ///   * Caller must be authenticated as the user being upgraded.
    ///     Session-elevation is not required — being logged in as
    ///     yourself IS the proof of intent.
    ///   * User must be `is_external = true` — else the entity refuses
    ///     with `UserError::AlreadyInternal`, surfaced as `error_type =
    ///     "AlreadyInternal"` (409).
    ///   * OIDC-linked users are refused (the IdP owns their identity).
    ///   * If `dto.password` is `None`, the deployment MUST have magic-
    ///     link login enabled — otherwise the upgraded user would have
    ///     no login path. Refused with `error_type = "PasswordRequired"`
    ///     (400) in that case.
    ///   * Domain-allowlist check lives at the HANDLER layer, mirroring
    ///     the register handler — the service doesn't hold that config.
    ///
    /// On success:
    ///   * User's `is_external` flipped to `false`.
    ///   * `password_hash` set from the provided password (Argon2id) or
    ///     left as-is (magic-link-only upgrade).
    ///   * `storage_quota_bytes` set to the default user quota (capped
    ///     by disk).
    ///   * `PersonalDriveLifecycleHook::on_upgraded_to_internal` runs and
    ///     provisions the home drive + root folder + owner grant via the
    ///     atomic CTE. Failure at this step is logged but the row update
    ///     stands — the next login's `on_user_login` safety-net retries
    ///     provisioning.
    ///   * `user_flags_cache` invalidated eagerly so per-request guards
    ///     (WebDAV / CalDAV / CardDAV) observe the new `is_external`
    ///     within cache-round-trip time, not the 30-second TTL.
    ///   * Audit log emits `event="user.upgraded_to_internal"` via the
    ///     `AuditLifecycleHook` on the dispatched event.
    pub async fn upgrade_to_internal(
        &self,
        caller_id: Uuid,
        dto: UpgradeToInternalDto,
    ) -> Result<UserDto, DomainError> {
        let mut user = self.user_storage.get_user_by_id(caller_id).await?;

        // Precondition: caller is currently external. Fast-path 409 so
        // the audit log carries a clear reason before the entity's own
        // guard fires.
        if !user.is_external() {
            tracing::info!(
                target: "audit",
                event = "user.upgrade_rejected",
                reason = "already_internal",
                user_id = %user.id(),
                username = %user.display_for_audit(),
                "👮🏻‍♂️ upgrade refused: user is already internal",
            );
            return Err(DomainError::new(
                ErrorKind::Conflict,
                "User",
                "Account is already internal",
            ));
        }

        // OIDC-linked: never. The IdP owns identity and role.
        if user.is_oidc_user() {
            tracing::info!(
                target: "audit",
                event = "user.upgrade_rejected",
                reason = "oidc_user",
                user_id = %user.id(),
                "👮🏻‍♂️ upgrade refused: OIDC-linked user is managed by the IdP",
            );
            return Err(DomainError::new(
                ErrorKind::AccessDenied,
                "User",
                "SSO/OIDC accounts are managed by your identity provider",
            ));
        }

        // Password policy composite:
        //   * Provided → validate + hash.
        //   * Omitted   → only accepted when magic-link login is on
        //     for this deployment (otherwise no login path post-upgrade).
        let password_hash = match dto.password.as_deref() {
            Some(pw) if !pw.is_empty() => {
                if pw.len() < 8 {
                    return Err(DomainError::new(
                        ErrorKind::InvalidInput,
                        "User",
                        "Password must be at least 8 characters long",
                    ));
                }
                Some(self.password_hasher.hash_password(pw).await?)
            }
            _ => {
                if !self.is_magic_link_login_allowed() {
                    tracing::info!(
                        target: "audit",
                        event = "user.upgrade_rejected",
                        reason = "password_required",
                        user_id = %user.id(),
                        "👮🏻‍♂️ upgrade refused: password omitted but magic-link login is not available on this deployment",
                    );
                    return Err(DomainError::new(
                        ErrorKind::InvalidInput,
                        "User",
                        "Password is required — magic-link login is not enabled on this deployment",
                    ));
                }
                None
            }
        };

        // Quota policy: same as a fresh regular-user signup.
        let quota = self.capped_quota(&UserRole::User);

        user.promote_to_internal(password_hash, quota)
            .map_err(|e| {
                // The entity refuses `AlreadyInternal` here belt-and-braces
                // against a race with a concurrent upgrade; the pre-check
                // above already covers the intended path.
                DomainError::new(
                    ErrorKind::Conflict,
                    "User",
                    format!("Upgrade refused: {}", e),
                )
            })?;

        let updated = self.user_storage.update_user(user).await?;

        // Invalidate the flags cache so subsequent per-request guards
        // observe the new `is_external=false` without waiting for the
        // 30-second TTL. Same pattern as `change_user_role`.
        self.user_flags_cache.invalidate(&caller_id).await;

        // Dispatch — home-drive provisioning happens here. Log-and-
        // continue: a provisioning failure leaves the row updated and
        // the next login's safety-net (`on_user_login`) retries.
        if let Some(lc) = &self.user_lifecycle {
            lc.dispatch_upgraded_to_internal(&updated).await;
        }

        Ok(UserDto::from(updated))
    }

    /// Admin-driven external → internal promotion.
    ///
    /// Same wire outcome as [`Self::upgrade_to_internal`] but the actor
    /// is an operator, not the target user. The target's password stays
    /// as it was (usually `None` — magic-link-only accounts) so the
    /// deployment MUST have magic-link login enabled, otherwise the
    /// promoted user has no login path at all.
    ///
    /// Refuses:
    /// - Target is already internal → 409 `AlreadyInternal`.
    /// - Target is OIDC-linked → 403 (IdP owns identity).
    /// - Magic-link login disabled deployment-wide → 400 with a hint.
    ///
    /// On success:
    /// - `is_external → false`, `storage_quota_bytes → capped default`.
    /// - Home-drive provisioning fires via
    ///   `PersonalDriveLifecycleHook::on_upgraded_to_internal` — same
    ///   hook the self-upgrade path uses.
    /// - `user_flags_cache` invalidated on the target so per-request
    ///   guards observe the new flag within one cache round-trip.
    /// - Audit line `event = "user.promoted_to_internal_by_admin"`
    ///   with `by = <admin_id>`, `target_id = <user_id>`.
    pub async fn admin_promote_external_to_internal(
        &self,
        admin_id: Uuid,
        target_id: Uuid,
    ) -> Result<UserDto, DomainError> {
        let mut user = self.user_storage.get_user_by_id(target_id).await?;

        if !user.is_external() {
            tracing::info!(
                target: "audit",
                event = "user.promote_rejected",
                reason = "already_internal",
                by = %admin_id,
                target_id = %target_id,
                "👮🏻‍♂️ admin-promote refused: target user is already internal",
            );
            return Err(DomainError::new(
                ErrorKind::Conflict,
                "User",
                "Account is already internal",
            ));
        }

        if user.is_oidc_user() {
            tracing::info!(
                target: "audit",
                event = "user.promote_rejected",
                reason = "oidc_user",
                by = %admin_id,
                target_id = %target_id,
                "👮🏻‍♂️ admin-promote refused: OIDC-linked user is managed by the IdP",
            );
            return Err(DomainError::new(
                ErrorKind::AccessDenied,
                "User",
                "SSO/OIDC accounts are managed by your identity provider",
            ));
        }

        // Admin can't set a password on the target's behalf, so the
        // upgraded account MUST have magic-link login available on the
        // deployment — otherwise no login path exists post-promotion.
        if !self.is_magic_link_login_allowed() {
            tracing::info!(
                target: "audit",
                event = "user.promote_rejected",
                reason = "no_login_path",
                by = %admin_id,
                target_id = %target_id,
                "👮🏻‍♂️ admin-promote refused: magic-link login disabled and admin can't set the target's password",
            );
            return Err(DomainError::new(
                ErrorKind::InvalidInput,
                "User",
                "Cannot promote: magic-link login is disabled on this deployment, so the user would have no login path.",
            ));
        }

        let quota = self.capped_quota(&UserRole::User);

        user.promote_to_internal(None, quota).map_err(|e| {
            DomainError::new(
                ErrorKind::Conflict,
                "User",
                format!("Promote refused: {}", e),
            )
        })?;

        let updated = self.user_storage.update_user(user).await?;

        // Invalidate the target's flags cache — same reason as the
        // self-upgrade path.
        self.user_flags_cache.invalidate(&target_id).await;

        if let Some(lc) = &self.user_lifecycle {
            lc.dispatch_upgraded_to_internal(&updated).await;
        }

        tracing::info!(
            target: "audit",
            event = "user.promoted_to_internal_by_admin",
            by = %admin_id,
            target_id = %target_id,
            "👮🏻‍♂️ external user promoted to internal by admin",
        );

        Ok(UserDto::from(updated))
    }

    /// `keep_session_id` — when `Some`, revoke every OTHER session for
    /// this user but leave the identified one alive. Classic
    /// "password change" pattern: log the user out from other devices
    /// but keep the current one authenticated so the SPA can complete
    /// follow-up work (OPAQUE envelope re-registration) without
    /// racing a session-death 401. When `None`, revokes all sessions
    /// (preserves the original behaviour for callers without session
    /// context).
    ///
    /// Handler-layer callers should extract the current session_id
    /// from the request's refresh-token cookie and pass it in; other
    /// callers (CLI, tests, admin flows that don't have a specific
    /// current session) leave it `None`.
    pub async fn change_password(
        &self,
        user_id: Uuid,
        dto: ChangePasswordDto,
        keep_session_id: Option<Uuid>,
    ) -> Result<(), DomainError> {
        // Get user
        let mut user = self.user_storage.get_user_by_id(user_id).await?;

        // Two structural refusals. Order chosen so the more-specific
        // "your credential is IdP-managed" wins for pure-OIDC users
        // (which is the case the message text addresses); the
        // deployment-wide "password auth is off" wins for everyone
        // else on an SSO-only deployment.
        //
        //   1. Pure-OIDC user (SSO-linked AND no local password).
        //      Hybrid accounts with an OIDC linkage BUT also a
        //      `password_hash` on file are a legitimate posture on
        //      deployments that offer SSO alongside password auth —
        //      they can and must be able to rotate the local
        //      credential from this endpoint.
        //
        //   2. Deployment has password auth disabled globally
        //      (`OXICLOUD_AUTH_METHODS` missing `password`, or the
        //      legacy `OXICLOUD_OIDC_DISABLE_PASSWORD_LOGIN` alias).
        //      Even a user who still has `password_hash` from before
        //      the operator flipped this shouldn't be updating that
        //      hash — they can't USE it to log in, and leaving a
        //      write path exposed keeps a live credential the
        //      operator likely wanted retired.
        if user.is_oidc_user() && !user.has_password() {
            return Err(DomainError::new(
                ErrorKind::AccessDenied,
                "Auth",
                "Password changes are not available for SSO/OIDC accounts. Your password is managed by your identity provider.",
            ));
        }
        if !self.is_password_login_allowed() {
            return Err(DomainError::new(
                ErrorKind::AccessDenied,
                "Auth",
                "Password login is disabled on this deployment; password change is not available.",
            ));
        }

        // Verify current password using the injected hasher
        let Some(hash) = user.password_hash() else {
            return Err(DomainError::new(
                ErrorKind::AccessDenied,
                "Auth",
                "Current password is incorrect",
            ));
        };
        let is_valid = self
            .password_hasher
            .verify_password(&dto.current_password, hash)
            .await?;

        if !is_valid {
            return Err(DomainError::new(
                ErrorKind::AccessDenied,
                "Auth",
                "Current password is incorrect",
            ));
        }

        // Validate new password
        if dto.new_password.len() < 8 {
            return Err(DomainError::new(
                ErrorKind::InvalidInput,
                "User",
                "Password must be at least 8 characters long",
            ));
        }

        // Reject same-as-current. Load-bearing when the caller is on
        // an admin-picked temp password (force_password_change_at_next_login
        // = TRUE): silently accepting the same string would clear the
        // force flag without the user actually rotating the credential,
        // defeating the whole "temporary password" pattern. Verify
        // against the stored hash (constant-time via `verify_password`)
        // rather than string-comparing plaintexts, so length / case
        // typos on the caller's part still fail cleanly. Handler
        // layer remaps the message to `error_type: "PasswordUnchanged"`.
        let same_as_current = self
            .password_hasher
            .verify_password(&dto.new_password, hash)
            .await?;
        if same_as_current {
            return Err(DomainError::new(
                ErrorKind::InvalidInput,
                "User",
                "New password must differ from the current password",
            ));
        }

        // Hash new password and update user
        let new_hash = self
            .password_hasher
            .hash_password(&dto.new_password)
            .await?;
        user.update_password_hash(Some(new_hash));

        // Save updated user
        self.user_storage.update_user(user.clone()).await?;

        // OPAQUE envelope handling: the OLD envelope was bound to the
        // OLD passphrase via the OPRF. Left in place, the next OPAQUE
        // login with the NEW password would derive a mismatched OPRF
        // output and fail the AKE with InvalidCredentials → user
        // locked out (Phase 3 SPA doesn't fall back OPAQUE→legacy).
        //
        // The RESPONSIBILITY for re-minting the envelope belongs to
        // the SPA — see `frontend/src/lib/api/endpoints/profile.ts`
        // → `changePassword` → `syncOpaqueEnvelope(newPw)`. That call
        // hits the session-authenticated `/register/*` endpoints
        // immediately after this handler returns 200. It works
        // because we keep the current session alive below
        // (`revoke_other_user_sessions` instead of the full-revocation
        // call this handler used to make).
        //
        // The server does NOT clear the envelope here. The SPA
        // re-registration is monotonic — the envelope transitions
        // straight from OLD-password bound to NEW-password bound
        // without a null intermediate. This matters for the migration
        // ledger: `opaque_migrated_at` stays intact, admin dashboards
        // don't see a spurious "unmigrated" blip.
        //
        // Recovery for the rare SPA-failure case: the operator runs
        // `oxicloud-cli opaque reset --user <id>` to null the
        // envelope; the user's next login goes through legacy path
        // (since `hasOpaque: false` after the CLI reset) and silent-
        // migration mints a fresh envelope under the new password.

        // Clear the admin-set "temporary password" marker — the user
        // has just picked their own password, so the next-login prompt
        // has served its purpose. Failure here is non-fatal (login
        // will just keep prompting until an admin resets or a later
        // change_password succeeds), but log so ops sees any
        // consistent drift.
        if let Err(e) = self.user_storage.clear_force_password_change(user_id).await {
            tracing::warn!(
                target: "audit",
                event = "auth.force_password_change_clear_failed",
                user_id = %user_id,
                error = %e,
                "clear_force_password_change failed after change_password success"
            );
        }

        // Evict the cached UserFlags entry so
        // `require_no_password_change_pending` sees the just-cleared
        // flag on the next request — otherwise the caller would keep
        // hitting 403 PasswordChangeRequired until the 30 s TTL rolls
        // over. (The revoke_all_user_sessions below will force a
        // re-login anyway, but the cache eviction covers the window
        // between change_password success and the new session mint.)
        self.user_flags_cache.invalidate(&user_id).await;

        // Session revocation posture: classic "password change" pattern
        // — kill every OTHER session for this user (any device / tab
        // that had cached the old credential), but keep the caller's
        // CURRENT session alive so the SPA can complete the OPAQUE
        // envelope re-registration on the same session cookie that
        // successfully hit this endpoint. Without the `keep_session_id`
        // preservation, `syncOpaqueEnvelope` in profile.ts would 401
        // (session gone), the envelope would stay bound to the OLD
        // password, and the user would be locked out on next OPAQUE
        // login. `None` = caller has no session context (CLI, admin
        // flows), fall back to full revocation.
        match keep_session_id {
            Some(keep) => {
                self.session_storage
                    .revoke_other_user_sessions(user_id, keep)
                    .await?;
            }
            None => {
                self.session_storage
                    .revoke_all_user_sessions(user_id)
                    .await?;
            }
        }

        // Lifecycle: PasswordChanged logout — fired once per logical
        // revoke-all call. PR 4 may refine to per-session firing.
        if let Some(lc) = &self.user_lifecycle {
            lc.dispatch_logout(user, LogoutReason::PasswordChanged);
        }

        Ok(())
    }

    /// Update the profile image for a non-OIDC user.
    pub async fn update_user_image(
        &self,
        caller_id: Uuid,
        image: Option<String>,
    ) -> Result<(), DomainError> {
        let user = self.user_storage.get_user_by_id(caller_id).await?;

        if user.is_oidc_user() {
            return Err(DomainError::new(
                ErrorKind::AccessDenied,
                "User",
                "Avatar is managed by your identity provider and cannot be changed here",
            ));
        }

        if let Some(ref img) = image {
            const MAX_BYTES: usize = 524_288; // 512 KiB
            if img.len() > MAX_BYTES {
                return Err(DomainError::new(
                    ErrorKind::InvalidInput,
                    "User",
                    "Image exceeds maximum allowed size (512 KiB)",
                ));
            }
            let valid = img.starts_with("https://")
                || img.starts_with("http://")
                || img.starts_with("data:image/png;base64,")
                || img.starts_with("data:image/webp;base64,")
                || img.starts_with("data:image/jpeg;base64,");
            if !valid {
                return Err(DomainError::new(
                    ErrorKind::InvalidInput,
                    "User",
                    "Image must be an https/http URL or a data URI (png, webp, jpeg)",
                ));
            }
        }

        self.user_storage
            .update_image(caller_id, image)
            .await
            .map_err(DomainError::from)?;

        Ok(())
    }

    pub async fn get_user(&self, user_id: Uuid) -> Result<UserDto, DomainError> {
        let user = self.user_storage.get_user_by_id(user_id).await?;
        Ok(UserDto::from(user))
    }

    /// Cached, image-free lookup of the caller's authorization flags
    /// (`role` / `is_external` / `active`). This is the per-request fast
    /// path for middleware guards: the full `get_user` row fetch drags the
    /// `image` column (a data URI of up to 512 KiB) across the wire, which
    /// a sync client issuing hundreds of DAV requests per minute paid on
    /// every single one just to read a boolean.
    ///
    /// Staleness is bounded by [`USER_FLAGS_CACHE_TTL`]; role and active
    /// changes made through this service invalidate the entry eagerly.
    /// Look up the session id for a refresh token string. Returns
    /// `Ok(None)` when the token doesn't match any session (typo,
    /// revoked, expired), `Err` only on real storage errors. Used by
    /// the change-password handler to identify the caller's current
    /// session so `revoke_other_user_sessions` can spare it while
    /// killing the rest.
    ///
    /// Kept as a thin lookup — this handler doesn't care about the
    /// full Session entity, only its id, so the caller doesn't have
    /// to reason about the wire shape of `Session`.
    pub async fn get_session_id_by_refresh_token(
        &self,
        refresh_token: &str,
    ) -> Result<Option<Uuid>, DomainError> {
        match self
            .session_storage
            .get_session_by_refresh_token(refresh_token)
            .await
        {
            Ok(session) => Ok(Some(session.id())),
            Err(e) if e.kind == ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Bind a DPoP JWK thumbprint to an EXISTING session — the
    /// post-redirect path for OIDC and magic-link, whose redemptions
    /// are GET requests and can't thread the thumbprint through the
    /// login body. The SPA calls this once, immediately after the
    /// redirect lands, with the thumbprint it generated at page load.
    ///
    /// Emits `auth.dpop_bind_rejected` on validation failure or when
    /// the caller tries to re-bind an already-bound session (anti-
    /// downgrade guard). Emits `auth.dpop_bound` on the accept path
    /// so operators can correlate binding events with sessions.
    pub async fn bind_dpop_jkt_to_session(
        &self,
        session_id: Uuid,
        dpop_jkt: &str,
    ) -> Result<(), DomainError> {
        let validated = validate_dpop_jkt(dpop_jkt).map_err(|e| {
            tracing::info!(
                target: "audit",
                event = "auth.dpop_bind_rejected",
                reason = "malformed_thumbprint",
                session_id = %session_id,
                "🔐 DPoP bind rejected: {}", e,
            );
            DomainError::new(
                ErrorKind::InvalidInput,
                "Auth",
                "dpop_jkt must be a 43-character base64url SHA-256 thumbprint (RFC 7638)",
            )
        })?;
        match self
            .session_storage
            .bind_dpop_jkt(session_id, &validated)
            .await
        {
            Ok(()) => {
                tracing::info!(
                    target: "audit",
                    event = "auth.dpop_bound",
                    session_id = %session_id,
                    "🔐 DPoP thumbprint bound to session",
                );
                Ok(())
            }
            Err(e) if e.kind == ErrorKind::AlreadyExists => {
                tracing::info!(
                    target: "audit",
                    event = "auth.dpop_bind_rejected",
                    reason = "already_bound",
                    session_id = %session_id,
                    "🔐 DPoP bind rejected: session already bound",
                );
                Err(e)
            }
            Err(e) => Err(e),
        }
    }

    pub async fn get_user_flags(&self, user_id: Uuid) -> Result<UserFlags, DomainError> {
        // Single-flight: concurrent misses for the same user coalesce
        // into ONE storage lookup; errors are never cached (same herd
        // shape ROUND3 fixed for basic-auth, minus the Argon2 cost).
        self.user_flags_cache
            .try_get_with(user_id, async {
                Ok::<_, DomainError>(self.user_storage.get_user_flags(user_id).await?)
            })
            .await
            // try_get_with hands back `Arc<DomainError>` shared by all
            // waiters; DomainError isn't Clone, so rebuild a fresh one
            // preserving the kind / entity / message.
            .map_err(|shared: std::sync::Arc<DomainError>| {
                DomainError::new(shared.kind, shared.entity_type, shared.message.clone())
            })
    }

    /// Apply a profile update on behalf of the calling user (PR 24).
    ///
    /// Hard rules:
    /// - **OIDC users are rejected outright (403)** — their profile
    ///   fields are owned by the IdP. Mirroring writes here would
    ///   create silent divergence.
    /// - **Username is claim-once**: present in `dto` ↔ caller's
    ///   current username must be `None`. Subsequent attempts are
    ///   rejected with 409 `UsernameImmutable`. The immutability
    ///   avoids DAV / NextCloud client breakage (paths include the
    ///   username as a stable identifier).
    /// - **Username uniqueness** is enforced on claim against other
    ///   users (`get_user_by_username`).
    /// - **Given / family names** are freely settable; passing an
    ///   empty string is rejected (use no field for "no change").
    ///
    /// The method is idempotent on no-op DTOs (all fields absent) and
    /// emits an `auth.profile_updated` audit line listing which fields
    /// changed.
    pub async fn update_profile_with_perms(
        &self,
        caller_id: Uuid,
        dto: crate::application::dtos::user_dto::UpdateProfileDto,
        locale_registry: &crate::common::locale::LocaleRegistry,
    ) -> Result<UserDto, DomainError> {
        let mut user = self.user_storage.get_user_by_id(caller_id).await?;

        if user.is_oidc_user() {
            tracing::info!(
                target: "audit",
                event = "auth.profile_update_rejected",
                reason = "oidc_user",
                caller_id = %caller_id,
                "👤 profile update rejected: caller is OIDC-managed",
            );
            return Err(DomainError::new(
                ErrorKind::AccessDenied,
                "User",
                "Your profile is managed by the identity provider and \
                 cannot be edited here. Update it at the IdP — changes \
                 will propagate on your next sign-in.",
            ));
        }

        let mut changed: Vec<&'static str> = Vec::new();

        // ── Username (claim-once) ──────────────────────────────
        if let Some(ref candidate) = dto.username {
            if user.username().is_some() {
                tracing::info!(
                    target: "audit",
                    event = "auth.profile_update_rejected",
                    reason = "username_immutable",
                    caller_id = %caller_id,
                    "👤 profile update rejected: username already claimed",
                );
                return Err(DomainError::new(
                    ErrorKind::AlreadyExists,
                    "User",
                    "Username is already claimed and cannot be changed. \
                     Contact an administrator if you need to rename.",
                ));
            }
            // Uniqueness against other users.
            if self
                .user_storage
                .get_user_by_username(candidate)
                .await
                .is_ok()
            {
                tracing::info!(
                    target: "audit",
                    event = "auth.profile_update_rejected",
                    reason = "username_taken",
                    caller_id = %caller_id,
                    attempted_username = %candidate,
                    "👤 profile update rejected: username '{}' is taken",
                    candidate,
                );
                return Err(DomainError::new(
                    ErrorKind::AlreadyExists,
                    "User",
                    format!("Username '{}' is already taken", candidate),
                ));
            }
            user.set_username(candidate.clone()).map_err(|e| {
                DomainError::new(
                    ErrorKind::InvalidInput,
                    "User",
                    format!("Invalid username: {}", e),
                )
            })?;
            changed.push("username");
        }

        // ── Given / family names ───────────────────────────────
        if let Some(ref g) = dto.given_name {
            if g.trim().is_empty() {
                return Err(DomainError::new(
                    ErrorKind::InvalidInput,
                    "User",
                    "given_name cannot be an empty string. Omit the field \
                     to leave it unchanged.",
                ));
            }
            user.set_given_name(Some(g.clone()));
            changed.push("given_name");
        }
        if let Some(ref f) = dto.family_name {
            if f.trim().is_empty() {
                return Err(DomainError::new(
                    ErrorKind::InvalidInput,
                    "User",
                    "family_name cannot be an empty string. Omit the field \
                     to leave it unchanged.",
                ));
            }
            user.set_family_name(Some(f.clone()));
            changed.push("family_name");
        }

        // ── Preferred locale ─────────────────────────────────────
        // Treat `""` as an explicit clear (frontend may send the empty
        // string when the user picks "Use server default"). Any other
        // non-empty value must resolve against the LocaleRegistry — an
        // unknown code is a 400 so the client can show the user a
        // useful error rather than silently dropping the change.
        if let Some(ref code) = dto.preferred_locale {
            let trimmed = code.trim();
            if trimmed.is_empty() {
                user.set_preferred_locale(None);
                changed.push("preferred_locale");
            } else if let Some(canonical) = locale_registry.parse(trimmed) {
                user.set_preferred_locale(Some(canonical.as_str().to_string()));
                changed.push("preferred_locale");
            } else {
                tracing::info!(
                    target: "audit",
                    event = "auth.profile_update_rejected",
                    reason = "unknown_locale",
                    caller_id = %caller_id,
                    attempted_locale = %trimmed,
                    "👤 profile update rejected: locale '{}' not in registry",
                    trimmed,
                );
                return Err(DomainError::new(
                    ErrorKind::InvalidInput,
                    "User",
                    format!(
                        "Unknown locale '{}'. Use one of the codes returned \
                         by /api/i18n/locales.",
                        trimmed,
                    ),
                ));
            }
        }

        // ── Share-notification opt-out (PR N1) ───────────────────
        // Boolean field; absent → no change. Idempotent — setting the
        // same value twice is fine but doesn't re-emit an audit row
        // because `changed` won't pick it up.
        if let Some(notify) = dto.notify_on_share
            && notify != user.notify_on_share()
        {
            user.set_notify_on_share(notify);
            changed.push("notify_on_share");
        }

        // ── UI preferences shallow-merge ──────────────────────────
        // The other fields above modify the in-memory `user` and land
        // via `update_user(user)` at the end. UI preferences take a
        // different path because the merge has to happen at write
        // time in SQL — two devices PATCH'ing partial patches
        // concurrently would otherwise race and clobber each other if
        // we did merge-then-write in application code. See
        // `UserPgRepository::update_ui_preferences` for the SQL.
        //
        // Boundary validation only: shape must be a JSON object.
        // Contents are opaque to the server — no key inspection here.
        // Size cap is enforced by the schema CHECK constraint; a
        // violating merge surfaces as a repo error.
        let ui_prefs_patch = if let Some(patch) = dto.ui_preferences.as_ref() {
            if !patch.is_object() {
                return Err(DomainError::validation_error(
                    "ui_preferences must be a JSON object".to_string(),
                ));
            }
            Some(patch.clone())
        } else {
            None
        };

        if changed.is_empty() && ui_prefs_patch.is_none() {
            // No-op — return the current user without a DB write.
            return Ok(UserDto::from(user));
        }

        // Persist the typed-field changes first (if any). Skip the
        // `update_user` call entirely when only `ui_preferences`
        // changed — the shallow-merge SQL below is authoritative for
        // that field, and running `update_user` unnecessarily would
        // rewrite every column with its current in-memory value.
        if !changed.is_empty() {
            self.user_storage.update_user(user).await?;
        }

        if let Some(patch) = ui_prefs_patch {
            self.user_storage
                .update_ui_preferences(caller_id, &patch)
                .await?;
            changed.push("ui_preferences");
        }

        tracing::info!(
            target: "audit",
            event = "auth.profile_updated",
            caller_id = %caller_id,
            fields = ?changed,
            "👤 profile updated for {}",
            caller_id,
        );

        // Refetch so the returned DTO reflects the merged JSONB bag
        // (the in-memory `user` above holds the pre-merge value).
        let refreshed = self.user_storage.get_user_by_id(caller_id).await?;
        Ok(UserDto::from(refreshed))
    }

    // Alias for consistency with handler method
    pub async fn get_user_by_id(&self, user_id: Uuid) -> Result<UserDto, DomainError> {
        self.get_user(user_id).await
    }

    /// Load the full `User` entity for the given id. Unlike
    /// `get_user_by_id` this returns the domain entity (not a DTO), so
    /// callers can read fields like `notify_on_share()`,
    /// `preferred_locale()`, or `is_external()` without round-tripping
    /// through the DTO shape. Used by `grant_handler::create_grant` to
    /// hand the granter entity to `RecipientNotificationService`.
    pub async fn get_user_entity(
        &self,
        user_id: Uuid,
    ) -> Result<crate::domain::entities::user::User, DomainError> {
        UserStoragePort::get_user_by_id(&*self.user_storage, user_id).await
    }

    /// Login-style identifier lookup: dispatches on `@` in the input
    /// (email path when present, username path when not), identical
    /// to `login()`'s dispatch. Exposed so the OPAQUE login handler
    /// (`opaque_auth_handler::login_ke1`) can resolve the same
    /// identifier shape without duplicating the `@` heuristic.
    ///
    /// Returns the raw DB error on miss — callers are responsible
    /// for the anti-enum shape (do NOT surface the DomainError kind
    /// distinction to unauthenticated clients).
    pub async fn lookup_user_for_login(
        &self,
        identifier: &str,
    ) -> Result<crate::domain::entities::user::User, DomainError> {
        if identifier.contains('@') {
            self.user_storage.get_user_by_email(identifier).await
        } else {
            self.user_storage.get_user_by_username(identifier).await
        }
    }

    /// Visibility-checked profile lookup for `GET /api/users/{id}`.
    ///
    /// Returns `NotFound` (not `AccessDenied`) when the caller has no
    /// legitimate relationship with the target — anti-enumeration: an
    /// attacker probing random UUIDs cannot distinguish "user doesn't
    /// exist" from "exists but you can't see them".
    ///
    /// Visibility rule, evaluated top-to-bottom:
    ///   1. **Self lookup** — `caller_id == target_id` always succeeds.
    ///   2. **Shared-grant relationship** — caller and target appear
    ///      together on at least one row of `storage.role_grants`,
    ///      either direction (caller-as-granter / target-as-subject,
    ///      or target-as-granter / caller-as-subject). Applies to both
    ///      internal and external callers. This is what lets an
    ///      external user resolve the display name + photo of the
    ///      internal user who shared a folder with them — the
    ///      `granted_by` column on the grant Bob received is Alice's
    ///      user_id, and SharedWithMe needs to render her vignette.
    ///   3. **External callers stop here.** Any remaining check would
    ///      let them enumerate the user directory; they have no
    ///      legitimate need beyond resolving people they're already in
    ///      a grant relationship with.
    ///   4. *(Internal callers only)* Target is internal AND
    ///      `expose_system_users` is on → already broadly visible via
    ///      the system address book; no extra check.
    ///   5. *(Internal callers only)* Caller is admin → always visible.
    ///   6. Anything else → `NotFound`.
    ///
    /// Subject-group co-membership is intentionally NOT a visibility
    /// path in v1; can be added later if a concrete need surfaces.
    pub async fn get_user_profile(
        &self,
        caller_id: Uuid,
        target_id: Uuid,
        expose_system_users: bool,
        pool: &sqlx::PgPool,
    ) -> Result<UserDto, DomainError> {
        // (1) Self — a single fetch suffices (the check compares the input
        // UUIDs, so the target read is never needed on this path).
        if caller_id == target_id {
            let caller = self.user_storage.get_user_by_id(caller_id).await?;
            return Ok(UserDto::from(caller));
        }

        // Caller and target are independent point reads (the self-case already
        // returned; the branch above compares input UUIDs, not fetched data) —
        // overlap them with `join!` instead of two serial round-trips.
        // `caller_res?` first preserves the caller-error precedence of the old
        // sequential form. (benches/ROUND23.md §P1)
        let (caller_res, target_res) = tokio::join!(
            self.user_storage.get_user_by_id(caller_id),
            self.user_storage.get_user_by_id(target_id)
        );
        let caller = caller_res?;

        // Anti-enumeration: NotFound for everything that doesn't pass.
        // Convert a real NotFound on `target` to the same anonymous 404,
        // so existence isn't leaked through differential responses.
        let target = match target_res {
            Ok(u) => u,
            Err(e) if e.kind == ErrorKind::NotFound => {
                tracing::info!(
                    target: "audit",
                    event = "user_profile.rejected",
                    reason = "target_not_found",
                    caller_id = %caller_id,
                    caller_is_external = caller.is_external(),
                    target_id = %target_id,
                    "👮🏻‍♂️ user-profile rejected: target '{}' does not exist (caller {})",
                    target_id,
                    caller_id,
                );
                return Err(DomainError::new(
                    ErrorKind::NotFound,
                    "User",
                    "User not found",
                ));
            }
            Err(e) => return Err(e),
        };

        // (2) Shared-grant relationship — works for both internal and
        // external callers. LIMIT 1 + the (granted_by) and
        // (subject_type, subject_id) indexes keep this cheap.
        let related: Option<i32> = sqlx::query_scalar(
            r#"
            SELECT 1
              FROM storage.role_grants
             WHERE (granted_by = $1 AND subject_type = 'user' AND subject_id = $2)
                OR (granted_by = $2 AND subject_type = 'user' AND subject_id = $1)
             LIMIT 1
            "#,
        )
        .bind(caller_id)
        .bind(target_id)
        .fetch_optional(pool)
        .await
        .map_err(|e| {
            DomainError::internal_error("UserProfile", format!("visibility query: {}", e))
        })?;

        if related.is_some() {
            return Ok(UserDto::from(target));
        }

        // (3) External callers stop here — no directory enumeration.
        if caller.is_external() {
            // Audit: an external user tried to look up someone they
            // don't share a grant with. Surfaces enumeration probes
            // from compromised magic-link sessions.
            tracing::info!(
                target: "audit",
                event = "user_profile.rejected",
                reason = "external_caller_no_relationship",
                caller_id = %caller_id,
                target_id = %target_id,
                target_is_external = target.is_external(),
                "👮🏻‍♂️ user-profile rejected: external user '{}' has no grant relationship with '{}'",
                caller_id,
                target_id,
            );
            return Err(DomainError::new(
                ErrorKind::NotFound,
                "User",
                "User not found",
            ));
        }

        // (4) Internal target + system-address-book exposed: already public.
        if !target.is_external() && expose_system_users {
            return Ok(UserDto::from(target));
        }

        // (5) Admin caller: always visible.
        if caller.role() == UserRole::Admin {
            return Ok(UserDto::from(target));
        }

        // (6) No relationship — anti-enumeration NotFound.
        // Audit: an internal user with no visibility path probed a user
        // they don't share with. Usually benign (stale UI state), but
        // recurring patterns from the same caller are worth surfacing.
        tracing::info!(
            target: "audit",
            event = "user_profile.rejected",
            reason = "no_visibility_path",
            caller_id = %caller_id,
            target_id = %target_id,
            target_is_external = target.is_external(),
            "👮🏻‍♂️ user-profile rejected: internal user '{}' has no visibility on '{}' (target is_external={})",
            caller_id,
            target_id,
            target.is_external(),
        );
        Err(DomainError::new(
            ErrorKind::NotFound,
            "User",
            "User not found",
        ))
    }

    /// Username-keyed sibling of [`Self::get_user_profile`], routing every
    /// lookup through the same visibility check as the user-profile REST
    /// endpoint. Preserves the anti-enum shape end-to-end: whether the
    /// username doesn't exist OR the caller has no visibility path, the
    /// response is `NotFound`.
    ///
    /// AuthZ audit #11 (2026-07-12): NextCloud OCS user-provisioning
    /// (`nextcloud/ocs_handler.rs::user_provisioning_response`) used to
    /// resolve `userid` via bare `get_user_by_username`, gated only by a
    /// bespoke `caller.role == "admin"` shortcut. Admins bypassed the
    /// `expose_system_users` gate; non-admins got a `403 Insufficient
    /// privileges` for any cross-user probe (leaking existence via the
    /// differential vs a genuine 404); zero audit lines. This wrapper
    /// closes all three.
    ///
    /// The username→id resolution happens here so the target isn't
    /// leaked through the audit line as a plaintext username on failure:
    /// the `target_username_not_found` event carries the string
    /// (unavoidable — we resolved it, we log it), but every other
    /// downstream event keys off `target_id` after resolution, matching
    /// the id-based endpoint.
    pub async fn get_user_profile_by_username_with_perms(
        &self,
        caller_id: Uuid,
        username: &str,
        expose_system_users: bool,
        pool: &sqlx::PgPool,
    ) -> Result<UserDto, DomainError> {
        let target = match self.user_storage.get_user_by_username(username).await {
            Ok(u) => u,
            Err(e) if e.kind == ErrorKind::NotFound => {
                tracing::info!(
                    target: "audit",
                    event = "user_profile.rejected",
                    reason = "target_username_not_found",
                    caller_id = %caller_id,
                    target_username = %username,
                    "👮🏻‍♂️ user-profile rejected: username '{}' does not exist (caller {})",
                    username,
                    caller_id,
                );
                return Err(DomainError::new(
                    ErrorKind::NotFound,
                    "User",
                    "User not found",
                ));
            }
            Err(e) => return Err(e),
        };
        self.get_user_profile(caller_id, target.id(), expose_system_users, pool)
            .await
    }

    // New method to get user by username - needed for admin user handling
    pub async fn get_user_by_username(&self, username: &str) -> Result<UserDto, DomainError> {
        let user = self.user_storage.get_user_by_username(username).await?;
        Ok(UserDto::from(user))
    }

    // Method to count how many admin users exist in the system
    // Used to determine if we have multiple admins or just the default one
    pub async fn count_admin_users(&self) -> Result<i64, DomainError> {
        // Scalar COUNT(*) — the old form fetched every admin's FULL row (incl.
        // the up-to-512 KiB avatar `image` + `ui_preferences` JSONB) only to
        // call `.len()`, on a status/init endpoint that is polled at bootstrap
        // (benches/ROUND29.md §G).
        self.user_storage.count_users_by_role("admin").await
    }

    /// Lists internal users only. External (grant-only) users are filtered
    /// out so that internal-user surfaces — system address book, OCS
    /// sharee search, etc. — never expose external identities. Admin
    /// surfaces that need the full list should call
    /// [`list_users_including_external_with_perms`] instead.
    pub async fn list_users(&self, limit: i64, offset: i64) -> Result<Vec<UserDto>, DomainError> {
        let users = self.user_storage.list_users(limit, offset, false).await?;
        Ok(users.into_iter().map(UserDto::from).collect())
    }

    /// Admin-only: lists users including external (grant-only) recipients.
    /// Used by the admin user-management UI.
    pub async fn list_users_including_external_with_perms<A: AuthorizationEngine>(
        &self,
        authorization: &A,
        caller_id: Uuid,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<UserDto>, DomainError> {
        self.require_admin_caller(authorization, caller_id).await?;
        let users = self.user_storage.list_users(limit, offset, true).await?;
        Ok(users.into_iter().map(UserDto::from).collect())
    }

    /// Admin-only compact listing.  The detail endpoint retains the complete
    /// [`UserDto`]; this path projects only what the management table renders so
    /// PostgreSQL never detoasts or transfers avatars/preferences for a page.
    pub async fn list_user_summaries_including_external_with_perms<A: AuthorizationEngine>(
        &self,
        authorization: &A,
        caller_id: Uuid,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<AdminUserSummaryDto>, DomainError> {
        self.require_admin_caller(authorization, caller_id).await?;
        let users = self
            .user_storage
            .list_user_summaries(limit, offset, true)
            .await?;
        Ok(users.into_iter().map(AdminUserSummaryDto::from).collect())
    }

    /// Service-layer gate for administrator-scoped user-directory operations.
    /// The route middleware remains a cheap first line of defence, but the
    /// application service is authoritative so alternate callers cannot bypass
    /// policy.  The lookup is the existing single-flight, image-free flags
    /// cache; a hot authorization check does not hydrate the user profile.
    async fn require_admin_caller<A: AuthorizationEngine>(
        &self,
        authorization: &A,
        caller_id: Uuid,
    ) -> Result<(), DomainError> {
        let flags = self.get_user_flags(caller_id).await?;
        authorization.require_system_admin(
            Subject::User(caller_id),
            flags.role,
            flags.is_external,
            flags.active,
        )
    }

    /// Searches internal users only. See [`list_users`] for the rationale.
    pub async fn search_users(&self, query: &str, limit: i64) -> Result<Vec<UserDto>, DomainError> {
        let users = self.user_storage.search_users(query, limit, false).await?;
        Ok(users.into_iter().map(UserDto::from).collect())
    }

    /// Username-only search for the NC sharee autocomplete: identical
    /// predicate / order / limit to [`search_users`], but the repository
    /// projects just `username` — no 21-column hydration (incl. the
    /// up-to-512 KiB avatar `image`) per matched row, per keystroke
    /// (benches/ROUND12.md §1). NULL usernames (email-only signups) are
    /// filtered app-side, exactly like the wide flow's post-limit filter.
    pub async fn search_sharee_usernames(
        &self,
        query: &str,
        limit: i64,
    ) -> Result<Vec<String>, DomainError> {
        let names = self
            .user_storage
            .search_usernames(query, limit, false)
            .await?;
        Ok(names.into_iter().flatten().collect())
    }

    // ========================================================================
    // Admin Session Management Methods
    // ========================================================================
    //
    // AuthZ posture: /api/admin/* is already protected by a
    // `require_admin` router layer (see
    // `interfaces/api/routes.rs::admin_router`) — but every admin
    // method here still calls `require_admin_caller` as a
    // defense-in-depth check, matching the pattern
    // `list_users_including_external_with_perms` established. If a
    // handler is ever wired outside the /admin subtree, the AuthZ
    // still holds.

    /// List sessions for the admin panel. `user_id_filter = Some(uuid)`
    /// narrows to one user; `None` returns cross-user. `include_revoked`
    /// controls whether to show revoked / expired rows — default UX
    /// hides them (checkbox to opt in for forensics).
    pub async fn admin_list_sessions_with_perms<A: AuthorizationEngine>(
        &self,
        authorization: &A,
        caller: crate::application::dtos::session_dto::SessionCaller<'_>,
        user_id_filter: Option<Uuid>,
        include_revoked: bool,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<crate::application::dtos::session_dto::SessionSummaryDto>, DomainError> {
        self.require_admin_caller(authorization, caller.id).await?;
        let sessions = self
            .session_storage
            .list_sessions_paginated(user_id_filter, include_revoked, limit, offset)
            .await?;
        Ok(sessions
            .into_iter()
            .map(|s| {
                crate::application::dtos::session_dto::SessionSummaryDto::from_session(
                    s,
                    caller.dpop_jkt,
                )
            })
            .collect())
    }

    /// Admin-driven session revocation. Sets `revoked = true` — the
    /// row remains for audit visibility, but its refresh token is
    /// dead and the next access-token refresh 401s naturally.
    ///
    /// Emits an audit line + counter increment so operators can trace
    /// who killed which session and when.
    pub async fn admin_revoke_session_with_perms<A: AuthorizationEngine>(
        &self,
        authorization: &A,
        caller: crate::application::dtos::session_dto::SessionCaller<'_>,
        session_id: Uuid,
    ) -> Result<(), DomainError> {
        self.require_admin_caller(authorization, caller.id).await?;
        // Resolve target user for the audit line before revocation —
        // once the session row is revoked the user_id is still readable
        // but the ORDER is stable this way.
        let target_user_id = self
            .session_storage
            .get_session_by_id(session_id)
            .await
            .ok()
            .map(|s| s.user_id());
        self.session_storage.revoke_session(session_id).await?;
        tracing::info!(
            target: "audit",
            event = "admin.session_revoked",
            caller_id = %caller.id,
            session_id = %session_id,
            target_user_id = target_user_id.map(|u| u.to_string()).unwrap_or_default(),
            "👮🏻‍♂️ Admin revoked session",
        );
        metrics::counter!("oxicloud_admin_session_revoked_total").increment(1);
        Ok(())
    }

    // ========================================================================
    // Admin User Management Methods
    // ========================================================================

    /// Admin-only: create a user bypassing registration guards.
    pub async fn admin_create_user(
        &self,
        dto: crate::application::dtos::settings_dto::AdminCreateUserDto,
    ) -> Result<UserDto, DomainError> {
        // Validate username length
        if dto.username.len() < 3 || dto.username.len() > 254 {
            return Err(DomainError::new(
                ErrorKind::InvalidInput,
                "User",
                "Username must be between 3 and 254 characters".to_string(),
            ));
        }

        // Check for duplicate username
        if self
            .user_storage
            .get_user_by_username(&dto.username)
            .await
            .is_ok()
        {
            return Err(DomainError::new(
                ErrorKind::AlreadyExists,
                "User",
                format!("User '{}' already exists", dto.username),
            ));
        }

        // Email: use provided or generate placeholder
        let email = dto
            .email
            .filter(|e| !e.trim().is_empty())
            .unwrap_or_else(|| format!("{}@oxicloud.local", dto.username));

        // Check email uniqueness
        if self.user_storage.get_user_by_email(&email).await.is_ok() {
            return Err(DomainError::new(
                ErrorKind::AlreadyExists,
                "User",
                format!("Email '{}' is already registered", email),
            ));
        }

        // Validate password
        if dto.password.len() < 8 {
            return Err(DomainError::new(
                ErrorKind::InvalidInput,
                "User",
                "Password must be at least 8 characters long".to_string(),
            ));
        }

        // Determine role
        let role = match dto.role.as_deref() {
            Some("admin") => UserRole::Admin,
            _ => UserRole::User,
        };

        let is_external = dto.is_external.unwrap_or(false);

        // Forbid external + admin combo. The DB `users_external_not_admin`
        // CHECK constraint would catch this too, but a 400 with an
        // explanatory message is friendlier than a generic 500 from a
        // constraint violation. See the CHECK definition in
        // migrations/20260612000002_auth_users_is_external.sql for the
        // rationale.
        if is_external && matches!(role, UserRole::Admin) {
            return Err(DomainError::new(
                ErrorKind::InvalidInput,
                "User",
                "External users cannot be admins. To promote an external user to admin, \
                 first convert them to internal (set is_external = false), then update \
                 the role separately."
                    .to_string(),
            ));
        }

        // External users never own storage. The DB `users_external_no_storage`
        // CHECK constraint enforces this; setting quota=0 here keeps the
        // domain consistent and matches `User::new(..., is_external = true)`.
        let quota = if is_external {
            0
        } else {
            dto.quota_bytes.unwrap_or_else(|| self.capped_quota(&role))
        };

        // Hash password (kept for both internal and external users — for
        // external users it's currently unused since they authenticate via
        // magic-link / OIDC, but the DB column is NOT NULL).
        let password_hash = self.password_hasher.hash_password(&dto.password).await?;

        // Create domain entity. External users are created with
        // is_external=true and role forced to User (the admin+external
        // combo was rejected above). For external users the supplied
        // password hash is persisted so the audit trail is preserved,
        // even though they authenticate via magic-link / OIDC.
        let user = if is_external {
            User::new(
                email,
                Some(dto.username.clone()),
                Some(password_hash),
                None, // federation_kind: admin-created external, no federation link yet
                None, // federation_issuer
                None, // federation_subject
                UserRole::User,
                0,
                true,
            )
        } else {
            User::new(
                email,
                Some(dto.username.clone()),
                Some(password_hash),
                None, // federation_kind: admin-created local user
                None, // federation_issuer
                None, // federation_subject
                role,
                quota,
                false,
            )
        }
        .map_err(|e| {
            DomainError::new(
                ErrorKind::InvalidInput,
                "User",
                format!("Error creating user: {}", e),
            )
        })?;

        // Admin fiat counts as verification. When
        // `OXICLOUD_REQUIRE_VERIFIED_EMAIL` is set, admin-created users
        // still get to log in without a magic-link round-trip — the
        // operator explicitly vouched for the address at creation. This
        // mirrors the OIDC-JIT convention (see `redeem_pending_oidc_token`
        // and `login_oidc_callback` which also stamp
        // `email_verified_at` on first sight).
        let mut user = user;
        user.mark_email_verified();

        // Persist
        let created = self.user_storage.create_user(user).await?;

        // Deactivate if requested (User::new always sets active=true)
        if let Some(false) = dto.active {
            self.user_storage
                .set_user_active_status(created.id(), false)
                .await?;
        }

        // Lifecycle: PersonalDriveLifecycleHook handles the home-folder
        // provisioning (idempotent + short-circuits on is_external).
        // Audit logs the creation event.
        if let Some(lc) = &self.user_lifecycle {
            lc.dispatch_created(&created).await;
        }

        tracing::info!(
            "Admin created user: {} ({}, is_external={})",
            dto.username,
            created.id(),
            created.is_external()
        );
        Ok(UserDto::from(created))
    }

    /// Admin-only: reset a user's password.
    pub async fn admin_reset_password(
        &self,
        user_id: Uuid,
        new_password: &str,
    ) -> Result<(), DomainError> {
        // Block password reset for OIDC-provisioned users
        let user = self.user_storage.get_user_by_id(user_id).await?;
        if user.is_oidc_user() {
            return Err(DomainError::new(
                ErrorKind::InvalidInput,
                "Auth",
                "Cannot reset password for SSO/OIDC accounts. The user's password is managed by their identity provider.",
            ));
        }

        if new_password.len() < 8 {
            return Err(DomainError::new(
                ErrorKind::InvalidInput,
                "User",
                "Password must be at least 8 characters long".to_string(),
            ));
        }
        let hash = self.password_hasher.hash_password(new_password).await?;
        self.user_storage.change_password(user_id, &hash).await?;

        // Mark the admin-picked password as temporary so the user gets
        // prompted to pick their own on next login. Two branches:
        //
        //   * OPAQUE wired: `clear_registration` is the atomic write
        //     that (a) NULLs the OPAQUE envelope + migration mark so
        //     the migrated user drops back to legacy login (the old
        //     envelope is bound to the OLD passphrase and would fail
        //     OPAQUE KE3), and (b) sets `force_password_change`.
        //     Silent-migration on the next legacy login re-mints a
        //     fresh envelope bound to the admin's new password; the
        //     force flag then routes the SPA to change-password.
        //
        //   * OPAQUE off: no envelope to invalidate; just flip the
        //     force flag directly via user_storage. Same downstream
        //     behaviour — SPA sees force_password_change=true on
        //     the next login response and routes accordingly.
        //
        // Both writes are non-fatal (logged at warn on failure): the
        // password reset itself succeeded, and a stale force flag is
        // recoverable on the next admin reset.
        if let Some(opaque) = self.opaque_repo.as_ref() {
            if let Err(e) = opaque.clear_registration(user_id).await {
                tracing::warn!(
                    target: "audit",
                    event = "auth.admin_reset_opaque_clear_failed",
                    user_id = %user_id,
                    error = %e,
                    "OPAQUE clear_registration failed during admin password reset — \
                     force flag + envelope invalidation deferred to next opportunity"
                );
            }
        } else if let Err(e) = self.user_storage.set_force_password_change(user_id).await {
            tracing::warn!(
                target: "audit",
                event = "auth.admin_reset_force_flag_failed",
                user_id = %user_id,
                error = %e,
                "set_force_password_change failed during admin password reset — \
                 user will not be prompted to change from admin's temp password"
            );
        }

        // Invalidate all existing sessions so the user must re-login
        // with the new password.  Mirrors the behaviour of change_password().
        self.session_storage
            .revoke_all_user_sessions(user_id)
            .await?;

        // Evict the cached UserFlags row so the next authenticated
        // request from this user (on their next session) sees the
        // updated force_password_change value without waiting for
        // the 30s TTL. The middleware
        // `require_no_password_change_pending` reads from this cache
        // — a stale FALSE would keep the API open to the admin's
        // temp-password holder until the TTL rolled over.
        self.user_flags_cache.invalidate(&user_id).await;

        tracing::info!(
            target: "audit",
            event = "auth.admin_reset_password",
            user_id = %user_id,
            opaque_wired = self.opaque_repo.is_some(),
            "👮🏻‍♂️ Admin reset password — sessions revoked, force-change flag set"
        );
        Ok(())
    }

    /// Get a single user by ID (for admin panel)
    pub async fn get_user_admin(&self, user_id: Uuid) -> Result<UserDto, DomainError> {
        let user = self.user_storage.get_user_by_id(user_id).await?;
        Ok(UserDto::from(user))
    }

    /// Delete a user by ID (admin only).
    ///
    /// Runs the whole flow in a single transaction so the lifecycle
    /// hooks (`SessionRevocationLifecycleHook` revoking sessions with
    /// audit, `AuthzCacheLifecycleHook` invalidating the Moka cache,
    /// `PersonalDriveLifecycleHook` for future trash policy, …) can do
    /// their work atomically with the user DELETE. If any hook returns
    /// `Err`, the transaction rolls back and the user remains intact.
    pub async fn delete_user_admin(&self, user_id: Uuid) -> Result<(), DomainError> {
        let user = self.user_storage.get_user_by_id(user_id).await?;
        tracing::info!(
            "Admin deleting user: {} ({})",
            user.display_for_audit(),
            user_id
        );

        let mut tx = self
            .user_storage
            .pool()
            .begin()
            .await
            .map_err(|e| DomainError::internal_error("Auth", format!("begin tx: {}", e)))?;

        // Hooks run inside the tx, BEFORE the user DELETE. They see the
        // row still present and can write cleanup queries against the
        // same tx (e.g. session revocation with per-session audit).
        if let Some(lc) = &self.user_lifecycle {
            lc.dispatch_deleted(&user, DeletionMode::AdminDelete, &mut tx)
                .await?;
        }

        // Now the DELETE — FK CASCADE handles the downstream cleanup
        // (sessions, folders, files, …) for anything the hooks didn't
        // explicitly remove.
        sqlx::query("DELETE FROM auth.users WHERE id = $1")
            .bind(user_id)
            .execute(&mut *tx)
            .await
            .map_err(|e| DomainError::internal_error("Auth", format!("delete user: {}", e)))?;

        tx.commit()
            .await
            .map_err(|e| DomainError::internal_error("Auth", format!("commit: {}", e)))?;

        Ok(())
    }

    /// Activate or deactivate a user (admin only)
    pub async fn set_user_active(&self, user_id: Uuid, active: bool) -> Result<(), DomainError> {
        self.user_storage
            .set_user_active_status(user_id, active)
            .await?;
        self.user_flags_cache.invalidate(&user_id).await;
        Ok(())
    }

    /// Change user role (admin only).
    ///
    /// Refuses `role = "admin"` when the target is external (grant-only).
    /// The DB CHECK `users_external_not_admin` would also refuse this at
    /// COMMIT, but surfacing it here yields a clean `InvalidInput` error
    /// with an audit line naming the reason, instead of a bare
    /// constraint-violation stringified out of Postgres.
    pub async fn change_user_role(&self, user_id: Uuid, role: &str) -> Result<(), DomainError> {
        if role != "admin" && role != "user" {
            return Err(DomainError::new(
                ErrorKind::InvalidInput,
                "User",
                format!("Invalid role: {}. Must be 'admin' or 'user'", role),
            ));
        }

        if role == "admin" {
            let target = self.user_storage.get_user_by_id(user_id).await?;
            if target.is_external() {
                tracing::info!(
                    target: "audit",
                    event = "user.role_change_rejected",
                    reason = "external_cannot_be_admin",
                    target_id = %user_id,
                    "👮🏻‍♂️ role change refused: external users cannot hold the admin role",
                );
                return Err(DomainError::new(
                    ErrorKind::InvalidInput,
                    "User",
                    "External accounts cannot hold the admin role. Promote the user to internal first.",
                ));
            }
        }

        self.user_storage.change_role(user_id, role).await?;
        self.user_flags_cache.invalidate(&user_id).await;
        Ok(())
    }

    /// Update user's storage quota (admin only)
    pub async fn update_user_quota(
        &self,
        user_id: Uuid,
        quota_bytes: i64,
    ) -> Result<(), DomainError> {
        if quota_bytes < 0 {
            return Err(DomainError::new(
                ErrorKind::InvalidInput,
                "User",
                "Quota must be non-negative".to_string(),
            ));
        }
        self.user_storage
            .update_storage_quota(user_id, quota_bytes)
            .await
    }

    /// Check if a user has enough quota for an upload of the given size
    pub async fn check_quota(
        &self,
        user_id: Uuid,
        additional_bytes: i64,
    ) -> Result<bool, DomainError> {
        let user = self.user_storage.get_user_by_id(user_id).await?;
        let quota = user.storage_quota_bytes();
        if quota <= 0 {
            // 0 or negative means unlimited
            return Ok(true);
        }
        Ok(user.storage_used_bytes() + additional_bytes <= quota)
    }

    /// Count users efficiently
    pub async fn count_users_efficient(&self) -> Result<i64, DomainError> {
        self.user_storage.count_users().await
    }

    // ========================================================================
    // OIDC Methods
    // ========================================================================

    /// Prepare the OIDC authorization flow: generates CSRF state, PKCE pair,
    /// nonce, stores them in pending_oidc_flows, and returns the authorize URL.
    pub async fn prepare_oidc_authorize(&self) -> Result<String, DomainError> {
        let oidc = self.oidc_service().ok_or_else(|| {
            DomainError::new(
                ErrorKind::InternalError,
                "OIDC",
                "OIDC service not configured",
            )
        })?;

        // Generate CSRF state token
        use rand_core::{OsRng, RngCore};
        let mut state_bytes = [0u8; 32];
        OsRng.fill_bytes(&mut state_bytes);
        let state_token = hex::encode(state_bytes);

        // Generate nonce for ID token binding
        let mut nonce_bytes = [0u8; 32];
        OsRng.fill_bytes(&mut nonce_bytes);
        let nonce = hex::encode(nonce_bytes);

        // Generate PKCE pair (RFC 7636, S256)
        let mut verifier_bytes = [0u8; 32];
        OsRng.fill_bytes(&mut verifier_bytes);
        let pkce_verifier = base64_url_encode(&verifier_bytes);
        let pkce_challenge = {
            use sha2::{Digest, Sha256};
            let hash = Sha256::digest(pkce_verifier.as_bytes());
            base64_url_encode(&hash)
        };

        // Store pending flow (auto-expires after 10 min via moka TTL)
        self.pending_oidc_flows.insert(
            state_token.clone(),
            PendingOidcFlow {
                pkce_verifier,
                nonce: nonce.clone(),
                nc_flow_token: None,
                intent: FlowIntent::Login,
            },
        );

        // Build authorization URL with state, nonce, and PKCE challenge
        let authorize_url = oidc
            .get_authorize_url(&state_token, &nonce, &pkce_challenge)
            .await?;

        tracing::info!(
            "OIDC authorize flow prepared (state={}...)",
            &state_token[..8]
        );

        Ok(authorize_url)
    }

    /// Prepare an OIDC authorize flow for the SELF-SERVICE LINK path.
    /// Same PKCE + nonce dance as `prepare_oidc_authorize`, but the
    /// pending-flow entry carries `FlowIntent::Link { user_id }` so the
    /// callback branches to the link handler instead of the login one.
    ///
    /// The caller MUST have already authenticated the user (this method
    /// takes user_id from the current session context). See
    /// docs/plan/oidc-account-linking.md § UX flow — link.
    pub async fn prepare_oidc_link(&self, user_id: Uuid) -> Result<String, DomainError> {
        let oidc = self.oidc_service().ok_or_else(|| {
            DomainError::new(
                ErrorKind::InternalError,
                "OIDC",
                "OIDC service not configured",
            )
        })?;

        // Anti-scope-creep pre-check: refuse if the user is already
        // linked. Callers get an immediate error rather than round-
        // tripping through the IdP just to be refused at callback time.
        // (The callback still re-checks — this is a UX shortcut, not
        // the source of truth.)
        let user = self.user_storage.get_user_by_id(user_id).await?;
        if user.federation_kind().is_some() {
            return Err(DomainError::new(
                ErrorKind::AlreadyExists,
                "Federation",
                "This user is already linked to a federation identity. \
                 Unlink first before re-linking.",
            ));
        }

        use rand_core::{OsRng, RngCore};
        use sha2::{Digest, Sha256};

        let mut state_bytes = [0u8; 32];
        OsRng.fill_bytes(&mut state_bytes);
        let state_token = hex::encode(state_bytes);

        let mut nonce_bytes = [0u8; 32];
        OsRng.fill_bytes(&mut nonce_bytes);
        let nonce = hex::encode(nonce_bytes);

        let mut verifier_bytes = [0u8; 32];
        OsRng.fill_bytes(&mut verifier_bytes);
        let pkce_verifier = base64_url_encode(&verifier_bytes);
        let pkce_challenge = {
            let hash = Sha256::digest(pkce_verifier.as_bytes());
            base64_url_encode(&hash)
        };

        self.pending_oidc_flows.insert(
            state_token.clone(),
            PendingOidcFlow {
                pkce_verifier,
                nonce: nonce.clone(),
                nc_flow_token: None,
                intent: FlowIntent::Link { user_id },
            },
        );

        let authorize_url = oidc
            .get_authorize_url(&state_token, &nonce, &pkce_challenge)
            .await?;

        tracing::info!(
            target: "audit",
            event = "federation.link_started",
            user_id = %user_id,
            "🔗 self-service OIDC link flow initiated"
        );

        Ok(authorize_url)
    }

    /// Detach the current OIDC identity from a user. Refuses if the
    /// user has no other authentication credential — otherwise the
    /// user would lock themselves out of their own account.
    ///
    /// "Other credential" = local password OR OPAQUE envelope on file.
    /// Magic-link doesn't count as a safe fallback: the OIDC-master
    /// rule refuses magic-link for OIDC-linked users, so its behavior
    /// FLIPS after unlink, creating surprise; and it depends on SMTP
    /// wiring which may not be present. See
    /// docs/plan/oidc-account-linking.md § Unlink refusal.
    /// Run the safety checks + UPDATE for the self-service link flow.
    /// Called from `oidc_callback` when `FlowIntent::Link { user_id }`
    /// was set at flow-start time. Returns `OidcCallbackResult` variants
    /// that the handler translates to a redirect (LinkCompleted →
    /// `/profile?linked=1`, LinkRefused → `/profile?link_error=<key>`).
    ///
    /// Safety checks (all refusals are wire-visible as `link_error=`):
    /// - Session valid — target user exists (state's user_id points to
    ///   a real row). If not, `session_expired`.
    /// - IdP provided an email — else `email_not_provided`.
    /// - Emails match under normalize_email_for_link — else
    ///   `email_mismatch`.
    /// - Identity `(kind, iss, sub)` not already linked to a DIFFERENT
    ///   user — else `already_linked_elsewhere`.
    /// - Current user isn't already linked to a DIFFERENT identity —
    ///   else `already_linked`. Same identity → idempotent success.
    async fn complete_oidc_link(
        &self,
        user_id: Uuid,
        claims: &OidcIdClaims,
    ) -> Result<OidcCallbackResult, DomainError> {
        use crate::common::text::normalize_email_for_link;

        // 1. Session validity — the target user must still exist.
        let user = match self.user_storage.get_user_by_id(user_id).await {
            Ok(u) => u,
            Err(_) => {
                tracing::info!(
                    target: "audit",
                    event = "federation.link_refused",
                    user_id = %user_id,
                    reason = "session_expired",
                    "🔗 link refused — target user not found (session may have ended)",
                );
                return Ok(OidcCallbackResult::LinkRefused {
                    reason: "session_expired",
                });
            }
        };

        // 2. IdP must provide an email — without it we can't verify
        //    ownership.
        let idp_email = match claims.email.as_ref() {
            Some(e) => e,
            None => {
                tracing::info!(
                    target: "audit",
                    event = "federation.link_refused",
                    user_id = %user_id,
                    reason = "email_not_provided",
                    "🔗 link refused — IdP did not return an email claim",
                );
                return Ok(OidcCallbackResult::LinkRefused {
                    reason: "email_not_provided",
                });
            }
        };

        // 3. Email match under +alias normalization.
        if normalize_email_for_link(idp_email) != normalize_email_for_link(user.email()) {
            tracing::info!(
                target: "audit",
                event = "federation.link_refused",
                user_id = %user_id,
                reason = "email_mismatch",
                oxicloud_email_normalized = %normalize_email_for_link(user.email()),
                idp_email_normalized = %normalize_email_for_link(idp_email),
                "🔗 link refused — IdP email doesn't match OxiCloud user email",
            );
            return Ok(OidcCallbackResult::LinkRefused {
                reason: "email_mismatch",
            });
        }

        // 4. Idempotent-if-same / refuse-if-different: check the current
        //    user's link state before we touch it.
        match (
            user.federation_kind(),
            user.federation_issuer(),
            user.federation_subject(),
        ) {
            (None, None, None) => {
                // Fresh — proceed to link.
            }
            (Some(kind), Some(iss), Some(sub))
                if kind.as_str() == "oidc" && iss == claims.iss && sub == claims.sub =>
            {
                // Same identity → idempotent no-op success.
                tracing::info!(
                    target: "audit",
                    event = "federation.link_completed",
                    user_id = %user_id,
                    reason = "idempotent_repeat",
                    federation_issuer = %claims.iss,
                    federation_subject = %claims.sub,
                    "🔗 link no-op — user already linked to this same identity",
                );
                return Ok(OidcCallbackResult::LinkCompleted { user_id });
            }
            _ => {
                tracing::info!(
                    target: "audit",
                    event = "federation.link_refused",
                    user_id = %user_id,
                    reason = "already_linked",
                    "🔗 link refused — user already linked to a different identity; unlink first",
                );
                return Ok(OidcCallbackResult::LinkRefused {
                    reason: "already_linked",
                });
            }
        }

        // 5. Identity not already linked to a DIFFERENT user. The
        //    UNIQUE(kind, issuer, subject) index would catch this at
        //    UPDATE time via link_federation_identity's AlreadyExists
        //    error, but we pre-check to emit a clean audit line and
        //    avoid the "AlreadyExists on user" confusion in the
        //    downstream error mapping.
        if let Ok(other) = self
            .user_storage
            .get_user_by_federation_subject(&claims.iss, &claims.sub)
            .await
            && other.id() != user_id
        {
            tracing::info!(
                target: "audit",
                event = "federation.link_refused",
                user_id = %user_id,
                other_user_id = %other.id(),
                reason = "already_linked_elsewhere",
                "🔗 link refused — this OIDC identity is already linked to a different OxiCloud user",
            );
            return Ok(OidcCallbackResult::LinkRefused {
                reason: "already_linked_elsewhere",
            });
        }

        // All checks passed — commit the link.
        self.user_storage
            .link_federation_identity(user_id, "oidc", &claims.iss, &claims.sub)
            .await?;

        tracing::info!(
            target: "audit",
            event = "federation.link_completed",
            user_id = %user_id,
            federation_kind = "oidc",
            federation_issuer = %claims.iss,
            federation_subject = %claims.sub,
            "🔗 self-service OIDC link completed",
        );

        Ok(OidcCallbackResult::LinkCompleted { user_id })
    }

    pub async fn unlink_oidc(&self, user_id: Uuid) -> Result<(), DomainError> {
        let user = self.user_storage.get_user_by_id(user_id).await?;

        // Idempotent: unlinking an already-unlinked user is a success.
        if user.federation_kind().is_none() {
            tracing::info!(
                target: "audit",
                event = "federation.unlinked",
                user_id = %user_id,
                already_unlinked = true,
                "🔗 unlink no-op — user was not linked"
            );
            return Ok(());
        }

        // The guard. `has_password` reads password_hash.is_some();
        // `opaque_registered` needs a separate lookup because the User
        // entity doesn't carry that flag today. We do that as a
        // targeted query rather than dragging the full opaque_envelope
        // column across the wire.
        let opaque_registered = self.user_storage.is_opaque_registered(user_id).await?;
        if !user.has_password() && !opaque_registered {
            tracing::info!(
                target: "audit",
                event = "federation.unlink_refused",
                user_id = %user_id,
                reason = "no_alternative_auth",
                "👮🏻‍♂️ unlink refused — user has no password/OPAQUE fallback"
            );
            return Err(DomainError::new(
                ErrorKind::AccessDenied,
                "Federation",
                "Cannot unlink — set a password first, or you will be locked out.",
            ));
        }

        self.user_storage
            .unlink_federation_identity(user_id)
            .await?;

        tracing::info!(
            target: "audit",
            event = "federation.unlinked",
            user_id = %user_id,
            "🔗 OIDC identity unlinked"
        );
        Ok(())
    }

    /// Prepare an OIDC authorization flow for a Nextcloud Login Flow v2 session.
    ///
    /// Works like [`prepare_oidc_authorize`] but associates the Nextcloud flow
    /// token with the OIDC state so that [`oidc_callback`] can complete the
    /// Nextcloud login flow (app-password + poll result) instead of issuing
    /// internal JWTs.
    pub async fn prepare_oidc_authorize_for_nextcloud(
        &self,
        nc_flow_token: &str,
    ) -> Result<String, DomainError> {
        let oidc = self.oidc_service().ok_or_else(|| {
            DomainError::new(
                ErrorKind::InternalError,
                "OIDC",
                "OIDC service not configured",
            )
        })?;

        use rand_core::{OsRng, RngCore};
        let mut state_bytes = [0u8; 32];
        OsRng.fill_bytes(&mut state_bytes);
        let state_token = hex::encode(state_bytes);

        let mut nonce_bytes = [0u8; 32];
        OsRng.fill_bytes(&mut nonce_bytes);
        let nonce = hex::encode(nonce_bytes);

        let mut verifier_bytes = [0u8; 32];
        OsRng.fill_bytes(&mut verifier_bytes);
        let pkce_verifier = base64_url_encode(&verifier_bytes);
        let pkce_challenge = {
            use sha2::{Digest, Sha256};
            let hash = Sha256::digest(pkce_verifier.as_bytes());
            base64_url_encode(&hash)
        };

        // Store pending flow (auto-expires after 10 min via moka TTL)
        self.pending_oidc_flows.insert(
            state_token.clone(),
            PendingOidcFlow {
                pkce_verifier,
                nonce: nonce.clone(),
                nc_flow_token: Some(nc_flow_token.to_string()),
                intent: FlowIntent::Login,
            },
        );

        let authorize_url = oidc
            .get_authorize_url(&state_token, &nonce, &pkce_challenge)
            .await?;

        tracing::info!(
            "OIDC authorize flow prepared for Nextcloud Login Flow v2 (state={}...)",
            &state_token[..8]
        );

        Ok(authorize_url)
    }

    /// Handle the OIDC callback: validate CSRF state, exchange code with PKCE,
    /// validate ID token nonce, find or create user (JIT provisioning),
    /// issue internal tokens, and return a one-time exchange code.
    ///
    /// If the pending flow carries a Nextcloud flow token, this method returns
    /// `Err(NcOidcComplete { .. })` with a special error kind so the handler
    /// layer can complete the Nextcloud flow instead.
    pub async fn oidc_callback(
        &self,
        code: &str,
        state: &str,
        locale_registry: &crate::common::locale::LocaleRegistry,
        client_ip: Option<String>,
        user_agent: Option<String>,
    ) -> Result<OidcCallbackResult, DomainError> {
        // 0. Validate CSRF state and retrieve PKCE verifier + nonce + optional NC token
        //    (entry is auto-expired by moka TTL — remove returns None if expired)
        let flow = match self.pending_oidc_flows.remove(state) {
            Some(flow) => flow,
            None => {
                if let Some(exchange_code) = self.completed_oidc_logins.get(state) {
                    tracing::info!(
                        target: "audit",
                        event = "oidc.callback_replayed",
                        reason = "duplicate_callback",
                        "👮🏻‍♂️ Replayed a recently-completed OIDC login for a duplicate callback (consumed state)",
                    );
                    return Ok(OidcCallbackResult::WebLogin { exchange_code });
                }
                tracing::warn!(
                    target: "audit",
                    event = "oidc.callback_rejected",
                    reason = "invalid_or_expired_state",
                    "👮🏻‍♂️ OIDC callback with invalid/expired state token",
                );
                return Err(DomainError::new(
                    ErrorKind::AccessDenied,
                    "OIDC",
                    "Invalid or expired OIDC state — possible CSRF attack. Please try logging in again.",
                ));
            }
        };
        let (pkce_verifier, nonce, nc_flow_token, intent) = (
            flow.pkce_verifier,
            flow.nonce,
            flow.nc_flow_token,
            flow.intent,
        );

        // Clone the Arc and config out of the RwLock so we don't hold the lock across await points
        let (oidc, oidc_config) = {
            let state = self.oidc.read().unwrap();
            let svc = state.service.clone().ok_or_else(|| {
                DomainError::new(
                    ErrorKind::InternalError,
                    "OIDC",
                    "OIDC service not configured",
                )
            })?;
            let cfg = state.config.clone().ok_or_else(|| {
                DomainError::new(
                    ErrorKind::InternalError,
                    "OIDC",
                    "OIDC config not available",
                )
            })?;
            (svc, cfg)
        };

        // 1. Exchange authorization code for tokens (with PKCE verifier)
        let token_set = oidc.exchange_code(code, &pkce_verifier).await?;

        // 2. Validate ID token and extract claims (with nonce verification)
        let claims = oidc
            .validate_id_token(&token_set.id_token, Some(&nonce))
            .await?;

        // 3. Try to enrich claims from UserInfo endpoint if email is missing
        let claims = if claims.email.is_none() {
            match oidc.fetch_user_info(&token_set.access_token).await {
                Ok(user_info) => OidcIdClaims {
                    email: user_info.email.or(claims.email),
                    preferred_username: user_info.preferred_username.or(claims.preferred_username),
                    name: user_info.name.or(claims.name),
                    given_name: user_info.given_name.or(claims.given_name),
                    family_name: user_info.family_name.or(claims.family_name),
                    email_verified: user_info.email_verified.or(claims.email_verified),
                    locale: user_info.locale.or(claims.locale),
                    groups: if user_info.groups.is_empty() {
                        claims.groups
                    } else {
                        user_info.groups
                    },
                    ..claims
                },
                Err(e) => {
                    tracing::warn!(
                        "Failed to fetch UserInfo (continuing with ID token claims): {}",
                        e
                    );
                    claims
                }
            }
        } else {
            claims
        };

        // ────────────────────────────────────────────────────────────
        // Flow-intent dispatch — if this callback was initiated by
        // the self-service link path (`POST /api/auth/oidc/link/start`),
        // divert here BEFORE the login-specific processing (email
        // verification gate / JIT / session mint). Login stays on the
        // fall-through path. See docs/plan/oidc-account-linking.md.
        // ────────────────────────────────────────────────────────────
        if let FlowIntent::Link {
            user_id: target_user_id,
        } = intent
        {
            return self.complete_oidc_link(target_user_id, &claims).await;
        }

        let provider_name = oidc.provider_name().to_string();
        // Email-verification gate. The operator flag
        // `OXICLOUD_REQUIRE_VERIFIED_EMAIL` is the master switch — an
        // operator who opts out is telling us they trust the configured
        // IdP end-to-end (e.g. corporate SSO where the directory already
        // vets identities out-of-band). Both rejection reasons collapse
        // to the same "flag off → accept" behaviour so the operator
        // lever means what it says.
        //
        // Two distinct signals are audit-logged even in the accept path
        // so operators can spot risky IdP behaviour after the fact:
        //
        //   Some(false)  → IdP is ACTIVELY asserting the email is
        //                  unverified. Riskier than absence: it's the
        //                  first-login takeover primitive (attacker
        //                  types victim's address into an IdP-with-no-
        //                  verify). Emit at info-level either way; the
        //                  reject branch adds `oidc.callback_rejected`,
        //                  the accept branch adds
        //                  `oidc.email_unverified_accepted` so operators
        //                  running with the flag off can still see the
        //                  underlying risky signal in the audit log.
        //   None         → IdP simply doesn't publish the claim.
        //                  Weaker signal; rejected only when the flag
        //                  is on. No audit line on the accept branch
        //                  (the absence of a signal is not itself a
        //                  signal — logging it would just be noise).
        //
        // We only evaluate when an email is present in the claims —
        // no email → nothing to verify (the JIT path synthesises a
        // placeholder later).
        if let Some(email) = &claims.email {
            let must_verify = self.require_verified_email();
            let (reject, reason) = match (claims.email_verified, must_verify) {
                (Some(true), _) => (false, None),
                (Some(false), true) => (true, Some("idp_asserts_unverified")),
                (Some(false), false) => (false, Some("idp_asserts_unverified_flag_off")),
                (None, true) => (true, Some("claim_absent_and_required")),
                (None, false) => (false, None),
            };
            if let Some(reason) = reason {
                tracing::info!(
                    target: "audit",
                    event = if reject { "oidc.callback_rejected" } else { "oidc.email_unverified_accepted" },
                    reason = reason,
                    provider = %provider_name,
                    email = %email,
                    "👮🏻‍♂️ OIDC callback: email-verification signal"
                );
            }
            if reject {
                return Err(DomainError::new(
                    ErrorKind::AccessDenied,
                    "OIDC",
                    "Email verification required. Please verify your email at the identity provider.",
                ));
            }
        }

        // 4. Determine username and email
        let oidc_username = claims
            .preferred_username
            .clone()
            .or(claims.name.clone())
            .unwrap_or_else(|| format!("oidc_{}", &claims.sub[..8.min(claims.sub.len())]));
        let oidc_email = claims
            .email
            .clone()
            .unwrap_or_else(|| format!("{}@oidc.local", oidc_username));

        // 5. Look up existing user by OIDC subject.
        //
        // Two-step lookup implements the Phase B lazy-rebind of the
        // federation-identity rename (docs/plan/ocm.md § Phase B):
        //   1. Canonical lookup keyed on the id_token's real `iss` claim.
        //      Post-migration this is what every fresh JIT row uses.
        //   2. Legacy fallback keyed on the OXICLOUD_OIDC_PROVIDER_NAME
        //      display label. Fires for rows minted before the rename.
        //      If the fallback hits, the row is rebound to the real iss
        //      before this branch returns — first login after upgrade
        //      self-heals the user; no admin action needed.
        // If both miss, JIT provisioning kicks in below and writes the
        // canonical value from the start.
        let canonical = self
            .user_storage
            .get_user_by_federation_subject(&claims.iss, &claims.sub)
            .await;
        let lookup_result = match canonical {
            Ok(u) => Ok(u),
            // Only fall through to the legacy lookup if the canonical one
            // said "not found" — treat all OTHER errors as fatal to avoid
            // masking DB failures with a lookup that would probably fail
            // the same way. NotFound is the only benign case here.
            Err(e) if e.kind == ErrorKind::NotFound => {
                self.user_storage
                    .get_user_by_federation_subject(&provider_name, &claims.sub)
                    .await
            }
            Err(e) => Err(e),
        };
        let user = match lookup_result {
            Ok(mut existing_user) => {
                // Lazy-rebind: if the row's stored issuer doesn't match
                // the real iss claim, update it now. Covers the legacy-
                // label case (fallback hit) AND any drift accumulated
                // during Phase A when JIT was still writing labels.
                // rebind_federation_issuer is a guarded UPDATE — same-value
                // no-op costs nothing.
                if existing_user.federation_issuer() != Some(claims.iss.as_str()) {
                    let old = existing_user
                        .federation_issuer()
                        .map(str::to_string)
                        .unwrap_or_default();
                    self.user_storage
                        .rebind_federation_issuer(existing_user.id(), &claims.iss)
                        .await?;
                    tracing::info!(
                        target: "audit",
                        event = "federation.issuer_rebound",
                        reason = "lazy_backfill",
                        user_id = %existing_user.id(),
                        federation_kind = "oidc",
                        old_issuer = %old,
                        new_issuer = %claims.iss,
                        "🔗 federation_issuer rebound from legacy label to true iss URL",
                    );
                }
                // User exists — dispatch login BEFORE register_login() so
                // hooks observe `last_login_at = None` on the very first
                // login (see tip #1 in the trait docstring).
                if let Some(lc) = &self.user_lifecycle {
                    lc.dispatch_login(&existing_user).await;
                }
                // Decide BEFORE mutating: the row just fetched already
                // carries the stored avatar + verification stamp, so the
                // repeat-login common case (same IdP picture, already
                // verified) skips the DB entirely — the old shape rewrote
                // all 17 columns per login, and even a guarded UPDATE
                // would ship the avatar over the wire just to compare it
                // (benches/ROUND12.md §3b).
                let needs_profile_sync = existing_user.email_verified_at().is_none()
                    || existing_user.image() != claims.picture.as_deref();
                existing_user.register_login();
                existing_user.set_image(claims.picture.clone());
                // PR 23: retroactive email verification for OIDC users
                // who predate the column. The OIDC callback already
                // enforced `claims.email_verified == true` upstream, so
                // any user reaching this branch has a verified email
                // by the IdP's word; stamping is safe and idempotent.
                existing_user.mark_email_verified();
                // Narrow guarded sync instead of the 17-column row rewrite:
                // persists the IdP avatar + the verification stamp only
                // when either actually changed; `last_login_at` is stamped
                // by `create_session` at the end of this flow
                // (benches/ROUND12.md §3).
                if needs_profile_sync {
                    self.user_storage
                        .sync_oidc_login_profile(existing_user.id(), claims.picture.as_deref())
                        .await?;
                }
                existing_user
            }
            Err(_) => {
                // User doesn't exist by federation subject — try to
                // match by email under the same normalization the
                // self-service link flow uses (lowercase + strip
                // `+alias`). Three possible outcomes:
                //   * 0 matches → JIT provision (existing branch).
                //   * 1 match  → run the auto-link decision tree.
                //   * >1 match → refuse `email_ambiguous`. Two local
                //     rows collapsing to the same normalized email
                //     (`alice@example.com` + `alice+work@example.com`)
                //     mean we can't safely pick one to auto-link;
                //     admin must resolve.
                let normalized = crate::common::text::normalize_email_for_link(&oidc_email);
                let candidates = self
                    .user_storage
                    .list_users_by_normalized_email(&normalized)
                    .await
                    .unwrap_or_default();

                if candidates.len() > 1 {
                    tracing::info!(
                        target: "audit",
                        event = "federation.auto_link_refused",
                        reason = "email_ambiguous",
                        normalized_email = %normalized,
                        candidate_count = candidates.len(),
                        "🔗 auto-link refused — multiple local users normalize to the IdP email",
                    );
                    return Ok(OidcCallbackResult::AutoLinkRefused {
                        reason: "email_ambiguous",
                    });
                }

                if let Some(matched) = candidates.into_iter().next() {
                    // Auto-link decision tree — see
                    // docs/plan/oidc-account-linking.md § Auto-link.
                    let can_auto_link = oidc_config.auto_link_email_match
                        && claims.email_verified == Some(true)
                        && matched.federation_kind().is_none();

                    if !can_auto_link {
                        let reason = if !oidc_config.auto_link_email_match {
                            "auto_link_disabled"
                        } else if claims.email_verified != Some(true) {
                            "auto_link_email_not_verified"
                        } else {
                            "already_linked_elsewhere"
                        };
                        tracing::info!(
                            target: "audit",
                            event = "federation.auto_link_refused",
                            user_id = %matched.id(),
                            reason = reason,
                            "🔗 auto-link refused",
                        );
                        // Ok(AutoLinkRefused) rather than Err(AlreadyExists)
                        // so the handler can map each reason to a distinct
                        // stable CamelCase error_type (AutoLinkDisabled /
                        // AutoLinkEmailNotVerified / AutoLinkAlreadyLinked-
                        // Elsewhere). Bubbling as a generic AlreadyExists
                        // would collapse all three reasons into "Already
                        // Exists" on the wire and leave the SPA without a
                        // switch arm for user-facing copy.
                        return Ok(OidcCallbackResult::AutoLinkRefused { reason });
                    }

                    // All checks passed — commit the auto-link, re-fetch
                    // to observe the fresh federation columns, then run
                    // the same login-side effects as the "existing user"
                    // arm above (lifecycle dispatch, register_login,
                    // avatar/verification sync).
                    self.user_storage
                        .link_federation_identity(matched.id(), "oidc", &claims.iss, &claims.sub)
                        .await?;
                    tracing::info!(
                        target: "audit",
                        event = "federation.auto_linked",
                        reason = "email_match_verified",
                        user_id = %matched.id(),
                        federation_kind = "oidc",
                        federation_issuer = %claims.iss,
                        federation_subject = %claims.sub,
                        "🔗 OIDC identity auto-linked to existing local user via verified email match",
                    );
                    let mut linked_user = self.user_storage.get_user_by_id(matched.id()).await?;
                    if let Some(lc) = &self.user_lifecycle {
                        lc.dispatch_login(&linked_user).await;
                    }
                    linked_user.register_login();
                    linked_user.set_image(claims.picture.clone());
                    linked_user.mark_email_verified();
                    self.user_storage
                        .sync_oidc_login_profile(linked_user.id(), claims.picture.as_deref())
                        .await?;
                    // Yield the linked user — same shape as the
                    // Ok(existing_user) arm's tail expression.
                    linked_user
                } else {
                    // No email match — JIT provision (existing behavior).
                    if !oidc_config.auto_provision {
                        return Err(DomainError::new(
                            ErrorKind::AccessDenied,
                            "OIDC",
                            "Auto-provisioning is disabled. Contact admin to create your account.",
                        ));
                    }

                    // Determine role from OIDC groups
                    let role = self.map_oidc_role(&claims.groups, &oidc_config);

                    let quota = self.capped_quota(&role);

                    // Sanitize username: if it looks like an email, extract the local part
                    // (some OIDC providers like Keycloak use email as the preferred username)
                    let base_username = if oidc_username.contains('@') {
                        oidc_username.split('@').next().unwrap_or(&oidc_username)
                    } else {
                        &oidc_username
                    };

                    // Filter to valid username characters only, then truncate to 32 chars
                    let mut username = base_username
                        .chars()
                        .filter(|c| {
                            c.is_ascii_alphanumeric() || *c == '-' || *c == '_' || *c == '.'
                        })
                        .take(32)
                        .collect::<String>();

                    // Filter helper: removes any chars that are not valid in a username
                    let filter_username_chars = |s: &str| {
                        s.chars()
                            .filter(|c| {
                                c.is_ascii_alphanumeric() || *c == '-' || *c == '_' || *c == '.'
                            })
                            .take(32)
                            .collect::<String>()
                    };

                    // Ensure minimum length (the padding suffix must also be filtered)
                    if username.len() < 3 {
                        let filtered_sub = filter_username_chars(&claims.sub);
                        username = format!("user_{}", &filtered_sub[..filtered_sub.len().min(8)]);
                    }

                    // Check for username collision
                    if self
                        .user_storage
                        .get_user_by_username(&username)
                        .await
                        .is_ok()
                    {
                        let filtered_sub = filter_username_chars(&claims.sub);
                        let suffix = &filtered_sub[..filtered_sub.len().min(4)];
                        username = format!("{}_{}", &username[..username.len().min(27)], suffix);
                    }

                    let mut new_user = User::new(
                        oidc_email,
                        Some(username.clone()),
                        None,
                        Some(crate::domain::entities::user::FederationKind::Oidc),
                        // Phase B canonical value: the id_token's real `iss`
                        // claim (validated to equal discovery.issuer in
                        // OidcService). No more display-label writes at JIT —
                        // legacy rows are fixed via lazy rebind in the
                        // existing-user branch above.
                        Some(claims.iss.clone()),
                        Some(claims.sub.clone()),
                        role,
                        quota,
                        false,
                    )
                    .map_err(|e| {
                        DomainError::new(
                            ErrorKind::InvalidInput,
                            "OIDC",
                            format!("Failed to create OIDC user: {}", e),
                        )
                    })?;
                    new_user.set_image(claims.picture.clone());
                    new_user.set_given_name(claims.given_name.clone());
                    new_user.set_family_name(claims.family_name.clone());
                    // PR C: provision the user's preferred_locale from the
                    // OIDC `locale` claim AT JIT ONLY. Subsequent logins
                    // never re-apply this — a UI-driven choice ("I prefer
                    // English even though my IdP says fr-CA") must not be
                    // silently overwritten on the next sign-in. We validate
                    // the claim against the registry so an obscure or
                    // malformed code (e.g. `klingon`, `fr-FR-x-private`)
                    // doesn't end up stored only to fail at render time;
                    // unresolvable claims fall through to NULL → server
                    // default.
                    if let Some(claim) = claims.locale.as_deref()
                        && let Some(canonical) = locale_registry.parse(claim)
                    {
                        new_user.set_preferred_locale(Some(canonical.as_str().to_string()));
                    }
                    // PR 23: the OIDC callback rejected any caller upstream
                    // whose `email_verified` claim wasn't true, so users
                    // reaching this branch have an IdP-vetted email. Stamp
                    // the verification at JIT-create time.
                    new_user.mark_email_verified();

                    let created_user = self.user_storage.create_user(new_user).await?;

                    // Lifecycle: created (audit + home-folder provisioning) +
                    // login (no register_login() for a fresh OIDC user means
                    // `last_login_at` is naturally None → first-login detection
                    // works). PersonalDriveLifecycleHook creates the home folder.
                    if let Some(lc) = &self.user_lifecycle {
                        lc.dispatch_created(&created_user).await;
                        lc.dispatch_login(&created_user).await;
                    }

                    tracing::info!(
                        "OIDC user provisioned: {} (provider: {}, sub: {})",
                        created_user.id(),
                        provider_name,
                        claims.sub
                    );

                    created_user
                }
            }
        };

        // ── Branch: Nextcloud Login Flow v2 vs regular web login ──
        if let Some(nc_token) = nc_flow_token {
            // Nextcloud path: return user info so the handler can mint an
            // app-password and complete the NC login flow.
            tracing::info!(
                user = %user.display_for_audit(),
                "OIDC login successful for Nextcloud Login Flow v2"
            );
            return Ok(OidcCallbackResult::NextcloudLogin {
                nc_flow_token: nc_token,
                user_id: user.id(),
                username: user.username().unwrap_or("").to_string(),
            });
        }

        // 6. Issue internal tokens (same as regular login). OIDC
        // callback is a GET redirect — no way to thread `dpop_jkt`
        // through the browser's redirect chain. Session is minted
        // unbound; the SPA calls `POST /api/auth/dpop/bind` post-
        // redirect to bind it (see Gate 3). Token accordingly ships
        // without `cnf.jkt`.
        let access_token = self.token_service.generate_access_token(&user, None)?;
        let refresh_token = self.token_service.generate_refresh_token();

        let mut session = Session::new(
            user.id(),
            refresh_token.clone(),
            client_ip,
            user_agent,
            self.token_service.refresh_token_expiry_days(),
            Uuid::new_v4(),
            crate::domain::entities::session::SessionOrigin::Oidc,
        )
        .with_oidc_id_token(token_set.id_token.clone());
        // Bind the IdP's session identifier so Back-Channel Logout can
        // revoke this specific device (see auth_ports::OidcLogoutClaims
        // and session_pg_repository::revoke_sessions_by_oidc_sid). IdPs
        // that don't emit sid leave this None; BCL then falls back to
        // sub-based revocation.
        if let Some(sid) = claims.sid.as_ref() {
            session = session.with_oidc_sid(sid.clone());
        }
        self.session_storage.create_session(session).await?;

        let force_password_change = self.read_force_password_change(user.id()).await;
        let auth_response = AuthResponseDto {
            user: UserDto::from(user),
            access_token,
            refresh_token,
            token_type: "Bearer".to_string(),
            expires_in: self.token_service.refresh_token_expiry_secs(),
            force_password_change,
        };

        // 7. Store auth response behind a one-time exchange code (Fix #4: no tokens in URL)
        let mut code_bytes = [0u8; 32];
        use rand_core::{OsRng, RngCore};
        OsRng.fill_bytes(&mut code_bytes);
        let exchange_code = hex::encode(code_bytes);

        // Store auth response (auto-expires after 60 s via moka TTL)
        self.pending_oidc_tokens
            .insert(exchange_code.clone(), PendingOidcToken { auth_response });

        self.completed_oidc_logins
            .insert(state.to_string(), exchange_code.clone());

        tracing::info!("OIDC login successful, one-time exchange code generated");

        Ok(OidcCallbackResult::WebLogin { exchange_code })
    }

    /// Exchange a one-time code for the authentication tokens.
    /// The code is single-use and expires after 60 seconds (moka TTL).
    pub fn exchange_oidc_token(&self, one_time_code: &str) -> Result<AuthResponseDto, DomainError> {
        let pending = self
            .pending_oidc_tokens
            .remove(one_time_code)
            .ok_or_else(|| {
                DomainError::new(
                    ErrorKind::AccessDenied,
                    "OIDC",
                    "Invalid or expired exchange code. Please try logging in again.",
                )
            })?;

        Ok(pending.auth_response)
    }

    /// Map OIDC groups to internal role
    fn map_oidc_role(&self, groups: &[String], config: &OidcConfig) -> UserRole {
        if config.admin_groups.is_empty() {
            return UserRole::User;
        }
        let admin_groups: Vec<&str> = config.admin_groups.split(',').map(|s| s.trim()).collect();
        for group in groups {
            if admin_groups.iter().any(|ag| ag.eq_ignore_ascii_case(group)) {
                return UserRole::Admin;
            }
        }
        UserRole::User
    }

    // `create_personal_folder` was removed in PR 3 of the
    // UserLifecycleHook migration — home-folder provisioning is now
    // owned by `PersonalDriveLifecycleHook` in folder_service.rs and runs
    // via `dispatch_created` / `dispatch_login`.
}

/// URL-safe base64 encoding without padding (RFC 4648 §5)
fn base64_url_encode(input: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(input)
}

// ── Phase 4 gate: end-to-end service-level integration test ─────────────
//
// The repo layer proves `is_migrated` returns the right bool
// (`opaque_pg_repository.rs::is_migrated_tracks_mark_and_clear_state_transitions`).
// This module proves the WIRING between `is_migrated` and `login()` —
// that a mark_migrated actually flips a subsequent legacy login from
// success to `OpaqueLoginRequired`-shaped `AccessDenied`. Without
// this test, the field / builder / gate could rot independently
// (silent field rename, missing `.with_opaque_repo` call in the
// factory, etc.) and only surface when an operator actually rolls
// out the substrate.
//
// Runs against the real test DB (same `oxicloud_test` guard as the
// other integration_tests modules). Gated on `all(test, integration_tests)`
// so the module compiles ONLY during `cargo test --cfg integration_tests`:
// a plain library build strips it, keeping `#[test]`-only imports from
// tripping the unused-imports lint. Mirrors the pattern in
// `folder_service::cascade_hook_integration_tests`.
#[cfg(all(test, integration_tests))]
mod phase4_gate_integration_tests {
    use super::*;
    use crate::application::ports::opaque_ports::OpaqueRepositoryPort;
    use crate::infrastructure::repositories::pg::{
        OpaquePgRepository, SessionPgRepository, UserPgRepository,
    };
    use crate::infrastructure::services::jwt_service::JwtTokenService;
    use crate::infrastructure::services::password_hasher::Argon2PasswordHasher;
    use crate::integration_test_support::{ensure_clean_test_db, test_db_url};
    use sqlx::postgres::PgPoolOptions;
    use std::path::PathBuf;

    /// Assemble a minimal `AuthApplicationService` wired with the
    /// real repos + a real (fast-KSF) password hasher against the
    /// integration test DB. Only what `login()` and the Phase 4
    /// gate touch — no lifecycle dispatcher, no magic-link repo,
    /// no OIDC. Returns the service, a handle to the concrete
    /// OPAQUE repo (so the test can call `mark_migrated` /
    /// `clear_registration` directly), and the pool for seeding.
    async fn build_service() -> (
        AuthApplicationService,
        Arc<OpaquePgRepository>,
        Arc<sqlx::PgPool>,
        Arc<Argon2PasswordHasher>,
    ) {
        let pool = Arc::new(
            PgPoolOptions::new()
                .max_connections(4)
                .connect(&test_db_url())
                .await
                .expect("connect to integration-test PostgreSQL"),
        );
        ensure_clean_test_db(&pool).await;

        let user_repo = Arc::new(UserPgRepository::new(pool.clone()));
        let session_repo = Arc::new(SessionPgRepository::new(pool.clone()));
        // Fast Argon2 so this test finishes in ms rather than seconds.
        // Real deployments run at the OXICLOUD_HASH_* values; the gate
        // logic under test doesn't care about hash cost.
        let hasher = Arc::new(Argon2PasswordHasher::new(8, 1, 1));
        let token = Arc::new(JwtTokenService::new(
            "test-secret-do-not-use-in-prod-minimum-32-chars".to_string(),
            3600,
            86400,
        ));
        let opaque_repo = Arc::new(OpaquePgRepository::new(pool.clone()));

        let svc = AuthApplicationService::new(
            user_repo,
            session_repo,
            hasher.clone(),
            token,
            PathBuf::from("/tmp"),
        )
        .with_opaque_repo(opaque_repo.clone());

        (svc, opaque_repo, pool, hasher)
    }

    /// Seed a user with a password hash + verified email — the
    /// minimum shape `login()` accepts. `email_verified_at` set so
    /// the (default-off) `require_verified_email` gate doesn't
    /// interfere with the Phase 4 branch we're isolating.
    async fn seed_user_with_password(
        pool: &sqlx::PgPool,
        hasher: &Argon2PasswordHasher,
        email: &str,
        password: &str,
    ) -> uuid::Uuid {
        use crate::application::ports::auth_ports::PasswordHasherPort;

        let id = uuid::Uuid::new_v4();
        let hash = hasher.hash_password(password).await.expect("hash password");
        sqlx::query(
            r#"
            INSERT INTO auth.users (
                id, username, email, password_hash, role,
                storage_quota_bytes, storage_used_bytes,
                created_at, updated_at, active,
                email_verified_at
            ) VALUES (
                $1, NULL, $2, $3, 'user'::auth.userrole,
                0, 0, NOW(), NOW(), TRUE, NOW()
            )
            "#,
        )
        .bind(id)
        .bind(email)
        .bind(hash)
        .execute(pool)
        .await
        .expect("seed test user");
        id
    }

    /// The full Phase 4 gate lifecycle: legacy login works, marking
    /// migrated flips subsequent legacy logins to the `OpaqueLoginRequired`
    /// shape, and admin-reset (`clear_registration`) re-opens legacy.
    ///
    /// The single test covers all three transitions so a regression
    /// in any leg (missing `with_opaque_repo`, wrong error message,
    /// clear-not-nulling-migrated-at) fails one assertion instead of
    /// three separate tests reporting the same drift.
    #[tokio::test]
    async fn login_flow_across_mark_migrated_and_clear_registration() {
        let (svc, opaque_repo, pool, hasher) = build_service().await;
        let email = format!("phase4-{}@example.invalid", uuid::Uuid::new_v4());
        let user_id = seed_user_with_password(&pool, &hasher, &email, "s3cret-passphrase").await;

        // Baseline — no envelope, no migration mark → legacy works.
        svc.login(
            crate::application::dtos::user_dto::LoginDto {
                username: email.clone(),
                password: "s3cret-passphrase".to_string(),
                dpop_jkt: None,
            },
            None,
            None,
        )
        .await
        .expect("baseline legacy login must succeed");

        // Simulate a successful OPAQUE handshake landing.
        opaque_repo
            .mark_migrated(user_id)
            .await
            .expect("mark migrated");

        // Now the Phase 4 gate fires: same credentials, same call,
        // but AccessDenied with the exact message the handler layer
        // remaps to `403 OpaqueLoginRequired`.
        let refused = svc
            .login(
                crate::application::dtos::user_dto::LoginDto {
                    username: email.clone(),
                    password: "s3cret-passphrase".to_string(),
                    dpop_jkt: None,
                },
                None,
                None,
            )
            .await
            .expect_err("legacy login must be refused post-migration");
        assert_eq!(
            refused.kind,
            ErrorKind::AccessDenied,
            "gate must return AccessDenied"
        );
        assert_eq!(
            refused.message, "Password login refused: this account has migrated to OPAQUE",
            "message must match what the handler remaps to `OpaqueLoginRequired` — \
             change either both sides at once or the handler stops recognising it"
        );

        // Wrong password on a migrated user MUST return the same
        // shape as any other wrong-password (`Invalid credentials`),
        // NOT the OPAQUE-migrated message — the gate lives AFTER the
        // password check specifically so an attacker without the
        // password learns nothing about migration state.
        let wrong = svc
            .login(
                crate::application::dtos::user_dto::LoginDto {
                    username: email.clone(),
                    password: "wrong-password".to_string(),
                    dpop_jkt: None,
                },
                None,
                None,
            )
            .await
            .expect_err("wrong password must still fail");
        assert_eq!(wrong.message, "Invalid credentials");

        // Admin-side password reset (clear_registration NULLs
        // opaque_migrated_at atomically) must re-open the fallback —
        // otherwise the admin-reset user is locked out (envelope
        // gone, gate still refusing).
        opaque_repo
            .clear_registration(user_id)
            .await
            .expect("clear registration");
        svc.login(
            crate::application::dtos::user_dto::LoginDto {
                username: email,
                password: "s3cret-passphrase".to_string(),
                dpop_jkt: None,
            },
            None,
            None,
        )
        .await
        .expect("legacy login must succeed again after admin clear_registration");
    }
}
