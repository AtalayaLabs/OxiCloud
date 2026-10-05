use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

// ============================================================================
// OIDC Settings DTOs (Admin Panel)
// ============================================================================

/// Current OIDC settings returned to admin UI (secrets masked)
#[derive(Debug, Serialize, Deserialize, ToSchema)]
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

/// Request body for transferring server ownership.
///
/// Separate from [`UpdateUserRoleDto`] on purpose: ownership is not an
/// assignable role. The generic role endpoint refuses `"owner"`, because
/// conferring it there would create a second owner rather than move the
/// one that exists.
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct TransferOwnershipDto {
    /// The user who becomes the new owner. The caller — who must be the
    /// current owner — is demoted to admin in the same transaction.
    pub new_owner_id: String,
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

/// Query parameters for listing users. `/api/admin/users` used to
/// bifurcate on `?summary=` (flat `PublicUserDto` vs nested
/// `FullUserDto`); that split was retired — the endpoint now always
/// returns `FullUserDto`. Unknown query params are ignored, so
/// existing callers still passing `?summary=true` keep working.
#[derive(Debug, Serialize, Deserialize)]
pub struct ListUsersQueryDto {
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

/// Query parameters for the admin sessions listing.
///
/// `user_id` is a String (not `Uuid`) because bad UUIDs need a clean
/// 400 response — the handler parses and rejects malformed input.
/// `include_revoked` defaults to `false` at the handler layer.
#[derive(Debug, Serialize, Deserialize)]
pub struct ListSessionsQueryDto {
    pub user_id: Option<String>,
    pub include_revoked: Option<bool>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

/// One row of the dashboard's quota panel — usage aggregate for a
/// single drive kind. Unlimited caps are excluded from `capped_quota_bytes`
/// and counted in `unlimited_count` so the panel can render the ratio
/// honestly ("X / Y over N capped drives · M unlimited").
#[derive(Debug, Serialize, Deserialize, ToSchema)]
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
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct DashboardStatsDto {
    // System info
    pub server_version: String,
    pub oidc_configured: bool,
    /// Currently-connected message-bus WebSocket sessions. One per
    /// browser tab that reached a folder view and hasn't closed the
    /// tab yet. Zero when `OXICLOUD_MESSAGEBUS_ENABLE=false`.
    /// Snapshot value — a subsequent request can see a different
    /// number if a connection opened/closed in between. Renders on
    /// the admin dashboard's "Live activity" section next to
    /// `online_sessions` (HTTP-driven distinct-user count).
    pub active_ws_sessions: u64,
    // ── User accounts (static breakdown of auth.users) ──
    // All four are counts of the SAME table under different
    // predicates. `active`, `admin`, `external` are all subsets of
    // `total`. `external` is disjoint from `admin` by DB constraint
    // (`users_external_not_admin`). The dashboard renders these as
    // one grouped section separate from the live-activity section
    // below, so admins don't confuse "as-of-now row count" with
    // "who's here right now".
    pub total_users: i64,
    pub active_users: i64,
    pub admin_users: i64,
    /// Grant-only accounts (magic-link / OIDC-only / OCM recipients).
    /// Filtered out of `total_users` / `active_users` since those
    /// columns count operational seats (see the SELECT comment). Here
    /// as its own metric because operators of external-heavy
    /// deployments (public shares, invited-collab shops) need to see
    /// the invited population at a glance.
    pub external_users: i64,
    // ── Live activity (projection over auth.sessions) ──
    // Both fields change minute-to-minute, unlike the user counts
    // above which only move on register/deactivate/role-toggle.
    // Same 5-min window as the Prometheus gauges
    // (`oxicloud_sessions_online[_users]` in
    // `session_liveness_gauges.rs`), computed via the shared
    // `ONLINE_WINDOW` constant so per-user badges + aggregate
    // counts + this dashboard number stay consistent by construction.
    /// Distinct users behind non-revoked sessions active in the last
    /// 5 min. Answers "how many humans are here right now?".
    pub online_users: i64,
    /// Non-revoked sessions active in the last 5 min. Answers "how
    /// many concurrent connections must I serve?". Ratio
    /// `online_sessions / online_users` is the multi-device factor.
    pub online_sessions: i64,
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
    /// `true` when the active storage backend is remote (S3 / Azure)
    /// and the local-disk blob cache is NOT enabled — i.e., every
    /// blob read pays a round-trip to the remote. Signals the admin
    /// UI to show the "enable the backend cache" banner. `false` on
    /// local-filesystem deployments (nothing to cache) and when the
    /// cache is already on.
    pub storage_cache_recommended: bool,
    /// Current occupancy of the in-memory file-content cache (moka).
    /// Admin dashboard renders `size_bytes / max_bytes` as a capacity
    /// bar; combined with `oxicloud_content_cache_hits_total /
    /// _misses_total` on `/metrics` the operator can see both
    /// "how full" and "how useful". Always present — the content
    /// cache runs unconditionally.
    pub content_cache: ContentCacheInfoDto,
    /// Current occupancy of the in-memory thumbnail cache (moka
    /// instance distinct from the file-content cache above —
    /// different key space, different budget, different eviction).
    /// Always present. See `docs/architecture/caching.md` for the
    /// two-tier memory topology and why they aren't merged.
    pub thumbnail_cache: ThumbnailCacheInfoDto,
    /// Current occupancy of the on-disk `.blob-cache` tier when
    /// enabled, `None` otherwise. `None` is the "local-only
    /// deployment or operator hasn't opted in" case, in which the
    /// admin UI hides the row entirely rather than drawing a disabled
    /// bar.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backend_cache: Option<BackendCacheInfoDto>,
}

/// Moka content-cache occupancy snapshot (see [`DashboardStatsDto::content_cache`]).
///
/// Each moka entry holds the ASSEMBLED bytes of one small file
/// (<10 MB) keyed by the file's content hash — not individual
/// chunks. So `files` is the honest name for the entry count here;
/// use it against the sibling `BackendCacheInfoDto`'s `chunks` to
/// avoid conflating the two tiers' granularities in dashboards or
/// alerts.
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct ContentCacheInfoDto {
    pub size_bytes: u64,
    pub max_bytes: u64,
    /// Number of cached files. For a file physically split into N
    /// chunks on-backend, moka still holds exactly ONE assembled
    /// entry here.
    pub files: u64,
}

/// `.blob-cache` tier occupancy snapshot (see [`DashboardStatsDto::backend_cache`]).
/// Separate struct from the moka one so adding tier-specific fields
/// later (eviction count, LRU age) doesn't force a shared schema.
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct BackendCacheInfoDto {
    pub size_bytes: u64,
    pub max_bytes: u64,
    /// Number of cached chunks (one entry per content-addressable
    /// blob). Unlike moka's assembled-file entries, these are the
    /// chunk-granular units of the dedup registry. The disk tier
    /// covers both source-file chunks AND satellite derived blobs
    /// (thumbnails, transcodes) — one unified disk cache serving
    /// every blob read.
    pub chunks: u64,
    /// On-disk location where the cache files live. Shown on the
    /// admin UI so operators know where to point `du`, backups, or
    /// an SSD mount.
    pub cache_dir: String,
}

/// Thumbnail-moka occupancy snapshot. Separate struct from the
/// file-content moka one because the granularity differs: each
/// thumbnail entry is one encoded WebP/AVIF payload keyed by
/// `(file_id, size)`, not an assembled file.
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct ThumbnailCacheInfoDto {
    pub size_bytes: u64,
    pub max_bytes: u64,
    /// Number of cached thumbnail payloads. Each entry = one
    /// `(file_id, size, format)` tuple of encoded bytes.
    pub thumbnails: u64,
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
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct StorageSettingsDto {
    // ── Current stats — pertain to the running process ──
    /// Backend type currently in use (`"local"` / `"s3"` / `"azure"`) —
    /// what the LIVE `blob_backend` is bound to, from
    /// `blob_handler.backend().backend_type()`. Redundant with
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
    /// Current backend write-lock holder, if any. `None` = writes
    /// are allowed (migration_readonly is false in that case too);
    /// `Some(_)` carries the typed reason so the admin UI can
    /// render a reason-aware card ("Migrating to s3-east", "Backing
    /// up to nightly-nas", "Manual: restic to NAS", etc.) and
    /// surface the Release button for an External hold.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backend_write_lock:
        Option<crate::application::services::backend_write_gate::BackendWriteLockReason>,
}

/// Request body for `POST /api/admin/storage/write-lock` — the
/// operator asks the server to engage the External lock variant on
/// their behalf.
#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct AcquireExternalLockDto {
    /// Human-readable one-line label shown in the UI banner and
    /// audit log. Required; empty label falls back to a generic
    /// "External maintenance in progress" caption.
    pub label: String,
    /// Optional override of the default 6-hour auto-expire. Clamped
    /// server-side to `[60s, 24h]` so a typo doesn't wedge the
    /// server for a week or a zero-second expiry defeats the lock
    /// altogether.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_in_seconds: Option<u64>,
}

/// Response body for the backend-write-lock endpoints — current
/// holder (if any), plus a stable `is_held` discriminator so the
/// client doesn't need to re-check the Option.
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct BackendWriteLockStatusDto {
    pub is_held: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub holder: Option<crate::application::services::backend_write_gate::BackendWriteLockReason>,
}

/// Per-entry summary emitted in `StorageSettingsDto.entries`. Never
/// carries credentials — those live in env vars only. `is_active`
/// marks which entry the LIVE backend uses right now (matches
/// `active_entry_name` on the parent DTO).
#[derive(Debug, Serialize, Deserialize, ToSchema)]
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
#[derive(Debug, Serialize, Deserialize, ToSchema)]
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
#[derive(Debug, Serialize, Deserialize, ToSchema)]
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

/// Read-only webhook info, shown beside SMTP on the admin Notifications
/// page. Like SMTP, configured only through environment variables.
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct WebhookInfoDto {
    /// Whether `OXICLOUD_WEBHOOK_URL` is set and the sink was built.
    pub enabled: bool,
    /// `generic`, `slack`, `discord`, `teams`, `telegram` or `ntfy`.
    pub format: String,
    /// Scheme and host of the endpoint — **never the full URL**.
    ///
    /// A Telegram endpoint embeds the bot token in its path
    /// (`/bot<TOKEN>/sendMessage`), and a Slack or Discord webhook URL is
    /// itself the credential: echoing either into an API response would
    /// put a secret in a browser's network log and anywhere that response
    /// gets pasted. The host is enough to confirm "it points where I
    /// think".
    pub host: String,
    /// Recipient for the formats that carry one — a Telegram chat id, an
    /// ntfy topic. Empty when unset or not applicable. Not a credential,
    /// and an operator checking their configuration needs to see it.
    pub target: String,
}

/// Read-only alerting policy, from `GET /api/admin/notify/info`.
///
/// The *what gets sent* half, where `SmtpInfoDto` and [`WebhookInfoDto`]
/// are the *how it travels* half. Configured through environment
/// variables only, shown for confirmation rather than editing.
#[derive(Debug, Serialize, Deserialize, ToSchema)]
pub struct NotifyInfoDto {
    /// Severity floor: `data_loss`, `inconsistent`, `anomaly`, or `none`.
    ///
    /// The field the panel was missing. An operator with a configured
    /// webhook and a healthy relay can still be told nothing, because the
    /// default floor admits only `data_loss` — and until this was
    /// surfaced, the only way to discover that was to read the server's
    /// environment.
    pub min_severity: String,
    /// Channels actually built and wired, by name — `webhook`, `email`.
    /// Empty means findings are recorded but nobody is told.
    pub sinks: Vec<String>,
    /// `OXICLOUD_JOBS_NOTIFY_EMAIL_TO`, as parsed. Empty when mail
    /// alerting is off.
    pub email_recipients: Vec<String>,
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
