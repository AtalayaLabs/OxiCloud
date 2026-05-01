use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

// ============================================================================
// OIDC Settings DTOs (Admin Panel)
// ============================================================================

/// Current OIDC settings returned to admin UI (secrets masked)
#[derive(Debug, Serialize, Deserialize)]
pub struct OidcSettingsDto {
    pub enabled: bool,
    pub issuer_url: String,
    pub client_id: String,
    /// True if a client secret is configured (never reveals the actual value)
    pub client_secret_set: bool,
    pub scopes: String,
    pub auto_provision: bool,
    pub admin_groups: String,
    pub disable_password_login: bool,
    pub provider_name: String,
    /// Auto-generated callback URL the admin must register in their IdP
    pub callback_url: String,
    /// Field names overridden by environment variables (read-only in UI)
    pub env_overrides: Vec<String>,
}

/// Request body for saving OIDC settings from the admin panel
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct SaveOidcSettingsDto {
    pub enabled: bool,
    pub issuer_url: String,
    pub client_id: String,
    /// Only update if provided and non-empty (None = keep existing)
    pub client_secret: Option<String>,
    pub scopes: Option<String>,
    pub auto_provision: Option<bool>,
    pub admin_groups: Option<String>,
    pub disable_password_login: Option<bool>,
    pub provider_name: Option<String>,
}

/// Request body for testing OIDC discovery
#[derive(Debug, Serialize, Deserialize)]
pub struct TestOidcConnectionDto {
    pub issuer_url: String,
}

/// Result of OIDC connection test
#[derive(Debug, Serialize, Deserialize)]
pub struct OidcTestResultDto {
    pub success: bool,
    pub message: String,
    pub issuer: Option<String>,
    pub authorization_endpoint: Option<String>,
    pub token_endpoint: Option<String>,
    pub userinfo_endpoint: Option<String>,
    /// Suggested provider name (derived from issuer hostname)
    pub provider_name_suggestion: Option<String>,
}

// ============================================================================
// Admin User Management DTOs
// ============================================================================

/// Request body for updating a user's role
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct UpdateUserRoleDto {
    pub role: String,
}

/// Request body for updating a user's active status
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct UpdateUserActiveDto {
    pub active: bool,
}

/// Request body for updating a user's storage quota
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct UpdateUserQuotaDto {
    /// Quota in bytes. Use 0 for unlimited.
    pub quota_bytes: i64,
}

/// Request body for admin-created users
#[derive(Debug, Serialize, Deserialize, Clone, ToSchema)]
pub struct AdminCreateUserDto {
    pub username: String,
    pub password: String,
    /// Optional — if omitted, a placeholder email is generated
    pub email: Option<String>,
    /// "admin" or "user"; defaults to "user"
    pub role: Option<String>,
    /// Storage quota in bytes; 0 = unlimited. If omitted, uses role default.
    pub quota_bytes: Option<i64>,
    /// Whether the account is active; defaults to true
    pub active: Option<bool>,
}

/// Request body for admin password reset
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct AdminResetPasswordDto {
    pub new_password: String,
}

/// Query parameters for listing users
#[derive(Debug, Serialize, Deserialize)]
pub struct ListUsersQueryDto {
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

/// Dashboard statistics
#[derive(Debug, Serialize, Deserialize)]
pub struct DashboardStatsDto {
    // System info
    pub server_version: String,
    pub auth_enabled: bool,
    pub oidc_configured: bool,
    pub quotas_enabled: bool,
    // User stats
    pub total_users: i64,
    pub active_users: i64,
    pub admin_users: i64,
    // Storage stats
    pub total_quota_bytes: i64,
    pub total_used_bytes: i64,
    pub storage_usage_percent: f64,
    pub users_over_80_percent: i64,
    pub users_over_quota: i64,
    pub registration_enabled: bool,
}

// ============================================================================
// Storage Settings DTOs (Admin Panel)
// ============================================================================

/// Current storage settings returned to admin UI (secrets masked)
#[derive(Debug, Serialize, Deserialize)]
pub struct StorageSettingsDto {
    /// Active backend type: "local" or "s3"
    pub backend: String,
    pub s3_endpoint_url: Option<String>,
    pub s3_bucket: Option<String>,
    pub s3_region: Option<String>,
    /// True if an access key is configured (never reveals the actual value)
    pub s3_access_key_set: bool,
    /// True if a secret key is configured (never reveals the actual value)
    pub s3_secret_key_set: bool,
    pub s3_force_path_style: bool,
    /// Field names overridden by environment variables (read-only in UI)
    pub env_overrides: Vec<String>,
    // ── Current stats ──
    pub current_backend: String,
    pub total_blobs: u64,
    pub total_bytes_stored: u64,
    pub dedup_ratio: f64,
}

/// Request body for saving storage settings from the admin panel
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct SaveStorageSettingsDto {
    pub backend: String,
    pub s3_endpoint_url: Option<String>,
    pub s3_bucket: Option<String>,
    pub s3_region: Option<String>,
    /// Only update if provided and non-empty (None = keep existing)
    pub s3_access_key: Option<String>,
    /// Only update if provided and non-empty (None = keep existing)
    pub s3_secret_key: Option<String>,
    pub s3_force_path_style: Option<bool>,
}

/// Request body for testing a storage connection
#[derive(Debug, Serialize, Deserialize)]
pub struct TestStorageConnectionDto {
    pub backend: String,
    pub s3_endpoint_url: Option<String>,
    pub s3_bucket: Option<String>,
    pub s3_region: Option<String>,
    pub s3_access_key: Option<String>,
    pub s3_secret_key: Option<String>,
    pub s3_force_path_style: Option<bool>,
}

/// Result of a storage connection test
#[derive(Debug, Serialize, Deserialize)]
pub struct StorageTestResultDto {
    pub connected: bool,
    pub message: String,
    pub backend_type: String,
    pub available_bytes: Option<u64>,
}

// ============================================================================
// Migration DTOs (Admin Panel — Storage Migration)
// ============================================================================

/// Migration progress returned by `GET /api/admin/storage/migration`.
/// Re-exports the `MigrationState` shape for the admin UI.
#[derive(Debug, Serialize, Deserialize)]
pub struct MigrationStateDto {
    pub status: String,
    pub total_blobs: u64,
    pub migrated_blobs: u64,
    pub migrated_bytes: u64,
    pub failed_blobs: Vec<String>,
    pub started_at: Option<String>,
    pub completed_at: Option<String>,
    /// Estimated throughput in bytes/sec (for UI ETA calculation).
    pub throughput_bytes_per_sec: Option<f64>,
}

/// Request body for `POST /api/admin/storage/migration/start`.
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct StartMigrationDto {
    /// How many blobs to copy in parallel (default: 4).
    pub concurrency: Option<usize>,
}

/// Request body (empty) for `POST /api/admin/storage/migration/verify`.
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct VerifyMigrationDto {
    /// Number of random blobs to sample-check (default: 100).
    pub sample_size: Option<usize>,
}
