//! OAuth 2.0 Device Authorization Grant service (RFC 8628).
//!
//! Orchestrates the full device flow:
//!   1. `initiate` — generates device_code + user_code, stores in DB
//!   2. `verify_user_code` — looks up pending code for the verification page
//!   3. `approve` — user approves, tokens are generated and stored
//!   4. `deny` — user denies the request
//!   5. `poll` — client polls by device_code; returns tokens or status error
//!   6. `cleanup_expired` — background job to purge stale entries

use std::sync::Arc;

use crate::application::dtos::device_auth_dto::*;
use crate::application::ports::auth_ports::SessionStoragePort;
use crate::application::ports::auth_ports::{
    DeviceCodeStoragePort, TokenServicePort, UserStoragePort,
};
use crate::common::errors::{DomainError, ErrorKind};
use crate::domain::entities::device_code::{DeviceCode, DeviceCodeStatus};
use crate::domain::entities::session::Session;
use crate::infrastructure::repositories::pg::DeviceCodePgRepository;
use crate::infrastructure::repositories::pg::SessionPgRepository;
use crate::infrastructure::repositories::pg::UserPgRepository;
use crate::infrastructure::services::jwt_service::JwtTokenService;
use uuid::Uuid;

/// Default device code lifetime: 15 minutes (RFC 8628 recommends 5-30 min).
const DEVICE_CODE_LIFETIME_SECS: i64 = 900;

/// Default polling interval in seconds (RFC 8628 §3.2 recommends 5s).
const DEFAULT_POLL_INTERVAL: i32 = 5;

/// Length of the device_code (hex-encoded, 64 chars = 32 bytes).
const DEVICE_CODE_BYTES: usize = 32;

/// User code format: 4 uppercase letters + hyphen + 4 digits → "ABCD-1234"
/// Short enough to type, long enough to avoid collisions with 26^4 * 10^4 = ~4.5 billion combos.
const USER_CODE_LETTER_LEN: usize = 4;
const USER_CODE_DIGIT_LEN: usize = 4;

/// How many of a user's sessions to scan for device-origin rows.
///
/// Generous by design: a person has a handful of devices, and the
/// alternative to a limit is an unbounded read on a user-triggered
/// endpoint. If someone genuinely exceeds this they have a bigger
/// problem than a truncated list.
const DEVICE_LIST_LIMIT: i64 = 200;

/// Recover the client name `approve()` stamped into `user_agent`.
///
/// It is written as `device:{client_name}`, so the prefix doubles as
/// the marker that a row came from the device flow. Falls back to the
/// raw value if the prefix is missing — a session whose user_agent was
/// set by something else should still render as itself rather than as
/// an empty cell.
fn device_client_name(user_agent: Option<&str>) -> String {
    match user_agent {
        Some(ua) => ua.strip_prefix("device:").unwrap_or(ua).to_string(),
        None => "unknown device".to_string(),
    }
}

pub struct DeviceAuthService {
    device_code_storage: Arc<DeviceCodePgRepository>,
    token_service: Arc<JwtTokenService>,
    user_storage: Arc<UserPgRepository>,
    session_storage: Arc<SessionPgRepository>,
    /// Base URL of the server (e.g. "https://cloud.example.com")
    base_url: String,
    /// Narrow capability to drop a user's cached flags, so revoking a
    /// device takes effect on its next request instead of within the
    /// cache TTL. A port rather than the whole auth service: this
    /// service revokes sessions, it has no business minting them.
    flags_invalidator:
        Arc<dyn crate::application::ports::authorization_ports::UserFlagsInvalidatorPort>,
    /// `OXICLOUD_REQUIRE_VERIFIED_EMAIL`, snapshotted like
    /// `AuthApplicationService` does. Needed because approving a device
    /// code mints a session, so this door owes the same account checks
    /// as every other one.
    require_verified_email: bool,
}

impl DeviceAuthService {
    pub fn new(
        device_code_storage: Arc<DeviceCodePgRepository>,
        token_service: Arc<JwtTokenService>,
        user_storage: Arc<UserPgRepository>,
        session_storage: Arc<SessionPgRepository>,
        base_url: String,
        require_verified_email: bool,
        flags_invalidator: Arc<
            dyn crate::application::ports::authorization_ports::UserFlagsInvalidatorPort,
        >,
    ) -> Self {
        Self {
            device_code_storage,
            token_service,
            user_storage,
            session_storage,
            base_url,
            require_verified_email,
            flags_invalidator,
        }
    }

    // ========================================================================
    // 1. Initiate — called by the DAV client
    // ========================================================================

    /// Start a new device authorization flow.
    ///
    /// Returns the response that the client displays to the user.
    pub async fn initiate(
        &self,
        req: DeviceAuthorizeRequestDto,
    ) -> Result<DeviceAuthorizeResponseDto, DomainError> {
        let device_code_token = generate_device_code();
        let user_code = generate_user_code();

        let verification_uri = format!("{}/device", self.base_url.trim_end_matches('/'));
        let verification_uri_complete = format!("{}?code={}", verification_uri, user_code);

        let dc = DeviceCode::new(
            device_code_token.clone(),
            user_code.clone(),
            req.client_name,
            req.scope,
            verification_uri.clone(),
            Some(verification_uri_complete.clone()),
            DEVICE_CODE_LIFETIME_SECS,
            DEFAULT_POLL_INTERVAL,
        );

        let dc = self.device_code_storage.create_device_code(dc).await?;

        tracing::info!(
            "Device auth flow initiated: user_code={}, expires_in={}s",
            user_code,
            DEVICE_CODE_LIFETIME_SECS
        );

        Ok(DeviceAuthorizeResponseDto {
            device_code: device_code_token,
            user_code,
            verification_uri,
            verification_uri_complete: Some(verification_uri_complete),
            expires_in: dc.seconds_remaining(),
            interval: DEFAULT_POLL_INTERVAL,
        })
    }

    // ========================================================================
    // 2. Verify — user opens the verification page, looks up pending code
    // ========================================================================

    /// Look up a pending device code by user_code for the verification page.
    pub async fn verify_user_code(
        &self,
        user_code: &str,
    ) -> Result<DeviceVerifyInfoDto, DomainError> {
        let normalized = user_code.trim().to_uppercase().replace(' ', "");

        match self
            .device_code_storage
            .get_pending_by_user_code(&normalized)
            .await
        {
            Ok(dc) => {
                if dc.is_expired() {
                    return Ok(DeviceVerifyInfoDto {
                        client_name: dc.client_name().to_string(),
                        scopes: dc.scopes().to_string(),
                        valid: false,
                    });
                }
                Ok(DeviceVerifyInfoDto {
                    client_name: dc.client_name().to_string(),
                    scopes: dc.scopes().to_string(),
                    valid: true,
                })
            }
            Err(_) => Ok(DeviceVerifyInfoDto {
                client_name: String::new(),
                scopes: String::new(),
                valid: false,
            }),
        }
    }

    // ========================================================================
    // 3. Approve — authenticated user approves the device code
    // ========================================================================

    /// Approve a device code, generating tokens for the polling client.
    ///
    /// * `user_code` — the code from the verification page
    /// * `user_id` — the authenticated user's ID (from session/JWT)
    /// * `client_ip` — the IP of the approver's browser; stamped on the
    ///   created `auth.sessions` row so the admin "IP" column has a value
    ///   consistent with other origins (login/magic/oidc all capture
    ///   creation-time IP). `None` when the handler couldn't derive one.
    pub async fn approve(
        &self,
        user_code: &str,
        user_id: Uuid,
        client_ip: Option<String>,
    ) -> Result<(), DomainError> {
        let normalized = user_code.trim().to_uppercase().replace(' ', "");

        let mut dc = self
            .device_code_storage
            .get_pending_by_user_code(&normalized)
            .await?;

        if dc.is_expired() {
            return Err(DomainError::new(
                ErrorKind::AccessDenied,
                "DeviceCode",
                "Device code has expired. Please start a new authorization flow.",
            ));
        }

        // Fetch user to generate tokens
        let user = self.user_storage.get_user_by_id(user_id).await?;

        // Account checks. This door ran none of them: approving a device
        // code minted a session for a deactivated account, or one with an
        // unverified address under `OXICLOUD_REQUIRE_VERIFIED_EMAIL`,
        // because it went from the user row straight to `Session::new`.
        //
        // The same function the password and OPAQUE doors use — this
        // service holds no `AuthApplicationService`, and a second copy of
        // the rule here is exactly how the door came to skip it.
        // `admit` because this door MINTS a session; the proof is what
        // `Session::new` requires, so the check cannot be skipped here
        // without failing to compile.
        let admitted = crate::application::services::auth_application_service::admit(
            user,
            self.require_verified_email,
            "device",
        )?;
        let user = admitted.user();

        // Generate internal JWT access token + refresh token. Device-
        // authorization sessions (RFC 8628) are for CLI / TV / device
        // clients that don't run WebCrypto — always unbound (`None`
        // for the `dpop_jkt` param), which the DPoP middleware exempts
        // from proof requirements. See `docs/plan/dpop.md` Gate 9.
        //
        // Session constructed first so its id feeds the token's
        // `sid` claim — the auth middleware uses this to stamp
        // per-session liveness without a DB round trip
        // (`docs/plan/sessions.md`).
        let refresh_token = self.token_service.generate_refresh_token();
        let session = Session::new(
            &admitted,
            refresh_token.clone(),
            client_ip, // created_ip — approver's browser IP
            Some(format!("device:{}", dc.client_name())), // user_agent
            self.token_service.refresh_token_expiry_days(),
            Uuid::new_v4(),
            crate::domain::entities::session::SessionOrigin::Device,
        );
        let access_token =
            self.token_service
                .generate_access_token(user, Some(session.id()), None)?;
        self.session_storage.create_session(session).await?;

        // Store tokens on the device code entity
        dc.authorize(user_id, access_token, refresh_token);
        self.device_code_storage.update_device_code(dc).await?;

        tracing::info!(
            "Device code approved by user {} (user_code={})",
            user_id,
            normalized
        );

        Ok(())
    }

    // ========================================================================
    // 4. Deny — authenticated user denies the device code
    // ========================================================================

    pub async fn deny(&self, user_code: &str) -> Result<(), DomainError> {
        let normalized = user_code.trim().to_uppercase().replace(' ', "");

        let mut dc = self
            .device_code_storage
            .get_pending_by_user_code(&normalized)
            .await?;

        dc.deny();
        self.device_code_storage.update_device_code(dc).await?;

        tracing::info!("Device code denied (user_code={})", normalized);

        Ok(())
    }

    // ========================================================================
    // 5. Poll — client polls by device_code for tokens
    // ========================================================================

    /// Client polls for tokens. Returns:
    /// - `Ok(DeviceTokenSuccessDto)` if authorized
    /// - `Err` with specific RFC 8628 error codes for pending/slow_down/expired/denied
    pub async fn poll(&self, device_code: &str) -> Result<DeviceTokenSuccessDto, DevicePollError> {
        let mut dc = self
            .device_code_storage
            .get_by_device_code(device_code)
            .await
            .map_err(|_| DevicePollError::InvalidDeviceCode)?;

        // Check expiry first
        if dc.is_expired() && dc.status() == DeviceCodeStatus::Pending {
            let mut expired_dc = dc.clone();
            expired_dc.mark_expired();
            let _ = self
                .device_code_storage
                .update_device_code(expired_dc)
                .await;
            return Err(DevicePollError::ExpiredToken);
        }

        match dc.status() {
            DeviceCodeStatus::Pending => {
                // Check for slow_down (polling too fast)
                if dc.is_polling_too_fast() {
                    return Err(DevicePollError::SlowDown);
                }
                // Record this poll
                dc.record_poll();
                let _ = self.device_code_storage.update_device_code(dc).await;
                Err(DevicePollError::AuthorizationPending)
            }
            DeviceCodeStatus::Authorized => {
                let access_token = dc.access_token().unwrap_or_default().to_string();
                let refresh_token = dc.refresh_token().unwrap_or_default().to_string();
                let scope = dc.scopes().to_string();

                // Delete the device code row now that tokens have been retrieved.
                // This prevents plain-text tokens from lingering in the database.
                let _ = self.device_code_storage.delete_by_id(dc.id()).await;

                Ok(DeviceTokenSuccessDto {
                    access_token,
                    token_type: "Bearer".to_string(),
                    refresh_token,
                    expires_in: self.token_service.refresh_token_expiry_secs(),
                    scope,
                })
            }
            DeviceCodeStatus::Denied => Err(DevicePollError::AccessDenied),
            DeviceCodeStatus::Expired => Err(DevicePollError::ExpiredToken),
        }
    }

    // ========================================================================
    // 6. Cleanup — purge expired entries
    // ========================================================================

    pub async fn cleanup_expired(&self) -> Result<u64, DomainError> {
        let deleted = self.device_code_storage.delete_expired().await?;
        if deleted > 0 {
            tracing::info!("Device code cleanup: {} expired entries removed", deleted);
        }
        Ok(deleted)
    }

    // ========================================================================
    // 7. List — the user's CONNECTED devices
    // ========================================================================

    /// Devices that actually hold a credential, read from
    /// `auth.sessions WHERE origin = 'device'`.
    ///
    /// This used to read `auth.device_codes`, which made it a list of
    /// things that are NOT connected. Four facts combined to produce
    /// that:
    ///
    /// 1. [`Self::approve`] creates a `Session` with
    ///    `origin = Device` — the durable record of a connected device.
    /// 2. [`Self::poll`] DELETES the `device_codes` row once the tokens
    ///    are collected, deliberately, so plain-text tokens do not
    ///    linger in the database.
    /// 3. this listed `device_codes`, unfiltered by status;
    /// 4. [`Self::revoke_device`] deleted a `device_codes` row.
    ///
    /// So a device that had actually connected was absent from its own
    /// list and unrevokable from it, while the rows that DID appear were
    /// authorizations nobody had collected yet. The session row is the
    /// one that matters and it carries more besides: genuine liveness,
    /// and both IP columns.
    ///
    /// Revoked sessions are excluded — a revoked device is not a
    /// connected one. Expired-but-unrevoked rows are included and
    /// labelled, because "this stopped working yesterday" is something
    /// an operator wants to see rather than have silently vanish.
    pub async fn list_user_devices(
        &self,
        user_id: Uuid,
    ) -> Result<Vec<DeviceInfoDto>, DomainError> {
        // Reuses the admin panel's lister rather than adding a near-copy
        // query. `include_revoked = false` does the filtering that
        // matters in SQL; the `origin` filter happens here because a
        // user has a handful of sessions, not thousands, and a second
        // query shape would have to be kept in step with this one.
        let sessions = self
            .session_storage
            .list_sessions_paginated(Some(user_id), false, DEVICE_LIST_LIMIT, 0)
            .await?;

        Ok(sessions
            .into_iter()
            .filter(|s| s.origin() == crate::domain::entities::session::SessionOrigin::Device)
            .map(|s| DeviceInfoDto {
                id: s.id().to_string(),
                client_name: device_client_name(s.user_agent()),
                // `created_at` on a device session IS the moment the
                // human approved it: `approve()` is what creates the
                // row. The device's own earlier request-time lived on
                // the `device_codes` row and is gone with it.
                created_at: s.created_at().to_rfc3339(),
                expires_at: s.expires_at().to_rfc3339(),
                last_seen_at: s.last_seen_at().to_rfc3339(),
                created_ip: s.created_ip().map(str::to_owned),
                last_seen_ip: s.last_seen_ip().map(str::to_owned),
                is_bound: s.dpop_jkt().is_some(),
                status: if s.is_expired() { "expired" } else { "active" }.to_string(),
            })
            .collect())
    }

    // ========================================================================
    // 8. Revoke — user revokes a device authorization
    // ========================================================================

    /// Revoke a connected device by revoking its SESSION.
    ///
    /// `device_id` is a session id, matching what [`Self::list_user_devices`]
    /// returns. It used to delete a `device_codes` row, which could only
    /// ever cancel an authorization the device had not collected yet —
    /// the one case where there is nothing to revoke.
    ///
    /// Revoking the session is what makes this stop a device that IS
    /// connected: per-session revocation puts the `sid` in the
    /// revoked set, so the access token it already holds is refused on
    /// its next request rather than living out its hour.
    ///
    /// Ownership is checked against the caller's own device sessions, so
    /// a caller cannot revoke another user's device by guessing an id —
    /// and an id that is already revoked is no longer in that list,
    /// which is what makes a second revoke answer 404 rather than
    /// silently succeeding.
    pub async fn revoke_device(&self, device_id: Uuid, user_id: Uuid) -> Result<(), DomainError> {
        let devices = self.list_user_devices(user_id).await?;
        let owned = devices.iter().any(|d| d.id == device_id.to_string());
        if !owned {
            tracing::info!(
                target: "audit",
                event = "auth.device_revoke_rejected",
                reason = "not_found_or_not_owned",
                caller_id = %user_id,
                session_id = %device_id,
                "👮🏻‍♂️ device revoke refused — no connected device with that id for this user",
            );
            return Err(DomainError::new(
                ErrorKind::NotFound,
                "Device",
                "Connected device not found or not owned by you",
            ));
        }

        self.session_storage.revoke_session(device_id).await?;
        // Drop the cached flags so the revoked `sid` is in the set on the
        // device's very next request, not within the 30 s TTL.
        self.flags_invalidator.invalidate_user_flags(user_id).await;

        tracing::info!(
            target: "audit",
            event = "auth.device_revoked",
            caller_id = %user_id,
            session_id = %device_id,
            "👮🏻‍♂️ connected device revoked by its owner",
        );
        Ok(())
    }
}

// ============================================================================
// Poll error (typed for RFC 8628 error responses)
// ============================================================================

/// Typed errors for the device token polling endpoint (RFC 8628 §3.5).
#[derive(Debug)]
pub enum DevicePollError {
    /// The authorization request is still pending (user hasn't acted yet).
    AuthorizationPending,
    /// The client is polling too fast; increase the interval.
    SlowDown,
    /// The user denied the authorization request.
    AccessDenied,
    /// The device_code has expired.
    ExpiredToken,
    /// The device_code is not recognized.
    InvalidDeviceCode,
}

impl DevicePollError {
    /// RFC 8628 error string for the JSON response.
    pub fn error_code(&self) -> &'static str {
        match self {
            Self::AuthorizationPending => "authorization_pending",
            Self::SlowDown => "slow_down",
            Self::AccessDenied => "access_denied",
            Self::ExpiredToken => "expired_token",
            Self::InvalidDeviceCode => "invalid_grant",
        }
    }

    pub fn description(&self) -> &'static str {
        match self {
            Self::AuthorizationPending => {
                "The authorization request is still pending. Continue polling."
            }
            Self::SlowDown => "You are polling too frequently. Please slow down.",
            Self::AccessDenied => "The user denied the authorization request.",
            Self::ExpiredToken => {
                "The device_code has expired. Please start a new authorization flow."
            }
            Self::InvalidDeviceCode => "The device_code is not recognized.",
        }
    }

    /// HTTP status code per RFC 8628 §3.5:
    /// - authorization_pending and slow_down: 400
    /// - access_denied: 403
    /// - expired_token: 400
    pub fn http_status(&self) -> u16 {
        match self {
            Self::AuthorizationPending | Self::SlowDown | Self::ExpiredToken => 400,
            Self::AccessDenied => 403,
            Self::InvalidDeviceCode => 400,
        }
    }
}

// ============================================================================
// Additional DTOs (used by service, not in the handler module)
// ============================================================================

/// One CONNECTED device, projected from its session row.
///
/// Two fields of the `device_codes`-era shape are gone, and their
/// absence is the point:
///
/// * `scopes` — recorded on the `device_codes` row, which `poll()`
///   deletes once the tokens are collected, so a connected device's
///   approved scopes are not retrievable. They are also not enforced
///   yet (`scope-gates.md`); the field returns when a session carries a
///   `ScopeSet` and the value is real.
/// * `authorized_at` — would duplicate `created_at`, since the session
///   row IS created at the moment of approval. The device's own earlier
///   request time lived on the deleted row.
#[derive(Debug, serde::Serialize, utoipa::ToSchema)]
pub struct DeviceInfoDto {
    /// The SESSION id — what `DELETE /api/auth/device/devices/{id}`
    /// takes. Previously a `device_codes` id, which named a row that no
    /// longer existed for any device that had actually connected.
    pub id: String,
    pub client_name: String,
    /// `active`, or `expired` for a row past `expires_at` that nobody
    /// revoked. Revoked sessions are not listed at all.
    pub status: String,
    /// When the device was AUTHORISED — the approval moment.
    pub created_at: String,
    pub expires_at: String,
    /// Genuine liveness, maintained per request. This is the field that
    /// makes the list answer "is this thing still talking to me?",
    /// which the old one could not.
    pub last_seen_at: String,
    /// Where the human approved it from. The device never reaches the
    /// approval endpoint, so this is the approver's browser.
    pub created_ip: Option<String>,
    /// Where the device is actually being used from, to within the
    /// last-seen flush interval. A mismatch against `created_ip` is
    /// worth an operator's attention.
    pub last_seen_ip: Option<String>,
    /// Whether the session is DPoP-bound. Device-flow sessions are
    /// unbound by design (CLI and TV clients have no WebCrypto), so a
    /// `true` here would be surprising and worth surfacing.
    pub is_bound: bool,
}

// ============================================================================
// Helpers
// ============================================================================

/// Generate a cryptographically random device_code (hex-encoded).
fn generate_device_code() -> String {
    use rand_core::{OsRng, RngCore};
    let mut bytes = [0u8; DEVICE_CODE_BYTES];
    OsRng.fill_bytes(&mut bytes);
    hex::encode(bytes)
}

/// Generate a human-readable user_code in the format "ABCD-1234".
fn generate_user_code() -> String {
    use rand_core::{OsRng, RngCore};
    let mut rng_bytes = [0u8; 8];
    OsRng.fill_bytes(&mut rng_bytes);

    let letters: String = (0..USER_CODE_LETTER_LEN)
        .map(|i| {
            let b = rng_bytes[i] % 26;
            (b'A' + b) as char
        })
        .collect();

    let digits: String = (0..USER_CODE_DIGIT_LEN)
        .map(|i| {
            let b = rng_bytes[USER_CODE_LETTER_LEN + i] % 10;
            (b'0' + b) as char
        })
        .collect();

    format!("{}-{}", letters, digits)
}
