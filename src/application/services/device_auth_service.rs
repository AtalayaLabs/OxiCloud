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

pub struct DeviceAuthService {
    device_code_storage: Arc<DeviceCodePgRepository>,
    token_service: Arc<JwtTokenService>,
    user_storage: Arc<UserPgRepository>,
    session_storage: Arc<SessionPgRepository>,
    /// Base URL of the server (e.g. "https://cloud.example.com")
    base_url: String,
}

impl DeviceAuthService {
    pub fn new(
        device_code_storage: Arc<DeviceCodePgRepository>,
        token_service: Arc<JwtTokenService>,
        user_storage: Arc<UserPgRepository>,
        session_storage: Arc<SessionPgRepository>,
        base_url: String,
    ) -> Self {
        Self {
            device_code_storage,
            token_service,
            user_storage,
            session_storage,
            base_url,
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
    pub async fn approve(&self, user_code: &str, user_id: Uuid) -> Result<(), DomainError> {
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

        // Generate internal JWT access token + refresh token. Device-
        // authorization sessions (RFC 8628) are for CLI / TV / device
        // clients that don't run WebCrypto — always unbound (`None`
        // for the `dpop_jkt` param), which the DPoP middleware exempts
        // from proof requirements. See `docs/plan/dpop.md` Gate 9.
        let access_token = self.token_service.generate_access_token(&user, None)?;
        let refresh_token = self.token_service.generate_refresh_token();

        // Persist refresh token as a session
        let session = Session::new(
            user_id,
            refresh_token.clone(),
            None,                                         // ip_address
            Some(format!("device:{}", dc.client_name())), // user_agent
            self.token_service.refresh_token_expiry_days(),
            Uuid::new_v4(),
        );
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
    // 7. List — user's authorized devices (for UI)
    // ========================================================================

    pub async fn list_user_devices(
        &self,
        user_id: Uuid,
    ) -> Result<Vec<DeviceInfoDto>, DomainError> {
        let codes = self.device_code_storage.list_by_user(user_id).await?;
        Ok(codes
            .into_iter()
            .map(|dc| DeviceInfoDto {
                id: dc.id().to_string(),
                client_name: dc.client_name().to_string(),
                scopes: dc.scopes().to_string(),
                status: dc.status().as_str().to_string(),
                created_at: dc.created_at().to_rfc3339(),
                authorized_at: dc.authorized_at().map(|t| t.to_rfc3339()),
                expires_at: dc.expires_at().to_rfc3339(),
            })
            .collect())
    }

    // ========================================================================
    // 8. Revoke — user revokes a device authorization
    // ========================================================================

    pub async fn revoke_device(&self, device_id: Uuid, user_id: Uuid) -> Result<(), DomainError> {
        // Verify ownership before deleting
        let devices = self.device_code_storage.list_by_user(user_id).await?;
        let found = devices.iter().any(|d| d.id() == device_id);
        if !found {
            return Err(DomainError::new(
                ErrorKind::NotFound,
                "DeviceCode",
                "Device authorization not found or not owned by you",
            ));
        }
        self.device_code_storage.delete_by_id(device_id).await
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

/// DTO for listing authorized devices in the user's profile.
#[derive(Debug, serde::Serialize)]
pub struct DeviceInfoDto {
    pub id: String,
    pub client_name: String,
    pub scopes: String,
    pub status: String,
    pub created_at: String,
    pub authorized_at: Option<String>,
    pub expires_at: String,
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
