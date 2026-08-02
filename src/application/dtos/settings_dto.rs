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
    /// Ignored when `is_external = true` (external users have no storage).
    pub quota_bytes: Option<i64>,
    /// Whether the account is active; defaults to true
    pub active: Option<bool>,
    /// `true` to create a grant-only external user (no home folder, no
    /// storage quota). Defaults to `false` (internal user). External
    /// users authenticate via magic-link / OIDC / OCM federation —
    /// password is set but never used.
    pub is_external: Option<bool>,
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
    /// Return only the fields rendered by the paginated management table.
    /// Defaults to `false` so existing API clients keep the full user shape.
    pub summary: Option<bool>,
}

/// One row of the dashboard's quota panel — usage aggregate for a
/// single drive kind. Unlimited caps are excluded from `capped_quota_bytes`
/// and counted in `unlimited_count` so the panel can render the ratio
/// honestly ("X / Y over N capped drives · M unlimited").
#[derive(Debug, Serialize, Deserialize)]
pub struct DriveKindUsageDto {
    /// `"personal"` or `"shared"`.
    pub kind: String,
    /// Total bytes stored across drives of this kind. Excludes trashed
    /// files (see `bug_trash_excluded_from_quota` for the known gap).
    pub used_bytes: i64,
    /// Sum of caps over capped drives only. `None` when there are no
    /// capped drives of this kind (would otherwise report `0 / 0`
    /// meaninglessly).
    pub capped_quota_bytes: Option<i64>,
    /// Count of drives (personal: users) with no cap. Personal-kind
    /// unlimited = `auth.users.storage_quota_bytes = 0`; shared-kind
    /// unlimited = `storage.drives.quota_bytes IS NULL`.
    pub unlimited_count: i64,
    /// Count of drives with a numeric cap. Used to hide rows with
    /// zero drives and denominate the ratio.
    pub capped_count: i64,
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
    // ── Per-drive-kind quota accounting ──
    // One row per drive kind (personal, shared). Pre-dedup, logical
    // file sizes summed from `drives.used_bytes` (personal rolls up
    // via the user envelope). Cap sums exclude unlimited entries;
    // `unlimited_count` tracks them separately so the ratio stays
    // honest.
    pub drive_usage: Vec<DriveKindUsageDto>,
    pub users_over_80_percent: i64,
    pub users_over_quota: i64,
    // ── Backend physical accounting ──
    // Bytes actually stored on the active backend (`storage.blobs`
    // aggregate) plus the dedup ratio (referenced / stored).
    // `total_bytes_stored` is typically << `total_used_bytes` on a
    // healthy deployment — dedup + shared blobs mean many user file
    // rows resolve to one physical blob. `None` when the dedup
    // stats service is unavailable or errored (dashboard renders as
    // "—" in that case).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_bytes_stored: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dedup_ratio: Option<f64>,
    pub registration_enabled: bool,
}

// ============================================================================
// Storage Settings DTOs (Admin Panel)
// ============================================================================

/// Current storage settings returned to admin UI.
///
/// Post-multi-entry (`docs/plan/storage-multi-entry.md`) this exposes:
/// - the entries declared in `.env` (safe: `location_hint` shows
///   provider+bucket, credential-related fields never appear),
/// - the name of the active entry (backend selection is admin-observable),
/// - the read-only flag (drives the UI banner during migration),
/// - the currently-live backend type + dedup stats (informational).
///
/// The pre-multi-entry `s3_*` / `backend` / `env_overrides` fields
/// used to also appear here — they duplicated `entries[]` and leaked
/// stale legacy admin_settings rows, so slice-6 dropped them. Consumers
/// wanting per-provider details read them off `entries[i].backend` and
/// `entries[i].location_hint` instead.
#[derive(Debug, Serialize, Deserialize)]
pub struct StorageSettingsDto {
    // ── Current stats — pertain to the running process ──
    /// Backend type currently in use (`"local"` / `"s3"` / `"azure"`) —
    /// what the LIVE `blob_backend` is bound to, from
    /// `dedup_service.backend().backend_type()`. Redundant with
    /// `entries[i where is_active].backend` in multi-entry mode; kept
    /// because pre-boot / mid-migration inspection may still find it
    /// useful.
    pub current_backend: String,
    pub total_blobs: u64,
    pub total_bytes_stored: u64,
    pub dedup_ratio: f64,
    // ── Multi-entry view (slice 6) ──
    /// All named storage entries declared in env. Empty when running
    /// in legacy single-backend mode (`OXICLOUD_STORAGE_ENTRIES`
    /// unset AND no legacy synthesis happened). Order matches
    /// `_ENTRIES`.
    pub entries: Vec<StorageEntrySummaryDto>,
    /// Name of the entry the LIVE backend is currently bound to.
    /// Populated as the boot-selected name (per
    /// `CoreServices.active_backend_name`). Empty string for the
    /// zero-entries legacy path (`"legacy"` sentinel).
    pub active_entry_name: String,
    /// Global read-only flag — when true, all write-adjacent
    /// AuthZ checks refuse. Set by the migration handler at run
    /// start; cleared by the boot-clear rule after operator
    /// restart. Frontend renders a banner on the storage tab when
    /// true.
    pub migration_readonly: bool,
}

/// Per-entry summary emitted in `StorageSettingsDto.entries`. Never
/// carries credentials — those live in env vars only. `is_active`
/// marks which entry the LIVE backend uses right now (matches
/// `active_entry_name` on the parent DTO).
#[derive(Debug, Serialize, Deserialize)]
pub struct StorageEntrySummaryDto {
    pub name: String,
    /// Backend type — "local" / "s3" / "azure".
    pub backend: String,
    /// True for exactly one entry (the entry the LIVE backend is on).
    /// Frontend uses this to badge the active row and to exclude it
    /// from the migration-target dropdown.
    pub is_active: bool,
    /// True when the entry has a per-entry encryption key. UI shows
    /// a lock icon. Presence-only — the key bytes never leave the
    /// server.
    pub encryption_enabled: bool,
    /// Human-readable physical location hint, if the backend surfaces
    /// one (`root_dir` for Local, `bucket` for S3, `container` for
    /// Azure). Cosmetic — helps the admin distinguish two Local
    /// entries pointing at different disks.
    pub location_hint: Option<String>,
    /// Ordered pair-list summary — one entry per configured pair in
    /// `OXICLOUD_STORAGE_<NAME>_ENCRYPTION_KEY`, oldest first, head
    /// last. Empty vec means the entry has no `_ENCRYPTION_KEY`
    /// declared at all (pure plaintext-v1 writes today, no crypto).
    ///
    /// Frontend renders this on the entry card so operators can:
    ///   - See which pairs are configured + their SSH-style
    ///     fingerprints without inspecting `.env`.
    ///   - Cross-reference the head pair against the `head_key_fp`
    ///     from the last `backend_rotate` completion — if they
    ///     match AND `failed = 0`, every on-disk blob is under the
    ///     head, and non-head pairs are safe to remove.
    #[serde(default)]
    pub encryption_pairs: Vec<StorageEncryptionPairDto>,
}

/// One `<cipher>:<key>` pair rendered for the admin UI. Never
/// carries key material — only cipher name + a truncated fingerprint
/// safe to show operators.
#[derive(Debug, Serialize, Deserialize)]
pub struct StorageEncryptionPairDto {
    /// `"aes-256-gcm"` for a real-cipher pair, `"none"` for a
    /// `none:` sentinel (writes as plaintext-v1).
    pub cipher: String,
    /// SSH-style colon-hex 8-byte truncation of `sha256(key)`.
    /// Matches the v1 header's `<key_fp>` field and the CLI's
    /// `oxicloud --fingerprint <key>` output. `None` for `none:`
    /// pairs (no key material to fingerprint).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<String>,
    /// True for the LAST pair in the list — the write pair. UI
    /// badges it distinctly ("← head" or an arrow). Exactly one
    /// pair has `is_head = true` when the list is non-empty.
    pub is_head: bool,
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

/// Request body for testing a storage connection.
///
/// Two shapes are accepted:
///
/// - Multi-entry test — set `entry_name` to the name of a declared entry
///   (from `OXICLOUD_STORAGE_ENTRIES`). Server looks it up, builds a fresh
///   backend via the shared factory, runs health-check + round-trip against
///   it. `backend` and the S3 fields are ignored in this mode.
/// - Legacy DTO test — leave `entry_name` unset and populate `backend` +
///   the S3 fields. Server builds a temporary backend from those values
///   (pre-multi-entry behaviour). Still supported for zero-entries
///   deployments; deprecated for new integrations.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct TestStorageConnectionDto {
    /// If set, all other fields are ignored — server resolves this
    /// name against `OXICLOUD_STORAGE_ENTRIES` and tests that entry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entry_name: Option<String>,
    #[serde(default)]
    pub backend: String,
    pub s3_endpoint_url: Option<String>,
    pub s3_bucket: Option<String>,
    pub s3_region: Option<String>,
    pub s3_access_key: Option<String>,
    pub s3_secret_key: Option<String>,
    pub s3_force_path_style: Option<bool>,
}

// Default is derived — every field is either an Option (defaults to
// None) or `backend: String` (empty via `#[serde(default)]`). The
// hand-rolled impl was flagged by clippy::derivable_impls.

/// Result of a storage connection + round-trip test.
///
/// `connected` is TRUE when the backend was reachable (health-check
/// passed — HEAD bucket / statfs). `roundtrip_passed` is TRUE when
/// the subsequent PUT → GET → verify → DELETE cycle succeeded — it
/// validates the exact permissions the migration job needs
/// (`s3:PutObject` + `s3:GetObject` + `s3:DeleteObject` on S3, disk
/// write permission on Local). All round-trip fields are `None` when
/// reachability failed (we don't attempt the round-trip if we can't
/// even HEAD the bucket).
///
/// `phase_reached` names the last step that succeeded — on
/// `roundtrip_passed = false` it pinpoints where the failure hit
/// (`put_ok` → wrote but couldn't confirm; `exists_ok` → wrote +
/// confirmed but GET failed; etc.). `cleanup_ok = false` means the
/// backend was readable + writable but the test object may be
/// orphaned on it (~100 B, content-addressed — harmless, admin can
/// reap by hash).
#[derive(Debug, Serialize, Deserialize)]
pub struct StorageTestResultDto {
    pub connected: bool,
    pub message: String,
    pub backend_type: String,
    pub available_bytes: Option<u64>,
    /// Set only when reachability passed AND a round-trip was
    /// attempted. `Some(true)` = full write + read + verify success;
    /// `Some(false)` = reachability OK, round-trip failed at
    /// `phase_reached`; `None` = round-trip not attempted (typically
    /// because reachability failed).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub roundtrip_passed: Option<bool>,
    /// Last round-trip phase completed successfully — one of
    /// `initialize`, `put_ok`, `exists_ok`, `get_ok`, `verify_ok`,
    /// `cleanup_ok`. `None` when round-trip wasn't attempted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub phase_reached: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bytes_written: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bytes_read: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub roundtrip_elapsed_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cleanup_ok: Option<bool>,
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
///
/// **Multi-entry contract** (see `docs/plan/storage-multi-entry.md`):
/// `target_name` is REQUIRED — it names the storage entry the copy
/// job will move blobs INTO. The admin picks it from the entries
/// declared in `OXICLOUD_STORAGE_ENTRIES`. The trigger endpoint
/// rejects the request when the name doesn't exist or equals the
/// currently-active entry (no-op guard).
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct StartMigrationDto {
    /// Name of the storage entry to migrate blobs INTO. Must be
    /// present in `OXICLOUD_STORAGE_ENTRIES` and must differ from
    /// the currently-active entry.
    pub target_name: String,
    /// How many blobs to copy in parallel (default: 4).
    ///
    /// **Currently ignored** — the recoverable copy loop is
    /// sequential (one blob at a time within the batch). Kept in
    /// the DTO for wire-compat with the admin UI form; will be
    /// honoured once per-batch fan-out lands (dual-write /
    /// concurrent-copy future slice).
    pub concurrency: Option<usize>,
}

// VerifyMigrationDto retired in slice 7 of
// docs/plan/storage-multi-entry.md — the corresponding endpoint's
// sample-based check is superseded by
// `blobs_consistency?storage=<name>`, a full walk that emits
// structured findings per mismatch.

// ============================================================================
// SMTP Settings DTOs (Admin Panel)
// ============================================================================

/// Read-only SMTP info shown on the admin SMTP page. SMTP configuration
/// is sourced exclusively from environment variables — these fields are
/// for display only and any change has to happen by updating the env
/// and restarting the server.
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct SmtpInfoDto {
    /// Whether `OXICLOUD_SMTP_HOST` is set and SMTP construction succeeded.
    pub enabled: bool,
    /// `OXICLOUD_SMTP_HOST`. Empty string when unset.
    pub host: String,
    /// `OXICLOUD_SMTP_PORT`. Default 587.
    pub port: u16,
    /// Transport encryption mode: `"starttls"`, `"tls"`, or `"none"`.
    pub tls: String,
    /// `OXICLOUD_SMTP_FROM` mailbox. Empty when unset.
    pub from: String,
    /// `<set>` if a SASL user is configured, `<anon>` otherwise.
    /// Never echoes the username — admins compare against the
    /// runtime config without having to look in `.env`.
    pub user_state: &'static str,
}

/// Request body for `POST /api/admin/smtp/test`: send a hardcoded
/// diagnostic email to the given recipient.
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct SendSmtpTestDto {
    pub to: String,
}

/// Result of a `POST /api/admin/smtp/test` invocation. `success=true`
/// carries the SMTP server's response code + first reply line; on
/// failure the relevant error message goes in `error`. Always 200 OK
/// so the frontend can render both outcomes in one place — the SMTP
/// failure is a normal operational state, not an HTTP error.
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct SmtpTestResultDto {
    pub success: bool,
    /// SMTP status code (e.g. 250). Only set on success.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<u16>,
    /// First line of the SMTP server's reply. Only set on success.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// Human-readable error message. Only set on failure.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}
