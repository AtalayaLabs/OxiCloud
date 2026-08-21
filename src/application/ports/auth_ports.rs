use crate::common::errors::DomainError;
use crate::domain::entities::app_password::AppPassword;
use crate::domain::entities::device_code::DeviceCode;
use crate::domain::entities::session::Session;
use crate::domain::entities::user::User;
use crate::domain::repositories::user_repository::UserListEntry;
use std::sync::Arc;
use uuid::Uuid;

// ============================================================================
// Cryptography Ports - Extracted from Domain to maintain Clean Architecture
// ============================================================================

/// Port for password hashing operations.
///
/// This trait abstracts cryptographic password operations, allowing the domain
/// layer to remain independent of specific hashing implementations (argon2, bcrypt, etc.)
///
/// Methods are async because implementations (e.g. Argon2) are CPU-intensive
/// and must run on a blocking thread pool to avoid starving Tokio workers.
pub trait PasswordHasherPort: Send + Sync + 'static {
    /// Hash a plain text password
    async fn hash_password(&self, password: &str) -> Result<String, DomainError>;

    /// Verify a plain text password against a hash
    async fn verify_password(&self, password: &str, hash: &str) -> Result<bool, DomainError>;
}

/// Claims contained in a JWT token
///
/// `username` / `email` are `Arc<str>` so the per-request `CurrentUser`
/// build clones them with a refcount bump instead of copying the strings —
/// the validation cache already hands the whole struct out behind an `Arc`,
/// but the two display fields still had to be deep-cloned out of it on
/// EVERY authenticated request (the "2 allocs/request" item deferred since
/// ROUND6).
#[derive(Debug, Clone)]
pub struct TokenClaims {
    /// Subject identifier (user ID)
    pub sub: String,
    /// `sub` pre-parsed to a `Uuid` at decode time so the auth middleware
    /// reads it as a `Copy` on every request instead of re-parsing the
    /// 36-char string per request — even on validation-cache hits, which
    /// return the same `Arc<TokenClaims>` (benches/ROUND14.md §A3). Nil only
    /// if a verified token somehow carried a non-UUID `sub` (unreachable for
    /// tokens we sign); the middleware rejects nil defensively.
    pub sub_id: Uuid,
    /// Expiration timestamp (seconds since Unix epoch)
    pub exp: i64,
    /// Issued at timestamp (seconds since Unix epoch)
    pub iat: i64,
    /// JWT unique ID
    pub jti: String,
    /// Username
    pub username: Arc<str>,
    /// User email
    pub email: Arc<str>,
    /// User role
    pub role: String,
    /// RFC 9449 §5 confirmation-key thumbprint — the JWK thumbprint
    /// of the DPoP keypair this session was bound to at login. `None`
    /// for unbound sessions (app passwords, NC clients, pre-DPoP).
    /// The DPoP middleware reads it from the already-validated token
    /// (no DB round trip) to enforce "bound session → proof required".
    pub dpop_jkt: Option<String>,
    /// Session identifier — the `auth.sessions.id` this access token
    /// was minted for. Read by the auth middleware to stamp
    /// per-session liveness via [`LastSeenTracker`](crate::infrastructure::services::last_seen_tracker)
    /// with no DB round trip. `None` for tokens minted by builds
    /// that predate the `sid` claim (backward compat during rollout;
    /// harmless — the missing sid just means no stamp fires, and
    /// the token still authenticates normally).
    pub sid: Option<Uuid>,
}

/// Port for JWT token operations.
///
/// This trait abstracts token generation and validation, allowing the domain
/// layer to remain independent of specific JWT implementations.
pub trait TokenServicePort: Send + Sync + 'static {
    /// Generate an access token for a user.
    ///
    /// `dpop_jkt` — if `Some`, the token carries an RFC 9449 §5
    /// `cnf.jkt` claim binding it to the browser-held keypair whose
    /// public JWK hashes to this thumbprint. Callers pass
    /// `session.dpop_jkt()` from the Session being minted; unbound
    /// sessions (app passwords, NC clients, pre-DPoP) pass `None`
    /// and get a plain token the middleware exempts from DPoP.
    fn generate_access_token(
        &self,
        user: &User,
        session_id: Option<Uuid>,
        dpop_jkt: Option<&str>,
    ) -> Result<String, DomainError>;

    /// Validate a token and extract its claims.
    ///
    /// Returns `Arc<TokenClaims>` so the implementation's validation cache can
    /// hand back a hot entry with a refcount bump instead of deep-cloning the
    /// (multi-`String`) claims on every authenticated request. Callers that
    /// only read fields go through `Deref`; the few that retain a field clone
    /// just that one.
    fn validate_token(&self, token: &str) -> Result<Arc<TokenClaims>, DomainError>;

    /// Generate a refresh token
    fn generate_refresh_token(&self) -> String;

    /// Get refresh token expiry in seconds
    fn refresh_token_expiry_secs(&self) -> i64;

    /// Get refresh token expiry in days
    fn refresh_token_expiry_days(&self) -> i64;
}

// ============================================================================
// Storage Ports
// ============================================================================

pub trait UserStoragePort: Send + Sync + 'static {
    /// Creates a new user
    async fn create_user(&self, user: User) -> Result<User, DomainError>;

    /// Gets a user by ID
    async fn get_user_by_id(&self, id: Uuid) -> Result<User, DomainError>;

    /// Fetch the full `User` + [`UserDerivedFlags`] in one query. See
    /// [`UserRepository::get_user_with_derived_flags`](crate::domain::repositories::user_repository::UserRepository::get_user_with_derived_flags)
    /// for the contract and the rationale for the single-query shape.
    async fn get_user_with_derived_flags(
        &self,
        id: Uuid,
    ) -> Result<
        (
            User,
            crate::domain::repositories::user_repository::UserDerivedFlags,
        ),
        DomainError,
    >;

    /// Paginated admin user listing with derived flags. See
    /// [`UserRepository::list_users_with_derived_flags`](crate::domain::repositories::user_repository::UserRepository::list_users_with_derived_flags)
    /// for the contract and rationale.
    async fn list_users_with_derived_flags(
        &self,
        limit: i64,
        offset: i64,
        include_external: bool,
    ) -> Result<
        Vec<(
            User,
            crate::domain::repositories::user_repository::UserDerivedFlags,
        )>,
        DomainError,
    >;

    /// Batch-loads users by id. Order is unspecified; missing ids are
    /// silently dropped. Used by group-recipient expansion in
    /// `RecipientNotificationService` to avoid N+1 lookups when notifying
    /// a group of size N.
    async fn get_users_by_ids(&self, ids: Vec<Uuid>) -> Result<Vec<User>, DomainError>;

    /// Gets a user by username
    async fn get_user_by_username(&self, username: &str) -> Result<User, DomainError>;

    /// Gets a user by email
    async fn get_user_by_email(&self, email: &str) -> Result<User, DomainError>;

    /// Returns every user whose email normalizes to `normalized_email`
    /// (see `UserRepository::list_users_by_normalized_email` for the
    /// full contract and the auto-link ambiguity-detection use case).
    async fn list_users_by_normalized_email(
        &self,
        normalized_email: &str,
    ) -> Result<Vec<User>, DomainError>;

    /// Updates an existing user
    async fn update_user(&self, user: User) -> Result<User, DomainError>;

    /// Updates only the storage usage of a user
    async fn update_storage_usage(
        &self,
        user_id: Uuid,
        usage_bytes: i64,
    ) -> Result<(), DomainError>;

    /// Lists users with pagination. `include_external` defaults to `false`
    /// at every call site that surfaces users to other internal users
    /// (autocomplete, sharee search, etc.); only the admin management UI
    /// passes `true`. See [`UserRepository::list_users`] for the rationale.
    async fn list_users(
        &self,
        limit: i64,
        offset: i64,
        include_external: bool,
    ) -> Result<Vec<User>, DomainError>;

    /// Narrow user-list projection for management tables.  Keeps heavyweight
    /// account-detail fields off the database and JSON hot path.
    async fn list_user_summaries(
        &self,
        limit: i64,
        offset: i64,
        include_external: bool,
    ) -> Result<Vec<UserListEntry>, DomainError>;

    /// Searches users by username or email (SQL ILIKE) with a limit.
    /// See [`list_users`] for the meaning of `include_external`.
    async fn search_users(
        &self,
        query: &str,
        limit: i64,
        include_external: bool,
    ) -> Result<Vec<User>, DomainError>;

    /// Username-only projection of [`search_users`] — same WHERE / ORDER /
    /// LIMIT semantics, but skips hydrating the 21-column row (incl. the
    /// up-to-512 KiB avatar `image`) when the caller only needs handles.
    /// Rows whose username is NULL are returned as `None` so callers can
    /// keep the wide flow's post-limit filtering semantics.
    async fn search_usernames(
        &self,
        query: &str,
        limit: i64,
        include_external: bool,
    ) -> Result<Vec<Option<String>>, DomainError>;

    /// Stamps `email_verified_at = NOW()` iff it is still NULL (idempotent,
    /// preserves the first timestamp — the SQL twin of
    /// `User::mark_email_verified`). Narrow single-column write; avoids the
    /// full-row [`update_user`] (incl. the avatar `image`) on the
    /// magic-link redemption path.
    async fn mark_email_verified(&self, user_id: Uuid) -> Result<(), DomainError>;

    /// OIDC repeat-login profile sync: persists the IdP-provided avatar and
    /// stamps `email_verified_at` (guarded, idempotent) in ONE narrow
    /// statement. The `IS DISTINCT FROM` guard makes the common case (same
    /// avatar, already verified) a zero-write no-op — vs the full 17-column
    /// row rewrite this path used to pay per login. `last_login_at` is NOT
    /// touched here: session creation stamps it, as on every login path.
    async fn sync_oidc_login_profile(
        &self,
        user_id: Uuid,
        image: Option<&str>,
    ) -> Result<(), DomainError>;

    /// Federation-identity Phase B lazy rebind: overwrite `federation_issuer`
    /// on a specific user row. Called when an OIDC login's id_token `iss`
    /// claim proves the stored value (typically a legacy display label) is
    /// out of sync with the true issuer URL. Guarded (`IS DISTINCT FROM`)
    /// so calling with the current value is a zero-write no-op.
    ///
    /// Audit signal for the rebind lives at the caller (auth service) —
    /// this repo method just moves the column value.
    async fn rebind_federation_issuer(
        &self,
        user_id: Uuid,
        new_issuer: &str,
    ) -> Result<(), DomainError>;

    /// Attach a federation identity to a user row that currently has
    /// none. Used by the self-service link flow and the auto-link
    /// branch of the OIDC callback. See
    /// docs/plan/oidc-account-linking.md.
    ///
    /// Enforces at the DB layer via the
    /// `idx_users_federation` UNIQUE index: if this triple is already
    /// bound to a DIFFERENT user, returns `AlreadyExists`. The caller
    /// (app service) translates that to a `already_linked_elsewhere`
    /// audit reason and a user-facing refusal.
    ///
    /// Does NOT overwrite an already-linked identity — the current
    /// user must be unlinked first. This is a "first link" primitive
    /// only; the app service's higher-level `link_oidc` orchestrates
    /// the pre-checks (idempotent-if-same / refuse-if-different).
    async fn link_federation_identity(
        &self,
        user_id: Uuid,
        kind: &str,
        issuer: &str,
        subject: &str,
    ) -> Result<(), DomainError>;

    /// Scalar `opaque_envelope IS NOT NULL` for the user. Used by the
    /// unlink refusal guard (a user with an OPAQUE envelope still has
    /// a working direct login even after OIDC unlink). Avoids
    /// dragging the full envelope bytes across the wire for a bool.
    async fn is_opaque_registered(&self, user_id: Uuid) -> Result<bool, DomainError>;

    /// Detach the current federation identity from a user row: set all
    /// three federation columns to NULL. The `has_password` or
    /// `opaque_registered` fallback guard lives at the app service
    /// layer — this method is a mechanical UPDATE.
    ///
    /// Idempotent: calling on an already-unlinked user is a no-op.
    async fn unlink_federation_identity(&self, user_id: Uuid) -> Result<(), DomainError>;

    /// Lists users by role (e.g., "admin" or "user")
    async fn list_users_by_role(&self, role: &str) -> Result<Vec<User>, DomainError>;

    /// Counts users with a given role WITHOUT hydrating their rows — a scalar
    /// `COUNT(*)` instead of fetching every full user row (incl. the up-to-512
    /// KiB avatar `image` and the `ui_preferences` JSONB) only to `.len()` them
    /// (benches/ROUND29.md §G).
    async fn count_users_by_role(&self, role: &str) -> Result<i64, DomainError>;

    /// Deletes a user by their ID
    async fn delete_user(&self, user_id: Uuid) -> Result<(), DomainError>;

    /// Changes a user's password
    async fn change_password(&self, user_id: Uuid, password_hash: &str) -> Result<(), DomainError>;

    /// Finds a user by federation (issuer, subject) pair. Historically
    /// called for OIDC lookups (the only federation kind in-tree at rename
    /// time); after Phase B/C of the federation-identity rename the
    /// caller passes the true `iss` URL rather than a display label. See
    /// `docs/plan/ocm.md § Schema rename` for the transition.
    async fn get_user_by_federation_subject(
        &self,
        issuer: &str,
        subject: &str,
    ) -> Result<User, DomainError>;

    /// Activates or deactivates a user
    async fn set_user_active_status(&self, user_id: Uuid, active: bool) -> Result<(), DomainError>;

    /// Changes a user's role
    async fn change_role(&self, user_id: Uuid, role: &str) -> Result<(), DomainError>;

    /// Updates a user's storage quota
    async fn update_storage_quota(
        &self,
        user_id: Uuid,
        quota_bytes: i64,
    ) -> Result<(), DomainError>;

    /// Counts the total number of users
    async fn count_users(&self) -> Result<i64, DomainError>;
}

// ============================================================================
// OIDC Port
// ============================================================================

/// Represents the token set returned by the OIDC provider after code exchange
#[derive(Debug, Clone)]
pub struct OidcTokenSet {
    pub access_token: String,
    pub id_token: String,
    pub refresh_token: Option<String>,
}

/// Claims extracted from the validated OIDC ID token
#[derive(Debug, Clone)]
pub struct OidcIdClaims {
    pub sub: String,
    /// The validated `iss` claim from the id_token. Equal to
    /// `discovery.issuer` (the validator enforces `iss == discovery.issuer`,
    /// so this is a safe echo of the authoritative issuer URL).
    ///
    /// Load-bearing for the federation-identity Phase B lazy-rebind: the
    /// app service compares this against `user.federation_issuer` and
    /// updates the row when the stored value is still a legacy display
    /// label (see docs/plan/ocm.md § Rename PR — Phase B).
    pub iss: String,
    pub email: Option<String>,
    pub email_verified: Option<bool>,
    pub preferred_username: Option<String>,
    pub name: Option<String>,
    /// Standard OpenID claim `given_name` (first name). Populated on the
    /// `User` row at JIT provisioning so the share-modal autocomplete and
    /// the system address book can surface real names instead of just the
    /// (often-cryptic) `preferred_username`.
    pub given_name: Option<String>,
    /// Standard OpenID claim `family_name` (last name). See `given_name`.
    pub family_name: Option<String>,
    pub groups: Vec<String>,
    pub picture: Option<String>,
    /// Standard OpenID claim `locale` (BCP-47 language tag, e.g.
    /// `"fr"`, `"zh-TW"`). Populated on the new `User` row at OIDC JIT
    /// provisioning if the claim resolves against the server's
    /// `LocaleRegistry`; ignored on subsequent logins so a later
    /// UI-driven choice isn't overwritten by the IdP.
    pub locale: Option<String>,
    /// OIDC session identifier. Populated only when the IdP emits `sid`
    /// on the id_token (Keycloak: "Backchannel Logout Session Required"
    /// on the client). When present, we persist it on the OxiCloud
    /// session so Back-Channel Logout can revoke that specific device.
    pub sid: Option<String>,
}

/// OIDC Back-Channel Logout 1.0 identifiers extracted from a validated
/// logout_token. The BCL handler uses these to resolve which OxiCloud
/// session(s) to revoke: `sid` for per-device (preferred), else `sub` for
/// all of the user's sessions.
#[derive(Debug, Clone)]
pub struct OidcLogoutClaims {
    pub sub: Option<String>,
    pub sid: Option<String>,
    /// JWT identifier — used by the app service to prevent replay of the
    /// same logout_token within the token's freshness window.
    pub jti: Option<String>,
    /// The validated `iss` claim from the logout_token — echoed from
    /// `discovery.issuer` (the validator enforces `iss == discovery.issuer`,
    /// so this is a safe echo of the authoritative issuer URL).
    ///
    /// Load-bearing for the sub-based revocation path (BCL without sid):
    /// the app service passes this to
    /// `revoke_user_sessions_by_federation_subject(issuer, sub)`, and the
    /// pg impl matches on `auth.users.federation_issuer` — which post
    /// Phase B stores the iss URL, NOT the display label. Passing the
    /// display label (via `oidc.provider_name()`) misses every row.
    pub iss: String,
}

/// Port for OIDC operations — implemented in infrastructure layer
pub trait OidcServicePort: Send + Sync + 'static {
    /// Get the authorization URL for redirecting the user to the IdP.
    /// Includes PKCE code_challenge (S256) and nonce for ID token binding.
    /// This is async because it may need to fetch the OIDC discovery document.
    async fn get_authorize_url(
        &self,
        state: &str,
        nonce: &str,
        pkce_challenge: &str,
    ) -> Result<String, DomainError>;

    /// Exchange an authorization code for tokens, providing PKCE code_verifier.
    async fn exchange_code(
        &self,
        code: &str,
        pkce_verifier: &str,
    ) -> Result<OidcTokenSet, DomainError>;

    /// Validate an ID token and extract claims.
    /// If `expected_nonce` is provided, verifies the `nonce` claim matches.
    async fn validate_id_token(
        &self,
        id_token: &str,
        expected_nonce: Option<&str>,
    ) -> Result<OidcIdClaims, DomainError>;

    /// Fetch user info from the UserInfo endpoint (fallback for missing ID token claims)
    async fn fetch_user_info(&self, access_token: &str) -> Result<OidcIdClaims, DomainError>;

    /// Get the OIDC provider display name
    fn provider_name(&self) -> &str;

    /// Validate an OIDC Back-Channel Logout 1.0 logout_token.
    ///
    /// Enforces all mandatory spec checks: JWKS signature, iss+aud match,
    /// `events` claim contains the backchannel-logout URI, presence of
    /// `sub` and/or `sid`, absence of `nonce`. On any failure returns
    /// `AccessDenied` — the handler translates to a 400 per spec.
    ///
    /// The caller is responsible for jti replay prevention (this validator
    /// is stateless).
    async fn validate_logout_token(
        &self,
        logout_token: &str,
    ) -> Result<OidcLogoutClaims, DomainError>;

    /// Build an RP-initiated logout URL (OIDC Session Management 1.0).
    ///
    /// Returns `Ok(None)` when the IdP's discovery document does not advertise
    /// an `end_session_endpoint` — some providers don't support RP-initiated
    /// logout, in which case the caller falls back to a local-only logout.
    ///
    /// `id_token_hint` is required by most IdPs (Keycloak in particular
    /// rejects the request without it) so the server can identify the session
    /// to terminate. `post_logout_redirect_uri` must be one of the URIs
    /// registered on the OIDC client, else the IdP refuses the redirect.
    async fn build_end_session_url(
        &self,
        id_token_hint: &str,
        post_logout_redirect_uri: &str,
    ) -> Result<Option<String>, DomainError>;
}

pub trait SessionStoragePort: Send + Sync + 'static {
    /// Creates a new session
    async fn create_session(&self, session: Session) -> Result<Session, DomainError>;

    /// Refresh-token rotation: revokes `old_session_id` and creates
    /// `new_session` in ONE transaction (the refresh path used to pay two
    /// full BEGIN/COMMIT round-trip pairs per rotation). Also stamps the
    /// user's `last_login_at` exactly like [`create_session`] does.
    async fn rotate_session(
        &self,
        old_session_id: Uuid,
        new_session: Session,
    ) -> Result<Session, DomainError>;

    /// Gets a session by refresh token
    async fn get_session_by_refresh_token(
        &self,
        refresh_token: &str,
    ) -> Result<Session, DomainError>;

    /// Revokes a specific session
    async fn revoke_session(&self, session_id: Uuid) -> Result<(), DomainError>;

    /// Revokes all sessions of a user
    async fn revoke_all_user_sessions(&self, user_id: Uuid) -> Result<u64, DomainError>;

    /// Revokes every session of a user EXCEPT `keep_session_id`.
    /// Classic "password change" pattern: kills OTHER devices' sessions
    /// while keeping the caller's current session alive so the SPA can
    /// complete follow-up work without a session-death race.
    async fn revoke_other_user_sessions(
        &self,
        user_id: Uuid,
        keep_session_id: Uuid,
    ) -> Result<u64, DomainError>;

    /// Revokes all sessions in a token family (used when replay of a revoked token is detected)
    async fn revoke_session_family(&self, family_id: Uuid) -> Result<u64, DomainError>;

    /// OIDC Back-Channel Logout: revoke sessions matching an IdP-supplied
    /// `sid` (per-device). Returns the user id(s) of revoked sessions so
    /// the caller can dispatch lifecycle hooks.
    async fn revoke_sessions_by_oidc_sid(&self, sid: &str) -> Result<Vec<Uuid>, DomainError>;

    /// OIDC Back-Channel Logout fallback when the IdP didn't supply a `sid`:
    /// revoke every session belonging to the user identified by
    /// `(federation_issuer, federation_subject)`. Returns the affected
    /// user id, or `None` if we don't know that user.
    async fn revoke_user_sessions_by_federation_subject(
        &self,
        issuer: &str,
        subject: &str,
    ) -> Result<Option<Uuid>, DomainError>;

    /// One-shot bind a DPoP JWK thumbprint to a session that was created
    /// without one (post-redirect flow — OIDC callback, magic-link
    /// redemption). Fails with `AlreadyExists` if the session already
    /// carries a thumbprint (anti-downgrade invariant, see
    /// `docs/plan/dpop.md`).
    async fn bind_dpop_jkt(&self, session_id: Uuid, dpop_jkt: &str) -> Result<(), DomainError>;

    /// Fetch a single session by id. Used by admin surfaces that need
    /// to resolve `target_user_id` for audit lines before a mutation.
    /// Returns `NotFound` when the id doesn't match any row.
    async fn get_session_by_id(&self, session_id: Uuid) -> Result<Session, DomainError>;

    /// Paginated cross-user listing for the admin sessions panel.
    /// `user_id_filter` narrows to a single user when `Some`; `None`
    /// spans all users. `include_revoked = false` (the default UX)
    /// returns only rows where `revoked = false AND expires_at > NOW()`.
    /// Ordered newest first (`created_at DESC`).
    async fn list_sessions_paginated(
        &self,
        user_id_filter: Option<Uuid>,
        include_revoked: bool,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<Session>, DomainError>;
}

// ============================================================================
// Device Authorization Grant Port (RFC 8628)
// ============================================================================

pub trait DeviceCodeStoragePort: Send + Sync + 'static {
    /// Persist a new device code flow
    async fn create_device_code(&self, device_code: DeviceCode) -> Result<DeviceCode, DomainError>;

    /// Find a device code by its opaque device_code token (used by client polling)
    async fn get_by_device_code(&self, device_code: &str) -> Result<DeviceCode, DomainError>;

    /// Find a pending device code by the short user_code (used on verification page)
    async fn get_pending_by_user_code(&self, user_code: &str) -> Result<DeviceCode, DomainError>;

    /// Update a device code (status change, token storage, poll timestamp, etc.)
    async fn update_device_code(&self, device_code: DeviceCode) -> Result<(), DomainError>;

    /// Delete expired device codes (cleanup job)
    async fn delete_expired(&self) -> Result<u64, DomainError>;

    /// List authorized device codes for a user (for UI management)
    async fn list_by_user(&self, user_id: Uuid) -> Result<Vec<DeviceCode>, DomainError>;

    /// Delete a specific device code by ID (revocation)
    async fn delete_by_id(&self, id: Uuid) -> Result<(), DomainError>;
}

// ============================================================================
// App Password Storage Port
// ============================================================================

/// Storage port for application-specific passwords (HTTP Basic Auth for DAV clients).
pub trait AppPasswordStoragePort: Send + Sync + 'static {
    /// Persist a new app password (hash already computed).
    async fn create(&self, app_password: AppPassword) -> Result<AppPassword, DomainError>;

    /// Get all active (non-expired) app passwords for a user.
    async fn list_by_user(&self, user_id: Uuid) -> Result<Vec<AppPassword>, DomainError>;

    /// Get a specific app password by ID.
    async fn get_by_id(&self, id: Uuid) -> Result<AppPassword, DomainError>;

    /// Get all active app passwords for a user ID (for Basic auth verification).
    /// This includes the password hash for verification.
    async fn get_active_by_user_id(&self, user_id: Uuid) -> Result<Vec<AppPassword>, DomainError>;

    /// Update the `last_used_at` timestamp after a successful authentication.
    async fn touch_last_used(&self, id: Uuid) -> Result<(), DomainError>;

    /// Get active app passwords for a user filtered by token prefix (first 8 chars).
    /// More efficient than `get_active_by_user_id` when the password prefix is known.
    async fn get_active_by_user_prefix(
        &self,
        user_id: Uuid,
        prefix: &str,
    ) -> Result<Vec<AppPassword>, DomainError>;

    /// Deactivate (soft-delete) an app password, scoped to the owning user.
    async fn revoke(&self, id: Uuid, user_id: Uuid) -> Result<(), DomainError>;

    /// Delete an app password owned by a specific user. Returns true if found and deleted.
    async fn delete_by_user_and_id(&self, id: Uuid, user_id: Uuid) -> Result<bool, DomainError>;

    /// Hard-delete expired/revoked app passwords (cleanup).
    async fn delete_expired(&self) -> Result<u64, DomainError>;
}
