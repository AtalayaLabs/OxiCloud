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

// ─────────────────────────────────────────────────────────────────────
// K1: pair-list encryption config (post-storage-multi-entry,
// pre-v1-header). See `docs/plan/storage-key-rotation.md`.
//
// [`CipherKind`], [`KeyPair`] and [`parse_encryption_pair_list`]
// replace the singular pre-K1 `EncryptionCipher` + `encryption_key_base64`
// + `encryption_cipher` model with a list-of-pairs model where the
// last pair wins on writes and every pair is a candidate for reads.
// The `none` cipher is a first-class citizen so plaintext ↔ encrypted
// transitions can be expressed as a single pair-list evolution.
//
// K1.2 wires the pair-list into `NamedStorageEntry`; K2 replaces the
// on-disk format (`.blob` → v1 header at same suffix) and the read
// path (magic-byte dispatch + `<key_fp>` lookup). See the plan.
// ─────────────────────────────────────────────────────────────────────

/// The AEAD (or absence of one) used by a single [`KeyPair`].
///
/// * `AesGcm256` — the only real AEAD OxiCloud ships today. On the
///   wire (v1 header): `[12-byte nonce] [ciphertext] [16-byte tag]`.
/// * `None` — no cipher. A pair with `CipherKind::None` says
///   "writes routed to this pair produce raw plaintext". This is
///   what makes the encrypt-a-plaintext-deployment and
///   decrypt-an-encrypted-deployment recipes expressible without a
///   second storage entry — see `docs/plan/storage-key-rotation.md`
///   §"Encrypting a previously-plaintext deployment" and the
///   symmetric decrypt recipe.
///
/// The type is carried by the v1 on-disk header's cipher field
/// (indirectly via `<version>` in v1, but explicitly if a future
/// v2 adds a cipher byte to the header — the enum grows without
/// touching call sites).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CipherKind {
    /// AES-256 in Galois/Counter Mode. 96-bit nonce + 128-bit tag.
    /// The only real AEAD OxiCloud ships today. On the wire (v1
    /// header): `[12-byte nonce] [ciphertext] [16-byte tag]`.
    AesGcm256,
    /// No cipher. Writes produce raw plaintext; reads return raw
    /// bytes. Used as the head pair for a plaintext-target rotation
    /// (`aes:K,none:`) or as a non-head legacy pair while an
    /// encrypt-in-place rotation is still upgrading old plaintext
    /// blobs (`none:,aes:K`). At most one `none` pair may appear in
    /// a list (parser-enforced).
    None,
}

impl CipherKind {
    /// Parse an env-var token: `"aes-256-gcm"` (with the `aes256gcm`
    /// alias kept for continuity with the pre-K1 spelling) or
    /// `"none"`. Case-insensitive.
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.to_ascii_lowercase().as_str() {
            "aes-256-gcm" | "aes256gcm" => Some(CipherKind::AesGcm256),
            "none" => Some(CipherKind::None),
            _ => None,
        }
    }

    /// Stable env-var-friendly name — the exact string the parser
    /// accepts back and the string admin surfaces render.
    pub fn as_str(self) -> &'static str {
        match self {
            CipherKind::AesGcm256 => "aes-256-gcm",
            CipherKind::None => "none",
        }
    }

    /// `true` iff this cipher carries key material. `false` for
    /// `CipherKind::None`. Used by the parser to enforce
    /// "no key after `none:`" and by K2's write path to skip the
    /// AEAD call.
    pub fn needs_key(self) -> bool {
        matches!(self, CipherKind::AesGcm256)
    }
}

/// One `<cipher>:<key>` pair from an `_ENCRYPTION_KEY` list.
///
/// List order carries semantics — the LAST pair is the write pair
/// (see `docs/plan/storage-key-rotation.md` §"The pair-list config").
/// `key_material` is `Some(bytes)` when `cipher.needs_key()`, else
/// `None`. Base64 is decoded once at parse time; downstream code
/// takes the raw bytes directly (no re-decoding on every read).
///
/// Deliberately not `Copy` — a 32-byte key isn't cheap enough to
/// silently `Copy` and cloning it in tests is a good deterrent
/// against accidental leaks into logs.
#[derive(Debug, Clone)]
pub struct KeyPair {
    /// Which AEAD (or none) this pair writes with.
    pub cipher: CipherKind,
    /// Raw 32-byte AES-256 key. Always `Some` for real ciphers,
    /// always `None` for `CipherKind::None`. This invariant is
    /// enforced at parse time; downstream can `unwrap()` when
    /// `cipher.needs_key()` returns `true`.
    pub key_material: Option<[u8; 32]>,
}

impl KeyPair {
    /// Construct a real-cipher pair with pre-decoded key material.
    /// Convenience for tests + the pre-multi-entry legacy synthesis
    /// path where the base64-decoded key is already in hand. New
    /// production code paths get their pairs from
    /// [`parse_encryption_pair_list`] which produces the same
    /// shape.
    pub fn new_aes_gcm(key: [u8; 32]) -> Self {
        Self {
            cipher: CipherKind::AesGcm256,
            key_material: Some(key),
        }
    }

    /// Construct a `none:` sentinel pair. Used by `entry_backend.rs`
    /// under the always-wrap rule to synthesise a 1-pair list for
    /// entries with no `_ENCRYPTION_KEY` declared, so those writes
    /// still get v1 headers (plaintext-v1 flavor).
    pub fn new_none() -> Self {
        Self {
            cipher: CipherKind::None,
            key_material: None,
        }
    }

    /// SSH-style colon-hex fingerprint of the key material — 8 bytes
    /// of SHA-256 truncation rendered as `xx:yy:zz:...`. Same
    /// truncation as the v1 header's `<key_fp>` field and the
    /// `head_key_fp` reported by `backend_rotate` on completion, so
    /// operators can cross-reference the boot log against a rotate
    /// report or the CLI's `oxicloud --fingerprint <base64key>`
    /// output without any format conversion.
    ///
    /// Returns `None` for `CipherKind::None` (nothing to
    /// fingerprint) — callers render as `—` in that case.
    pub fn fingerprint_short(&self) -> Option<String> {
        use sha2::{Digest, Sha256};
        let mat = self.key_material.as_ref()?;
        let full = Sha256::digest(mat);
        Some(
            full[..8]
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<Vec<_>>()
                .join(":"),
        )
    }

    /// 8-byte SHA-256 truncation used as the v1 header's `<key_fp>`
    /// field on every encrypted blob. Read dispatch looks up the
    /// matching [`KeyPair`] in the pair list by this value in O(1),
    /// so a blob written under any pair in the list can be decrypted
    /// without falling through candidate keys.
    ///
    /// Semantic contract with the on-disk format:
    ///
    /// * `CipherKind::AesGcm256` pair → 8 bytes of `sha256(key)[..8]`.
    ///   Effectively unique per configured pair (2⁻⁶⁴ collision on
    ///   random keys — never in practice on the ~1-3 pairs a real
    ///   deployment has).
    /// * `CipherKind::None` pair → all-zero. This is what marks a
    ///   plaintext-v1 blob so the read path can dispatch "return
    ///   post-header raw bytes" without consulting a key. Real
    ///   ciphers cannot collide with all-zero: an
    ///   `sha256(key)[..8] == 0` real key is 2⁻⁶⁴ improbable AND
    ///   the parser wouldn't accept two pairs with the same fp
    ///   (uniqueness check on raw key material — same key = same
    ///   fp).
    ///
    /// Distinct truncation from [`Self::fingerprint_short`] on
    /// purpose: this is a raw byte string embedded in every
    /// encrypted blob (compactness matters), while the boot log
    /// wants a legible short-hex string.
    pub fn key_fp(&self) -> [u8; 8] {
        use sha2::{Digest, Sha256};
        let mut fp = [0u8; 8];
        if let Some(mat) = self.key_material.as_ref() {
            let full = Sha256::digest(mat);
            fp.copy_from_slice(&full[..8]);
        }
        fp
    }
}

/// Parse the `OXICLOUD_STORAGE_<name>_ENCRYPTION_KEY` env var value
/// into an ordered pair list.
///
/// Grammar (informal):
///
/// ```text
/// pair_list := pair ("," pair)*
/// pair      := (cipher ":")? material
/// cipher    := "aes-256-gcm" | "none"      (case-insensitive)
/// material  := base64_key                  (for real ciphers)
///            | ε                           (for `none:`)
/// ```
///
/// * Whitespace around commas / colons is tolerated.
/// * A pair without a colon defaults its cipher to `aes-256-gcm`
///   (since that's the only shipping AEAD today; new ciphers
///   MUST use the explicit `<cipher>:<key>` form).
/// * A `none` pair MUST use the explicit `none:` form (with the
///   trailing colon and empty material) — omitting the colon
///   would be ambiguous with a real key that happens to base64
///   to `none`.
///
/// Guaranteed non-empty on `Ok`. All error messages carry the
/// entry name so operators see which env var failed.
///
/// Errors:
/// * Empty list.
/// * Empty pair (leading / trailing / duplicate comma).
/// * Unknown cipher name.
/// * `none` pair with non-empty material.
/// * More than one `none` pair.
/// * Real-cipher pair with empty material.
/// * Non-base64 key material.
/// * Wrong-length key material (≠ 32 bytes).
/// * Duplicate key material (same 32 bytes twice).
pub fn parse_encryption_pair_list(entry_name: &str, raw: &str) -> Result<Vec<KeyPair>, String> {
    use base64::Engine;
    let raw = raw.trim();
    if raw.is_empty() {
        return Err(format!(
            "OXICLOUD_STORAGE_{entry_name}_ENCRYPTION_KEY is empty — set at least \
             one `<cipher>:<key>` pair, or omit the variable entirely for an \
             unencrypted entry."
        ));
    }

    let mut pairs: Vec<KeyPair> = Vec::new();
    let mut seen_none = false;
    let mut seen_keys: Vec<[u8; 32]> = Vec::new();

    for (idx0, pair_raw) in raw.split(',').enumerate() {
        let pos = idx0 + 1;
        let pair_raw = pair_raw.trim();
        if pair_raw.is_empty() {
            return Err(format!(
                "OXICLOUD_STORAGE_{entry_name}_ENCRYPTION_KEY has an empty pair at \
                 position {pos} — remove leading/trailing/duplicate commas."
            ));
        }

        // `split_once(':')` gives us (cipher, key); no colon = key-only,
        // implicit AES-256-GCM (the only shipping real cipher).
        let (cipher_tok, key_b64) = match pair_raw.split_once(':') {
            Some((c, k)) => (c.trim(), k.trim()),
            None => ("aes-256-gcm", pair_raw),
        };

        let cipher = CipherKind::parse(cipher_tok).ok_or_else(|| {
            format!(
                "OXICLOUD_STORAGE_{entry_name}_ENCRYPTION_KEY pair {pos} has unknown \
                 cipher `{cipher_tok}` — supported: `aes-256-gcm`, `none`."
            )
        })?;

        if !cipher.needs_key() {
            if !key_b64.is_empty() {
                return Err(format!(
                    "OXICLOUD_STORAGE_{entry_name}_ENCRYPTION_KEY pair {pos} declares \
                     cipher `none` but has key material — use `none:` (trailing \
                     colon, empty key)."
                ));
            }
            if seen_none {
                return Err(format!(
                    "OXICLOUD_STORAGE_{entry_name}_ENCRYPTION_KEY has more than one \
                     `none` pair — at most one is allowed."
                ));
            }
            seen_none = true;
            pairs.push(KeyPair {
                cipher,
                key_material: None,
            });
            continue;
        }

        // Real cipher — decode + length-check the key.
        if key_b64.is_empty() {
            return Err(format!(
                "OXICLOUD_STORAGE_{entry_name}_ENCRYPTION_KEY pair {pos} has empty \
                 key material for cipher `{}`.",
                cipher.as_str()
            ));
        }
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(key_b64)
            .map_err(|e| {
                format!(
                    "OXICLOUD_STORAGE_{entry_name}_ENCRYPTION_KEY pair {pos} is not \
                     valid base64: {e}"
                )
            })?;
        if decoded.len() != 32 {
            return Err(format!(
                "OXICLOUD_STORAGE_{entry_name}_ENCRYPTION_KEY pair {pos} decodes to \
                 {} bytes; must be exactly 32 bytes (AES-256).",
                decoded.len()
            ));
        }
        let mut key = [0u8; 32];
        key.copy_from_slice(&decoded);

        if seen_keys.iter().any(|k| k == &key) {
            return Err(format!(
                "OXICLOUD_STORAGE_{entry_name}_ENCRYPTION_KEY has the same key \
                 material twice — each pair must be unique."
            ));
        }
        seen_keys.push(key);

        pairs.push(KeyPair {
            cipher,
            key_material: Some(key),
        });
    }

    // Belt-and-braces: the loop above rejects empty pairs, so this
    // can only fire if the whole raw input was pure whitespace, which
    // we already caught at the top. Kept as an invariant guard so a
    // future refactor can't silently produce an empty vec.
    if pairs.is_empty() {
        return Err(format!(
            "OXICLOUD_STORAGE_{entry_name}_ENCRYPTION_KEY produced no pairs after \
             parsing — this should never happen; please file a bug."
        ));
    }

    Ok(pairs)
}

/// One-shot helper that computes the SSH-style colon-hex fingerprint
/// of a base64-encoded AES-256 key.
///
/// Wraps [`KeyPair::new_aes_gcm`] + [`KeyPair::fingerprint_short`]
/// with the same base64 / length validation the pair-list parser
/// uses, so callers don't have to reimplement it.
///
/// Used by the `oxicloud --fingerprint <base64>` CLI subcommand so
/// admins can identify which key in their `.env` corresponds to the
/// `head_key_fp` a `backend_rotate` run reported on completion —
/// see `docs/plan/storage-key-rotation.md`.
///
/// Errors on non-base64 input or on decoded length ≠ 32 bytes (the
/// AES-256 key size constraint).
pub fn fingerprint_from_base64_key(key_b64: &str) -> Result<String, String> {
    use base64::Engine;
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(key_b64.trim())
        .map_err(|e| format!("input is not valid base64: {e}"))?;
    if decoded.len() != 32 {
        return Err(format!(
            "decoded key is {} bytes; AES-256 requires exactly 32",
            decoded.len()
        ));
    }
    let mut key = [0u8; 32];
    key.copy_from_slice(&decoded);
    Ok(KeyPair::new_aes_gcm(key)
        .fingerprint_short()
        .expect("aes_gcm pair always has key material"))
}

/// Emit a per-entry boot line summarising the pair-list — one line
/// per encrypted entry, with each pair's cipher and truncated
/// fingerprint, and a `←` marker on the head pair (the write pair).
///
/// The head-vs-non-head fingerprint comparison detects a rotation
/// window (operator added a new pair but the format-upgrade job
/// hasn't reconciled existing blobs yet). That's not an error — it's
/// the expected state during a live rotation — but it's worth a
/// prominent `warn!` line so operators can spot "we're mid-rotation"
/// at a glance during boot audits.
///
/// Unencrypted entries emit a single `info!` line at
/// `target: "oxicloud::storage"`, keeping the boot output symmetric.
///
/// The truncated fingerprint uses 12 hex chars (6 bytes of SHA-256)
/// — enough to distinguish keys within a small pair list, short
/// enough to keep the log line tight. The on-blob v1 header uses a
/// DIFFERENT truncation (8 bytes / 16 hex chars) — see
/// `KeyPair::fingerprint_short` for the rationale.
pub fn log_storage_encryption_summary(entries: &[NamedStorageEntry]) {
    for entry in entries {
        match &entry.encryption {
            None => {
                tracing::info!(
                    target: "oxicloud::storage",
                    entry = %entry.name,
                    "storage entry `{}` — unencrypted", entry.name
                );
            }
            Some(pairs) => {
                let head_idx = pairs.len() - 1;
                let rendered: Vec<String> = pairs
                    .iter()
                    .enumerate()
                    .map(|(i, kp)| {
                        let fp = kp.fingerprint_short().unwrap_or_else(|| "—".to_string());
                        let head_mark = if i == head_idx { " ← head" } else { "" };
                        format!("{}:{}{}", kp.cipher.as_str(), fp, head_mark)
                    })
                    .collect();

                tracing::info!(
                    target: "oxicloud::storage",
                    entry = %entry.name,
                    pairs = pairs.len(),
                    "storage entry `{}` — {} pair(s): {}",
                    entry.name,
                    pairs.len(),
                    rendered.join(", ")
                );

                // Warn on head-vs-other fingerprint divergence — the
                // signal for "rotation in progress, don't drop the
                // older key yet". Uses fingerprints (not raw
                // material) so the comparison stays cheap and the
                // log line reveals no key bytes.
                let head_fp = pairs[head_idx].fingerprint_short();
                let non_head_fps: Vec<Option<String>> = pairs[..head_idx]
                    .iter()
                    .map(|kp| kp.fingerprint_short())
                    .collect();
                if !non_head_fps.is_empty() && non_head_fps.iter().any(|fp| fp != &head_fp) {
                    tracing::warn!(
                        target: "oxicloud::storage",
                        entry = %entry.name,
                        head_fp = ?head_fp,
                        "storage entry `{}` has a rotation window open — head pair differs \
                         from at least one older pair. Existing blobs written under an older \
                         key stay readable, but keep the older pair in `.env` until the \
                         format-upgrade job has reconciled them.",
                        entry.name
                    );
                }
            }
        }
    }
}

/// One named storage entry declared in `.env`.
///
/// See `docs/plan/storage-multi-entry.md`. Each entry is a fully-realised
/// backend configuration that the admin can point the runtime at via
/// `admin_settings.storage.active_backend_name`. Migrations move blobs
/// between two entries; consistency audits can be scoped to any registered
/// entry via `?storage=<name>`.
///
/// Entries are parsed from env at boot and held on `AppConfig.storage_entries`.
/// The set is immutable per-deploy — adding/removing entries requires a
/// server restart. The DB pointer `active_backend_name` is the ONLY mutable
/// runtime storage-selection surface.
#[derive(Debug, Clone)]
pub struct NamedStorageEntry {
    /// Stable admin-authored identifier, `[a-z0-9_-]{1,32}`. Unique
    /// within the entry list. Referenced from
    /// `admin_settings.storage.active_backend_name` and from
    /// `?storage=<name>` on the audit APIs.
    pub name: String,
    /// Which backend type this entry uses.
    pub backend: StorageBackendType,
    /// Root directory for `Local`. `None` for `S3`/`Azure`.
    /// Defaults to `"storage"` when the entry is Local and no
    /// `_ROOT_DIR` is set (matches today's flat-var default).
    pub root_dir: Option<String>,
    /// S3 configuration for `S3`. `None` for other backends.
    pub s3: Option<S3StorageConfig>,
    /// Azure configuration for `Azure`. `None` for other backends.
    pub azure: Option<AzureStorageConfig>,
    /// Ordered list of `<cipher>:<key>` pairs from
    /// `OXICLOUD_STORAGE_<NAME>_ENCRYPTION_KEY`. `None` means the
    /// operator did not set the variable — the entry is
    /// unencrypted, writes and reads pass through the raw backend.
    ///
    /// `Some(pairs)` is guaranteed non-empty (parser rejects the
    /// empty-list case). The LAST pair is the write pair; every
    /// pair is a candidate for reads. See
    /// `docs/plan/storage-key-rotation.md` §"The pair-list config".
    ///
    /// Access this field through the [`Self::head_key_material`],
    /// [`Self::head_cipher`], [`Self::is_encrypted`], and
    /// [`Self::encryption_pairs`] helpers rather than pattern-matching
    /// directly — they encapsulate the "unencrypted vs `none:`-headed
    /// pair-list" distinction and keep call sites stable across
    /// future K2 changes.
    pub encryption: Option<Vec<KeyPair>>,
}

impl NamedStorageEntry {
    /// Head pair — the one used for writes (K1) and, once K2 wires
    /// the header, for read-dispatch of blobs whose header advertises
    /// the head pair's `key_fp`.
    ///
    /// `None` when the entry has no `_ENCRYPTION_KEY` at all OR when
    /// the head pair is `none:` (writes produce plaintext, so no key
    /// material to hand to the AEAD).
    pub fn head_key_material(&self) -> Option<&[u8; 32]> {
        self.encryption
            .as_ref()
            .and_then(|pairs| pairs.last())
            .and_then(|kp| kp.key_material.as_ref())
    }

    /// Head pair's cipher choice. `None` when the entry has no
    /// `_ENCRYPTION_KEY` at all. `Some(CipherKind::None)` when the
    /// head pair is explicitly `none:` — semantically distinct from
    /// "unconfigured", useful during decrypt-in-place migrations.
    pub fn head_cipher(&self) -> Option<CipherKind> {
        self.encryption
            .as_ref()
            .and_then(|pairs| pairs.last())
            .map(|kp| kp.cipher)
    }

    /// `true` iff writes to this entry produce ciphertext right now.
    /// Distinct from "the operator has ever configured encryption" —
    /// during a decrypt-in-place migration this returns `false` (head
    /// is `none:`) even though older pairs in the list are real
    /// ciphers used to READ existing encrypted blobs.
    pub fn is_encrypted(&self) -> bool {
        self.head_cipher().is_some_and(|c| c != CipherKind::None)
    }

    /// The whole pair list, or an empty slice when the entry is
    /// unencrypted. Used by K2's read path to walk pairs and by
    /// `backend_rotate` to enumerate legacy pairs. Callers that only
    /// need the write pair should prefer [`Self::head_key_material`].
    pub fn encryption_pairs(&self) -> &[KeyPair] {
        self.encryption.as_deref().unwrap_or(&[])
    }
}

/// Validation for a `NamedStorageEntry.name`. Restricts to a safe subset
/// so that entry names embed cleanly in env-var suffixes without
/// escaping (`OXICLOUD_STORAGE_<NAME>_BACKEND=...`) and in query params
/// (`?storage=<name>`) without URL-encoding surprises.
///
/// Rules:
/// - 1 to 32 chars.
/// - Lowercase ASCII letters, digits, `_`, `-` only.
///
/// Deliberately no uppercase — the env-var expansion is
/// `OXICLOUD_STORAGE_<NAME>_...`, and mixing case would create confusing
/// same-looking-different-behaving variants (`_S3_` vs `_s3_`). Lowercase-
/// only keeps the surface unambiguous.
pub fn is_valid_entry_name(name: &str) -> bool {
    let len = name.len();
    if !(1..=32).contains(&len) {
        return false;
    }
    name.chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

/// Env-var names counted as "legacy flat storage-backend vars" — set that
/// triggers the conflict-with-`_ENTRIES` fail-fast. Kept in one place so
/// docs, error messages, and tests reference the same list.
///
/// **Not included**: `OXICLOUD_STORAGE_PATH` — that variable drives
/// multiple non-backend things (chunk-dir default, ambient storage path)
/// and remains valid alongside `_ENTRIES`. Per-entry `_ROOT_DIR` falls
/// back to it for Local entries when unset.
pub const LEGACY_STORAGE_BACKEND_VARS: &[&str] = &[
    "OXICLOUD_STORAGE_BACKEND",
    "OXICLOUD_S3_ENDPOINT_URL",
    "OXICLOUD_S3_BUCKET",
    "OXICLOUD_S3_REGION",
    "OXICLOUD_S3_ACCESS_KEY",
    "OXICLOUD_S3_SECRET_KEY",
    "OXICLOUD_S3_FORCE_PATH_STYLE",
    "OXICLOUD_AZURE_ACCOUNT_NAME",
    "OXICLOUD_AZURE_ACCOUNT_KEY",
    "OXICLOUD_AZURE_CONTAINER",
    "OXICLOUD_AZURE_SAS_TOKEN",
    "OXICLOUD_AZURE_ENDPOINT_URL",
    "OXICLOUD_STORAGE_ENCRYPTION_ENABLED",
    "OXICLOUD_STORAGE_ENCRYPTION_KEY",
];

/// Parse the multi-entry storage config from env vars.
///
/// See `docs/plan/storage-multi-entry.md` for the full model. This
/// function encodes the four-cell decision matrix documented there:
///
/// | `_ENTRIES` set? | legacy vars present? | Return                                        |
/// |---|---|---|
/// | No  | No  | `Ok(vec![])` — no entries; caller uses framework defaults. |
/// | No  | Yes | `Ok(vec![synthesized_default])` — upgrade path.            |
/// | Yes | No  | `Ok(vec![entry_1, entry_2, …])` — the declared entries.    |
/// | Yes | Yes | `Err("legacy vars alongside _ENTRIES: …")` — fail-fast.    |
///
/// All fatal shapes return `Err`. Callers in the boot path
/// (`AppConfig::from_env`) `.expect(...)` on the result — an invalid
/// storage config must abort at startup, not silently degrade.
pub fn parse_storage_entries() -> Result<Vec<NamedStorageEntry>, String> {
    let entries_raw = env::var("OXICLOUD_STORAGE_ENTRIES").unwrap_or_default();
    let entries_raw = entries_raw.trim();
    let legacy_present: Vec<&&str> = LEGACY_STORAGE_BACKEND_VARS
        .iter()
        .filter(|v| env::var(v).is_ok())
        .collect();

    if entries_raw.is_empty() {
        // Cell (No, No) OR (No, Yes).
        if legacy_present.is_empty() {
            return Ok(Vec::new());
        }
        // Legacy synthesis: build one `default` entry from flat vars.
        // Same field-extraction logic AppConfig::from_env uses today
        // for `storage.backend` / `storage.s3` / `storage.azure` /
        // `storage.encryption`; centralised here so the new entry
        // model and the legacy path stay bit-identical.
        //
        // Emit a deprecation warning naming every legacy var we saw
        // so operators see the exact cleanup list in boot logs. Also
        // logs to `target: "audit"` so it lands on the operational
        // stream that gets watched by monitoring, not just the debug
        // channel. Removal target isn't fixed yet — legacy still
        // works — but the earlier operators start migrating, the
        // less churn when we do pull the plug.
        let names: Vec<&str> = legacy_present.iter().map(|v| **v).collect();
        tracing::warn!(
            target: "audit",
            event = "storage.legacy_flat_vars_deprecated",
            vars = ?names,
            "⚠️  DEPRECATED: booted with legacy flat storage vars ({vars:?}). \
             Migrate each var into `OXICLOUD_STORAGE_<NAME>_*` under an entry \
             declared in `OXICLOUD_STORAGE_ENTRIES`. See \
             docs/plan/storage-multi-entry.md §'Legacy flat-var interaction'.",
            vars = names,
        );
        return Ok(vec![synthesize_default_from_legacy_vars()?]);
    }

    // `_ENTRIES` is set.
    if !legacy_present.is_empty() {
        // Cell (Yes, Yes) — fail-fast. Name every legacy var found so
        // the operator sees the exact cleanup list, not a generic
        // "conflict" message.
        let names: Vec<String> = legacy_present.iter().map(|v| (**v).to_string()).collect();
        return Err(format!(
            "OXICLOUD_STORAGE_ENTRIES is set, but legacy storage-backend env vars are also \
             present: [{}]. In multi-entry mode these flat vars are ignored — remove them \
             from your environment / .env, or migrate their values into per-entry \
             OXICLOUD_STORAGE_<NAME>_* form. See docs/plan/storage-multi-entry.md \
             §'Legacy flat-var interaction'.",
            names.join(", ")
        ));
    }

    // Cell (Yes, No): validate the name list structure first (empty
    // names, invalid chars, duplicates), THEN parse each entry's
    // fields. Two-phase separation guarantees the operator sees the
    // structural problem — "you have a typo in `_ENTRIES` itself" —
    // before any per-entry field-missing message, which would be
    // more confusing than helpful when the underlying issue is the
    // list itself.
    let raw_names: Vec<&str> = entries_raw.split(',').map(str::trim).collect();
    let mut names_seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for name in &raw_names {
        if name.is_empty() {
            return Err(format!(
                "OXICLOUD_STORAGE_ENTRIES contains an empty name (from `{entries_raw}`) — \
                 commas must separate non-empty names."
            ));
        }
        if !is_valid_entry_name(name) {
            return Err(format!(
                "OXICLOUD_STORAGE_ENTRIES: invalid entry name `{name}` — allowed characters \
                 are lowercase ASCII letters, digits, `_`, `-`; length 1-32."
            ));
        }
        if !names_seen.insert((*name).to_string()) {
            return Err(format!(
                "OXICLOUD_STORAGE_ENTRIES: duplicate name `{name}` — entry names must be unique."
            ));
        }
    }
    let mut out: Vec<NamedStorageEntry> = Vec::with_capacity(raw_names.len());
    for name in raw_names {
        out.push(parse_named_entry(name)?);
    }
    Ok(out)
}

/// Read the per-entry env vars for `name` and build a
/// [`NamedStorageEntry`]. Called by [`parse_storage_entries`] for each
/// declared name.
fn parse_named_entry(name: &str) -> Result<NamedStorageEntry, String> {
    let backend_raw = env::var(format!("OXICLOUD_STORAGE_{name}_BACKEND")).map_err(|_| {
        format!(
            "OXICLOUD_STORAGE_{name}_BACKEND is missing — every entry declared in \
             OXICLOUD_STORAGE_ENTRIES must specify its backend type (local / s3 / azure)."
        )
    })?;
    let backend = match backend_raw.to_lowercase().as_str() {
        "local" => StorageBackendType::Local,
        "s3" => StorageBackendType::S3,
        "azure" => StorageBackendType::Azure,
        other => {
            return Err(format!(
                "OXICLOUD_STORAGE_{name}_BACKEND=`{other}` is not a known backend type — \
                 expected one of: local, s3, azure."
            ));
        }
    };

    let mut root_dir = None;
    let mut s3 = None;
    let mut azure = None;

    match backend {
        StorageBackendType::Local => {
            // `_ROOT_DIR` optional; falls back to `OXICLOUD_STORAGE_PATH`,
            // then to the framework default at build time. The fallback
            // means a Local entry can be declared with just `_BACKEND=local`
            // in .env — matches the friction level of the flat-var world.
            root_dir = env::var(format!("OXICLOUD_STORAGE_{name}_ROOT_DIR"))
                .ok()
                .or_else(|| env::var("OXICLOUD_STORAGE_PATH").ok());
        }
        StorageBackendType::S3 => {
            let bucket = env::var(format!("OXICLOUD_STORAGE_{name}_S3_BUCKET"))
                .map_err(|_| {
                    format!(
                        "OXICLOUD_STORAGE_{name}_S3_BUCKET is required when \
                         OXICLOUD_STORAGE_{name}_BACKEND=s3."
                    )
                })?
                .trim()
                .to_string();
            if bucket.is_empty() {
                return Err(format!(
                    "OXICLOUD_STORAGE_{name}_S3_BUCKET is empty — bucket name is required for S3 \
                     backend entries."
                ));
            }
            s3 = Some(S3StorageConfig {
                endpoint_url: env::var(format!("OXICLOUD_STORAGE_{name}_S3_ENDPOINT_URL")).ok(),
                bucket,
                region: env::var(format!("OXICLOUD_STORAGE_{name}_S3_REGION"))
                    .unwrap_or_else(|_| "us-east-1".to_string()),
                access_key: env::var(format!("OXICLOUD_STORAGE_{name}_S3_ACCESS_KEY"))
                    .unwrap_or_default(),
                secret_key: env::var(format!("OXICLOUD_STORAGE_{name}_S3_SECRET_KEY"))
                    .unwrap_or_default(),
                force_path_style: env::var(format!("OXICLOUD_STORAGE_{name}_S3_FORCE_PATH_STYLE"))
                    .map(|v| v.parse::<bool>().unwrap_or(false))
                    .unwrap_or(false),
            });
        }
        StorageBackendType::Azure => {
            let container = env::var(format!("OXICLOUD_STORAGE_{name}_AZURE_CONTAINER"))
                .map_err(|_| {
                    format!(
                        "OXICLOUD_STORAGE_{name}_AZURE_CONTAINER is required when \
                         OXICLOUD_STORAGE_{name}_BACKEND=azure."
                    )
                })?
                .trim()
                .to_string();
            if container.is_empty() {
                return Err(format!(
                    "OXICLOUD_STORAGE_{name}_AZURE_CONTAINER is empty — container name is required \
                     for Azure backend entries."
                ));
            }
            azure = Some(AzureStorageConfig {
                account_name: env::var(format!("OXICLOUD_STORAGE_{name}_AZURE_ACCOUNT_NAME"))
                    .unwrap_or_default(),
                account_key: env::var(format!("OXICLOUD_STORAGE_{name}_AZURE_ACCOUNT_KEY"))
                    .unwrap_or_default(),
                container,
                sas_token: env::var(format!("OXICLOUD_STORAGE_{name}_AZURE_SAS_TOKEN")).ok(),
                endpoint_url: env::var(format!("OXICLOUD_STORAGE_{name}_AZURE_ENDPOINT_URL")).ok(),
            });
        }
    }

    // Encryption — pair-list format. Parser accepts both the K1
    // shape (`aes-256-gcm:<K>` or bare `<K>`) and future shapes
    // (2-pair rotation, `none:` head, …). See
    // `docs/plan/storage-key-rotation.md`.
    let encryption = match env::var(format!("OXICLOUD_STORAGE_{name}_ENCRYPTION_KEY")) {
        Ok(raw) if !raw.trim().is_empty() => Some(parse_encryption_pair_list(name, &raw)?),
        _ => None,
    };

    Ok(NamedStorageEntry {
        name: name.to_string(),
        backend,
        root_dir,
        s3,
        azure,
        encryption,
    })
}

/// Build the synthesized `default` entry from the pre-multi-entry flat
/// vars. Called only when `_ENTRIES` is unset AND at least one legacy
/// storage-backend var is present. Preserves the exact field-population
/// logic `AppConfig::from_env` used to run inline.
fn synthesize_default_from_legacy_vars() -> Result<NamedStorageEntry, String> {
    let backend = match env::var("OXICLOUD_STORAGE_BACKEND")
        .unwrap_or_default()
        .to_lowercase()
        .as_str()
    {
        "s3" => StorageBackendType::S3,
        "azure" => StorageBackendType::Azure,
        _ => StorageBackendType::Local,
    };

    let mut root_dir = None;
    let mut s3 = None;
    let mut azure = None;

    match backend {
        StorageBackendType::Local => {
            root_dir = env::var("OXICLOUD_STORAGE_PATH").ok();
        }
        StorageBackendType::S3 => {
            let bucket = env::var("OXICLOUD_S3_BUCKET").unwrap_or_default();
            if bucket.is_empty() {
                return Err(
                    "OXICLOUD_STORAGE_BACKEND=s3 but OXICLOUD_S3_BUCKET is not set — legacy \
                     synthesis requires the bucket name."
                        .to_string(),
                );
            }
            s3 = Some(S3StorageConfig {
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
        StorageBackendType::Azure => {
            let container = env::var("OXICLOUD_AZURE_CONTAINER").unwrap_or_default();
            if container.is_empty() {
                return Err(
                    "OXICLOUD_STORAGE_BACKEND=azure but OXICLOUD_AZURE_CONTAINER is not set — \
                     legacy synthesis requires the container name."
                        .to_string(),
                );
            }
            azure = Some(AzureStorageConfig {
                account_name: env::var("OXICLOUD_AZURE_ACCOUNT_NAME").unwrap_or_default(),
                account_key: env::var("OXICLOUD_AZURE_ACCOUNT_KEY").unwrap_or_default(),
                container,
                sas_token: env::var("OXICLOUD_AZURE_SAS_TOKEN").ok(),
                endpoint_url: env::var("OXICLOUD_AZURE_ENDPOINT_URL").ok(),
            });
        }
    }

    // Legacy synthesis path: the pre-multi-entry world had a single
    // `OXICLOUD_STORAGE_ENCRYPTION_KEY` (raw base64, no cipher/none
    // syntax). The pair-list parser accepts that shape as a
    // 1-pair `aes-256-gcm:<K>` after defaulting the cipher, so we
    // route through it and get the same validation for free.
    //
    // The legacy flat surface has no cipher variable, so no
    // agreement check is needed here.
    let encryption = match env::var("OXICLOUD_STORAGE_ENCRYPTION_KEY") {
        Ok(k) if !k.trim().is_empty() => Some(parse_encryption_pair_list("default", &k)?),
        _ => None,
    };

    Ok(NamedStorageEntry {
        name: "default".to_string(),
        backend,
        root_dir,
        s3,
        azure,
        encryption,
    })
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

    /// DPoP session-binding enforcement (RFC 9449). Bound sessions —
    /// those created with a `dpop_jkt` supplied at login — carry a
    /// browser-held keypair thumbprint; the middleware verifies a
    /// per-request signed proof so that stealing the session cookie
    /// alone is useless without the private key.
    ///
    /// Modes (see `DpopMode` enum):
    ///   * `Off` (default) — middleware is a pass-through; no
    ///     verification even when a proof is present. Ship-safe
    ///     default while the client rollout catches up.
    ///   * `Opportunistic` — verify when a proof is present, reject
    ///     mismatches; skip when absent. Warn on
    ///     `dpop.header_missing_but_session_bound`. Rollout mode.
    ///   * `Required` — bound sessions MUST present a valid proof.
    ///     Unbound sessions (`dpop_jkt IS NULL` — app passwords,
    ///     Nextcloud clients, legacy) remain exempt at the
    ///     middleware level.
    ///
    /// Env: `OXICLOUD_DPOP_MODE` in `{off,opportunistic,required}`
    /// (default `off`).
    pub dpop_mode: DpopMode,
}

/// DPoP session-binding enforcement mode. See `AuthConfig::dpop_mode`
/// and `docs/plan/dpop.md` for the rollout strategy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DpopMode {
    /// Middleware pass-through — DPoP header is neither required nor
    /// verified. Default: safe while clients roll out proof-signing.
    #[default]
    Off,
    /// Verify when present, allow when absent. Bound sessions still
    /// get a warning audit line when they arrive without a proof.
    Opportunistic,
    /// Bound sessions (`dpop_jkt IS NOT NULL`) MUST present a valid
    /// proof or 401. Unbound sessions remain exempt.
    Required,
}

impl DpopMode {
    pub fn from_env_str(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "off" => Some(Self::Off),
            "opportunistic" => Some(Self::Opportunistic),
            "required" => Some(Self::Required),
            _ => None,
        }
    }
}

/// Self-service auth method. Exposed as `AuthConfig::allowed_auth_methods`
/// and parsed from `OXICLOUD_AUTH_METHODS` (comma-separated). OIDC is
/// a first-class allowlist token: `OXICLOUD_AUTH_METHODS=oidc` = OIDC
/// only (needs `OXICLOUD_OIDC_ENABLED=true` + a full OIDC config bucket
/// or the boot rejects — cross-validation lives in `AppConfig::from_env`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthMethod {
    Password,
    MagicLink,
    Oidc,
}

impl AuthMethod {
    /// Case-insensitive parse: accepts `password`, `magic_link`, and the
    /// dash form `magic-link` (some operators habitually use dashes),
    /// plus `oidc`. Unknown token returns `None`; the caller (env
    /// parser) treats that as a fatal boot error rather than a warning.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "password" => Some(Self::Password),
            "magic_link" | "magic-link" | "magiclink" => Some(Self::MagicLink),
            "oidc" | "sso" => Some(Self::Oidc),
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

    /// When OIDC is the ONLY auth method available (standalone SSO
    /// posture — no password + no magic-link), instruct the login SPA
    /// to auto-redirect to the OIDC authorize endpoint on page load
    /// instead of showing a click-to-continue button.
    ///
    /// Opt-in because:
    ///
    /// - Auto-redirect can create loops on IdP failure (login → IdP
    ///   error → back to login → auto-redirect again).
    /// - Logout followed by "visit login page" would bounce the user
    ///   right back into the app they just logged out of.
    ///
    /// Only takes effect when the effective allowlist is `[Oidc]`
    /// (or magic-link is off via the OIDC-master rule and password is
    /// disabled): if any other method is live the policy is a silent
    /// no-op (there's a choice to render, not a single path).
    AutoRedirectIfStandaloneOidc,
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
            "auto_redirect_if_standalone_oidc" | "auto-redirect-if-standalone-oidc" => {
                Some(Self::AutoRedirectIfStandaloneOidc)
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
            dpop_mode: DpopMode::Off,
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
    /// When TRUE (default), an OIDC login whose subject doesn't match
    /// any existing user AUTO-LINKS to the local user with the same
    /// verified email address (if any). Requires `email_verified=true`
    /// from the IdP. See docs/plan/oidc-account-linking.md § Auto-link.
    ///
    /// Set FALSE for compliance postures that require explicit consent
    /// for every OIDC linkage. Self-service link flow still works
    /// regardless of this flag.
    pub auto_link_email_match: bool,
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
            auto_link_email_match: true,
        }
    }
}

/// OPAQUE aPAKE configuration (RFC 9807, Phase 0 substrate).
///
/// OPAQUE is a zero-knowledge password-authenticated key exchange: the
/// passphrase never leaves the client. This struct carries the runtime
/// knobs the server needs (mode, ciphersuite version, persisted
/// [`ServerSetup`] blob) plus the client-side KSF params the SPA reads
/// out of `/api/health` to configure its Argon2.
///
/// **The KSF params are client-side.** RFC 9807 runs Argon2 on the client
/// before the OPRF exchange; the server never invokes it. The params live
/// here so the operator has one source of truth and the SPA can fetch
/// them at page load — changing them requires re-registration for
/// affected users.
#[derive(Debug, Clone)]
pub struct OpaqueConfig {
    /// Runtime mode gate. See
    /// [`crate::infrastructure::services::opaque_service::OpaqueMode`]
    /// for the state-machine and the phase-plan mapping.
    ///
    /// Env: `OXICLOUD_AUTH_OPAQUE_MODE` (`off` | `migrate` | `opaque_only`).
    /// Default: `off`.
    pub mode: crate::infrastructure::services::opaque_service::OpaqueMode,
    /// Base64-encoded [`opaque_ke::ServerSetup`] blob. Generated once
    /// per deployment and persisted verbatim — rotating this invalidates
    /// every user's registration. Runbook: on first boot with
    /// `OXICLOUD_AUTH_OPAQUE_MODE != off`, if this is unset, print a fatal
    /// message with a fresh setup for the operator to paste into their
    /// env, then exit.
    ///
    /// Env: `OXICLOUD_AUTH_OPAQUE_SERVER_SETUP`. No default.
    pub server_setup_b64: Option<String>,
    /// Ciphersuite version stamped into `auth.users.opaque_ciphersuite_version`
    /// on registration. Bumping this without changing the actual
    /// ciphersuite type alias in the service module is meaningless;
    /// bumping this WITH a type change invalidates all envelopes.
    ///
    /// Env: not exposed. Compile-time constant, currently `1`.
    pub ciphersuite_version: i16,
    /// Client-side Argon2id memory cost in KiB. Published to the SPA so
    /// the client can construct a matching `argon2::Argon2` before
    /// running `ClientRegistration::start` / `ClientLogin::start`.
    ///
    /// **The KSF runs client-side, twice per login** (once each in
    /// OPAQUE's `start` and `finish` steps), inside a synchronous WASM
    /// call on the main thread. So the interactive login latency the
    /// user perceives is roughly `2 × Argon2(memory, iterations)`.
    ///
    /// Default: `47104` KiB (46 MiB) — matches the OWASP recommendation
    /// for interactive password-based KDF. Rationale in
    /// `docs/config/authentication.md § OPAQUE — KSF parameters`.
    ///
    /// Env: `OXICLOUD_AUTH_OPAQUE_KSF_MEMORY_KIB`.
    pub ksf_memory_kib: u32,
    /// Client-side Argon2id iteration count.
    ///
    /// Env: `OXICLOUD_AUTH_OPAQUE_KSF_ITERATIONS`. Default: `1`
    /// (OWASP recommendation for interactive auth).
    pub ksf_iterations: u32,
    /// Client-side Argon2id parallelism (lanes).
    ///
    /// Env: `OXICLOUD_AUTH_OPAQUE_KSF_PARALLELISM`. Default: `1`
    /// (OWASP recommendation). Higher values only help on multi-core
    /// hardware and hurt single-core / older mobile devices.
    pub ksf_parallelism: u32,
}

impl Default for OpaqueConfig {
    fn default() -> Self {
        Self {
            mode: crate::infrastructure::services::opaque_service::OpaqueMode::Off,
            server_setup_b64: None,
            ciphersuite_version: 1,
            // OWASP recommended interactive-auth Argon2id parameters
            // (2024 password-storage cheat sheet): 46 MiB / 1 iter /
            // 1 lane. Keeps interactive login usable on older /
            // low-end / mobile devices where a heavier memory budget
            // either takes tens of seconds OR fails to allocate WASM
            // heap outright (iOS Safari + old Android WebView cap).
            // Full rationale in docs/config/authentication.md.
            //
            // Changing these values does NOT invalidate existing
            // envelopes — the KSF params are effectively baked into
            // the envelope at register time. Silent-migration
            // re-mints under the current params on the user's next
            // password change.
            ksf_memory_kib: 47_104,
            ksf_iterations: 1,
            ksf_parallelism: 1,
        }
    }
}

impl OpaqueConfig {
    /// Load OPAQUE configuration from environment variables. Mirrors the
    /// pattern used by [`OidcConfig::from_env`] — every field falls back
    /// to the [`Default`] impl when unset, so the config is safe to
    /// construct even in `Off` mode.
    pub fn from_env() -> Self {
        use std::env;
        let mut cfg = Self::default();
        if let Ok(v) = env::var("OXICLOUD_AUTH_OPAQUE_MODE") {
            match crate::infrastructure::services::opaque_service::OpaqueMode::parse(&v) {
                Some(m) => cfg.mode = m,
                None => {
                    tracing::warn!(
                        target: "oxicloud::config",
                        value = %v,
                        "OXICLOUD_AUTH_OPAQUE_MODE has an unrecognised value — keeping default (off). \
                         Accepted: off | migrate | opaque_only"
                    );
                }
            }
        }
        if let Ok(v) = env::var("OXICLOUD_AUTH_OPAQUE_SERVER_SETUP") {
            cfg.server_setup_b64 = Some(v);
        }
        if let Ok(v) = env::var("OXICLOUD_AUTH_OPAQUE_KSF_MEMORY_KIB")
            && let Ok(n) = v.parse::<u32>()
        {
            cfg.ksf_memory_kib = n;
        }
        if let Ok(v) = env::var("OXICLOUD_AUTH_OPAQUE_KSF_ITERATIONS")
            && let Ok(n) = v.parse::<u32>()
        {
            cfg.ksf_iterations = n;
        }
        if let Ok(v) = env::var("OXICLOUD_AUTH_OPAQUE_KSF_PARALLELISM")
            && let Ok(n) = v.parse::<u32>()
        {
            cfg.ksf_parallelism = n;
        }
        cfg
    }

    /// Runtime mode after cross-checking against the auth-method allowlist.
    ///
    /// OPAQUE is fundamentally a **password** mechanism — its only reason
    /// to exist is to replace `POST /api/auth/login`. An operator running
    /// OIDC-only or magic-link-only (`OXICLOUD_AUTH_METHODS=oidc` or
    /// `=magic_link`) has no password path for OPAQUE to shadow; any
    /// non-`Off` mode would be a no-op that still nagged them for
    /// `OXICLOUD_AUTH_OPAQUE_SERVER_SETUP` at boot.
    ///
    /// This helper resolves the misconfig quietly: if password isn't in
    /// the allowlist AND OPAQUE mode is non-`Off`, we downgrade to `Off`
    /// and emit an audit-channel INFO explaining why (so it shows up in
    /// operator log tailing without being a startup warning that fails
    /// health checks). Every OPAQUE-facing caller — the DI factory, the
    /// endpoint router, the migration hook — MUST read this and never
    /// touch `self.mode` directly.
    pub fn effective_mode(
        &self,
        auth: &AuthConfig,
    ) -> crate::infrastructure::services::opaque_service::OpaqueMode {
        use crate::infrastructure::services::opaque_service::OpaqueMode;
        if self.mode == OpaqueMode::Off {
            return OpaqueMode::Off;
        }
        if !auth.is_method_allowed(AuthMethod::Password) {
            tracing::info!(
                target: "audit",
                event = "opaque.mode_downgraded",
                reason = "password_auth_disabled",
                configured_mode = ?self.mode,
                "OXICLOUD_AUTH_OPAQUE_MODE is configured but password auth is disabled \
                 via OXICLOUD_AUTH_METHODS — treating OPAQUE as off. \
                 OPAQUE only replaces the password login path; enable password \
                 in OXICLOUD_AUTH_METHODS to make this setting take effect."
            );
            return OpaqueMode::Off;
        }
        self.mode
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
        if let Ok(v) = env::var("OXICLOUD_OIDC_AUTO_LINK_EMAIL_MATCH") {
            cfg.auto_link_email_match = v.parse::<bool>().unwrap_or(true);
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
    /// Directory for tier-1 temporary data — pure scratch, safe to
    /// lose at reboot. Backend `stream_to_tempfile` writes here so
    /// extractors that require a `&Path` (id3, mp3_duration,
    /// ffprobe, nom-exif video) can operate on a local file
    /// without the service ever seeing a raw blob path.
    ///
    /// Env: `OXICLOUD_TEMP_DIR`. Default: `std::env::temp_dir()`
    /// (respects `$TMPDIR`). On Linux this is typically `/tmp`,
    /// often mounted as tmpfs (RAM-backed); production
    /// deployments concerned about physical RAM under
    /// high concurrency should point this at a disk-backed
    /// directory (e.g. `/var/lib/oxicloud/tmp`).
    pub temp_dir: PathBuf,
    /// Server port
    pub server_port: u16,
    /// Server host
    pub server_host: String,
    /// Prometheus `/metrics` listener address, or `None` to disable.
    ///
    /// Env: `OXICLOUD_METRICS_LISTEN` (e.g. `127.0.0.1:9090`).
    /// Unset / empty = no metrics recorder is installed and no
    /// `/metrics` endpoint is bound (default). When set, a separate
    /// axum listener on this address exposes the text-format scrape
    /// — deliberately NOT merged into the main API so operators can
    /// bind to loopback / a private interface without exposing
    /// metrics publicly.
    pub metrics_listen: Option<std::net::SocketAddr>,
    /// Cache configuration
    pub cache: CacheConfig,
    /// Timeout configuration
    pub timeouts: TimeoutConfig,
    /// Resource configuration
    pub resources: ResourceConfig,
    /// Concurrency configuration
    pub concurrency: ConcurrencyConfig,
    /// Storage configuration.
    ///
    /// **Legacy flat-var surface.** Populated from `OXICLOUD_STORAGE_*`,
    /// `OXICLOUD_S3_*`, `OXICLOUD_AZURE_*`, `OXICLOUD_STORAGE_ENCRYPTION_*`.
    /// Represents the single-backend model that predates
    /// `docs/plan/storage-multi-entry.md`. When that plan lands,
    /// runtime boot reads `active_backend_name` from DB and picks
    /// an entry from [`Self::storage_entries`] instead — but existing
    /// deployments and code paths that still consult this field keep
    /// working via the legacy-synthesis fallback (if `_ENTRIES` is
    /// empty, one entry named `default` is synthesized from these
    /// flat vars and mirrored here).
    pub storage: StorageConfig,
    /// Named storage entries declared in `.env` via
    /// `OXICLOUD_STORAGE_ENTRIES=name1,name2,...` plus per-entry
    /// `OXICLOUD_STORAGE_<NAME>_*` env vars. See
    /// `docs/plan/storage-multi-entry.md`.
    ///
    /// - Empty when neither `_ENTRIES` nor any legacy flat storage
    ///   var is set (fresh install without any explicit storage
    ///   config — the default is used, matching today's behaviour).
    /// - Exactly one synthesized `default` entry when `_ENTRIES` is
    ///   unset/empty but legacy flat vars are present (upgrade
    ///   path — existing deployments keep working without touching
    ///   `.env`).
    /// - N entries when `_ENTRIES` is set — one per name.
    ///
    /// Boot / migration / consistency-audit code looks entries up
    /// by name in this vec. Order is preserved from `_ENTRIES` for
    /// the "no active pointer yet, pick the first one" fallback.
    pub storage_entries: Vec<NamedStorageEntry>,
    /// Database configuration
    pub database: DatabaseConfig,
    /// Authentication configuration
    pub auth: AuthConfig,
    /// OPAQUE (RFC 9807) zero-knowledge password auth configuration.
    /// Substrate only in Phase 0 — endpoints are inert until
    /// `OXICLOUD_AUTH_OPAQUE_MODE != off`.
    pub opaque: OpaqueConfig,
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
            temp_dir: env::temp_dir(),
            server_port: 8086,
            server_host: "127.0.0.1".to_string(),
            cache: CacheConfig::default(),
            timeouts: TimeoutConfig::default(),
            resources: ResourceConfig::default(),
            concurrency: ConcurrencyConfig::default(),
            storage: StorageConfig::default(),
            storage_entries: Vec::new(),
            database: DatabaseConfig::default(),
            auth: AuthConfig::default(),
            opaque: OpaqueConfig::default(),
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
            metrics_listen: None,
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

        if let Ok(temp_dir) = env::var("OXICLOUD_TEMP_DIR") {
            config.temp_dir = PathBuf::from(temp_dir);
        }

        if let Ok(server_port) = env::var("OXICLOUD_SERVER_PORT")
            && let Ok(port) = server_port.parse::<u16>()
        {
            config.server_port = port;
        }

        if let Ok(server_host) = env::var("OXICLOUD_SERVER_HOST") {
            config.server_host = server_host;
        }

        // Prometheus /metrics listener — opt-in, off by default. Empty
        // string treated the same as unset (a common bare-word `=` shape
        // in .env files). Parse failure is a fatal-shaped warning so
        // operators don't silently ship without metrics they expected.
        if let Ok(raw) = env::var("OXICLOUD_METRICS_LISTEN")
            && !raw.trim().is_empty()
        {
            match raw.parse::<std::net::SocketAddr>() {
                Ok(addr) => config.metrics_listen = Some(addr),
                Err(err) => tracing::warn!(
                    "OXICLOUD_METRICS_LISTEN={raw:?} is not a valid socket address ({err}) \
                     — metrics endpoint will NOT be exposed"
                ),
            }
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
            // Fail-fast on operator error: an unknown token, an empty
            // allowlist, or a listed method whose infrastructure isn't
            // wired all indicate a misconfiguration that would silently
            // change auth surface behaviour (per memory
            // `feedback_fail_fast_config`: boot panic > silent skip
            // for anything a mistyped env var could break).
            let mut methods: Vec<AuthMethod> = Vec::new();
            for raw in v.split(',') {
                let token = raw.trim();
                if token.is_empty() {
                    continue;
                }
                match AuthMethod::parse(token) {
                    Some(m) => methods.push(m),
                    None => panic!(
                        "OXICLOUD_AUTH_METHODS: unknown token '{}' — expected any of: \
                         password, magic_link, oidc",
                        token
                    ),
                }
            }
            if methods.is_empty() {
                panic!(
                    "OXICLOUD_AUTH_METHODS is set to '{}' but produced an empty allowlist. \
                     Either unset the variable (default = password, magic_link) or list at \
                     least one method (password, magic_link, oidc).",
                    v
                );
            }
            // Cross-validation A: `oidc` in the allowlist requires OIDC
            // to be enabled. Otherwise the login page would advertise a
            // method the server can't actually serve.
            let oidc_env_enabled = env::var("OXICLOUD_OIDC_ENABLED")
                .ok()
                .and_then(|s| s.parse::<bool>().ok())
                .unwrap_or(false);
            if methods.contains(&AuthMethod::Oidc) && !oidc_env_enabled {
                panic!(
                    "OXICLOUD_AUTH_METHODS includes 'oidc' but OXICLOUD_OIDC_ENABLED \
                     is not 'true'. Either set OXICLOUD_OIDC_ENABLED=true (plus \
                     OXICLOUD_OIDC_ISSUER_URL / OXICLOUD_OIDC_CLIENT_ID / \
                     OXICLOUD_OIDC_CLIENT_SECRET), or configure OIDC via the admin \
                     panel and drop 'oidc' from OXICLOUD_AUTH_METHODS until it's ready."
                );
            }

            // Cross-validation B: the reverse — OIDC enabled but the
            // admin's explicit AUTH_METHODS list doesn't include `oidc`.
            // Today: warn loudly (soft mismatch). PLANNED for the next
            // major release: escalate to a fail-fast panic to match the
            // symmetric cross-validation A above. The current loose
            // behaviour silently serves OIDC in addition to what
            // AUTH_METHODS lists — the enabled flag wins — which
            // contradicts the "AUTH_METHODS is the authoritative
            // allowlist" mental model.
            if !methods.contains(&AuthMethod::Oidc) && oidc_env_enabled {
                eprintln!(
                    "⚠️  OXICLOUD_AUTH_METHODS excludes 'oidc' but \
                     OXICLOUD_OIDC_ENABLED=true — OIDC will be served \
                     regardless. Add 'oidc' to OXICLOUD_AUTH_METHODS to \
                     make the allowlist authoritative, or set \
                     OXICLOUD_OIDC_ENABLED=false to exclude OIDC. \
                     A future release will escalate this to a fatal boot error."
                );
            }
            config.auth.allowed_auth_methods = methods;
        }

        // Legacy alias: OXICLOUD_OIDC_DISABLE_PASSWORD_LOGIN=true still
        // removes Password from the allowlist. Its main handling in the
        // OIDC config block below is preserved for the `login_options`
        // response; this line makes the effect apply uniformly through
        // `is_method_allowed(Password)` so services don't need to check
        // both flags.
        //
        // Deprecated in favour of the composable `OXICLOUD_AUTH_METHODS=oidc`
        // allowlist which handles the same SSO-only intent alongside the
        // AUTH_POLICIES vector. Warn every time the env var is observed so
        // operators migrating a config from a pre-AUTH_METHODS release see
        // the recommendation on the first boot after upgrade. Removal is
        // slated for the next major release; the setting continues to work
        // until then to avoid breaking existing deployments.
        if let Ok(v) = env::var("OXICLOUD_OIDC_DISABLE_PASSWORD_LOGIN") {
            let parsed = v.parse::<bool>().unwrap_or(false);
            tracing::warn!(
                "OXICLOUD_OIDC_DISABLE_PASSWORD_LOGIN is DEPRECATED and will \
                 be removed in a future major release. Use \
                 `OXICLOUD_AUTH_METHODS=oidc` instead (add \
                 `OXICLOUD_AUTH_POLICIES=auto_redirect_if_standalone_oidc` \
                 to also enable server-side /login redirect). \
                 Current value: {} — {}",
                v,
                if parsed {
                    "password login is disabled"
                } else {
                    "no effect (value must be `true` to take effect)"
                },
            );
            if parsed {
                config
                    .auth
                    .allowed_auth_methods
                    .retain(|m| *m != AuthMethod::Password);
            }
        }

        if let Ok(v) = env::var("OXICLOUD_DPOP_MODE") {
            match DpopMode::from_env_str(&v) {
                Some(mode) => config.auth.dpop_mode = mode,
                None => panic!(
                    "OXICLOUD_DPOP_MODE={v:?} — expected one of off / opportunistic / required"
                ),
            }
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

        // Multi-entry storage config (docs/plan/storage-multi-entry.md).
        // Parsed here so a bad config aborts boot with a clear error
        // message before any downstream service tries to use it.
        // Legacy synthesis is folded into the same call — when
        // `_ENTRIES` is unset AND legacy flat vars are present, we get
        // back a one-element vec named `default`. The flat-var block
        // below still populates the legacy `config.storage.*` fields
        // for any code path that hasn't migrated to reading from
        // `storage_entries` yet — the two live side by side until
        // slice 2 flips the boot backend to read from entries.
        config.storage_entries = parse_storage_entries().unwrap_or_else(|e| {
            panic!("Invalid storage configuration in environment: {e}");
        });
        log_storage_encryption_summary(&config.storage_entries);

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
        if let Ok(v) = env::var("OXICLOUD_OIDC_AUTO_LINK_EMAIL_MATCH") {
            config.oidc.auto_link_email_match = v.parse::<bool>().unwrap_or(true);
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

        // OPAQUE aPAKE — env-driven substrate wired via its own loader so the
        // AppConfig::from_env body doesn't have to know the internals of the
        // new mode enum / KSF param triple. See `OpaqueConfig::from_env`.
        config.opaque = OpaqueConfig::from_env();

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

    // ── Multi-entry storage config parser tests
    //
    // These tests mutate process env. Cargo runs tests in parallel by
    // default, so a shared Mutex serialises the env-touching sections.
    // Every test acquires the guard, wipes the entire storage-related
    // env surface, applies its own fixture, calls parse_storage_entries,
    // and drops out. State is scrubbed by the pre-test wipe rather than
    // any post-test cleanup — safer against a panic mid-test.
    mod storage_entry_parser {
        use super::*;
        use std::sync::Mutex;

        static ENV_GUARD: Mutex<()> = Mutex::new(());

        /// Every env var the parser reads. Wiped before each test so
        /// leftover state from another test (or from the developer's
        /// shell) can't influence the result.
        const ALL_PARSER_VARS: &[&str] = &[
            "OXICLOUD_STORAGE_ENTRIES",
            "OXICLOUD_STORAGE_PATH",
            // Legacy flat vars
            "OXICLOUD_STORAGE_BACKEND",
            "OXICLOUD_S3_ENDPOINT_URL",
            "OXICLOUD_S3_BUCKET",
            "OXICLOUD_S3_REGION",
            "OXICLOUD_S3_ACCESS_KEY",
            "OXICLOUD_S3_SECRET_KEY",
            "OXICLOUD_S3_FORCE_PATH_STYLE",
            "OXICLOUD_AZURE_ACCOUNT_NAME",
            "OXICLOUD_AZURE_ACCOUNT_KEY",
            "OXICLOUD_AZURE_CONTAINER",
            "OXICLOUD_AZURE_SAS_TOKEN",
            "OXICLOUD_AZURE_ENDPOINT_URL",
            "OXICLOUD_STORAGE_ENCRYPTION_ENABLED",
            "OXICLOUD_STORAGE_ENCRYPTION_KEY",
            // Common per-entry vars used in these tests
            "OXICLOUD_STORAGE_local_main_BACKEND",
            "OXICLOUD_STORAGE_local_main_ROOT_DIR",
            "OXICLOUD_STORAGE_local_main_ENCRYPTION_KEY",
            "OXICLOUD_STORAGE_s3_prod_BACKEND",
            "OXICLOUD_STORAGE_s3_prod_S3_BUCKET",
            "OXICLOUD_STORAGE_s3_prod_S3_ENDPOINT_URL",
            "OXICLOUD_STORAGE_s3_prod_S3_REGION",
            "OXICLOUD_STORAGE_s3_prod_S3_ACCESS_KEY",
            "OXICLOUD_STORAGE_s3_prod_S3_SECRET_KEY",
            "OXICLOUD_STORAGE_s3_prod_S3_FORCE_PATH_STYLE",
            "OXICLOUD_STORAGE_s3_prod_ENCRYPTION_KEY",
            "OXICLOUD_STORAGE_foo_BACKEND",
            "OXICLOUD_STORAGE_bar_BACKEND",
            "OXICLOUD_STORAGE_bar_S3_BUCKET",
        ];

        fn wipe_env() {
            for k in ALL_PARSER_VARS {
                // SAFETY: the ENV_GUARD mutex serialises all env
                // mutation inside this module; the outer test harness
                // makes no other calls to `set_var`/`remove_var` from
                // concurrent threads for these keys.
                unsafe { env::remove_var(k) };
            }
        }

        fn set(k: &str, v: &str) {
            unsafe { env::set_var(k, v) };
        }

        /// Base64 of 32 zero bytes — a syntactically-valid AES-256 key.
        const VALID_KEY_B64: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=";

        // ── Cell (No, No) — no entries, no legacy vars

        #[test]
        fn empty_no_legacy_returns_empty_vec() {
            let _g = ENV_GUARD.lock().unwrap_or_else(|p| p.into_inner());
            wipe_env();
            assert_eq!(parse_storage_entries().unwrap(), vec![]);
        }

        // ── Cell (No, Yes) — legacy synthesis

        #[test]
        fn legacy_local_synthesizes_default_entry() {
            let _g = ENV_GUARD.lock().unwrap_or_else(|p| p.into_inner());
            wipe_env();
            set("OXICLOUD_STORAGE_BACKEND", "local");
            set("OXICLOUD_STORAGE_PATH", "/data");
            let entries = parse_storage_entries().unwrap();
            assert_eq!(entries.len(), 1);
            assert_eq!(entries[0].name, "default");
            assert_eq!(entries[0].backend, StorageBackendType::Local);
            assert_eq!(entries[0].root_dir.as_deref(), Some("/data"));
            assert!(entries[0].s3.is_none());
            assert!(entries[0].encryption.is_none());
        }

        #[test]
        fn legacy_s3_synthesizes_default_entry() {
            let _g = ENV_GUARD.lock().unwrap_or_else(|p| p.into_inner());
            wipe_env();
            set("OXICLOUD_STORAGE_BACKEND", "s3");
            set("OXICLOUD_S3_BUCKET", "my-bucket");
            set("OXICLOUD_S3_REGION", "eu-west-1");
            let entries = parse_storage_entries().unwrap();
            assert_eq!(entries.len(), 1);
            assert_eq!(entries[0].name, "default");
            assert_eq!(entries[0].backend, StorageBackendType::S3);
            let s3 = entries[0].s3.as_ref().unwrap();
            assert_eq!(s3.bucket, "my-bucket");
            assert_eq!(s3.region, "eu-west-1");
        }

        #[test]
        fn legacy_encryption_key_carries_into_synthesized_default() {
            let _g = ENV_GUARD.lock().unwrap_or_else(|p| p.into_inner());
            wipe_env();
            // Only encryption var set — counts as legacy present, so
            // synthesis fires. Backend defaults to Local.
            set("OXICLOUD_STORAGE_ENCRYPTION_KEY", VALID_KEY_B64);
            let entries = parse_storage_entries().unwrap();
            assert_eq!(entries.len(), 1);
            // Legacy flat-var path routes through `parse_encryption_pair_list`
            // and produces a 1-pair `aes-256-gcm:<K>` list.
            let pairs = entries[0].encryption.as_ref().expect("expected pair list");
            assert_eq!(pairs.len(), 1);
            assert_eq!(pairs[0].cipher, CipherKind::AesGcm256);
            assert_eq!(pairs[0].key_material, Some([0u8; 32]));
        }

        #[test]
        fn legacy_s3_without_bucket_fails() {
            let _g = ENV_GUARD.lock().unwrap_or_else(|p| p.into_inner());
            wipe_env();
            set("OXICLOUD_STORAGE_BACKEND", "s3");
            let err = parse_storage_entries().unwrap_err();
            assert!(err.contains("OXICLOUD_S3_BUCKET"), "err was: {err}");
        }

        // ── Cell (Yes, No) — declared entries

        #[test]
        fn two_entries_local_and_s3() {
            let _g = ENV_GUARD.lock().unwrap_or_else(|p| p.into_inner());
            wipe_env();
            set("OXICLOUD_STORAGE_ENTRIES", "local_main,s3_prod");
            set("OXICLOUD_STORAGE_local_main_BACKEND", "local");
            set("OXICLOUD_STORAGE_local_main_ROOT_DIR", "/srv/oxicloud");
            set("OXICLOUD_STORAGE_s3_prod_BACKEND", "s3");
            set("OXICLOUD_STORAGE_s3_prod_S3_BUCKET", "prod-bucket");

            let entries = parse_storage_entries().unwrap();
            assert_eq!(entries.len(), 2);
            assert_eq!(entries[0].name, "local_main");
            assert_eq!(entries[0].backend, StorageBackendType::Local);
            assert_eq!(entries[0].root_dir.as_deref(), Some("/srv/oxicloud"));
            assert_eq!(entries[1].name, "s3_prod");
            assert_eq!(entries[1].backend, StorageBackendType::S3);
            assert_eq!(entries[1].s3.as_ref().unwrap().bucket, "prod-bucket");
        }

        #[test]
        fn entries_order_preserved_from_env() {
            let _g = ENV_GUARD.lock().unwrap_or_else(|p| p.into_inner());
            wipe_env();
            set("OXICLOUD_STORAGE_ENTRIES", "bar,foo");
            set("OXICLOUD_STORAGE_bar_BACKEND", "s3");
            set("OXICLOUD_STORAGE_bar_S3_BUCKET", "b");
            set("OXICLOUD_STORAGE_foo_BACKEND", "local");
            let entries = parse_storage_entries().unwrap();
            assert_eq!(entries[0].name, "bar");
            assert_eq!(entries[1].name, "foo");
        }

        #[test]
        fn per_entry_encryption_key_parsed_and_kept() {
            let _g = ENV_GUARD.lock().unwrap_or_else(|p| p.into_inner());
            wipe_env();
            set("OXICLOUD_STORAGE_ENTRIES", "local_main");
            set("OXICLOUD_STORAGE_local_main_BACKEND", "local");
            set("OXICLOUD_STORAGE_local_main_ENCRYPTION_KEY", VALID_KEY_B64);
            let entries = parse_storage_entries().unwrap();
            assert_eq!(entries.len(), 1);
            let pairs = entries[0].encryption.as_ref().expect("expected pair list");
            assert_eq!(pairs.len(), 1);
            assert_eq!(pairs[0].cipher, CipherKind::AesGcm256);
            assert_eq!(pairs[0].key_material, Some([0u8; 32]));
        }

        // ── Cell (Yes, Yes) — fail fast on conflict

        #[test]
        fn entries_plus_legacy_vars_fails_and_lists_conflicts() {
            let _g = ENV_GUARD.lock().unwrap_or_else(|p| p.into_inner());
            wipe_env();
            set("OXICLOUD_STORAGE_ENTRIES", "local_main");
            set("OXICLOUD_STORAGE_local_main_BACKEND", "local");
            set("OXICLOUD_STORAGE_BACKEND", "s3");
            set("OXICLOUD_S3_BUCKET", "stale-bucket");
            let err = parse_storage_entries().unwrap_err();
            assert!(err.contains("OXICLOUD_STORAGE_ENTRIES"), "err was: {err}");
            assert!(err.contains("OXICLOUD_STORAGE_BACKEND"), "err was: {err}");
            assert!(err.contains("OXICLOUD_S3_BUCKET"), "err was: {err}");
        }

        // ── Name validation

        #[test]
        fn uppercase_name_rejected() {
            let _g = ENV_GUARD.lock().unwrap_or_else(|p| p.into_inner());
            wipe_env();
            set("OXICLOUD_STORAGE_ENTRIES", "S3prod");
            let err = parse_storage_entries().unwrap_err();
            assert!(err.contains("S3prod"), "err was: {err}");
        }

        #[test]
        fn empty_name_between_commas_rejected() {
            let _g = ENV_GUARD.lock().unwrap_or_else(|p| p.into_inner());
            wipe_env();
            set("OXICLOUD_STORAGE_ENTRIES", "foo,,bar");
            let err = parse_storage_entries().unwrap_err();
            assert!(err.contains("empty name"), "err was: {err}");
        }

        #[test]
        fn over_length_name_rejected() {
            let _g = ENV_GUARD.lock().unwrap_or_else(|p| p.into_inner());
            wipe_env();
            let too_long = "a".repeat(33);
            set("OXICLOUD_STORAGE_ENTRIES", &too_long);
            let err = parse_storage_entries().unwrap_err();
            assert!(err.contains(&too_long), "err was: {err}");
        }

        #[test]
        fn duplicate_names_rejected() {
            let _g = ENV_GUARD.lock().unwrap_or_else(|p| p.into_inner());
            wipe_env();
            set("OXICLOUD_STORAGE_ENTRIES", "foo,foo");
            let err = parse_storage_entries().unwrap_err();
            assert!(err.contains("duplicate name"), "err was: {err}");
            assert!(err.contains("foo"), "err was: {err}");
        }

        // ── Per-entry field validation

        #[test]
        fn missing_backend_for_declared_entry_rejected() {
            let _g = ENV_GUARD.lock().unwrap_or_else(|p| p.into_inner());
            wipe_env();
            set("OXICLOUD_STORAGE_ENTRIES", "foo");
            let err = parse_storage_entries().unwrap_err();
            assert!(
                err.contains("OXICLOUD_STORAGE_foo_BACKEND"),
                "err was: {err}"
            );
        }

        #[test]
        fn s3_entry_missing_bucket_rejected() {
            let _g = ENV_GUARD.lock().unwrap_or_else(|p| p.into_inner());
            wipe_env();
            set("OXICLOUD_STORAGE_ENTRIES", "s3_prod");
            set("OXICLOUD_STORAGE_s3_prod_BACKEND", "s3");
            let err = parse_storage_entries().unwrap_err();
            assert!(
                err.contains("OXICLOUD_STORAGE_s3_prod_S3_BUCKET"),
                "err was: {err}"
            );
        }

        #[test]
        fn unknown_backend_type_rejected() {
            let _g = ENV_GUARD.lock().unwrap_or_else(|p| p.into_inner());
            wipe_env();
            set("OXICLOUD_STORAGE_ENTRIES", "foo");
            set("OXICLOUD_STORAGE_foo_BACKEND", "gcs");
            let err = parse_storage_entries().unwrap_err();
            assert!(err.contains("gcs"), "err was: {err}");
            assert!(err.contains("local, s3, azure"), "err was: {err}");
        }

        // ── Encryption key validation

        #[test]
        fn invalid_base64_encryption_key_rejected() {
            let _g = ENV_GUARD.lock().unwrap_or_else(|p| p.into_inner());
            wipe_env();
            set("OXICLOUD_STORAGE_ENTRIES", "local_main");
            set("OXICLOUD_STORAGE_local_main_BACKEND", "local");
            set(
                "OXICLOUD_STORAGE_local_main_ENCRYPTION_KEY",
                "!!!not-base64!!!",
            );
            let err = parse_storage_entries().unwrap_err();
            assert!(
                err.contains("OXICLOUD_STORAGE_local_main_ENCRYPTION_KEY"),
                "err was: {err}"
            );
            assert!(err.contains("base64"), "err was: {err}");
        }

        #[test]
        fn wrong_length_encryption_key_rejected() {
            let _g = ENV_GUARD.lock().unwrap_or_else(|p| p.into_inner());
            wipe_env();
            set("OXICLOUD_STORAGE_ENTRIES", "local_main");
            set("OXICLOUD_STORAGE_local_main_BACKEND", "local");
            // "AAAA" decodes to 3 bytes — valid base64, wrong length.
            set("OXICLOUD_STORAGE_local_main_ENCRYPTION_KEY", "AAAA");
            let err = parse_storage_entries().unwrap_err();
            assert!(err.contains("32 bytes"), "err was: {err}");
        }
    }

    impl PartialEq for NamedStorageEntry {
        fn eq(&self, other: &Self) -> bool {
            self.name == other.name && self.backend == other.backend
        }
    }

    // ─────────────────────────────────────────────────────────────
    // K1: pair-list encryption parser tests.
    //
    // Pure function; no env-var seeding needed. Table-driven where
    // the shape allows, individual tests where the error message is
    // load-bearing.
    // ─────────────────────────────────────────────────────────────
    mod pair_list_parser {
        use super::*;

        /// 32-byte base64 string, deterministic across tests. Two
        /// distinct valid keys for multi-pair tests.
        const K1_B64: &str = "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8="; // 0x00..0x1F
        const K2_B64: &str = "IHwgP2AVE1E6VwlbT8BjSggJc9OjNXJDKf8bF19HYPU="; // random

        #[test]
        fn single_key_no_cipher_prefix_defaults_to_aes_gcm() {
            let pairs = parse_encryption_pair_list("t", K1_B64).unwrap();
            assert_eq!(pairs.len(), 1);
            assert_eq!(pairs[0].cipher, CipherKind::AesGcm256);
            assert!(pairs[0].key_material.is_some());
        }

        #[test]
        fn single_key_with_explicit_cipher_prefix() {
            let pairs = parse_encryption_pair_list("t", &format!("aes-256-gcm:{K1_B64}")).unwrap();
            assert_eq!(pairs.len(), 1);
            assert_eq!(pairs[0].cipher, CipherKind::AesGcm256);
        }

        #[test]
        fn cipher_prefix_is_case_insensitive() {
            for tok in [
                "aes-256-gcm",
                "AES-256-GCM",
                "Aes-256-Gcm",
                "aes256gcm",
                "AES256GCM",
            ] {
                let pairs = parse_encryption_pair_list("t", &format!("{tok}:{K1_B64}")).unwrap();
                assert_eq!(pairs.len(), 1, "failed on token `{tok}`");
                assert_eq!(pairs[0].cipher, CipherKind::AesGcm256);
            }
        }

        #[test]
        fn two_pair_rotation_last_wins_on_writes() {
            let raw = format!("aes-256-gcm:{K1_B64},aes-256-gcm:{K2_B64}");
            let pairs = parse_encryption_pair_list("t", &raw).unwrap();
            assert_eq!(pairs.len(), 2);
            // Head pair (the write pair) is the LAST one — this test
            // pins that invariant. When K2 wires the head-pair
            // helpers, `pairs.last()` MUST resolve to K2's material.
            let head = pairs.last().unwrap();
            assert_eq!(head.cipher, CipherKind::AesGcm256);
            // Materials differ.
            assert_ne!(pairs[0].key_material, pairs[1].key_material);
        }

        #[test]
        fn none_alone_is_legal_and_equivalent_to_unencrypted() {
            let pairs = parse_encryption_pair_list("t", "none:").unwrap();
            assert_eq!(pairs.len(), 1);
            assert_eq!(pairs[0].cipher, CipherKind::None);
            assert!(pairs[0].key_material.is_none());
        }

        #[test]
        fn none_first_then_aes_is_encrypt_migration_shape() {
            // `none:,aes:K` — head is aes, writes now encrypt. Legacy
            // plaintext blobs still read via the `none` pair while the
            // rotation job walks them.
            let raw = format!("none:,aes-256-gcm:{K1_B64}");
            let pairs = parse_encryption_pair_list("t", &raw).unwrap();
            assert_eq!(pairs.len(), 2);
            assert_eq!(pairs[0].cipher, CipherKind::None);
            assert_eq!(pairs[1].cipher, CipherKind::AesGcm256);
            assert!(pairs[1].key_material.is_some());
        }

        #[test]
        fn aes_first_then_none_is_decrypt_migration_shape() {
            // `aes:K,none:` — head is none, writes now produce plaintext.
            let raw = format!("aes-256-gcm:{K1_B64},none:");
            let pairs = parse_encryption_pair_list("t", &raw).unwrap();
            assert_eq!(pairs.len(), 2);
            assert_eq!(pairs[0].cipher, CipherKind::AesGcm256);
            assert_eq!(pairs[1].cipher, CipherKind::None);
        }

        #[test]
        fn whitespace_around_separators_is_tolerated() {
            let raw = format!(" aes-256-gcm : {K1_B64} , none: ");
            let pairs = parse_encryption_pair_list("t", &raw).unwrap();
            assert_eq!(pairs.len(), 2);
            assert_eq!(pairs[0].cipher, CipherKind::AesGcm256);
            assert_eq!(pairs[1].cipher, CipherKind::None);
        }

        #[test]
        fn empty_input_rejected() {
            for raw in ["", "   ", "\t\n "] {
                let err = parse_encryption_pair_list("t", raw).unwrap_err();
                assert!(err.contains("empty"), "raw={raw:?} err={err}");
            }
        }

        #[test]
        fn leading_or_trailing_comma_rejected() {
            for raw in [
                format!(",aes-256-gcm:{K1_B64}"),
                format!("aes-256-gcm:{K1_B64},"),
                format!("aes-256-gcm:{K1_B64},,aes-256-gcm:{K2_B64}"),
            ] {
                let err = parse_encryption_pair_list("t", &raw).unwrap_err();
                assert!(err.contains("empty pair"), "raw={raw:?} err={err}");
            }
        }

        #[test]
        fn unknown_cipher_rejected() {
            let err = parse_encryption_pair_list("t", &format!("chacha20:{K1_B64}")).unwrap_err();
            assert!(err.contains("unknown cipher"), "err was: {err}");
            assert!(err.contains("chacha20"), "err was: {err}");
        }

        #[test]
        fn none_with_material_rejected() {
            // `none:<something>` — nonsense. Must be `none:` (empty
            // material after the colon).
            let err = parse_encryption_pair_list("t", &format!("none:{K1_B64}")).unwrap_err();
            assert!(
                err.contains("cipher `none` but has key material"),
                "err was: {err}"
            );
        }

        #[test]
        fn multiple_none_pairs_rejected() {
            let err = parse_encryption_pair_list("t", "none:,none:").unwrap_err();
            assert!(err.contains("more than one `none` pair"), "err was: {err}");
        }

        #[test]
        fn empty_material_for_real_cipher_rejected() {
            let err = parse_encryption_pair_list("t", "aes-256-gcm:").unwrap_err();
            assert!(err.contains("empty key material"), "err was: {err}");
        }

        #[test]
        fn non_base64_key_rejected() {
            let err = parse_encryption_pair_list("t", "aes-256-gcm:not_base64!!").unwrap_err();
            assert!(err.contains("not valid base64"), "err was: {err}");
        }

        #[test]
        fn wrong_length_key_rejected() {
            // "AAAA" decodes to 3 bytes — valid base64, wrong length.
            let err = parse_encryption_pair_list("t", "aes-256-gcm:AAAA").unwrap_err();
            assert!(err.contains("32 bytes"), "err was: {err}");
        }

        #[test]
        fn duplicate_key_material_rejected() {
            let raw = format!("aes-256-gcm:{K1_B64},aes-256-gcm:{K1_B64}");
            let err = parse_encryption_pair_list("t", &raw).unwrap_err();
            assert!(err.contains("same key material twice"), "err was: {err}");
        }

        #[test]
        fn entry_name_appears_in_error_message() {
            let err = parse_encryption_pair_list("s3_prod", "").unwrap_err();
            assert!(err.contains("s3_prod"), "err was: {err}");
        }

        #[test]
        fn fingerprint_is_ssh_style_colon_hex_for_real_cipher_and_none_for_none() {
            // K3.7: display fp switched from 12-char raw hex to
            // SSH-style 8-byte colon-hex (16 hex + 7 colons = 23 chars)
            // so operators can cross-reference against the v1 header's
            // `<key_fp>` field + `backend_rotate`'s `head_key_fp`
            // output + the `oxicloud --fingerprint` CLI.
            let pairs =
                parse_encryption_pair_list("t", &format!("aes-256-gcm:{K1_B64},none:")).unwrap();
            let fp0 = pairs[0].fingerprint_short().unwrap();
            assert_eq!(
                fp0.len(),
                23,
                "expected xx:yy:… shape (23 chars), got {fp0:?}"
            );
            assert_eq!(fp0.matches(':').count(), 7);
            assert!(fp0.chars().all(|c| c == ':' || c.is_ascii_hexdigit()));
            assert!(pairs[1].fingerprint_short().is_none());
        }

        #[test]
        fn fingerprint_stable_across_calls() {
            let pairs_a = parse_encryption_pair_list("t", K1_B64).unwrap();
            let pairs_b = parse_encryption_pair_list("t", K1_B64).unwrap();
            assert_eq!(
                pairs_a[0].fingerprint_short(),
                pairs_b[0].fingerprint_short()
            );
        }

        #[test]
        fn fingerprint_differs_between_different_keys() {
            let pairs = parse_encryption_pair_list(
                "t",
                &format!("aes-256-gcm:{K1_B64},aes-256-gcm:{K2_B64}"),
            )
            .unwrap();
            assert_ne!(pairs[0].fingerprint_short(), pairs[1].fingerprint_short());
        }

        // ── key_fp (8-byte header field) tests ────────────────────

        #[test]
        fn key_fp_is_eight_bytes() {
            let pairs = parse_encryption_pair_list("t", K1_B64).unwrap();
            let fp = pairs[0].key_fp();
            assert_eq!(fp.len(), 8);
            // At least one byte non-zero (K1 = 0x00..0x1F sha256 has
            // ample entropy; if this ever asserts we've got a truly
            // improbable collision).
            assert!(fp.iter().any(|b| *b != 0), "fp = {fp:?}");
        }

        #[test]
        fn key_fp_zero_for_none_cipher() {
            let pairs = parse_encryption_pair_list("t", "none:").unwrap();
            assert_eq!(pairs[0].key_fp(), [0u8; 8]);
        }

        #[test]
        fn key_fp_stable_across_calls() {
            let pairs_a = parse_encryption_pair_list("t", K1_B64).unwrap();
            let pairs_b = parse_encryption_pair_list("t", K1_B64).unwrap();
            assert_eq!(pairs_a[0].key_fp(), pairs_b[0].key_fp());
        }

        #[test]
        fn key_fp_differs_between_different_keys() {
            let pairs = parse_encryption_pair_list(
                "t",
                &format!("aes-256-gcm:{K1_B64},aes-256-gcm:{K2_B64}"),
            )
            .unwrap();
            assert_ne!(pairs[0].key_fp(), pairs[1].key_fp());
        }

        #[test]
        fn key_fp_and_fingerprint_short_are_the_same_underlying_bytes() {
            // K3.7 unified: both render the FIRST 8 bytes of
            // sha256(key). `key_fp` returns them raw for the header;
            // `fingerprint_short` renders them as colon-hex for
            // display. Stripping the colons from the display form
            // should match `hex::encode(key_fp)` exactly. Pinning
            // this alignment protects against a future refactor that
            // silently switches one to a different truncation.
            let pairs = parse_encryption_pair_list("t", K1_B64).unwrap();
            let display_fp = pairs[0].fingerprint_short().unwrap();
            let raw_fp = pairs[0].key_fp();
            let display_stripped: String = display_fp.chars().filter(|c| *c != ':').collect();
            assert_eq!(display_stripped, hex::encode(raw_fp));
        }

        // ── `fingerprint_from_base64_key` (CLI helper) ───────────────

        #[test]
        fn fingerprint_from_base64_key_all_zero_key() {
            // 32 zero bytes → deterministic sha256; the first 8 bytes
            // truncation is the SSH-style prefix that ships everywhere
            // (v1 header, rotate output, boot log, CLI).
            let fp = fingerprint_from_base64_key("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=")
                .unwrap();
            assert_eq!(fp, "66:68:7a:ad:f8:62:bd:77");
        }

        #[test]
        fn fingerprint_from_base64_key_wrong_length_rejected() {
            // "AAAA" base64-decodes to 3 bytes, not 32.
            let err = fingerprint_from_base64_key("AAAA").unwrap_err();
            assert!(err.contains("32"), "err was: {err}");
        }

        #[test]
        fn fingerprint_from_base64_key_non_base64_rejected() {
            let err = fingerprint_from_base64_key("not@base64!").unwrap_err();
            assert!(err.contains("base64"), "err was: {err}");
        }

        #[test]
        fn fingerprint_from_base64_key_matches_pair_list_fp() {
            // Passing the same key material through both paths — the
            // CLI helper and the pair-list parser — MUST yield the
            // same fingerprint. Guards against a future refactor that
            // silently switches truncation or hash between the two
            // consumers.
            let cli_fp = fingerprint_from_base64_key(K1_B64).unwrap();
            let pairs = parse_encryption_pair_list("t", K1_B64).unwrap();
            let parser_fp = pairs[0].fingerprint_short().unwrap();
            assert_eq!(cli_fp, parser_fp);
        }
    }

    // ── OPAQUE effective-mode cross-check ────────────────────────────────
    //
    // OPAQUE is fundamentally a password mechanism; enabling its mode when
    // password auth is disabled would be a no-op that still nagged
    // operators for `OXICLOUD_AUTH_OPAQUE_SERVER_SETUP` at boot. The
    // `effective_mode` helper resolves that quietly by downgrading to
    // Off + emitting an audit log, and these tests pin the truth table.

    fn auth_with_methods(methods: Vec<AuthMethod>) -> AuthConfig {
        AuthConfig {
            allowed_auth_methods: methods,
            ..AuthConfig::default()
        }
    }

    #[test]
    fn effective_mode_stays_off_when_configured_off() {
        use crate::infrastructure::services::opaque_service::OpaqueMode;
        let opaque = OpaqueConfig::default(); // mode = Off
        let auth = auth_with_methods(vec![AuthMethod::Password]);
        assert_eq!(opaque.effective_mode(&auth), OpaqueMode::Off);
    }

    #[test]
    fn effective_mode_passes_through_when_password_allowed() {
        use crate::infrastructure::services::opaque_service::OpaqueMode;
        for mode in [OpaqueMode::Migrate, OpaqueMode::OpaqueOnly] {
            let opaque = OpaqueConfig {
                mode,
                ..OpaqueConfig::default()
            };
            // Empty allowlist means "all methods allowed" per the existing
            // convention, so password is implicitly in.
            let empty_auth = auth_with_methods(vec![]);
            assert_eq!(opaque.effective_mode(&empty_auth), mode);
            // Explicit allowlist including Password.
            let with_password = auth_with_methods(vec![AuthMethod::Password]);
            assert_eq!(opaque.effective_mode(&with_password), mode);
            // Multi-method allowlist including Password.
            let mixed = auth_with_methods(vec![AuthMethod::Password, AuthMethod::MagicLink]);
            assert_eq!(opaque.effective_mode(&mixed), mode);
        }
    }

    #[test]
    fn effective_mode_downgrades_to_off_when_password_disabled() {
        use crate::infrastructure::services::opaque_service::OpaqueMode;
        for mode in [OpaqueMode::Migrate, OpaqueMode::OpaqueOnly] {
            let opaque = OpaqueConfig {
                mode,
                ..OpaqueConfig::default()
            };
            // Magic-link-only deployment — no password path for OPAQUE
            // to shadow, so effective mode must be Off regardless of the
            // configured value. The audit log line is a side effect we
            // don't try to assert on (tracing capture would be overkill
            // for this straightforward truth table).
            let magic_only = auth_with_methods(vec![AuthMethod::MagicLink]);
            assert_eq!(opaque.effective_mode(&magic_only), OpaqueMode::Off);
        }
    }
}
