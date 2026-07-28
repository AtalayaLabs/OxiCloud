use std::env;
use std::path::PathBuf;
use std::time::Duration;

/// Cache configuration
#[derive(Debug, Clone)]
pub struct CacheConfig {
    /// TTL for file cache entries (ms)
    pub file_ttl_ms: u64,
    /// TTL for directory cache entries (ms)
    pub directory_ttl_ms: u64,
    /// Maximum number of cache entries
    pub max_entries: usize,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            file_ttl_ms: 60_000,       // 1 minute
            directory_ttl_ms: 120_000, // 2 minutes
            max_entries: 10_000,       // 10,000 entries
        }
    }
}

/// Timeout configuration for different operations
#[derive(Debug, Clone)]
pub struct TimeoutConfig {
    /// Timeout for file operations (ms)
    pub file_operation_ms: u64,
    /// Timeout for directory operations (ms)
    pub dir_operation_ms: u64,
    /// Timeout for lock acquisition (ms)
    pub lock_acquisition_ms: u64,
    /// Timeout for network operations (ms)
    pub network_operation_ms: u64,
    /// Timeout for thumbnail generation (ms)
    pub thumbnail_generation_ms: u64,
}

impl Default for TimeoutConfig {
    fn default() -> Self {
        Self {
            file_operation_ms: 10000,       // 10 seconds
            dir_operation_ms: 30000,        // 30 seconds
            lock_acquisition_ms: 5000,      // 5 seconds
            network_operation_ms: 15000,    // 15 seconds
            thumbnail_generation_ms: 30000, // 30 seconds
        }
    }
}

impl TimeoutConfig {
    /// Gets a Duration for file operations
    pub fn file_timeout(&self) -> Duration {
        Duration::from_millis(self.file_operation_ms)
    }

    /// Gets a Duration for file write operations
    pub fn file_write_timeout(&self) -> Duration {
        Duration::from_millis(self.file_operation_ms)
    }

    /// Gets a Duration for file read operations
    pub fn file_read_timeout(&self) -> Duration {
        Duration::from_millis(self.file_operation_ms)
    }

    /// Gets a Duration for file delete operations
    pub fn file_delete_timeout(&self) -> Duration {
        Duration::from_millis(self.file_operation_ms)
    }

    /// Gets a Duration for directory operations
    pub fn dir_timeout(&self) -> Duration {
        Duration::from_millis(self.dir_operation_ms)
    }

    /// Gets a Duration for lock acquisition
    pub fn lock_timeout(&self) -> Duration {
        Duration::from_millis(self.lock_acquisition_ms)
    }

    /// Gets a Duration for network operations
    pub fn network_timeout(&self) -> Duration {
        Duration::from_millis(self.network_operation_ms)
    }

    /// Gets a Duration for thumbnail generation operations
    pub fn thumbnail_timeout(&self) -> Duration {
        Duration::from_millis(self.thumbnail_generation_ms)
    }
}

/// Configuration for large resource handling
#[derive(Debug, Clone)]
pub struct ResourceConfig {
    /// Threshold in MB to consider a file as large
    pub large_file_threshold_mb: u64,
    /// Entry threshold to consider a directory as large
    pub large_dir_threshold_entries: usize,
    /// Chunk size for large file processing (bytes)
    pub chunk_size_bytes: usize,
    /// File size limit for loading into memory (MB)
    pub max_in_memory_file_size_mb: u64,
}

impl Default for ResourceConfig {
    fn default() -> Self {
        Self {
            large_file_threshold_mb: 100,      // 100 MB
            large_dir_threshold_entries: 1000, // 1000 entries
            chunk_size_bytes: 1024 * 1024,     // 1 MB
            max_in_memory_file_size_mb: 50,    // 50 MB
        }
    }
}

impl ResourceConfig {
    /// Converts a size in bytes to MB
    pub fn bytes_to_mb(&self, bytes: u64) -> u64 {
        bytes / (1024 * 1024)
    }

    /// Determines if a file is considered large
    pub fn is_large_file(&self, size_bytes: u64) -> bool {
        self.bytes_to_mb(size_bytes) >= self.large_file_threshold_mb
    }

    /// Determines if a file is large enough for parallel processing
    pub fn needs_parallel_processing(&self, size_bytes: u64, config: &ConcurrencyConfig) -> bool {
        self.bytes_to_mb(size_bytes) >= config.min_size_for_parallel_chunks_mb
    }

    /// Determines if a file can be fully loaded into memory
    pub fn can_load_in_memory(&self, size_bytes: u64) -> bool {
        self.bytes_to_mb(size_bytes) <= self.max_in_memory_file_size_mb
    }

    /// Determines if a directory is considered large
    pub fn is_large_directory(&self, entry_count: usize) -> bool {
        entry_count >= self.large_dir_threshold_entries
    }

    /// Calculates the number of chunks for parallel processing
    pub fn calculate_optimal_chunks(&self, size_bytes: u64, config: &ConcurrencyConfig) -> usize {
        // If the file is not large enough, return 1
        if !self.needs_parallel_processing(size_bytes, config) {
            return 1;
        }

        // Calculate the number of chunks based on size
        let chunk_count = (size_bytes as usize).div_ceil(config.parallel_chunk_size_bytes);

        // Limit to the maximum number of parallel chunks
        chunk_count.min(config.max_parallel_chunks)
    }

    /// Calculates the optimal size of each chunk for parallel processing
    pub fn calculate_chunk_size(&self, file_size: u64, chunk_count: usize) -> usize {
        if chunk_count <= 1 {
            return file_size as usize;
        }

        // Distribute the size evenly among the chunks
        (file_size as usize).div_ceil(chunk_count)
    }
}

/// Configuration for concurrent operations
#[derive(Debug, Clone)]
pub struct ConcurrencyConfig {
    /// Maximum concurrent file tasks
    pub max_concurrent_files: usize,
    /// Maximum concurrent directory tasks
    pub max_concurrent_dirs: usize,
    /// Maximum concurrent IO operations
    pub max_concurrent_io: usize,
    /// Maximum chunks to process in parallel per file
    pub max_parallel_chunks: usize,
    /// Minimum file size (MB) to apply parallel chunk processing
    pub min_size_for_parallel_chunks_mb: u64,
    /// Chunk size for parallel processing (bytes)
    pub parallel_chunk_size_bytes: usize,
}

impl Default for ConcurrencyConfig {
    fn default() -> Self {
        Self {
            max_concurrent_files: 10,
            max_concurrent_dirs: 5,
            max_concurrent_io: 20,
            max_parallel_chunks: 8,
            min_size_for_parallel_chunks_mb: 200,       // 200 MB
            parallel_chunk_size_bytes: 8 * 1024 * 1024, // 8 MB
        }
    }
}

/// Storage configuration
#[derive(Debug, Clone)]
pub struct StorageConfig {
    /// Root directory for storage
    pub root_dir: String,
    /// Chunk size for file processing
    pub chunk_size: usize,
    /// Threshold for parallel processing
    pub parallel_threshold: usize,
    /// Retention days for files in the trash
    pub trash_retention_days: u32,
    /// Maximum upload file size in bytes (default: 10 GB).
    /// Applied as a hard limit to WebDAV PUT and streaming uploads.
    pub max_upload_size: usize,
    /// Maximum size of a single chunk in a chunked-upload session, in bytes
    /// (default: 100 MB). Distinct from [`max_upload_size`] (which bounds the
    /// total file size): NC desktop and other clients split large files into
    /// many smaller PUTs against `/dav/uploads/…`, so the per-chunk cap can
    /// be far tighter than the whole-file cap and prevents one HTTP request
    /// from monopolising server memory or disk. Env: `OXICLOUD_CHUNK_MAX_BYTES`.
    pub chunk_max_bytes: usize,
    /// Maximum size of a single non-chunked PUT body, in bytes (default:
    /// 1 GiB). Set below `max_upload_size` so files larger than this are
    /// pushed onto the chunked-upload protocol (`/api/uploads/…` or
    /// `/dav/uploads/…`) — which is resilient to mid-transfer failures,
    /// resumable, and bounded per-request by `chunk_max_bytes`. Without
    /// this cap a 10 GB direct PUT spools 10 GB to disk in a single
    /// request; a connection drop at 95 % loses everything. The server
    /// returns 413 with a "use chunked upload" hint when a direct PUT
    /// exceeds this cap. Env: `OXICLOUD_DIRECT_PUT_MAX_BYTES`.
    pub direct_put_max_bytes: usize,
    /// Root directory for chunked-upload sessions. When `Some`, chunks land
    /// under `{chunk_dir}/{upload_id}/` (REST) and
    /// `{chunk_dir}/nextcloud/{user}/{upload_id}/` (NC). When `None`, falls
    /// back to `{root_dir}/.uploads/`. Pointing this at the **same
    /// filesystem** as `.blobs/` keeps the final assembled-to-blob promotion
    /// an atomic `rename(2)` rather than a full cross-FS copy; pointing it
    /// at fast storage (NVMe) accelerates the chunk-write + assembly loop
    /// independently of where final blobs live. Env: `OXICLOUD_CHUNK_DIR`.
    pub chunk_dir: Option<PathBuf>,
    /// Interval (seconds) of the background sweep that reconciles every user's
    /// cached `storage_used_bytes` with the real sum of their files. Keeps the
    /// quota fresh for all mutations without recomputing on the request path.
    /// Default: 600 (10 min). Env: `OXICLOUD_STORAGE_USAGE_RECONCILE_SECS`.
    pub usage_reconcile_secs: u64,
    /// Interval (milliseconds) of the background job that drains
    /// `storage.tree_etag_dirty` and bumps folder `tree_modified_at`
    /// (collection ETags). Write paths only enqueue — this is the upper
    /// bound on how stale an ancestor folder's ETag can be after a change.
    /// Default: 500. Env: `OXICLOUD_TREE_ETAG_FLUSH_MS`.
    pub tree_etag_flush_ms: u64,
    /// Startup background migration that re-chunks legacy whole-file blobs
    /// (written before CDC chunking landed) into chunk manifests, so Range
    /// reads stop paying a full-blob read — and, with encryption enabled, a
    /// full-blob decrypt. Idempotent and incremental; a no-op (one COUNT
    /// query) once no legacy blobs remain. Disable on metered remote
    /// backends where the one-time re-read of every legacy blob should be
    /// scheduled deliberately. Default: true. Env: `OXICLOUD_LEGACY_RECHUNK`.
    pub legacy_rechunk_enabled: bool,
    /// Which blob storage backend to use (`local`, `s3`, or `azure`).
    pub backend: StorageBackendType,
    /// S3-compatible backend configuration (used when `backend == S3`).
    pub s3: Option<S3StorageConfig>,
    /// Azure Blob Storage configuration (used when `backend == Azure`).
    pub azure: Option<AzureStorageConfig>,
    /// Local disk cache for remote backends.
    pub cache: BlobCacheConfig,
    /// Client-side encryption.
    pub encryption: EncryptionConfig,
    /// Retry policy for remote backends.
    pub retry: RetryConfig,
}

/// Which blob storage backend to use.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum StorageBackendType {
    /// Local filesystem (default).
    #[default]
    Local,
    /// Any S3-compatible object store (AWS, Backblaze B2, R2, MinIO, …).
    S3,
    /// Azure Blob Storage.
    Azure,
}

/// Configuration for an S3-compatible blob storage backend.
#[derive(Debug, Clone)]
pub struct S3StorageConfig {
    /// Custom endpoint URL (required for non-AWS providers).
    pub endpoint_url: Option<String>,
    /// S3 bucket name.
    pub bucket: String,
    /// AWS region (default: `us-east-1`).
    pub region: String,
    /// Access key ID.
    pub access_key: String,
    /// Secret access key.
    pub secret_key: String,
    /// Force path-style access (required for MinIO, R2, some providers).
    pub force_path_style: bool,
}

/// Configuration for Azure Blob Storage.
#[derive(Debug, Clone)]
pub struct AzureStorageConfig {
    /// Azure storage account name.
    pub account_name: String,
    /// Azure storage account key.
    pub account_key: String,
    /// Container name.
    pub container: String,
    /// Optional SAS token (alternative to account key).
    pub sas_token: Option<String>,
    /// Optional custom endpoint (Azurite emulator, private deployments,
    /// benches). `None` = the public cloud URL derived from the account
    /// name. Mirrors S3's `endpoint_url`.
    pub endpoint_url: Option<String>,
}

/// LRU local disk cache configuration for remote blob backends.
#[derive(Debug, Clone)]
pub struct BlobCacheConfig {
    /// Enable the LRU disk cache (only useful for remote backends).
    pub enabled: bool,
    /// Maximum cache size in bytes (default: 50 GB).
    pub max_size_bytes: u64,
    /// Cache directory path (default: `{root_dir}/.blob-cache`).
    pub cache_path: Option<String>,
}

impl Default for BlobCacheConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            max_size_bytes: 50 * 1024 * 1024 * 1024, // 50 GB
            cache_path: None,
        }
    }
}

/// Client-side encryption configuration.
#[derive(Debug, Clone)]
pub struct EncryptionConfig {
    /// Enable AES-256-GCM encryption for blobs at rest.
    pub enabled: bool,
    /// Base64-encoded 32-byte encryption key.
    pub key_base64: Option<String>,
}

impl Default for EncryptionConfig {
    #[allow(clippy::derivable_impls)]
    fn default() -> Self {
        Self {
            enabled: false,
            key_base64: None,
        }
    }
}

/// Retry policy configuration for remote backends.
#[derive(Debug, Clone)]
pub struct RetryConfig {
    /// Enable retry with exponential backoff.
    pub enabled: bool,
    /// Maximum number of retry attempts.
    pub max_retries: u32,
    /// Initial backoff in milliseconds.
    pub initial_backoff_ms: u64,
    /// Maximum backoff in milliseconds.
    pub max_backoff_ms: u64,
    /// Backoff multiplier.
    pub backoff_multiplier: f64,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_retries: 3,
            initial_backoff_ms: 100,
            max_backoff_ms: 10_000,
            backoff_multiplier: 2.0,
        }
    }
}

impl Default for StorageConfig {
    fn default() -> Self {
        // Architecture-appropriate max upload size to avoid overflow on 32-bit systems
        const MAX_UPLOAD_SIZE: usize = if cfg!(target_pointer_width = "64") {
            10 * 1024 * 1024 * 1024 // 10 GB on 64-bit
        } else {
            1024 * 1024 * 1024 // 1 GB on 32-bit
        };
        Self {
            root_dir: "storage".to_string(),
            chunk_size: 1024 * 1024,               // 1 MB
            parallel_threshold: 100 * 1024 * 1024, // 100 MB
            trash_retention_days: 30,              // 30 days
            max_upload_size: MAX_UPLOAD_SIZE,
            chunk_max_bytes: 100 * 1024 * 1024, // 100 MB — sane upper bound for a single chunked-upload PUT
            direct_put_max_bytes: 1024 * 1024 * 1024, // 1 GiB — pushes larger uploads onto the chunked protocol
            chunk_dir: None,
            usage_reconcile_secs: 600, // 10 minutes
            tree_etag_flush_ms: 500,
            legacy_rechunk_enabled: true,
            backend: StorageBackendType::Local,
            s3: None,
            azure: None,
            cache: BlobCacheConfig::default(),
            encryption: EncryptionConfig::default(),
            retry: RetryConfig::default(),
        }
    }
}

/// Database configuration
#[derive(Debug, Clone)]
pub struct DatabaseConfig {
    pub connection_string: String,
    pub max_connections: u32,
    pub min_connections: u32,
    pub connect_timeout_secs: u64,
    pub idle_timeout_secs: u64,
    pub max_lifetime_secs: u64,
    /// Maximum connections for the maintenance pool (background/batch tasks).
    /// Defaults to 25% of `max_connections` (minimum 2).
    pub maintenance_max_connections: u32,
    /// Minimum connections for the maintenance pool.
    /// Defaults to 1.
    pub maintenance_min_connections: u32,
    /// Per-statement timeout (seconds) applied to the **primary** pool via
    /// `SET statement_timeout` on every connection. Bounds the worst-case query
    /// so a single runaway statement can't pin a pool slot and starve
    /// interactive requests (correlated tail-latency cliff). `0` disables it.
    /// The maintenance pool is always exempt — its batch jobs (integrity scans,
    /// GC) may legitimately run long. Env: `OXICLOUD_DB_STATEMENT_TIMEOUT_SECS`.
    pub statement_timeout_secs: u64,
    /// Interval (seconds) of the background watchdog that samples primary-pool
    /// saturation and logs a WARN when connections are near exhaustion (the
    /// signal to raise `max_connections` or hunt slow queries). `0` disables
    /// it. Default: 30. Env: `OXICLOUD_DB_POOL_MONITOR_INTERVAL_SECS`.
    pub pool_monitor_interval_secs: u64,
}

impl Default for DatabaseConfig {
    fn default() -> Self {
        Self {
            // Updated connection string with default credentials that PostgreSQL often uses
            connection_string: "postgres://postgres:postgres@localhost:5432/oxicloud".to_string(),
            max_connections: 20,
            min_connections: 5,
            connect_timeout_secs: 10,
            idle_timeout_secs: 300,
            max_lifetime_secs: 1800,
            maintenance_max_connections: 5,
            maintenance_min_connections: 1,
            statement_timeout_secs: 30,
            pool_monitor_interval_secs: 30,
        }
    }
}

/// Authentication configuration
#[derive(Debug, Clone)]
pub struct AuthConfig {
    pub jwt_secret: String,
    pub access_token_expiry_secs: i64,
    pub refresh_token_expiry_secs: i64,
    /// Argon2id memory cost in KiB (default 65536 = 64 MiB)
    pub hash_memory_cost: u32,
    /// Argon2id time cost / iterations (default 3)
    pub hash_time_cost: u32,
    /// Argon2id parallelism lanes (default 2)
    pub hash_parallelism: u32,
    /// Rate limiting / account lockout configuration
    pub rate_limit: RateLimitConfig,
    /// Allowlist of email domains accepted on the public `POST
    /// /api/auth/register` endpoint. Empty = no restriction (any
    /// domain is allowed). Entries are lowercased and trimmed at
    /// load time; matching is case-insensitive exact-match on the
    /// post-`@` part of the address.
    ///
    /// This is DISTINCT from
    /// [`MagicLinkConfig::allowed_email_domains`], which gates who
    /// can be INVITED (email-typed grants + magic-link login for
    /// existing recipients). This list gates SELF-registration
    /// only. An operator can, for example, keep public registration
    /// open to `partner-a.com` and `partner-b.io` while allowing
    /// invitations to any domain — the two lists are independent.
    ///
    /// Example: `["partner-a.com", "partner-b.io"]` — only
    /// addresses `<anything>@partner-a.com` or
    /// `<anything>@partner-b.io` can self-register; everything else
    /// is rejected with 403 `RegistrationDomainNotAllowed`.
    ///
    /// Wildcards / subdomain semantics are intentionally out of
    /// scope (mirroring `MagicLinkConfig::allowed_email_domains`):
    /// `partner.com` does NOT match `eng.partner.com`. List every
    /// subdomain explicitly.
    ///
    /// Env: `OXICLOUD_REGISTRATION_ALLOWED_EMAIL_DOMAINS` (comma-
    /// separated).
    pub registration_allowed_email_domains: Vec<String>,
    /// Additive auth-policy toggles the operator has opted into.
    /// Distinct from `allowed_auth_methods` (which enables/disables a
    /// method wholesale) — this vector composes policy switches that
    /// tweak the default auth behaviour. Empty = pure defaults in
    /// effect, matching legacy behaviour.
    ///
    /// Vector shape (rather than one boolean per policy) so future
    /// switches can be added by appending a variant instead of
    /// growing the env-var surface — `OXICLOUD_AUTH_POLICIES=policy_a,policy_b`.
    /// Each variant's name carries its own polarity (`Permit...`,
    /// future `Require...` / `Deny...`); the field name stays neutral
    /// so a future deny-style policy reads correctly at the call site.
    ///
    /// Env: `OXICLOUD_AUTH_POLICIES` (comma-separated).
    ///
    /// Deprecated legacy alias: `OXICLOUD_MAGIC_LINK_OPEN_TO_PASSWORD_USERS=true`
    /// still adds `PermitMagicLinkForPasswordUsers` to the vector for
    /// backwards compatibility; emits a startup warning encouraging
    /// migration to the vector form.
    pub auth_policies: Vec<AuthPolicy>,
    /// Allowlist of self-service auth methods offered on the login
    /// page and accepted by their respective endpoints. Empty (the
    /// default) = both methods allowed, matching legacy behaviour.
    /// OIDC is orthogonal — controlled via `OxidcConfig::enabled`.
    ///
    /// Semantics:
    ///   * `AuthMethod::Password` allowed → `POST /api/auth/login`
    ///     accepts credentials; password-based `register` works.
    ///   * `AuthMethod::MagicLink` allowed → `POST /api/auth/magic-
    ///     link/send` mints tokens; email-only `register` works.
    ///
    /// A method NOT in the list returns 403 with a specific
    /// `error_type` (`PasswordLoginDisabled`,
    /// `MagicLinkLoginDisabled`) so frontends can render a
    /// contextual message rather than a generic auth error.
    ///
    /// Startup guard: when `MagicLink` is in the list but
    /// `SmtpConfig::is_enabled()` is false, the server refuses to
    /// start. A magic-link policy without a mail sender is a
    /// misconfiguration that silently locks users out.
    ///
    /// Env: `OXICLOUD_AUTH_METHODS` (comma-separated:
    /// `password,magic_link`). Alias: the older
    /// `OXICLOUD_OIDC_DISABLE_PASSWORD_LOGIN=true` still removes
    /// Password from this list when set (backwards-compat).
    pub allowed_auth_methods: Vec<AuthMethod>,
    /// Require the user's email to be verified before login is
    /// permitted. When `true`, `POST /api/auth/login` returns 403
    /// `EmailNotVerified` for any account whose `email_verified_at`
    /// is NULL. Users can prove control by clicking a magic-link
    /// (which stamps `email_verified_at`) — so this composes with
    /// `AuthMethod::MagicLink` in the allowlist above to provide a
    /// verification path.
    ///
    /// Admin-created users (`POST /api/admin/users`) and the
    /// first-run setup admin (`POST /api/setup`) get
    /// `email_verified_at = NOW()` at creation — admin fiat counts
    /// as verification, matching the OIDC-JIT convention.
    ///
    /// Env: `OXICLOUD_REQUIRE_VERIFIED_EMAIL` (default `false`).
    pub require_verified_email: bool,
}

/// Self-service auth method. Exposed as `AuthConfig::allowed_auth_methods`
/// and parsed from `OXICLOUD_AUTH_METHODS` (comma-separated). OIDC is
/// deliberately excluded — it lives in `OidcConfig` with its own gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthMethod {
    Password,
    MagicLink,
}

impl AuthMethod {
    /// Case-insensitive parse: accepts `password`, `magic_link`, and the
    /// dash form `magic-link` (some operators habitually use dashes).
    /// Unknown token returns `None` so the caller can log-and-skip.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "password" => Some(Self::Password),
            "magic_link" | "magic-link" | "magiclink" => Some(Self::MagicLink),
            _ => None,
        }
    }
}

/// Additive auth-policy switches. Exposed as `AuthConfig::auth_policies`
/// and parsed from `OXICLOUD_AUTH_POLICIES` (comma-separated). Each
/// variant's name states its own polarity — `Permit...` grants an
/// exception, future `Require...` / `Deny...` variants restrict.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthPolicy {
    /// Allow magic-link login for accounts that ALSO have a password
    /// configured. Off by default — magic-link is otherwise gated by
    /// `magic_link_eligibility()` to users without a password
    /// (mailbox-strength should not shadow a stronger credential).
    /// Enabling this weakens the password to mailbox-strength for
    /// affected accounts; opt-in only.
    ///
    /// Deprecated legacy alias: `OXICLOUD_MAGIC_LINK_OPEN_TO_PASSWORD_USERS=true`
    /// adds this variant to the vector with a startup warning.
    PermitMagicLinkForPasswordUsers,
}

impl AuthPolicy {
    /// Case-insensitive parse: accepts `permit_magic_link_for_password_users`
    /// (canonical) and the dash form. Unknown token returns `None` so
    /// the caller can log-and-skip.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "permit_magic_link_for_password_users" | "permit-magic-link-for-password-users" => {
                Some(Self::PermitMagicLinkForPasswordUsers)
            }
            _ => None,
        }
    }
}

/// Rate limiting and brute-force protection configuration.
#[derive(Debug, Clone)]
pub struct RateLimitConfig {
    /// Max login attempts per IP per window (default: 10)
    pub login_max_requests: u32,
    /// Login rate-limit window in seconds (default: 60)
    pub login_window_secs: u64,
    /// Max registration attempts per IP per window (default: 5)
    pub register_max_requests: u32,
    /// Registration rate-limit window in seconds (default: 3600)
    pub register_window_secs: u64,
    /// Max token refresh attempts per IP per window (default: 20)
    pub refresh_max_requests: u32,
    /// Refresh rate-limit window in seconds (default: 60)
    pub refresh_window_secs: u64,
    /// Consecutive failed logins before account lockout (default: 5)
    pub lockout_max_failures: u32,
    /// Account lockout duration in seconds (default: 900 = 15 min)
    pub lockout_duration_secs: u64,
}

impl Default for RateLimitConfig {
    fn default() -> Self {
        Self {
            login_max_requests: 10,
            login_window_secs: 60,
            register_max_requests: 5,
            register_window_secs: 3600,
            refresh_max_requests: 20,
            refresh_window_secs: 60,
            lockout_max_failures: 5,
            lockout_duration_secs: 900,
        }
    }
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            // SECURITY: This default is intentionally insecure to force operators
            // to set OXICLOUD_JWT_SECRET in production. The from_env() method
            // will validate this and warn/panic if not configured.
            jwt_secret: String::new(),
            access_token_expiry_secs: 3600,    // 1 hour
            refresh_token_expiry_secs: 604800, // 7 days — with rotation, active sessions auto-renew
            hash_memory_cost: 65536,           // 64 MiB
            hash_time_cost: 3,
            hash_parallelism: 2,
            rate_limit: RateLimitConfig::default(),
            registration_allowed_email_domains: Vec::new(),
            auth_policies: Vec::new(),
            allowed_auth_methods: vec![AuthMethod::Password, AuthMethod::MagicLink],
            require_verified_email: false,
        }
    }
}

impl AuthConfig {
    /// True iff `method` is enabled (or the allowlist is empty — meaning
    /// "all methods allowed", matching pre-`OXICLOUD_AUTH_METHODS`
    /// behaviour when the operator hasn't opted in yet).
    pub fn is_method_allowed(&self, method: AuthMethod) -> bool {
        self.allowed_auth_methods.is_empty() || self.allowed_auth_methods.contains(&method)
    }

    /// True iff `policy` has been opted into via `OXICLOUD_AUTH_POLICIES`
    /// (or its legacy alias). Default policies are OFF — the vector is
    /// additive only, no invert / defaults.
    pub fn has_policy(&self, policy: AuthPolicy) -> bool {
        self.auth_policies.contains(&policy)
    }
}

/// OpenID Connect (OIDC) configuration
#[derive(Debug, Clone)]
pub struct OidcConfig {
    /// Whether OIDC authentication is enabled
    pub enabled: bool,
    /// OIDC Issuer URL (e.g. https://authentik.example.com/application/o/oxicloud/)
    pub issuer_url: String,
    /// OIDC Client ID
    pub client_id: String,
    /// OIDC Client Secret
    pub client_secret: String,
    /// Redirect URI after OIDC authentication (must match IdP config)
    pub redirect_uri: String,
    /// OIDC scopes to request
    pub scopes: String,
    /// Frontend URL to redirect after successful OIDC login (tokens appended as fragment)
    pub frontend_url: String,
    /// Whether to auto-create users on first OIDC login (JIT provisioning)
    pub auto_provision: bool,
    /// Comma-separated list of OIDC groups that map to admin role
    pub admin_groups: String,
    /// Whether to disable password-based login entirely
    pub disable_password_login: bool,
    /// OIDC provider display name (shown in UI)
    pub provider_name: String,
}

impl Default for OidcConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            issuer_url: String::new(),
            client_id: String::new(),
            client_secret: String::new(),
            redirect_uri: "http://localhost:8086/api/auth/oidc/callback".to_string(),
            scopes: "openid profile email".to_string(),
            frontend_url: "http://localhost:8086".to_string(),
            auto_provision: true,
            admin_groups: String::new(),
            disable_password_login: false,
            provider_name: "SSO".to_string(),
        }
    }
}

impl OidcConfig {
    /// Load OIDC configuration from environment variables only
    pub fn from_env() -> Self {
        use std::env;
        let mut cfg = Self::default();
        if let Ok(v) = env::var("OXICLOUD_OIDC_ENABLED") {
            cfg.enabled = v.parse::<bool>().unwrap_or(false);
        }
        if let Ok(v) = env::var("OXICLOUD_OIDC_ISSUER_URL") {
            cfg.issuer_url = v;
        }
        if let Ok(v) = env::var("OXICLOUD_OIDC_CLIENT_ID") {
            cfg.client_id = v;
        }
        if let Ok(v) = env::var("OXICLOUD_OIDC_CLIENT_SECRET") {
            cfg.client_secret = v;
        }
        if let Ok(v) = env::var("OXICLOUD_OIDC_REDIRECT_URI") {
            cfg.redirect_uri = v;
        }
        if let Ok(v) = env::var("OXICLOUD_OIDC_SCOPES") {
            cfg.scopes = v;
        }
        if let Ok(v) = env::var("OXICLOUD_OIDC_FRONTEND_URL") {
            cfg.frontend_url = v;
        }
        if let Ok(v) = env::var("OXICLOUD_OIDC_AUTO_PROVISION") {
            cfg.auto_provision = v.parse::<bool>().unwrap_or(true);
        }
        if let Ok(v) = env::var("OXICLOUD_OIDC_ADMIN_GROUPS") {
            cfg.admin_groups = v;
        }
        if let Ok(v) = env::var("OXICLOUD_OIDC_DISABLE_PASSWORD_LOGIN") {
            cfg.disable_password_login = v.parse::<bool>().unwrap_or(false);
        }
        if let Ok(v) = env::var("OXICLOUD_OIDC_PROVIDER_NAME") {
            cfg.provider_name = v;
        }
        cfg
    }
}

/// WOPI (Web Application Open Platform Interface) configuration
#[derive(Debug, Clone)]
pub struct WopiConfig {
    /// Whether WOPI integration is enabled
    pub enabled: bool,
    /// URL to the WOPI client's discovery endpoint
    /// e.g., "http://collabora:9980/hosting/discovery"
    pub discovery_url: String,
    /// Secret key for signing WOPI access tokens
    /// Falls back to JWT secret if empty
    pub secret: String,
    /// Access token TTL in seconds (default: 86400 = 24 hours)
    pub token_ttl_secs: i64,
    /// Lock expiration in seconds (default: 1800 = 30 minutes)
    pub lock_ttl_secs: u64,
}

impl Default for WopiConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            discovery_url: String::new(),
            secret: String::new(),
            token_ttl_secs: 86400,
            lock_ttl_secs: 1800,
        }
    }
}

/// Nextcloud compatibility configuration
#[derive(Debug, Clone)]
pub struct NextcloudConfig {
    /// Whether the Nextcloud compatibility layer is enabled
    pub enabled: bool,
    /// Instance ID suffix for oc:id formatting (e.g., "ocnca")
    pub instance_id: String,
    /// Emulated Nextcloud version (major.minor.patch).
    /// Clients use this to decide which features to enable.
    pub emulated_version: (u32, u32, u32),
    /// Login Flow v2 token TTL in seconds (default: 600 = 10 minutes)
    pub login_flow_ttl_secs: u64,
}

impl Default for NextcloudConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            instance_id: "ocnca".to_string(),
            emulated_version: (28, 0, 4),
            login_flow_ttl_secs: 600,
        }
    }
}

impl NextcloudConfig {
    /// Version string, e.g. "28.0.4".
    pub fn version_string(&self) -> String {
        let (maj, min, pat) = self.emulated_version;
        format!("{}.{}.{}", maj, min, pat)
    }
}

/// Transport encryption mode for the SMTP relay. Picked at startup
/// from `OXICLOUD_SMTP_TLS=starttls|tls|none`. The default for an
/// unconfigured deployment is `Starttls` (port 587 with `STARTTLS`),
/// matching the most common modern submission setup.
///
/// `None` is allowed for development against MailHog / a local
/// netcat trap. Production deployments using `None` get a startup
/// `WARN` log so the choice is visible in operational telemetry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SmtpTlsMode {
    /// Plain submission with `STARTTLS` upgrade (RFC 3207). Standard
    /// for port 587.
    Starttls,
    /// Implicit TLS from the first byte (RFC 8314). Standard for
    /// port 465.
    Tls,
    /// No encryption. Development only.
    None,
}

impl SmtpTlsMode {
    fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "starttls" => Some(Self::Starttls),
            "tls" | "implicit" | "smtps" => Some(Self::Tls),
            "none" | "plain" => Some(Self::None),
            _ => None,
        }
    }
}

/// Outbound SMTP transport configuration. Sourced exclusively from
/// `OXICLOUD_SMTP_*` env vars. `host` empty means the feature is
/// disabled — every endpoint that needs email returns 503 in that
/// state so admins notice misconfiguration immediately rather than
/// silently dropping mail.
#[derive(Debug, Clone)]
pub struct SmtpConfig {
    /// SMTP server hostname or IP. Empty string disables the feature.
    pub host: String,
    /// Submission port (typically 587 for STARTTLS, 465 for implicit
    /// TLS, 25 for relay-to-relay).
    pub port: u16,
    /// SASL username. Empty = no authentication (anonymous relay).
    pub user: String,
    /// SASL password. Logged as `***` redacted in startup banner.
    pub pass: String,
    /// `From:` mailbox. Either a bare address (`noreply@example.com`)
    /// or RFC 5322 name-address (`OxiCloud <noreply@example.com>`).
    pub from: String,
    /// Transport encryption mode. See [`SmtpTlsMode`].
    pub tls: SmtpTlsMode,
}

impl Default for SmtpConfig {
    fn default() -> Self {
        Self {
            host: String::new(),
            port: 587,
            user: String::new(),
            pass: String::new(),
            from: String::new(),
            tls: SmtpTlsMode::Starttls,
        }
    }
}

impl SmtpConfig {
    /// `true` iff `OXICLOUD_SMTP_HOST` was set to a non-empty value.
    /// Used by DI to decide whether to construct an `EmailSender`.
    pub fn is_enabled(&self) -> bool {
        !self.host.is_empty()
    }
}

/// Magic-link authentication configuration. Knobs that are specific to
/// the invite-by-email / login-via-email flow.
#[derive(Debug, Clone)]
pub struct MagicLinkConfig {
    /// TTL for **login-via-email** tokens (the ones a user requests
    /// themselves from their own browser). Short by design — the user
    /// just clicked the button moments before; if they take >10 minutes
    /// to click the link, something's wrong. Combined with the per-
    /// request challenge cookie (PR 22), this bounds the window for
    /// mailbox compromise to turn into a session.
    ///
    /// Default: 10 minutes.
    pub login_ttl_minutes: u64,
    /// TTL for **invitation** tokens (the ones a sharer mints via
    /// `POST /api/grants` for a recipient who has no prior browser
    /// context with the server). Long because the recipient may not
    /// check their email for hours or days. Cross-device by design;
    /// no challenge cookie.
    ///
    /// Default: 24 hours. The legacy `OXICLOUD_MAGIC_LINK_TTL_HOURS`
    /// env var is a deprecated alias that writes here.
    pub invite_ttl_hours: u64,
    /// Kill switch for the whole magic-link flow. When `false`:
    /// - `POST /api/grants` rejects `subject.type = "email"` for unknown
    ///   email addresses (no lazy external-user creation).
    /// - `POST /api/auth/magic-link/send` returns the uniform stub
    ///   response without actually issuing a token.
    ///
    /// This is the coarser "turn it all off" switch; the fine-grained
    /// version is [`allowed_email_domains`] below.
    pub allow_external_users: bool,
    /// Allowlist of email domains accepted when minting a new external
    /// user. Empty = no restriction (any domain is allowed, subject to
    /// [`allow_external_users`]). Entries are lowercased and trimmed
    /// at load time; matching is case-insensitive exact-match on the
    /// post-`@` part of the address.
    ///
    /// Example: `["partner-a.com", "partner-b.io"]` — only addresses
    /// `<anything>@partner-a.com` or `<anything>@partner-b.io` can be
    /// invited; everything else is rejected with 403.
    ///
    /// Wildcards / subdomain semantics are intentionally out of scope:
    /// `partner.com` does NOT match `eng.partner.com`. List every
    /// subdomain explicitly.
    pub allowed_email_domains: Vec<String>,
    /// Per-sharer ceiling on email-typed grant invitations from
    /// `POST /api/grants`. Keyed on `caller_id`. Exceeding the ceiling
    /// returns 429. Default: 50/hour.
    pub invite_per_caller_per_hour: u32,
    /// Per-target-email ceiling on `POST /api/auth/magic-link/send`,
    /// keyed on the normalised recipient address. Anti-bombing.
    /// Exceeding the ceiling is silently absorbed (uniform 200) so
    /// the response shape can't be used as an enumeration oracle.
    /// Default: 5/hour.
    pub send_per_email_per_hour: u32,
    /// Per-source-IP backstop on `POST /api/auth/magic-link/send`,
    /// keyed on the trusted client IP. Bounds the cost of an attacker
    /// spreading low per-email volume across many target addresses.
    /// Default: 200/hour.
    pub send_per_ip_per_hour: u32,
    /// Policy switch: whether magic-link is offered to users who
    /// already have a password configured.
    ///
    /// - `false` (default, strict): users with a password get
    ///   audit-logged `has_password` and no mail. Their password is
    ///   the only authentication path; magic-link would weaken it to
    ///   "mailbox compromise = account compromise".
    /// - `true` (lenient): users with a password can also request a
    ///   magic-link as a sign-in path. Aligns with modern SaaS UX
    ///   (Slack, Notion, etc.) — operators who treat email as the
    ///   canonical recovery channel anyway pick this.
    ///
    /// OIDC-linked users are **always** rejected from magic-link
    /// regardless of this flag — the IdP is the security boundary and
    /// may enforce MFA we shouldn't bypass. See
    /// `magic_link_eligibility()` for the precedence ladder.
    pub open_to_password_users: bool,
    /// Operator-level kill switch for plain-notification emails to
    /// internal users (PR N1). When `true` (default), users who can't
    /// receive a magic link (password users, OIDC users) get a "Hey,
    /// you got a new grant" mail with a `/login` deep link on every
    /// share. When `false`, the plain-notification arm is suppressed
    /// entirely — internal users discover shares only on next login.
    ///
    /// This is a coarser knob than the per-user
    /// `auth.users.notify_on_share` column: when this is `false`, the
    /// user-level opt-in does not matter. External-user magic-link
    /// invitations are NOT affected by this flag — those always send,
    /// because the link is the only way the recipient can claim the
    /// share for the first time.
    pub notify_internal_users_on_share: bool,
}

impl Default for MagicLinkConfig {
    fn default() -> Self {
        Self {
            login_ttl_minutes: 10,
            invite_ttl_hours: 24,
            allow_external_users: true,
            allowed_email_domains: Vec::new(),
            invite_per_caller_per_hour: 50,
            send_per_email_per_hour: 5,
            send_per_ip_per_hour: 200,
            open_to_password_users: false,
            notify_internal_users_on_share: true,
        }
    }
}

impl MagicLinkConfig {
    /// Whether an email address is allowed under the current allowlist.
    ///
    /// Returns `true` when the allowlist is empty (no restriction).
    /// Otherwise the domain part of `email` (lowercased) must match one
    /// of the allowlist entries exactly. Malformed addresses without an
    /// `@` always return `false` — fail closed so a typo in the
    /// upstream validator can't slip past this check.
    ///
    /// Caller is expected to have already passed `email` through the
    /// email regex / normaliser; this method does not re-validate. It
    /// only performs the domain comparison.
    pub fn is_email_allowed(&self, email: &str) -> bool {
        if self.allowed_email_domains.is_empty() {
            return true;
        }
        let Some((_, domain)) = email.rsplit_once('@') else {
            return false;
        };
        let domain_lc = domain.to_ascii_lowercase();
        self.allowed_email_domains
            .iter()
            .any(|d| d.as_str() == domain_lc.as_str())
    }
}

/// Feature configuration (feature flags)
#[derive(Debug, Clone)]
pub struct FeaturesConfig {
    pub enable_auth: bool,
    pub enable_user_storage_quotas: bool,
    pub enable_file_sharing: bool,
    pub enable_trash: bool,
    pub enable_search: bool,
    pub enable_music: bool,
    /// Lists the user's geotagged photos on a map (GET /api/photos/geo).
    pub enable_places: bool,
    /// Face detection + identity clustering for the photo library ("People").
    /// Biometric data — OFF by default; opt-in per deployment/user.
    pub enable_faces: bool,
    /// Expose other OxiCloud users as a read-only "system" address book
    /// at GET /api/address-books. Set to false to hide the user directory.
    pub expose_system_users: bool,
    /// Generate video thumbnails server-side via `ffmpeg` on upload. When true
    /// (and ffmpeg is detected at startup) videos get a representative-frame
    /// thumbnail through the same WebP pipeline as photos; otherwise videos have
    /// no thumbnail. Env: `OXICLOUD_ENABLE_VIDEO_THUMBNAILS`.
    pub enable_video_thumbnails: bool,
    /// Expose admin-configured external filesystem mounts (raw host fs, …) as
    /// folders inside a user's drive. Contents are read live from the backend
    /// and are a deliberately limited, separate storage type (no dedup/sharing/
    /// trash/search). OFF by default — opt-in per deployment.
    /// Env: `OXICLOUD_ENABLE_EXTERNAL_MOUNTS`.
    pub enable_external_mounts: bool,
    /// Native WebDAV path segment that lists the caller's drives.
    ///
    /// * Default `"@drive"` — bare `/webdav/` addresses the caller's
    ///   default personal drive (back-compat). Drive listing lives at
    ///   `/webdav/@drive/`; explicit drive at
    ///   `/webdav/@drive/<uuid|name>/…`.
    /// * `""` (empty) — no default-drive shortcut. Bare `/webdav/`
    ///   returns the drive listing; explicit drive at
    ///   `/webdav/<uuid|name>/…`. Operators who don't want a "default
    ///   drive" concept exposed via WebDAV pick this.
    /// * Any other string (e.g. `"drives"`) — same shape as the default,
    ///   just with that path segment. Loaded via `trim_matches('/')`
    ///   so operators can safely pass `"/drives/"`.
    ///
    /// Env: `OXICLOUD_WEBDAV_DRIVE_LISTING_PREFIX`.
    pub webdav_drive_listing_prefix: String,

    /// Background purge of expired `storage.role_grants` rows.
    ///
    /// The AuthZ engine already filters expired grants out of every
    /// permission check at read time (`expires_at IS NULL OR
    /// expires_at > NOW()`), so leaving the rows in place is a
    /// hygiene issue — not a security one. This purge deletes rows
    /// whose `expires_at` is more than [`GrantCleanupConfig::grace_days`]
    /// in the past, preserving the audit / support answer to
    /// "what happened to my access?" for the grace window.
    ///
    /// Enabled by default: expired-auth-row cleanup is a
    /// security-hygiene default, not opt-in.
    pub grant_cleanup: GrantCleanupConfig,
}

/// Config for the daily expired-grant purge (see
/// [`FeaturesConfig::grant_cleanup`]).
#[derive(Debug, Clone)]
pub struct GrantCleanupConfig {
    /// Master switch. Env: `OXICLOUD_GRANT_CLEANUP_ENABLED`
    /// (default `true`).
    pub enabled: bool,
    /// Days past a grant's `expires_at` before the row is eligible
    /// for deletion. Env: `OXICLOUD_GRANT_CLEANUP_GRACE_DAYS`
    /// (default `15`).
    ///
    /// The recommendation is `> 15` — enough to answer
    /// support/audit questions about recently-lapsed grants without
    /// keeping dead rows forever.
    pub grace_days: u32,
    /// How often the daemon fires, in hours. Env:
    /// `OXICLOUD_GRANT_CLEANUP_INTERVAL_HOURS` (default `24`).
    pub interval_hours: u64,
}

impl Default for GrantCleanupConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            grace_days: 15,
            interval_hours: 24,
        }
    }
}

impl Default for FeaturesConfig {
    fn default() -> Self {
        Self {
            enable_auth: true, // Enable authentication by default
            enable_user_storage_quotas: false,
            enable_file_sharing: true,     // Enable file sharing by default
            enable_trash: true,            // Enable trash feature
            enable_search: true,           // Enable search feature
            enable_music: true,            // Enable music feature
            enable_places: true,           // Photo map (GET /api/photos/geo + Places tab)
            enable_faces: false,           // People/faces (biometric) — opt-in, off by default
            expose_system_users: true,     // Expose OxiCloud users as address book by default
            enable_video_thumbnails: true, // Video thumbs via ffmpeg (if detected)
            enable_external_mounts: false, // External mounts — opt-in, off by default
            // Back-compat with pre-multi-drive clients — bare `/webdav/`
            // maps to the caller's default drive; drive listing is
            // reachable at `/webdav/@drive/`.
            webdav_drive_listing_prefix: "@drive".to_string(),
            grant_cleanup: GrantCleanupConfig::default(),
        }
    }
}

/// Face-recognition (People) model configuration.
///
/// Only consulted when the `faces-onnx` cargo feature is compiled in *and*
/// [`FeaturesConfig::enable_faces`] is true; otherwise the inert
/// `NoopFaceAnalyzer` is used regardless of these values. The ONNX Runtime
/// dylib and both model files are operator-provided at runtime (never
/// committed) — when any is unset or fails to load, the People pipeline
/// silently falls back to the no-op analyzer and the server still boots.
#[derive(Debug, Clone)]
pub struct FacesConfig {
    /// `libonnxruntime.{so,dylib,dll}`. Falls back to the `ORT_DYLIB_PATH`
    /// environment variable when unset. Env: `OXICLOUD_FACES_ORT_DYLIB`.
    pub ort_dylib: Option<PathBuf>,
    /// SCRFD/RetinaFace detector model with 5-point landmarks.
    /// Env: `OXICLOUD_FACES_DETECTOR_MODEL`.
    pub detector_model: Option<PathBuf>,
    /// ArcFace embedder model (112×112 → 512-d).
    /// Env: `OXICLOUD_FACES_EMBEDDER_MODEL`.
    pub embedder_model: Option<PathBuf>,
    /// Detector square input size in pixels (default 640).
    /// Env: `OXICLOUD_FACES_DET_SIZE`.
    pub det_size: u32,
    /// Minimum detector confidence to keep a face (default 0.5).
    /// Env: `OXICLOUD_FACES_DET_THRESHOLD`.
    pub det_threshold: f32,
    /// IoU threshold for non-max suppression (default 0.4).
    /// Env: `OXICLOUD_FACES_NMS_THRESHOLD`.
    pub nms_threshold: f32,
    /// ONNX Runtime intra-op threads (0 = let ORT decide).
    /// Env: `OXICLOUD_FACES_INTRA_THREADS`.
    pub intra_threads: usize,
}

impl Default for FacesConfig {
    fn default() -> Self {
        Self {
            ort_dylib: None,
            detector_model: None,
            embedder_model: None,
            det_size: 640,
            det_threshold: 0.5,
            nms_threshold: 0.4,
            intra_threads: 0,
        }
    }
}

/// Content-search configuration (embedded Tantivy index over file names and
/// extracted file content).
///
/// The index is a derived artifact fed by a background worker on the
/// maintenance pool — none of these knobs affect request-path latency.
#[derive(Debug, Clone)]
pub struct ContentSearchConfig {
    /// Master switch. When disabled, search falls back to name-only SQL and
    /// a janitor keeps the (always-installed) dirty queue empty.
    /// Env: `OXICLOUD_ENABLE_CONTENT_SEARCH`.
    pub enabled: bool,
    /// Index directory. Default: `{storage_path}/.search-index`.
    /// Env: `OXICLOUD_CONTENT_INDEX_DIR`.
    pub index_dir: Option<PathBuf>,
    /// Worker drain cadence in milliseconds — the upper bound on how long a
    /// new upload takes to become content-searchable. Default: 1500.
    /// Env: `OXICLOUD_CONTENT_INDEX_FLUSH_MS`.
    pub flush_interval_ms: u64,
    /// Files larger than this are indexed by NAME only (no text extraction).
    /// Default: 32 MiB. Env: `OXICLOUD_CONTENT_INDEX_MAX_FILE_BYTES`.
    pub max_extract_file_bytes: u64,
    /// Hard cap on extracted text per blob fed to the index. Default: 1 MiB.
    /// Env: `OXICLOUD_CONTENT_INDEX_MAX_TEXT_BYTES`.
    pub max_text_bytes: usize,
}

impl Default for ContentSearchConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            index_dir: None,
            flush_interval_ms: 1500,
            max_extract_file_bytes: 32 * 1024 * 1024,
            max_text_bytes: 1024 * 1024,
        }
    }
}

/// Search-results cache configuration — the per-user results-page cache
/// inside `SearchService`, not the Tantivy content index above.
///
/// The cache is **byte-bounded**: each entry is weighed by the approximate
/// heap size of its result page (see `search_results_entry_weight`) and moka
/// evicts once the summed weight exceeds `max_bytes` — the same byte-budget
/// pattern the file-content cache and the dedup manifest cache use. This
/// replaced an entry-count capacity: with cache keys spanning
/// user × query × offset × limit and up to 500 enriched rows per page, an
/// entry count said nothing about resident memory (1000 entries could pin
/// ~300 MB for the TTL). No entry-count knob is kept — bytes are the only
/// dimension that matters here.
#[derive(Debug, Clone)]
pub struct SearchCacheConfig {
    /// Byte budget for cached search-result pages. Default: 32 MiB.
    /// Env: `OXICLOUD_SEARCH_CACHE_MAX_BYTES`.
    pub max_bytes: u64,
}

impl Default for SearchCacheConfig {
    fn default() -> Self {
        Self {
            max_bytes: 32 * 1024 * 1024,
        }
    }
}

/// WASM plugin runtime configuration (M0 walking skeleton).
///
/// The runtime is doubly gated: it is only compiled when the `plugins` cargo
/// feature is enabled, and only activated when `enabled` is `true`. The limits
/// below are conservative starting defaults, not part of the plugin ABI — each
/// deployment may tune them.
#[derive(Debug, Clone)]
pub struct PluginConfig {
    /// Master switch. When disabled, no plugins are loaded and the lifecycle
    /// bridge hook is never registered. Env: `OXICLOUD_ENABLE_PLUGINS`.
    pub enabled: bool,
    /// Directory scanned for plugins at startup; each plugin is a subdirectory
    /// containing `plugin.toml` + its `.wasm`. Default: `{storage_path}/.plugins`.
    /// Env: `OXICLOUD_PLUGINS_DIR`.
    pub plugins_dir: Option<PathBuf>,
    /// Wall-clock timeout for a single `handle` invocation. A runaway plugin
    /// cannot stall the upload path beyond this. Default: 250.
    /// Env: `OXICLOUD_PLUGIN_TIMEOUT_MS`.
    pub invocation_timeout_ms: u64,
    /// Max linear memory per plugin instance, in WASM pages (64 KiB each).
    /// Default: 256 (≈ 16 MiB). Env: `OXICLOUD_PLUGIN_MAX_MEMORY_PAGES`.
    pub max_memory_pages: u32,
    /// Hard cap on the serialized event payload handed to a plugin. Default:
    /// 256 KiB. Env: `OXICLOUD_PLUGIN_MAX_INPUT_BYTES`.
    pub max_input_bytes: usize,
    /// Directory under which per-plugin log files live (one subdir per plugin id,
    /// holding `events.jsonl` + rotated `events.jsonl.<ts>.gz` + `retention.json`).
    /// Default: `{storage_path}/.plugin-logs`. Env: `OXICLOUD_PLUGIN_LOG_DIR`.
    pub log_dir: Option<PathBuf>,
    /// Size at which a plugin's active `events.jsonl` is rotated into a new gzip
    /// segment. Default: 5 MiB. Env: `OXICLOUD_PLUGIN_LOG_MAX_FILE_BYTES`.
    pub log_max_file_bytes: u64,
    /// Coarse ceiling on the number of rotated `.gz` segments kept per plugin
    /// (file-rotate `FileLimit::MaxFiles`); the real limits are the per-plugin
    /// retention sweep. Default: 10. Env: `OXICLOUD_PLUGIN_LOG_MAX_SEGMENTS`.
    pub log_max_segments: u32,
    /// Default age (in days) past which a plugin's rotated log segments are
    /// pruned by the maintenance sweep. Overridable per plugin via its
    /// `retention.json`. Default: 30. Env: `OXICLOUD_PLUGIN_LOG_RETENTION_DAYS`.
    pub log_retention_days: u32,
    /// Default aggregate byte cap on kept log segments for a single plugin; the
    /// sweep deletes oldest-first past this. Overridable per plugin. Default:
    /// 256 MiB. Env: `OXICLOUD_PLUGIN_LOG_TOTAL_MAX_BYTES`.
    pub log_total_max_bytes: u64,
    /// Max plugin invocations running concurrently across all plugins. Dispatch
    /// sheds load (drops the event, audit-logged) past this rather than
    /// unbounded `spawn_blocking`, so plugins can't starve the shared blocking
    /// pool. Default: 16. Env: `OXICLOUD_PLUGIN_MAX_CONCURRENT_INVOCATIONS`.
    pub max_concurrent_invocations: usize,
    /// Bounded depth of the log-store command channel. A flood past this drops
    /// the oldest-arriving log batch (never blocks dispatch). Default: 1024.
    /// Env: `OXICLOUD_PLUGIN_LOG_QUEUE_CAPACITY`.
    pub log_queue_capacity: usize,
    /// Idle window after which a plugin's cached compiled module is dropped to
    /// reclaim memory; the next event recompiles from wasmtime's on-disk cache.
    /// Default: 300 (5 min). Env: `OXICLOUD_PLUGIN_CACHE_IDLE_TTL_SECS`.
    pub cache_idle_ttl_secs: u64,
    /// Aggregate decompressed-byte ceiling enforced while unpacking an install
    /// bundle (zip-bomb guard; the install route also caps the compressed body).
    /// Default: 64 MiB. Env: `OXICLOUD_PLUGIN_MAX_BUNDLE_DECOMPRESSED_BYTES`.
    pub max_bundle_decompressed_bytes: u64,
}

impl Default for PluginConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            plugins_dir: None,
            invocation_timeout_ms: 250,
            max_memory_pages: 256,
            max_input_bytes: 256 * 1024,
            log_dir: None,
            log_max_file_bytes: 5 * 1024 * 1024,
            log_max_segments: 10,
            log_retention_days: 30,
            log_total_max_bytes: 256 * 1024 * 1024,
            max_concurrent_invocations: 16,
            log_queue_capacity: 1024,
            cache_idle_ttl_secs: 300,
            max_bundle_decompressed_bytes: 64 * 1024 * 1024,
        }
    }
}

/// Global application configuration
#[derive(Debug, Clone)]
pub struct AppConfig {
    /// Storage directory path
    pub storage_path: PathBuf,
    /// Static files directory path
    pub static_path: PathBuf,
    /// Server port
    pub server_port: u16,
    /// Server host
    pub server_host: String,
    /// Cache configuration
    pub cache: CacheConfig,
    /// Timeout configuration
    pub timeouts: TimeoutConfig,
    /// Resource configuration
    pub resources: ResourceConfig,
    /// Concurrency configuration
    pub concurrency: ConcurrencyConfig,
    /// Storage configuration
    pub storage: StorageConfig,
    /// Database configuration
    pub database: DatabaseConfig,
    /// Authentication configuration
    pub auth: AuthConfig,
    /// Feature configuration
    pub features: FeaturesConfig,
    /// OIDC configuration
    pub oidc: OidcConfig,
    /// WOPI configuration
    pub wopi: WopiConfig,
    /// Nextcloud compatibility configuration
    pub nextcloud: NextcloudConfig,
    /// Outbound SMTP configuration (magic-link invitations, etc.)
    pub smtp: SmtpConfig,
    /// Magic-link authentication configuration (TTL, external-users kill switch)
    pub magic_link: MagicLinkConfig,
    /// I18n configuration (default locale for server-rendered surfaces)
    pub i18n: I18nConfig,
    /// Content-search configuration (embedded full-text index)
    pub content_search: ContentSearchConfig,
    /// Search-results cache configuration (byte-bounded moka cache)
    pub search_cache: SearchCacheConfig,
    /// WASM plugin runtime configuration
    pub plugins: PluginConfig,
    /// Face-recognition (People) model configuration
    pub faces: FacesConfig,
}

/// Server-side i18n knobs.
///
/// Locale discovery itself is driven by `static/locales/*.json` at boot
/// (see [`crate::common::locale::LocaleRegistry`]) — no hardcoded list,
/// no `build.rs`. This struct only carries the configurable defaults
/// around that discovery.
#[derive(Debug, Clone)]
pub struct I18nConfig {
    /// Fallback locale used when:
    /// - an anonymous request's `Accept-Language` matches nothing in
    ///   the registry,
    /// - a user's `preferred_locale` is `NULL`,
    /// - an OIDC `locale` claim doesn't resolve.
    ///
    /// Must be present in `static/locales/`; the registry-build step
    /// errors at startup if this is set to a locale we don't ship.
    /// Defaults to `"en"`. Override via `OXICLOUD_DEFAULT_LOCALE`.
    pub default_locale: String,
}

impl Default for I18nConfig {
    fn default() -> Self {
        Self {
            default_locale: "en".to_string(),
        }
    }
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            storage_path: PathBuf::from("./storage"),
            static_path: PathBuf::from("./static"),
            server_port: 8086,
            server_host: "127.0.0.1".to_string(),
            cache: CacheConfig::default(),
            timeouts: TimeoutConfig::default(),
            resources: ResourceConfig::default(),
            concurrency: ConcurrencyConfig::default(),
            storage: StorageConfig::default(),
            database: DatabaseConfig::default(),
            auth: AuthConfig::default(),
            features: FeaturesConfig::default(),
            oidc: OidcConfig::default(),
            wopi: WopiConfig::default(),
            nextcloud: NextcloudConfig::default(),
            smtp: SmtpConfig::default(),
            magic_link: MagicLinkConfig::default(),
            i18n: I18nConfig::default(),
            content_search: ContentSearchConfig::default(),
            search_cache: SearchCacheConfig::default(),
            plugins: PluginConfig::default(),
            faces: FacesConfig::default(),
        }
    }
}

impl AppConfig {
    pub fn from_env() -> Self {
        let mut config = Self::default();

        // Use environment variables to override default values
        if let Ok(storage_path) = env::var("OXICLOUD_STORAGE_PATH") {
            config.storage_path = PathBuf::from(storage_path);
        }

        if let Ok(static_path) = env::var("OXICLOUD_STATIC_PATH") {
            config.static_path = PathBuf::from(static_path);
        }

        if let Ok(server_port) = env::var("OXICLOUD_SERVER_PORT")
            && let Ok(port) = server_port.parse::<u16>()
        {
            config.server_port = port;
        }

        if let Ok(server_host) = env::var("OXICLOUD_SERVER_HOST") {
            config.server_host = server_host;
        }

        // Database configuration
        if let Ok(connection_string) = env::var("OXICLOUD_DB_CONNECTION_STRING") {
            config.database.connection_string = connection_string;
        }

        if let Ok(max_connections) =
            env::var("OXICLOUD_DB_MAX_CONNECTIONS").map(|v| v.parse::<u32>())
            && let Ok(val) = max_connections
        {
            config.database.max_connections = val;
        }

        if let Ok(min_connections) =
            env::var("OXICLOUD_DB_MIN_CONNECTIONS").map(|v| v.parse::<u32>())
            && let Ok(val) = min_connections
        {
            config.database.min_connections = val;
        }

        if let Ok(max_conn) =
            env::var("OXICLOUD_DB_MAINTENANCE_MAX_CONNECTIONS").map(|v| v.parse::<u32>())
            && let Ok(val) = max_conn
        {
            config.database.maintenance_max_connections = val;
        }

        if let Ok(min_conn) =
            env::var("OXICLOUD_DB_MAINTENANCE_MIN_CONNECTIONS").map(|v| v.parse::<u32>())
            && let Ok(val) = min_conn
        {
            config.database.maintenance_min_connections = val;
        }

        if let Ok(stmt_timeout) =
            env::var("OXICLOUD_DB_STATEMENT_TIMEOUT_SECS").map(|v| v.parse::<u64>())
            && let Ok(val) = stmt_timeout
        {
            config.database.statement_timeout_secs = val;
        }

        if let Ok(interval) =
            env::var("OXICLOUD_DB_POOL_MONITOR_INTERVAL_SECS").map(|v| v.parse::<u64>())
            && let Ok(val) = interval
        {
            config.database.pool_monitor_interval_secs = val;
        }

        // Auth configuration
        if let Some(jwt_secret) = env::var("OXICLOUD_JWT_SECRET")
            .ok()
            .filter(|s| !s.is_empty())
        {
            // SECURITY: Validate JWT secret minimum entropy (RFC 7518 §3.2
            // recommends ≥256 bits for HS256). Panic on dangerously short
            // secrets, warn on sub-optimal ones.
            let len = jwt_secret.len();
            if config.features.enable_auth && len < 16 {
                panic!(
                    "FATAL: OXICLOUD_JWT_SECRET is dangerously short ({} bytes). \
                     Minimum: 32 bytes (256 bits) for HS256. \
                     Generate a secure secret with: openssl rand -hex 32",
                    len
                );
            } else if config.features.enable_auth && len < 32 {
                tracing::warn!("==========================================================");
                tracing::warn!(
                    "OXICLOUD_JWT_SECRET is only {} bytes — recommended minimum is 32 (256 bits).",
                    len
                );
                tracing::warn!("Generate a stronger secret with: openssl rand -hex 32");
                tracing::warn!("==========================================================");
            }
            config.auth.jwt_secret = jwt_secret;
        }

        // SECURITY: Auto-persist JWT secret to storage so it survives restarts.
        // Priority: env var > persisted file > generate new.
        if config.features.enable_auth && config.auth.jwt_secret.is_empty() {
            let secret_file = config.storage_path.join(".jwt_secret");

            if secret_file.exists() {
                // Read persisted secret from previous run
                match std::fs::read_to_string(&secret_file) {
                    Ok(persisted) => {
                        let persisted = persisted.trim().to_string();
                        if persisted.len() >= 32 {
                            config.auth.jwt_secret = persisted;
                            tracing::info!("JWT secret loaded from {}", secret_file.display());
                        } else {
                            tracing::warn!(
                                "Persisted JWT secret too short ({}B), regenerating",
                                persisted.len()
                            );
                        }
                    }
                    Err(e) => {
                        tracing::warn!("Failed to read {}: {}", secret_file.display(), e);
                    }
                }
            }

            // Still empty → generate and persist
            if config.auth.jwt_secret.is_empty() {
                use rand_core::{OsRng, RngCore};
                let mut key = [0u8; 32];
                OsRng.fill_bytes(&mut key);
                let generated_secret: String = key.iter().map(|b| format!("{:02x}", b)).collect();

                // Persist to storage volume so it survives container restarts
                if let Err(e) = std::fs::write(&secret_file, &generated_secret) {
                    tracing::error!(
                        "Failed to persist JWT secret to {}: {}. \
                         Tokens will be invalidated on restart!",
                        secret_file.display(),
                        e
                    );
                } else {
                    // Restrict file permissions (owner-only read/write)
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        let _ = std::fs::set_permissions(
                            &secret_file,
                            std::fs::Permissions::from_mode(0o600),
                        );
                    }
                    tracing::info!(
                        "JWT secret auto-generated and persisted to {}",
                        secret_file.display()
                    );
                }

                config.auth.jwt_secret = generated_secret;
            }
        }

        if let Ok(access_token_expiry) =
            env::var("OXICLOUD_ACCESS_TOKEN_EXPIRY_SECS").map(|v| v.parse::<i64>())
            && let Ok(val) = access_token_expiry
        {
            config.auth.access_token_expiry_secs = val;
        }

        if let Ok(refresh_token_expiry) =
            env::var("OXICLOUD_REFRESH_TOKEN_EXPIRY_SECS").map(|v| v.parse::<i64>())
            && let Ok(val) = refresh_token_expiry
        {
            config.auth.refresh_token_expiry_secs = val;
        }

        // Argon2 hashing parameters
        if let Ok(v) = env::var("OXICLOUD_HASH_MEMORY_COST").map(|v| v.parse::<u32>())
            && let Ok(val) = v
        {
            config.auth.hash_memory_cost = val;
        }
        if let Ok(v) = env::var("OXICLOUD_HASH_TIME_COST").map(|v| v.parse::<u32>())
            && let Ok(val) = v
        {
            config.auth.hash_time_cost = val;
        }
        if let Ok(v) = env::var("OXICLOUD_HASH_PARALLELISM").map(|v| v.parse::<u32>())
            && let Ok(val) = v
        {
            config.auth.hash_parallelism = val;
        }

        // Rate limiting / account lockout
        if let Ok(v) = env::var("OXICLOUD_RATE_LIMIT_LOGIN_MAX").map(|v| v.parse::<u32>())
            && let Ok(val) = v
        {
            config.auth.rate_limit.login_max_requests = val;
        }
        if let Ok(v) = env::var("OXICLOUD_RATE_LIMIT_LOGIN_WINDOW_SECS").map(|v| v.parse::<u64>())
            && let Ok(val) = v
        {
            config.auth.rate_limit.login_window_secs = val;
        }
        if let Ok(v) = env::var("OXICLOUD_RATE_LIMIT_REGISTER_MAX").map(|v| v.parse::<u32>())
            && let Ok(val) = v
        {
            config.auth.rate_limit.register_max_requests = val;
        }
        if let Ok(v) =
            env::var("OXICLOUD_RATE_LIMIT_REGISTER_WINDOW_SECS").map(|v| v.parse::<u64>())
            && let Ok(val) = v
        {
            config.auth.rate_limit.register_window_secs = val;
        }
        if let Ok(v) = env::var("OXICLOUD_RATE_LIMIT_REFRESH_MAX").map(|v| v.parse::<u32>())
            && let Ok(val) = v
        {
            config.auth.rate_limit.refresh_max_requests = val;
        }
        if let Ok(v) = env::var("OXICLOUD_RATE_LIMIT_REFRESH_WINDOW_SECS").map(|v| v.parse::<u64>())
            && let Ok(val) = v
        {
            config.auth.rate_limit.refresh_window_secs = val;
        }
        if let Ok(v) = env::var("OXICLOUD_LOCKOUT_MAX_FAILURES").map(|v| v.parse::<u32>())
            && let Ok(val) = v
        {
            config.auth.rate_limit.lockout_max_failures = val;
        }
        if let Ok(v) = env::var("OXICLOUD_LOCKOUT_DURATION_SECS").map(|v| v.parse::<u64>())
            && let Ok(val) = v
        {
            config.auth.rate_limit.lockout_duration_secs = val;
        }

        // Registration email-domain allowlist. Distinct from
        // `OXICLOUD_EXTERNAL_EMAIL_DOMAINS` (which gates who can be
        // INVITED via grants + magic link) — this one gates who can
        // SELF-register via `POST /api/auth/register`. Empty = no
        // restriction. Same parse shape as the external-domains list:
        // comma-separated, lowercased, trimmed, empties dropped.
        if let Ok(v) = env::var("OXICLOUD_REGISTRATION_ALLOWED_EMAIL_DOMAINS") {
            config.auth.registration_allowed_email_domains = v
                .split(',')
                .map(|d| d.trim().to_ascii_lowercase())
                .filter(|d| !d.is_empty())
                .collect();
        }

        // Self-service auth-method allowlist. Empty (unset) = both methods
        // allowed. Unknown tokens are logged-and-skipped; a completely
        // unparseable value falls back to the default rather than locking
        // the operator out. If the resulting list is empty (e.g. the
        // operator wrote `OXICLOUD_AUTH_METHODS=nope`), we restore the
        // default — a zero-method allowlist would refuse every login.
        if let Ok(v) = env::var("OXICLOUD_AUTH_METHODS") {
            let methods: Vec<AuthMethod> = v
                .split(',')
                .filter_map(|s| {
                    let parsed = AuthMethod::parse(s);
                    if parsed.is_none() && !s.trim().is_empty() {
                        eprintln!(
                            "⚠️  OXICLOUD_AUTH_METHODS: ignoring unknown token '{}' \
                             (expected: password, magic_link)",
                            s.trim()
                        );
                    }
                    parsed
                })
                .collect();
            if methods.is_empty() {
                eprintln!(
                    "⚠️  OXICLOUD_AUTH_METHODS parsed to an empty allowlist; \
                     falling back to default (password, magic_link)"
                );
            } else {
                config.auth.allowed_auth_methods = methods;
            }
        }

        // Legacy alias: OXICLOUD_OIDC_DISABLE_PASSWORD_LOGIN=true still
        // removes Password from the allowlist. Its main handling in the
        // OIDC config block below is preserved for the `login_options`
        // response; this line makes the effect apply uniformly through
        // `is_method_allowed(Password)` so services don't need to check
        // both flags.
        if let Ok(v) = env::var("OXICLOUD_OIDC_DISABLE_PASSWORD_LOGIN")
            && v.parse::<bool>().unwrap_or(false)
        {
            config
                .auth
                .allowed_auth_methods
                .retain(|m| *m != AuthMethod::Password);
        }

        if let Ok(v) = env::var("OXICLOUD_REQUIRE_VERIFIED_EMAIL") {
            config.auth.require_verified_email = v.parse::<bool>().unwrap_or(false);
        }

        // Auth-policy vector. Additive — each recognised token adds a
        // variant; unknown tokens are logged-and-skipped so a typo
        // doesn't silently zero the whole vector (an operator wanting
        // "no policies" simply doesn't set the env var).
        //
        // The legacy alias
        // `OXICLOUD_MAGIC_LINK_OPEN_TO_PASSWORD_USERS=true` is applied
        // AFTER this block (see the MagicLinkConfig section below) so a
        // deployment setting BOTH env vars ends up with a single copy
        // of `PermitMagicLinkForPasswordUsers` regardless of order.
        if let Ok(v) = env::var("OXICLOUD_AUTH_POLICIES") {
            for token in v.split(',') {
                match AuthPolicy::parse(token) {
                    Some(policy) => {
                        if !config.auth.auth_policies.contains(&policy) {
                            config.auth.auth_policies.push(policy);
                        }
                    }
                    None if !token.trim().is_empty() => {
                        eprintln!(
                            "⚠️  OXICLOUD_AUTH_POLICIES: ignoring unknown token '{}' \
                             (known: permit_magic_link_for_password_users)",
                            token.trim()
                        );
                    }
                    None => {}
                }
            }
            // Reflect the vector into the legacy magic_link config field
            // so `magic_link_eligibility()` (the site that reads the
            // boolean today) doesn't need to know about the new form.
            if config
                .auth
                .auth_policies
                .contains(&AuthPolicy::PermitMagicLinkForPasswordUsers)
            {
                config.magic_link.open_to_password_users = true;
            }
        }

        // Feature flags
        if let Ok(enable_auth) = env::var("OXICLOUD_ENABLE_AUTH").map(|v| v.parse::<bool>())
            && let Ok(val) = enable_auth
        {
            config.features.enable_auth = val;
        }

        if let Ok(enable_user_storage_quotas) =
            env::var("OXICLOUD_ENABLE_USER_STORAGE_QUOTAS").map(|v| v.parse::<bool>())
            && let Ok(val) = enable_user_storage_quotas
        {
            config.features.enable_user_storage_quotas = val;
        }

        if let Ok(enable_file_sharing) =
            env::var("OXICLOUD_ENABLE_FILE_SHARING").map(|v| v.parse::<bool>())
            && let Ok(val) = enable_file_sharing
        {
            config.features.enable_file_sharing = val;
        }

        if let Ok(enable_trash) = env::var("OXICLOUD_ENABLE_TRASH").map(|v| v.parse::<bool>())
            && let Ok(val) = enable_trash
        {
            config.features.enable_trash = val;
        }

        if let Ok(enable_search) = env::var("OXICLOUD_ENABLE_SEARCH").map(|v| v.parse::<bool>())
            && let Ok(val) = enable_search
        {
            config.features.enable_search = val;
        }

        if let Ok(enable_music) = env::var("OXICLOUD_ENABLE_MUSIC").map(|v| v.parse::<bool>())
            && let Ok(val) = enable_music
        {
            config.features.enable_music = val;
        }

        if let Ok(enable_places) = env::var("OXICLOUD_ENABLE_PLACES").map(|v| v.parse::<bool>())
            && let Ok(val) = enable_places
        {
            config.features.enable_places = val;
        }

        if let Ok(enable_video_thumbnails) =
            env::var("OXICLOUD_ENABLE_VIDEO_THUMBNAILS").map(|v| v.parse::<bool>())
            && let Ok(val) = enable_video_thumbnails
        {
            config.features.enable_video_thumbnails = val;
        }

        // Grant-cleanup daemon. Purges rows from `storage.role_grants`
        // whose `expires_at` is more than `grace_days` in the past.
        // See `GrantCleanupConfig` for defaults + rationale.
        if let Ok(v) = env::var("OXICLOUD_GRANT_CLEANUP_ENABLED").map(|v| v.parse::<bool>())
            && let Ok(val) = v
        {
            config.features.grant_cleanup.enabled = val;
        }
        if let Ok(v) = env::var("OXICLOUD_GRANT_CLEANUP_GRACE_DAYS").map(|v| v.parse::<u32>())
            && let Ok(val) = v
        {
            config.features.grant_cleanup.grace_days = val;
        }
        if let Ok(v) = env::var("OXICLOUD_GRANT_CLEANUP_INTERVAL_HOURS").map(|v| v.parse::<u64>())
            && let Ok(val) = v
        {
            config.features.grant_cleanup.interval_hours = val.max(1);
        }

        // Native WebDAV drive-picker path segment. Sanitised by
        // stripping leading/trailing slashes so operators can pass
        // `/drives/` or `drives` interchangeably; empty string means
        // "no default-drive shortcut, `/webdav/` IS the drive listing".
        // See `FeaturesConfig::webdav_drive_listing_prefix`.
        if let Ok(raw) = env::var("OXICLOUD_WEBDAV_DRIVE_LISTING_PREFIX") {
            config.features.webdav_drive_listing_prefix = raw.trim_matches('/').to_string();
        }

        if let Ok(enable_faces) = env::var("OXICLOUD_ENABLE_FACES").map(|v| v.parse::<bool>())
            && let Ok(val) = enable_faces
        {
            config.features.enable_faces = val;
        }

        if let Ok(enable_external_mounts) =
            env::var("OXICLOUD_ENABLE_EXTERNAL_MOUNTS").map(|v| v.parse::<bool>())
            && let Ok(val) = enable_external_mounts
        {
            config.features.enable_external_mounts = val;
        }

        // Faces (People) ONNX runtime + models — operator-provided at runtime.
        if let Ok(v) = env::var("OXICLOUD_FACES_ORT_DYLIB").or_else(|_| env::var("ORT_DYLIB_PATH"))
            && !v.is_empty()
        {
            config.faces.ort_dylib = Some(PathBuf::from(v));
        }
        if let Ok(v) = env::var("OXICLOUD_FACES_DETECTOR_MODEL")
            && !v.is_empty()
        {
            config.faces.detector_model = Some(PathBuf::from(v));
        }
        if let Ok(v) = env::var("OXICLOUD_FACES_EMBEDDER_MODEL")
            && !v.is_empty()
        {
            config.faces.embedder_model = Some(PathBuf::from(v));
        }
        if let Ok(v) = env::var("OXICLOUD_FACES_DET_SIZE").map(|v| v.parse::<u32>())
            && let Ok(val) = v
        {
            config.faces.det_size = val;
        }
        if let Ok(v) = env::var("OXICLOUD_FACES_DET_THRESHOLD").map(|v| v.parse::<f32>())
            && let Ok(val) = v
        {
            config.faces.det_threshold = val;
        }
        if let Ok(v) = env::var("OXICLOUD_FACES_NMS_THRESHOLD").map(|v| v.parse::<f32>())
            && let Ok(val) = v
        {
            config.faces.nms_threshold = val;
        }
        if let Ok(v) = env::var("OXICLOUD_FACES_INTRA_THREADS").map(|v| v.parse::<usize>())
            && let Ok(val) = v
        {
            config.faces.intra_threads = val;
        }

        // Content search (embedded Tantivy index)
        if let Ok(v) = env::var("OXICLOUD_ENABLE_CONTENT_SEARCH").map(|v| v.parse::<bool>())
            && let Ok(val) = v
        {
            config.content_search.enabled = val;
        }
        if let Ok(dir) = env::var("OXICLOUD_CONTENT_INDEX_DIR")
            && !dir.trim().is_empty()
        {
            config.content_search.index_dir = Some(PathBuf::from(dir.trim()));
        }
        if let Ok(v) = env::var("OXICLOUD_CONTENT_INDEX_FLUSH_MS").map(|v| v.parse::<u64>())
            && let Ok(val) = v
        {
            config.content_search.flush_interval_ms = val;
        }
        if let Ok(v) = env::var("OXICLOUD_CONTENT_INDEX_MAX_FILE_BYTES").map(|v| v.parse::<u64>())
            && let Ok(val) = v
        {
            config.content_search.max_extract_file_bytes = val;
        }
        if let Ok(v) = env::var("OXICLOUD_CONTENT_INDEX_MAX_TEXT_BYTES").map(|v| v.parse::<usize>())
            && let Ok(val) = v
        {
            config.content_search.max_text_bytes = val;
        }

        // Search-results cache (byte-bounded)
        if let Ok(v) = env::var("OXICLOUD_SEARCH_CACHE_MAX_BYTES").map(|v| v.parse::<u64>())
            && let Ok(val) = v
        {
            config.search_cache.max_bytes = val;
        }

        // WASM plugin runtime
        if let Ok(v) = env::var("OXICLOUD_ENABLE_PLUGINS").map(|v| v.parse::<bool>())
            && let Ok(val) = v
        {
            config.plugins.enabled = val;
        }
        if let Ok(dir) = env::var("OXICLOUD_PLUGINS_DIR")
            && !dir.trim().is_empty()
        {
            config.plugins.plugins_dir = Some(PathBuf::from(dir.trim()));
        }
        if let Ok(v) = env::var("OXICLOUD_PLUGIN_TIMEOUT_MS").map(|v| v.parse::<u64>())
            && let Ok(val) = v
        {
            config.plugins.invocation_timeout_ms = val;
        }
        if let Ok(v) = env::var("OXICLOUD_PLUGIN_MAX_MEMORY_PAGES").map(|v| v.parse::<u32>())
            && let Ok(val) = v
        {
            config.plugins.max_memory_pages = val;
        }
        if let Ok(v) = env::var("OXICLOUD_PLUGIN_MAX_INPUT_BYTES").map(|v| v.parse::<usize>())
            && let Ok(val) = v
        {
            config.plugins.max_input_bytes = val;
        }
        if let Ok(dir) = env::var("OXICLOUD_PLUGIN_LOG_DIR")
            && !dir.trim().is_empty()
        {
            config.plugins.log_dir = Some(PathBuf::from(dir.trim()));
        }
        if let Ok(v) = env::var("OXICLOUD_PLUGIN_LOG_MAX_FILE_BYTES").map(|v| v.parse::<u64>())
            && let Ok(val) = v
        {
            config.plugins.log_max_file_bytes = val;
        }
        if let Ok(v) = env::var("OXICLOUD_PLUGIN_LOG_MAX_SEGMENTS").map(|v| v.parse::<u32>())
            && let Ok(val) = v
        {
            config.plugins.log_max_segments = val;
        }
        if let Ok(v) = env::var("OXICLOUD_PLUGIN_LOG_RETENTION_DAYS").map(|v| v.parse::<u32>())
            && let Ok(val) = v
        {
            config.plugins.log_retention_days = val;
        }
        if let Ok(v) = env::var("OXICLOUD_PLUGIN_LOG_TOTAL_MAX_BYTES").map(|v| v.parse::<u64>())
            && let Ok(val) = v
        {
            config.plugins.log_total_max_bytes = val;
        }
        if let Ok(v) =
            env::var("OXICLOUD_PLUGIN_MAX_CONCURRENT_INVOCATIONS").map(|v| v.parse::<usize>())
            && let Ok(val) = v
        {
            config.plugins.max_concurrent_invocations = val;
        }
        if let Ok(v) = env::var("OXICLOUD_PLUGIN_LOG_QUEUE_CAPACITY").map(|v| v.parse::<usize>())
            && let Ok(val) = v
        {
            config.plugins.log_queue_capacity = val;
        }
        if let Ok(v) = env::var("OXICLOUD_PLUGIN_CACHE_IDLE_TTL_SECS").map(|v| v.parse::<u64>())
            && let Ok(val) = v
        {
            config.plugins.cache_idle_ttl_secs = val;
        }
        if let Ok(v) =
            env::var("OXICLOUD_PLUGIN_MAX_BUNDLE_DECOMPRESSED_BYTES").map(|v| v.parse::<u64>())
            && let Ok(val) = v
        {
            config.plugins.max_bundle_decompressed_bytes = val;
        }

        if let Ok(v) = env::var("OXICLOUD_EXPOSE_SYSTEM_USERS").map(|v| v.parse::<bool>())
            && let Ok(val) = v
        {
            config.features.expose_system_users = val;
        }

        // Storage limits
        if let Ok(max_upload) = env::var("OXICLOUD_MAX_UPLOAD_SIZE").map(|v| v.parse::<usize>())
            && let Ok(val) = max_upload
        {
            config.storage.max_upload_size = val;
        }
        if let Ok(chunk_max) = env::var("OXICLOUD_CHUNK_MAX_BYTES").map(|v| v.parse::<usize>())
            && let Ok(val) = chunk_max
        {
            config.storage.chunk_max_bytes = val;
        }
        if let Ok(direct_max) =
            env::var("OXICLOUD_DIRECT_PUT_MAX_BYTES").map(|v| v.parse::<usize>())
            && let Ok(val) = direct_max
        {
            config.storage.direct_put_max_bytes = val;
        }

        // Chunked-upload session root — chunked sessions accumulate disk on
        // long uploads (multi-chunk resumable transfers); sysadmins commonly
        // want them on fast/local storage (NVMe). This knob lets that be
        // expressed.
        if let Ok(dir) = env::var("OXICLOUD_CHUNK_DIR")
            && !dir.trim().is_empty()
        {
            config.storage.chunk_dir = Some(PathBuf::from(dir.trim()));
        }

        // Background storage-usage reconciliation interval
        if let Ok(secs) =
            env::var("OXICLOUD_STORAGE_USAGE_RECONCILE_SECS").map(|v| v.parse::<u64>())
            && let Ok(val) = secs
        {
            config.storage.usage_reconcile_secs = val;
        }

        // Tree-ETag dirty-queue flush cadence
        if let Ok(ms) = env::var("OXICLOUD_TREE_ETAG_FLUSH_MS").map(|v| v.parse::<u64>())
            && let Ok(val) = ms
        {
            config.storage.tree_etag_flush_ms = val;
        }

        // Legacy whole-file blob re-chunk migration (startup background task)
        if let Ok(enabled) = env::var("OXICLOUD_LEGACY_RECHUNK") {
            config.storage.legacy_rechunk_enabled =
                enabled.eq_ignore_ascii_case("true") || enabled == "1";
        }

        // Storage backend selection
        if let Ok(backend) = env::var("OXICLOUD_STORAGE_BACKEND") {
            match backend.to_lowercase().as_str() {
                "s3" => config.storage.backend = StorageBackendType::S3,
                "azure" => config.storage.backend = StorageBackendType::Azure,
                _ => config.storage.backend = StorageBackendType::Local,
            }
        }

        // S3-compatible storage configuration
        if config.storage.backend == StorageBackendType::S3 {
            let bucket = env::var("OXICLOUD_S3_BUCKET").unwrap_or_default();
            if bucket.is_empty() {
                tracing::warn!("OXICLOUD_STORAGE_BACKEND=s3 but OXICLOUD_S3_BUCKET is not set");
            }
            config.storage.s3 = Some(S3StorageConfig {
                endpoint_url: env::var("OXICLOUD_S3_ENDPOINT_URL").ok(),
                bucket,
                region: env::var("OXICLOUD_S3_REGION").unwrap_or_else(|_| "us-east-1".to_string()),
                access_key: env::var("OXICLOUD_S3_ACCESS_KEY").unwrap_or_default(),
                secret_key: env::var("OXICLOUD_S3_SECRET_KEY").unwrap_or_default(),
                force_path_style: env::var("OXICLOUD_S3_FORCE_PATH_STYLE")
                    .map(|v| v.parse::<bool>().unwrap_or(false))
                    .unwrap_or(false),
            });
        }

        // Azure Blob Storage configuration
        if config.storage.backend == StorageBackendType::Azure {
            let container = env::var("OXICLOUD_AZURE_CONTAINER").unwrap_or_default();
            if container.is_empty() {
                tracing::warn!(
                    "OXICLOUD_STORAGE_BACKEND=azure but OXICLOUD_AZURE_CONTAINER is not set"
                );
            }
            config.storage.azure = Some(AzureStorageConfig {
                account_name: env::var("OXICLOUD_AZURE_ACCOUNT_NAME").unwrap_or_default(),
                account_key: env::var("OXICLOUD_AZURE_ACCOUNT_KEY").unwrap_or_default(),
                container,
                sas_token: env::var("OXICLOUD_AZURE_SAS_TOKEN").ok(),
                endpoint_url: env::var("OXICLOUD_AZURE_ENDPOINT_URL").ok(),
            });
        }

        // Blob cache configuration
        if let Ok(v) = env::var("OXICLOUD_STORAGE_CACHE_ENABLED") {
            config.storage.cache.enabled = v.parse::<bool>().unwrap_or(false);
        }
        if let Ok(v) = env::var("OXICLOUD_STORAGE_CACHE_MAX_SIZE")
            && let Ok(bytes) = v.parse::<u64>()
        {
            config.storage.cache.max_size_bytes = bytes;
        }
        if let Ok(v) = env::var("OXICLOUD_STORAGE_CACHE_PATH") {
            config.storage.cache.cache_path = Some(v);
        }

        // Encryption configuration
        if let Ok(v) = env::var("OXICLOUD_STORAGE_ENCRYPTION_ENABLED") {
            config.storage.encryption.enabled = v.parse::<bool>().unwrap_or(false);
        }
        if let Ok(v) = env::var("OXICLOUD_STORAGE_ENCRYPTION_KEY") {
            config.storage.encryption.key_base64 = Some(v);
        }

        // Retry configuration
        if let Ok(v) = env::var("OXICLOUD_STORAGE_RETRY_ENABLED") {
            config.storage.retry.enabled = v.parse::<bool>().unwrap_or(true);
        }
        if let Ok(v) = env::var("OXICLOUD_STORAGE_RETRY_MAX_RETRIES")
            && let Ok(n) = v.parse::<u32>()
        {
            config.storage.retry.max_retries = n;
        }
        if let Ok(v) = env::var("OXICLOUD_STORAGE_RETRY_INITIAL_BACKOFF_MS")
            && let Ok(n) = v.parse::<u64>()
        {
            config.storage.retry.initial_backoff_ms = n;
        }
        if let Ok(v) = env::var("OXICLOUD_STORAGE_RETRY_MAX_BACKOFF_MS")
            && let Ok(n) = v.parse::<u64>()
        {
            config.storage.retry.max_backoff_ms = n;
        }
        if let Ok(v) = env::var("OXICLOUD_STORAGE_RETRY_BACKOFF_MULTIPLIER")
            && let Ok(n) = v.parse::<f64>()
        {
            config.storage.retry.backoff_multiplier = n;
        }

        // OIDC configuration
        if let Ok(v) = env::var("OXICLOUD_OIDC_ENABLED") {
            config.oidc.enabled = v.parse::<bool>().unwrap_or(false);
        }
        if let Ok(v) = env::var("OXICLOUD_OIDC_ISSUER_URL") {
            config.oidc.issuer_url = v;
        }
        if let Ok(v) = env::var("OXICLOUD_OIDC_CLIENT_ID") {
            config.oidc.client_id = v;
        }
        if let Ok(v) = env::var("OXICLOUD_OIDC_CLIENT_SECRET") {
            config.oidc.client_secret = v;
        }
        if let Ok(v) = env::var("OXICLOUD_OIDC_REDIRECT_URI") {
            config.oidc.redirect_uri = v;
        }
        if let Ok(v) = env::var("OXICLOUD_OIDC_SCOPES") {
            config.oidc.scopes = v;
        }
        if let Ok(v) = env::var("OXICLOUD_OIDC_FRONTEND_URL") {
            config.oidc.frontend_url = v;
        }
        if let Ok(v) = env::var("OXICLOUD_OIDC_AUTO_PROVISION") {
            config.oidc.auto_provision = v.parse::<bool>().unwrap_or(true);
        }
        if let Ok(v) = env::var("OXICLOUD_OIDC_ADMIN_GROUPS") {
            config.oidc.admin_groups = v;
        }
        if let Ok(v) = env::var("OXICLOUD_OIDC_DISABLE_PASSWORD_LOGIN") {
            config.oidc.disable_password_login = v.parse::<bool>().unwrap_or(false);
        }
        if let Ok(v) = env::var("OXICLOUD_OIDC_PROVIDER_NAME") {
            config.oidc.provider_name = v;
        }

        // Validate OIDC config when enabled
        if config.oidc.enabled
            && (config.oidc.issuer_url.is_empty()
                || config.oidc.client_id.is_empty()
                || config.oidc.client_secret.is_empty())
        {
            tracing::error!(
                "OIDC is enabled but OXICLOUD_OIDC_ISSUER_URL, OXICLOUD_OIDC_CLIENT_ID, or OXICLOUD_OIDC_CLIENT_SECRET are not set"
            );
            config.oidc.enabled = false;
        }

        // WOPI configuration
        if let Ok(v) = env::var("OXICLOUD_WOPI_ENABLED") {
            config.wopi.enabled = v.parse::<bool>().unwrap_or(false);
        }
        if let Ok(v) = env::var("OXICLOUD_WOPI_DISCOVERY_URL") {
            config.wopi.discovery_url = v;
        }
        if let Ok(v) = env::var("OXICLOUD_WOPI_SECRET") {
            config.wopi.secret = v;
        }
        if let Ok(v) = env::var("OXICLOUD_WOPI_TOKEN_TTL_SECS")
            && let Ok(val) = v.parse::<i64>()
        {
            config.wopi.token_ttl_secs = val;
        }
        if let Ok(v) = env::var("OXICLOUD_WOPI_LOCK_TTL_SECS")
            && let Ok(val) = v.parse::<u64>()
        {
            config.wopi.lock_ttl_secs = val;
        }

        // WOPI secret fallback: use JWT secret if WOPI secret not set
        if config.wopi.enabled && config.wopi.secret.is_empty() {
            config.wopi.secret = config.auth.jwt_secret.clone();
            tracing::info!("WOPI secret not set, falling back to JWT secret");
        }

        // Nextcloud compatibility configuration
        if let Ok(v) = env::var("OXICLOUD_NEXTCLOUD_ENABLED") {
            config.nextcloud.enabled = v.parse::<bool>().unwrap_or(false);
        }
        if let Ok(v) = env::var("OXICLOUD_NEXTCLOUD_INSTANCE_ID") {
            let trimmed = v.trim();
            if !trimmed.is_empty() {
                config.nextcloud.instance_id = trimmed.to_string();
            }
        }
        if let Ok(v) = env::var("OXICLOUD_NEXTCLOUD_VERSION") {
            // Expected format: "28.0.4"
            let parts: Vec<&str> = v.trim().splitn(3, '.').collect();
            if parts.len() == 3
                && let (Ok(maj), Ok(min), Ok(pat)) = (
                    parts[0].parse::<u32>(),
                    parts[1].parse::<u32>(),
                    parts[2].parse::<u32>(),
                )
            {
                config.nextcloud.emulated_version = (maj, min, pat);
            }
        }

        // SMTP configuration. `HOST` empty = feature disabled — every
        // endpoint that needs email returns 503 in that state.
        if let Ok(v) = env::var("OXICLOUD_SMTP_HOST") {
            config.smtp.host = v.trim().to_string();
        }
        if let Ok(v) = env::var("OXICLOUD_SMTP_PORT")
            && let Ok(p) = v.parse::<u16>()
        {
            config.smtp.port = p;
        }
        if let Ok(v) = env::var("OXICLOUD_SMTP_USER") {
            config.smtp.user = v;
        }
        if let Ok(v) = env::var("OXICLOUD_SMTP_PASS") {
            config.smtp.pass = v;
        }
        if let Ok(v) = env::var("OXICLOUD_SMTP_FROM") {
            config.smtp.from = v;
        }
        if let Ok(v) = env::var("OXICLOUD_SMTP_TLS")
            && let Some(mode) = SmtpTlsMode::parse(&v)
        {
            config.smtp.tls = mode;
        }

        if config.smtp.is_enabled() && config.smtp.tls == SmtpTlsMode::None {
            tracing::warn!(
                "OXICLOUD_SMTP_TLS=none — outbound mail will travel in plaintext. \
                 Use 'starttls' or 'tls' for production deployments."
            );
        }

        // Magic-link configuration
        // Legacy `OXICLOUD_MAGIC_LINK_TTL_HOURS` is preserved as a
        // deprecated alias for `OXICLOUD_MAGIC_LINK_INVITE_TTL_HOURS`.
        // Existing deployments keep working with their old env var;
        // the new explicit var wins if both are set.
        if let Ok(v) = env::var("OXICLOUD_MAGIC_LINK_TTL_HOURS")
            && let Ok(h) = v.parse::<u64>()
            && h > 0
        {
            tracing::warn!(
                "OXICLOUD_MAGIC_LINK_TTL_HOURS is deprecated — \
                 use OXICLOUD_MAGIC_LINK_INVITE_TTL_HOURS (invitations) \
                 and OXICLOUD_MAGIC_LINK_LOGIN_TTL_MINUTES (login-via-email)."
            );
            config.magic_link.invite_ttl_hours = h;
        }
        if let Ok(v) = env::var("OXICLOUD_MAGIC_LINK_INVITE_TTL_HOURS")
            && let Ok(h) = v.parse::<u64>()
            && h > 0
        {
            config.magic_link.invite_ttl_hours = h;
        }
        if let Ok(v) = env::var("OXICLOUD_MAGIC_LINK_LOGIN_TTL_MINUTES")
            && let Ok(m) = v.parse::<u64>()
            && m > 0
        {
            config.magic_link.login_ttl_minutes = m;
        }
        if let Ok(v) = env::var("OXICLOUD_ALLOW_EXTERNAL_USERS") {
            config.magic_link.allow_external_users = v.parse::<bool>().unwrap_or(true);
        }
        if let Ok(v) = env::var("OXICLOUD_EXTERNAL_EMAIL_DOMAINS") {
            config.magic_link.allowed_email_domains = v
                .split(',')
                .map(|d| d.trim().to_ascii_lowercase())
                .filter(|d| !d.is_empty())
                .collect();
        }
        if let Ok(v) = env::var("OXICLOUD_MAGIC_LINK_INVITE_PER_CALLER_PER_HOUR")
            && let Ok(n) = v.parse::<u32>()
            && n > 0
        {
            config.magic_link.invite_per_caller_per_hour = n;
        }
        if let Ok(v) = env::var("OXICLOUD_MAGIC_LINK_SEND_PER_EMAIL_PER_HOUR")
            && let Ok(n) = v.parse::<u32>()
            && n > 0
        {
            config.magic_link.send_per_email_per_hour = n;
        }
        if let Ok(v) = env::var("OXICLOUD_MAGIC_LINK_SEND_PER_IP_PER_HOUR")
            && let Ok(n) = v.parse::<u32>()
            && n > 0
        {
            config.magic_link.send_per_ip_per_hour = n;
        }
        // Legacy alias — writes the same effect as
        // `OXICLOUD_AUTH_POLICIES=permit_magic_link_for_password_users`.
        // Warn once at boot so operators know to migrate before we drop
        // the old var. Kept indefinitely for compat, but the encouraged
        // form is the vector.
        if let Ok(v) = env::var("OXICLOUD_MAGIC_LINK_OPEN_TO_PASSWORD_USERS") {
            let enabled = v == "true" || v == "1";
            config.magic_link.open_to_password_users = enabled;
            if enabled
                && !config
                    .auth
                    .auth_policies
                    .contains(&AuthPolicy::PermitMagicLinkForPasswordUsers)
            {
                config
                    .auth
                    .auth_policies
                    .push(AuthPolicy::PermitMagicLinkForPasswordUsers);
            }
            eprintln!(
                "⚠️  OXICLOUD_MAGIC_LINK_OPEN_TO_PASSWORD_USERS is deprecated. \
                 Use `OXICLOUD_AUTH_POLICIES=permit_magic_link_for_password_users` instead."
            );
        }
        if let Ok(v) = env::var("OXICLOUD_NOTIFY_INTERNAL_USERS_ON_SHARE") {
            config.magic_link.notify_internal_users_on_share = v == "true" || v == "1";
        }

        if let Ok(v) = env::var("OXICLOUD_DEFAULT_LOCALE") {
            let trimmed = v.trim();
            if !trimmed.is_empty() {
                config.i18n.default_locale = trimmed.to_string();
            }
        }

        config
    }

    pub fn with_features(mut self, features: FeaturesConfig) -> Self {
        self.features = features;
        self
    }

    pub fn db_enabled(&self) -> bool {
        self.features.enable_auth
    }

    pub fn auth_enabled(&self) -> bool {
        self.features.enable_auth
    }

    /// Build the public base URL for generating share links and other external URLs.
    ///
    /// Priority:
    /// 1. `OXICLOUD_BASE_URL` env var (used as-is)
    /// 2. If `server_host` already contains a scheme (`http://` or `https://`),
    ///    treat it as a full origin and do **not** prepend a scheme or append a port.
    /// 3. Otherwise, fall back to `http://{server_host}:{server_port}`.
    pub fn base_url(&self) -> String {
        if let Ok(explicit) = std::env::var("OXICLOUD_BASE_URL") {
            return explicit.trim_end_matches('/').to_string();
        }

        let host = self.server_host.trim_end_matches('/');

        if host.starts_with("http://") || host.starts_with("https://") {
            // The user already provided a full origin — use it directly.
            host.to_string()
        } else {
            format!("http://{}:{}", host, self.server_port)
        }
    }
}

/// Gets a default global configuration
pub fn default_config() -> AppConfig {
    AppConfig::default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_allowlist_accepts_any_email() {
        let cfg = MagicLinkConfig::default();
        assert!(cfg.allowed_email_domains.is_empty());
        assert!(cfg.is_email_allowed("alice@example.com"));
        assert!(cfg.is_email_allowed("bob@whatever.io"));
    }

    #[test]
    fn allowlist_matches_case_insensitively() {
        let cfg = MagicLinkConfig {
            allowed_email_domains: vec!["partner-a.com".to_string(), "partner-b.io".to_string()],
            ..MagicLinkConfig::default()
        };
        assert!(cfg.is_email_allowed("alice@partner-a.com"));
        // Uppercase domain in the email — must still match.
        assert!(cfg.is_email_allowed("alice@PARTNER-A.COM"));
        assert!(cfg.is_email_allowed("eve@partner-b.io"));
        // Unlisted domain — rejected.
        assert!(!cfg.is_email_allowed("mallory@other.com"));
    }

    #[test]
    fn allowlist_does_not_match_subdomains_implicitly() {
        let cfg = MagicLinkConfig {
            allowed_email_domains: vec!["partner.com".to_string()],
            ..MagicLinkConfig::default()
        };
        assert!(cfg.is_email_allowed("alice@partner.com"));
        // Subdomain must be listed explicitly — exact match only.
        assert!(!cfg.is_email_allowed("alice@eng.partner.com"));
        // Suffix match is not enough — different domain.
        assert!(!cfg.is_email_allowed("alice@evilpartner.com"));
    }

    #[test]
    fn malformed_email_fails_closed() {
        let cfg = MagicLinkConfig {
            allowed_email_domains: vec!["partner.com".to_string()],
            ..MagicLinkConfig::default()
        };
        // No `@` — rejected even though allowlist is set.
        assert!(!cfg.is_email_allowed("not-an-email"));
        assert!(!cfg.is_email_allowed(""));
    }
}
