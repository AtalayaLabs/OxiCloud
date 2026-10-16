//! DTOs for App Password (application-specific passwords for DAV clients).

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

// ============================================================================
// Request DTOs
// ============================================================================

/// POST /api/auth/app-passwords — create a new app password
#[derive(Debug, Deserialize, ToSchema)]
pub struct CreateAppPasswordRequestDto {
    /// Human-readable label (e.g. "DAVx5 on Pixel 8")
    pub label: String,
    /// Comma-separated scopes (defaults to all DAV protocols)
    #[serde(default = "default_scopes")]
    pub scopes: String,
    /// Optional expiration in days (None = never expires)
    pub expires_in_days: Option<u32>,
}

fn default_scopes() -> String {
    "webdav,caldav,carddav".to_string()
}

// ============================================================================
// Response DTOs
// ============================================================================

/// Response when an app password is created — includes the plain-text password
/// that is shown ONCE to the user.
#[derive(Debug, Serialize, ToSchema)]
pub struct AppPasswordCreatedResponseDto {
    /// Unique identifier for this app password.
    pub id: String,
    /// The label chosen by the user.
    pub label: String,
    /// The plain-text app password — shown only ONCE.
    /// Format: `oxicloud-XXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXX`
    pub password: String,
    /// The username to use with HTTP Basic Auth.
    pub username: String,
    /// Active scopes.
    pub scopes: String,
    /// Expiration date or null for never.
    pub expires_at: Option<String>,
    /// Usage instructions for common clients.
    pub instructions: AppPasswordInstructions,
}

/// Usage instructions included in the creation response.
#[derive(Debug, Serialize, ToSchema)]
pub struct AppPasswordInstructions {
    pub davx5: String,
    pub thunderbird: String,
    pub rclone: String,
    pub curl_example: String,
}

/// Summary of an app password (list view — never includes the plain-text password).
#[derive(Debug, Serialize, ToSchema)]
pub struct AppPasswordSummaryDto {
    pub id: String,
    pub label: String,
    /// First 8 chars of the token for identification.
    pub prefix: String,
    pub scopes: String,
    pub created_at: String,
    pub last_used_at: Option<String>,
    pub expires_at: Option<String>,
    pub active: bool,
}

/// Response for list endpoint.
#[derive(Debug, Serialize, ToSchema)]
pub struct AppPasswordListResponseDto {
    pub app_passwords: Vec<AppPasswordSummaryDto>,
    pub total: usize,
}

/// Response for revoke endpoint.
#[derive(Debug, Serialize, ToSchema)]
pub struct AppPasswordRevokeResponseDto {
    pub status: String,
    pub id: String,
}
