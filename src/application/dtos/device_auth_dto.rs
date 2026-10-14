//! DTOs for OAuth 2.0 Device Authorization Grant (RFC 8628).

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

// ============================================================================
// Request DTOs
// ============================================================================

/// POST /api/auth/device/authorize — request body
#[derive(Debug, Deserialize, ToSchema)]
pub struct DeviceAuthorizeRequestDto {
    /// Human-readable name of the client (e.g. "rclone", "DAVx⁵")
    #[serde(default = "default_client_name")]
    pub client_name: String,
    /// Comma-separated scopes (e.g. "webdav,caldav,carddav")
    #[serde(default = "default_scopes")]
    pub scope: String,
}

fn default_client_name() -> String {
    "Unknown Client".to_string()
}

fn default_scopes() -> String {
    "webdav,caldav,carddav".to_string()
}

/// POST /api/auth/device/verify — user submits the code from the browser
#[derive(Debug, Deserialize, ToSchema)]
pub struct DeviceVerifyRequestDto {
    /// The user_code displayed on the client device
    pub user_code: String,
    /// Whether the user approves ("approve") or denies ("deny")
    pub action: String,
}

/// POST /api/auth/device/token — client polls for tokens
#[derive(Debug, Deserialize, ToSchema)]
pub struct DeviceTokenRequestDto {
    /// The device_code received from the initial authorize call
    pub device_code: String,
    /// Must be "urn:ietf:params:oauth:grant-type:device_code"
    #[serde(default)]
    pub grant_type: String,
}

// ============================================================================
// Response DTOs
// ============================================================================

/// Response to POST /api/auth/device/authorize (RFC 8628 §3.2)
#[derive(Debug, Serialize, ToSchema)]
pub struct DeviceAuthorizeResponseDto {
    /// The device verification code
    pub device_code: String,
    /// The end-user verification code (short, human-readable)
    pub user_code: String,
    /// The end-user verification URI
    pub verification_uri: String,
    /// Optional: verification URI with user_code pre-filled
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verification_uri_complete: Option<String>,
    /// Lifetime in seconds of the device_code and user_code
    pub expires_in: i64,
    /// Minimum polling interval in seconds
    pub interval: i32,
}

/// Response to POST /api/auth/device/token when authorization is still pending
#[derive(Debug, Serialize, ToSchema)]
pub struct DeviceTokenPendingDto {
    pub error: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_description: Option<String>,
}

/// Response to POST /api/auth/device/token when authorization is complete
#[derive(Debug, Serialize, ToSchema)]
pub struct DeviceTokenSuccessDto {
    pub access_token: String,
    pub token_type: String,
    pub refresh_token: String,
    pub expires_in: i64,
    pub scope: String,
}

/// GET /api/auth/device/verify — info about the pending device code
#[derive(Debug, Serialize, ToSchema)]
pub struct DeviceVerifyInfoDto {
    /// The client name requesting access
    pub client_name: String,
    /// Scopes being requested
    pub scopes: String,
    /// Whether the user_code is valid and pending
    pub valid: bool,
}

/// Response to POST /api/auth/device/verify — approve/deny status
/// confirmation. Replaces the previous ad-hoc
/// `json!({"status": "approved"})` body so the TS SDK gets a typed
/// discriminant it can switch on.
#[derive(Debug, Serialize, ToSchema)]
pub struct DeviceVerifyActionResponseDto {
    /// `"approved"` or `"denied"` — mirrors the inbound `action` the
    /// user picked.
    pub status: String,
}
