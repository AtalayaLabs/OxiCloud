# External Storage Backends — Implementation Plan

> **Historical implementation prompt.** Its Local/S3/Azure backends, admin
> configuration, migration, cache and server-side encryption now have
> implementations. Use [the current backend architecture](docs/architecture/backend-storage.md),
> [storage consistency](docs/plan/storage-consistency.md) and `AGENTS.md` for new
> work. The paths, SQL details and failure handling below describe an earlier
> design; they are not instructions to recreate those systems. In particular,
> failed work must pause/resume rather than report successful completion.
>
> Task 4.2 describes **server-side encryption at rest**: the OxiCloud server
> holds the key and decrypts the content. It is not browser/client end-to-end
> encryption. The separate [Vault proposal](docs/plan/vault-e2ee.md) covers that
> remaining feature and is not an implemented capability.

> **Purpose**: This prompt provides Claude Code with full architectural context to implement pluggable blob storage backends (S3, Backblaze B2, MinIO, etc.) for OxiCloud, across 4 phases. All changes MUST respect the existing hexagonal architecture, BLAKE3 dedup system, and coding conventions defined in `CLAUDE.md`.

---

## Current Architecture Summary

### How blobs are stored today

- **Content-addressable**: Files hashed with BLAKE3 → stored at `.blobs/{2-char-prefix}/{hash}.blob`
- **Dedup index**: PostgreSQL `storage.blobs` table (hash PK, ref_count, size, content_type)
- **Write-first strategy**: Blob written to disk BEFORE PostgreSQL upsert (PG connection never held during disk I/O)
- **Streaming reads**: 256 KB chunks via `tokio::fs::File` + `ReaderStream`
- **Range support**: `AsyncSeekExt::seek()` + `file.take()` for HTTP Range requests

### Key files

| File | Role |
|------|------|
| `src/application/ports/dedup_ports.rs` | `DedupPort` trait — 12 methods, the hexagonal port |
| `src/infrastructure/services/dedup_service.rs` | `DedupService` struct — sole implementation of `DedupPort` |
| `src/common/di.rs` | `AppServiceFactory` — DI composition root, builds `DedupService` |
| `src/common/config.rs` | `StorageConfig`, `AppConfig` — env var loading |
| `src/interfaces/api/handlers/admin_handler.rs` | Admin API handlers (OIDC pattern to follow) |
| `src/application/services/admin_settings_service.rs` | `AdminSettingsService` — runtime settings with env override |
| `src/domain/repositories/settings_repository.rs` | `SettingsRepository` trait |
| `src/infrastructure/repositories/pg/settings_pg_repository.rs` | PostgreSQL settings impl |
| `static/admin.html` | Admin panel HTML (3 tabs: Dashboard, Users, OIDC) |
| `static/js/views/admin/admin.js` | Admin panel JS logic |

### DedupService filesystem operations (candidates for extraction)

These are the EXACT `tokio::fs` calls inside `DedupService` that must be delegated to the new `BlobStorageBackend` trait:

```
initialize()        → fs::create_dir_all (blob_root, temp_root, 256 prefix dirs)
store_from_file()   → fs::metadata, fs::try_exists, fs::rename, fs::copy, fs::remove_file
read_blob_stream()  → File::open + ReaderStream
read_blob_range_stream() → File::open + seek + take + ReaderStream
blob_size()         → fs::metadata
remove_reference()  → fs::remove_file (after PG commit)
verify_integrity()  → spawn_blocking with path checks
blob_path()         → PathBuf computation (sync)
```

### DedupService PostgreSQL operations (stay in DedupService, untouched)

```
store_from_file()   → INSERT … ON CONFLICT … RETURNING ref_count
blob_exists()       → SELECT EXISTS from storage.blobs
get_blob_metadata() → SELECT from storage.blobs
add_reference()     → UPDATE ref_count + 1
remove_reference()  → BEGIN TX → SELECT FOR UPDATE → DELETE if ref_count=0 → COMMIT
get_stats()         → SELECT COUNT, SUM from storage.blobs
verify_integrity()  → Streaming cursor SELECT from storage.blobs
```

---

## Phase 1 — Foundation (Backend Trait + Local + S3)

### Task 1.1: Create `BlobStorageBackend` trait

**File**: `src/application/ports/blob_storage_ports.rs` (NEW)

Create a minimal trait that abstracts ONLY raw byte I/O operations:

```rust
use async_trait::async_trait;
use bytes::Bytes;
use futures::Stream;
use std::path::Path;
use std::pin::Pin;
use crate::domain::errors::DomainError;

/// Health check result for storage backend connectivity
#[derive(Debug, Clone, serde::Serialize)]
pub struct StorageHealthStatus {
    pub connected: bool,
    pub backend_type: String,
    pub message: String,
    /// Optional: available space in bytes (if backend reports it)
    pub available_bytes: Option<u64>,
}

/// Minimal trait for blob byte I/O — decoupled from dedup logic.
///
/// Implementations: `LocalBlobBackend`, `S3BlobBackend`, etc.
/// DedupService owns an `Arc<dyn BlobStorageBackend>` and delegates
/// all filesystem/object-store operations through this trait.
#[async_trait]
pub trait BlobStorageBackend: Send + Sync + 'static {
    /// Initialize the backend (create directories, verify bucket access, etc.)
    async fn initialize(&self) -> Result<(), DomainError>;

    /// Store a blob from a local temporary file.
    /// The backend MUST handle the case where the blob already exists (idempotent).
    /// Returns the number of bytes stored.
    async fn put_blob(&self, hash: &str, source_path: &Path) -> Result<u64, DomainError>;

    /// Stream the full blob content.
    async fn get_blob_stream(
        &self,
        hash: &str,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<Bytes, std::io::Error>> + Send>>, DomainError>;

    /// Stream a byte range of the blob (for HTTP Range requests).
    async fn get_blob_range_stream(
        &self,
        hash: &str,
        start: u64,
        end: Option<u64>,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<Bytes, std::io::Error>> + Send>>, DomainError>;

    /// Delete a blob by hash. Must be idempotent (no error if already deleted).
    async fn delete_blob(&self, hash: &str) -> Result<(), DomainError>;

    /// Check if a blob exists in the backend.
    async fn blob_exists(&self, hash: &str) -> Result<bool, DomainError>;

    /// Get blob size in bytes without downloading content.
    async fn blob_size(&self, hash: &str) -> Result<u64, DomainError>;

    /// Verify connectivity and permissions. Used by admin "Test Connection" button.
    async fn health_check(&self) -> Result<StorageHealthStatus, DomainError>;

    /// Return the backend type name (for display in admin panel).
    fn backend_type(&self) -> &'static str;
}
```

Register in `src/application/ports/mod.rs` and `src/application/mod.rs`.

### Task 1.2: Create `LocalBlobBackend`

**File**: `src/infrastructure/services/local_blob_backend.rs` (NEW)

Extract ALL `tokio::fs` operations from `DedupService` into this struct. This is a **pure refactor** — zero behavior change.

```rust
pub struct LocalBlobBackend {
    blob_root: PathBuf,
    temp_root: PathBuf,
}
```

Methods to implement from the trait, mapping from current DedupService code:

| Trait method | Source in DedupService | Key logic |
|---|---|---|
| `initialize()` | `DedupService::initialize()` lines 115-154 | Create `.blobs/`, `.dedup_temp/`, 256 prefix dirs |
| `put_blob()` | `DedupService::store_from_file()` lines 196-253 | `fs::try_exists` → `fs::rename` (EXDEV fallback `fs::copy`) → cleanup source |
| `get_blob_stream()` | `DedupService::read_blob_stream()` lines 306-323 | `File::open` → `ReaderStream::with_capacity(256KB)` |
| `get_blob_range_stream()` | `DedupService::read_blob_range_stream()` lines 325-353 | `File::open` → `seek` → `take` → `ReaderStream` |
| `delete_blob()` | `DedupService::remove_reference()` line ~469 | `fs::remove_file` |
| `blob_exists()` | New (was inline `fs::try_exists`) | `fs::try_exists(blob_path)` |
| `blob_size()` | `DedupService::blob_size()` lines 355-369 | `fs::metadata().len()` |
| `health_check()` | New | Check blob_root is writable, return disk available via `statvfs` |
| `backend_type()` | New | Return `"local"` |

Also add a public helper:
```rust
pub fn blob_path(&self, hash: &str) -> PathBuf {
    let prefix = &hash[..2];
    self.blob_root.join(prefix).join(format!("{hash}.blob"))
}
```

Register in `src/infrastructure/services/mod.rs`.

### Task 1.3: Refactor `DedupService` to use `BlobStorageBackend`

**File**: `src/infrastructure/services/dedup_service.rs` (MODIFY)

Changes:
1. Add field: `backend: Arc<dyn BlobStorageBackend>`
2. Remove fields: `blob_root: PathBuf`, `temp_root: PathBuf` (moved to `LocalBlobBackend`)
3. Update constructor to accept `Arc<dyn BlobStorageBackend>` instead of `storage_root: &Path`
4. Replace all direct `tokio::fs` calls with `self.backend.*` calls
5. Keep `blob_path()` in DedupPort as a delegation: `self.backend.blob_path()` — BUT since `blob_path()` returns `PathBuf` and is used by thumbnails/caching services, consider adding it to the backend trait OR keeping a separate method. For S3 backends, this should return a virtual path or the method should be deprecated in favor of streaming.

**Critical**: `hash_file()` stays in `DedupService` (BLAKE3 hashing is NOT a backend concern — it always runs on local temp files before upload).

**Critical**: The write-first strategy is preserved:
```
1. hash_file() on local temp file
2. self.backend.put_blob(hash, temp_path)    ← backend moves/uploads
3. INSERT INTO storage.blobs … ON CONFLICT   ← PostgreSQL upsert
```

**Critical**: `remove_reference()` flow preserved:
```
1. BEGIN TX → SELECT FOR UPDATE → check ref_count
2. If ref_count == 1 → DELETE FROM storage.blobs → COMMIT
3. self.backend.delete_blob(hash)             ← after PG commit
```

### Task 1.4: Create `S3BlobBackend`

**File**: `src/infrastructure/services/s3_blob_backend.rs` (NEW)

**Dependency to add to `Cargo.toml`**:
```toml
aws-sdk-s3 = "1"
aws-config = { version = "1", features = ["behavior-version-latest"] }
aws-smithy-types = "1"           # For ByteStream
```

> Note: `aws-sdk-s3` is the official AWS SDK for Rust. It's compatible with ALL S3-compatible services (Backblaze B2, MinIO, Cloudflare R2, DigitalOcean Spaces, Wasabi) via custom endpoint configuration.

```rust
pub struct S3BlobBackend {
    client: aws_sdk_s3::Client,
    bucket: String,
}
```

**S3 key scheme**: Same as local — `{2-char-prefix}/{hash}.blob` (e.g., `a3/a3c5f2e8d1…blob`)

Method mapping:

| Trait method | S3 operation |
|---|---|
| `initialize()` | `head_bucket()` to verify bucket exists + permissions |
| `put_blob()` | `put_object()` with `Body::from_path(source_path)`. Check existence with `head_object()` first for idempotency |
| `get_blob_stream()` | `get_object()` → `.body.into_async_read()` → `ReaderStream` |
| `get_blob_range_stream()` | `get_object().range(format!("bytes={start}-{end}"))` → stream |
| `delete_blob()` | `delete_object()` (already idempotent in S3) |
| `blob_exists()` | `head_object()` — 200 = true, 404 = false |
| `blob_size()` | `head_object()` → `.content_length()` |
| `health_check()` | `head_bucket()` + `list_objects_v2(max_keys=1)` |
| `backend_type()` | Return `"s3"` |

**S3 Client construction**: Must support custom endpoints for non-AWS providers:

```rust
impl S3BlobBackend {
    pub async fn new(config: &S3StorageConfig) -> Result<Self, DomainError> {
        let mut s3_config_builder = aws_sdk_s3::config::Builder::new()
            .region(aws_sdk_s3::config::Region::new(config.region.clone()))
            .credentials_provider(
                aws_sdk_s3::config::Credentials::new(
                    &config.access_key,
                    &config.secret_key,
                    None, None, "oxicloud",
                )
            )
            .behavior_version_latest();

        if let Some(endpoint) = &config.endpoint_url {
            s3_config_builder = s3_config_builder
                .endpoint_url(endpoint)
                .force_path_style(config.force_path_style);
        }

        let client = aws_sdk_s3::Client::from_conf(s3_config_builder.build());
        Ok(Self { client, bucket: config.bucket.clone() })
    }
}
```

### Task 1.5: Add storage backend configuration

**File**: `src/common/config.rs` (MODIFY)

Add to existing `StorageConfig`:

```rust
#[derive(Debug, Clone)]
pub enum StorageBackendType {
    Local,
    S3,
}

#[derive(Debug, Clone)]
pub struct S3StorageConfig {
    pub endpoint_url: Option<String>,    // OXICLOUD_S3_ENDPOINT_URL
    pub bucket: String,                  // OXICLOUD_S3_BUCKET
    pub region: String,                  // OXICLOUD_S3_REGION (default: "us-east-1")
    pub access_key: String,              // OXICLOUD_S3_ACCESS_KEY
    pub secret_key: String,              // OXICLOUD_S3_SECRET_KEY
    pub force_path_style: bool,          // OXICLOUD_S3_FORCE_PATH_STYLE (default: false)
}

// Add to existing StorageConfig:
pub struct StorageConfig {
    pub root_dir: String,                // existing
    pub chunk_size: usize,               // existing
    pub parallel_threshold: usize,       // existing
    pub trash_retention_days: u32,       // existing
    pub max_upload_size: usize,          // existing
    pub backend: StorageBackendType,     // NEW — OXICLOUD_STORAGE_BACKEND (default: "local")
    pub s3: Option<S3StorageConfig>,     // NEW — populated when backend=s3
}
```

**Env var loading** in `AppConfig::from_env()`:
```
OXICLOUD_STORAGE_BACKEND     → "local" | "s3" (default: "local")
OXICLOUD_S3_ENDPOINT_URL     → Optional custom endpoint
OXICLOUD_S3_BUCKET           → Required when backend=s3
OXICLOUD_S3_REGION           → Default "us-east-1"
OXICLOUD_S3_ACCESS_KEY       → Required when backend=s3
OXICLOUD_S3_SECRET_KEY       → Required when backend=s3
OXICLOUD_S3_FORCE_PATH_STYLE → Default false
```

### Task 1.6: Wire backend selection in DI

**File**: `src/common/di.rs` (MODIFY)

In `create_core_services()`, replace the current DedupService construction:

```rust
// Build storage backend based on config
let blob_backend: Arc<dyn BlobStorageBackend> = match self.config.storage.backend {
    StorageBackendType::Local => {
        Arc::new(LocalBlobBackend::new(&self.storage_path))
    }
    StorageBackendType::S3 => {
        let s3_config = self.config.storage.s3.as_ref()
            .expect("S3 config required when backend=s3");
        Arc::new(S3BlobBackend::new(s3_config).await?)
    }
};

blob_backend.initialize().await?;

let dedup_service = Arc::new(DedupService::new(
    blob_backend.clone(),
    db_pool.clone(),
    maintenance_pool.clone(),
));
```

### Task 1.7: Handle `blob_path()` deprecation path

The `DedupPort::blob_path()` method returns a `PathBuf` and is used by:
- Thumbnail generation service (needs local file access)
- File content caching (moka cache)

For S3 backends, `blob_path()` has no meaning. Solutions:
1. **For thumbnails**: Change thumbnail service to accept a `Stream` instead of a `PathBuf`, OR download to a temp file first
2. **For caching**: Cache already works with streams
3. Keep `blob_path()` on `DedupPort` but make it return `Option<PathBuf>` (None for remote backends) — consumers must handle the None case

Search for all callers of `blob_path()` and update them.

### Task 1.8: Tests

- Unit test `LocalBlobBackend` in isolation (mock filesystem with temp dirs)
- Unit test `S3BlobBackend` with a mock S3 (use `aws-smithy-runtime` test utilities or `mockall`)
- Integration test: `DedupService` with `LocalBlobBackend` must pass ALL existing tests unchanged (this proves the refactor is correct)
- Add `#[cfg(test)]` inline tests in each new file following existing project convention

### Task 1.9: Pre-commit validation

```bash
cargo fmt --all
cargo clippy -- -D warnings
cargo test --workspace
```

ALL existing ~208 tests MUST pass. Zero regressions.

---

## Phase 2 — Admin Panel Configuration

### Task 2.1: Storage settings service

**File**: `src/application/services/storage_settings_service.rs` (NEW)

Follow the EXACT same pattern as `AdminSettingsService` for OIDC. Create `StorageSettingsService`:

```rust
pub struct StorageSettingsService {
    settings_repo: Arc<SettingsPgRepository>,
    env_storage_config: StorageConfig,       // from AppConfig at startup
}
```

**Methods** (follow OIDC pattern):

| Method | Purpose |
|---|---|
| `get_storage_settings()` | Load from DB (category: `"storage"`), mask secrets, mark env overrides |
| `save_storage_settings(dto, user_id)` | Upsert each field to `admin_settings`, mark secrets with `is_secret: true` |
| `test_storage_connection(dto)` | Build temporary backend from DTO config, call `health_check()`, return result |
| `load_effective_storage_config()` | Merge: DB settings + env var overrides + defaults |
| `get_env_overrides()` | Return list of `OXICLOUD_S3_*` / `OXICLOUD_STORAGE_*` env vars that are set |

**DB keys** (category: `"storage"`):

```
storage.backend              → "local" | "s3"
storage.s3.endpoint_url      → string (optional)
storage.s3.bucket            → string
storage.s3.region            → string
storage.s3.access_key        → string (is_secret: true)
storage.s3.secret_key        → string (is_secret: true)
storage.s3.force_path_style  → "true" | "false"
```

### Task 2.2: Storage admin API endpoints

**File**: `src/interfaces/api/handlers/admin_handler.rs` (MODIFY)

Add to `admin_routes()`:

```rust
.route("/settings/storage", get(get_storage_settings))
.route("/settings/storage", put(save_storage_settings))
.route("/settings/storage/test", post(test_storage_connection))
```

**Handler implementations** (follow OIDC handlers exactly):

| Handler | Method | Body | Response |
|---|---|---|---|
| `get_storage_settings` | GET | — | `StorageSettingsDto` (secrets masked, env_overrides listed) |
| `save_storage_settings` | PUT | `SaveStorageSettingsDto` | `{ "message": "Storage settings saved" }` |
| `test_storage_connection` | POST | `TestStorageConnectionDto` | `StorageTestResultDto { connected, message, available_bytes }` |

**DTOs** (add to `src/application/dtos/`):

```rust
#[derive(Serialize)]
pub struct StorageSettingsDto {
    pub backend: String,                     // "local" | "s3"
    pub s3_endpoint_url: Option<String>,
    pub s3_bucket: Option<String>,
    pub s3_region: Option<String>,
    pub s3_access_key_set: bool,             // masked — never send actual key
    pub s3_secret_key_set: bool,             // masked — never send actual secret
    pub s3_force_path_style: bool,
    pub env_overrides: Vec<String>,          // which fields are locked by env vars
    // Current stats
    pub current_backend: String,
    pub total_blobs: u64,
    pub total_bytes_stored: u64,
    pub dedup_ratio: f64,
}

#[derive(Deserialize)]
pub struct SaveStorageSettingsDto {
    pub backend: String,
    pub s3_endpoint_url: Option<String>,
    pub s3_bucket: Option<String>,
    pub s3_region: Option<String>,
    pub s3_access_key: Option<String>,       // only sent if changed
    pub s3_secret_key: Option<String>,       // only sent if changed
    pub s3_force_path_style: Option<bool>,
}

#[derive(Deserialize)]
pub struct TestStorageConnectionDto {
    pub backend: String,
    pub s3_endpoint_url: Option<String>,
    pub s3_bucket: Option<String>,
    pub s3_region: Option<String>,
    pub s3_access_key: Option<String>,
    pub s3_secret_key: Option<String>,
    pub s3_force_path_style: Option<bool>,
}

#[derive(Serialize)]
pub struct StorageTestResultDto {
    pub connected: bool,
    pub message: String,
    pub backend_type: String,
    pub available_bytes: Option<u64>,
}
```

### Task 2.3: Wire `StorageSettingsService` into DI

**File**: `src/common/di.rs` (MODIFY)

Add `StorageSettingsService` construction alongside `AdminSettingsService`. Add to `AppState`.

### Task 2.4: Admin Panel — Storage tab (HTML)

**File**: `static/admin.html` (MODIFY)

Add a 4th tab button after the OIDC tab:

```html
<button class="admin-tab" id="tab-btn-storage">
    <i class="fas fa-database"></i> <span data-i18n="admin.tab_storage">Storage</span>
</button>
```

Add `tab-storage` content div with:

1. **Backend selector** — Radio buttons: Local Filesystem / S3-Compatible
2. **S3 configuration form** (shown/hidden based on selector):
   - Provider preset dropdown (Amazon S3, Backblaze B2, Cloudflare R2, MinIO, DigitalOcean Spaces, Wasabi, Custom)
   - Endpoint URL field
   - Bucket field
   - Region field
   - Access Key ID field
   - Secret Key field (password type)
   - Force Path Style checkbox
   - ENV badges on fields overridden by env vars (same as OIDC)
3. **Test Connection button** → calls `POST /api/admin/settings/storage/test`
4. **Save button** → calls `PUT /api/admin/settings/storage`
5. **Current Status section**:
   - Active backend type
   - Total blobs / total size / dedup ratio (from `DedupStatsDto`)
6. **Migration section** (Phase 3 — can be placeholder with "Coming soon")

### Task 2.5: Admin Panel — Storage tab (JS)

**File**: `static/js/views/admin/admin.js` (MODIFY)

Follow OIDC tab patterns:

```javascript
// Provider presets
const STORAGE_PRESETS = {
    'aws':           { endpoint: '',                                  region: 'us-east-1',    pathStyle: false },
    'backblaze':     { endpoint: 's3.{region}.backblazeb2.com',       region: 'us-west-004',  pathStyle: false },
    'cloudflare-r2': { endpoint: '{accountId}.r2.cloudflarestorage.com', region: 'auto',      pathStyle: true  },
    'minio':         { endpoint: 'http://localhost:9000',             region: 'us-east-1',    pathStyle: true  },
    'digitalocean':  { endpoint: '{region}.digitaloceanspaces.com',   region: 'nyc3',         pathStyle: false },
    'wasabi':        { endpoint: 's3.{region}.wasabisys.com',         region: 'us-east-1',    pathStyle: false },
    'custom':        { endpoint: '',                                  region: '',             pathStyle: false },
};
```

Functions:
- `loadStorage()` — `GET /api/admin/settings/storage` → populate form
- `saveStorage()` — Collect form → `PUT /api/admin/settings/storage`
- `testStorageConnection()` — Collect form → `POST /api/admin/settings/storage/test` → show result
- `onPresetChange(preset)` — Auto-fill endpoint/region/pathStyle from preset
- `toggleS3Form(visible)` — Show/hide S3 fields when backend radio changes

### Task 2.6: i18n keys

**Files**: `static/locales/*.json` (MODIFY at minimum `en.json` and `es.json`)

Add translation keys for all new UI labels:
```
admin.tab_storage
admin.storage_backend
admin.storage_local
admin.storage_s3
admin.storage_provider_preset
admin.storage_endpoint_url
admin.storage_bucket
admin.storage_region
admin.storage_access_key
admin.storage_secret_key
admin.storage_path_style
admin.storage_test_connection
admin.storage_test_success
admin.storage_test_failure
admin.storage_save
admin.storage_saved
admin.storage_current_status
admin.storage_total_blobs
admin.storage_total_size
admin.storage_dedup_ratio
admin.storage_migration
admin.storage_migration_coming_soon
```

### Task 2.7: CSS for storage tab

**File**: `static/css/admin.css` (MODIFY)

Add styles for:
- `.storage-backend-selector` — Radio button group
- `.storage-provider-presets` — Dropdown styling
- `.storage-form` — Form fields (reuse existing OIDC form patterns)
- `.storage-status-grid` — Stats display
- BEM methodology, CSS custom properties for colors, no raw hex/rgb

---

## Phase 3 — Migration Between Backends

### Task 3.1: `MigrationBlobBackend` wrapper

**File**: `src/infrastructure/services/migration_blob_backend.rs` (NEW)

A `BlobStorageBackend` decorator that enables zero-downtime migration between backends:

```rust
pub struct MigrationBlobBackend {
    source: Arc<dyn BlobStorageBackend>,    // old backend (read fallback)
    target: Arc<dyn BlobStorageBackend>,    // new backend (primary for writes)
    state: Arc<RwLock<MigrationState>>,
}

pub struct MigrationState {
    pub status: MigrationStatus,
    pub total_blobs: u64,
    pub migrated_blobs: u64,
    pub migrated_bytes: u64,
    pub failed_blobs: Vec<String>,          // hashes that failed
    pub started_at: Option<chrono::DateTime<chrono::Utc>>,
    pub completed_at: Option<chrono::DateTime<chrono::Utc>>,
}

pub enum MigrationStatus {
    Idle,           // no migration in progress
    Running,        // background job active
    Paused,         // manually paused
    Completed,      // all blobs migrated
    Failed,         // unrecoverable error
}
```

**Behavior**:

| Operation | During Migration |
|---|---|
| `put_blob()` | Write to **target** only |
| `get_blob_stream()` | Try **target** first → fallback to **source** (+ schedule lazy copy) |
| `get_blob_range_stream()` | Same fallback strategy |
| `delete_blob()` | Delete from **both** (best-effort on source) |
| `blob_exists()` | Check **target** first, then **source** |
| `blob_size()` | Check **target** first, then **source** |

### Task 3.2: Background migration job

**File**: `src/infrastructure/services/migration_job.rs` (NEW)

```rust
pub async fn run_migration(
    source: Arc<dyn BlobStorageBackend>,
    target: Arc<dyn BlobStorageBackend>,
    pool: Arc<PgPool>,
    state: Arc<RwLock<MigrationState>>,
    concurrency: usize,              // default: 4 parallel transfers
    bandwidth_limit: Option<u64>,    // bytes/sec, None = unlimited
) -> Result<(), DomainError>
```

**Algorithm**:
1. Query `SELECT hash, size FROM storage.blobs ORDER BY hash` with streaming cursor
2. For each blob hash:
   a. Check if already exists in target (`target.blob_exists(hash)`)
   b. If not: stream from source → write to temp file → `target.put_blob(hash, temp)`
   c. Update `MigrationState` counters
   d. Respect bandwidth limit via `tokio::time::sleep` throttling
3. Use `futures::stream::buffer_unordered(concurrency)` for parallel transfers
4. On error: log, add to `failed_blobs`, continue (don't abort entire migration)

### Task 3.3: Migration API endpoints

**File**: `src/interfaces/api/handlers/admin_handler.rs` (MODIFY)

Add to `admin_routes()`:

```rust
.route("/storage/migration", get(get_migration_status))
.route("/storage/migration/start", post(start_migration))
.route("/storage/migration/pause", post(pause_migration))
.route("/storage/migration/resume", post(resume_migration))
.route("/storage/migration/complete", post(complete_migration))
```

| Endpoint | Purpose |
|---|---|
| `GET /migration` | Return `MigrationState` (status, progress, ETA) |
| `POST /migration/start` | Begin background migration from current → configured backend |
| `POST /migration/pause` | Pause the background job |
| `POST /migration/resume` | Resume paused migration |
| `POST /migration/complete` | Finalize: switch primary backend, optionally clean up source |

### Task 3.4: Migration UI in admin panel

In the Storage tab's Migration section:

1. **Start Migration button** (when backend config differs from active)
2. **Progress bar**: `{migrated} / {total} blobs ({percent}%) — {bytes_migrated} transferred`
3. **Estimated time remaining** (based on throughput)
4. **Pause / Resume button**
5. **Complete Migration button** (enabled only when 100% migrated)
6. **Failed blobs list** (expandable, with retry button)
7. **Status badge**: Idle / Running / Paused / Completed / Failed

### Task 3.5: Integrity verification post-migration

After migration completes (before `complete_migration`):
1. Run `verify_integrity()` against the target backend
2. Compare blob count in PG vs target backend
3. Sample-verify N random blobs (download + BLAKE3 hash check)
4. Show verification results in admin UI before allowing finalization

---

## Phase 4 — Enterprise Extras

### Task 4.1: `CachedBlobBackend` — LRU local disk cache

**File**: `src/infrastructure/services/cached_blob_backend.rs` (NEW)

A `BlobStorageBackend` decorator for remote backends (S3, Azure) that caches hot blobs on local SSD:

```rust
pub struct CachedBlobBackend {
    inner: Arc<dyn BlobStorageBackend>,       // S3 backend
    cache_dir: PathBuf,                        // local cache directory
    max_cache_bytes: u64,                      // configurable limit
    index: Arc<RwLock<LruCache<String, CacheEntry>>>,
    current_size: Arc<AtomicU64>,
}

struct CacheEntry {
    size: u64,
    last_accessed: Instant,
}
```

**Behavior**:
- **Reads**: Check local cache first → cache hit returns local file stream → cache miss downloads from inner backend, writes to cache, returns stream
- **Writes**: `put_blob()` writes to inner backend AND local cache simultaneously
- **Eviction**: LRU eviction when `current_size` exceeds `max_cache_bytes`
- **Startup**: Scan cache directory to rebuild index

**Configuration** (admin panel):
```
storage.cache.enabled          → "true" | "false"
storage.cache.max_size_bytes   → u64 (default: 50 GB)
storage.cache.path             → PathBuf (default: "{storage_root}/.cache")
```

**Env vars**:
```
OXICLOUD_STORAGE_CACHE_ENABLED=true
OXICLOUD_STORAGE_CACHE_MAX_SIZE=53687091200    # 50 GB
OXICLOUD_STORAGE_CACHE_PATH=/fast-ssd/oxicloud-cache
```

### Task 4.2: Server-side encryption at rest (AES-256-GCM)

**File**: `src/infrastructure/services/encrypted_blob_backend.rs` (NEW)

Another `BlobStorageBackend` decorator that encrypts blobs before sending to the inner backend:

```rust
pub struct EncryptedBlobBackend {
    inner: Arc<dyn BlobStorageBackend>,
    encryption_key: [u8; 32],                  // AES-256 key
}
```

**Dependency**: Add `aes-gcm = "0.10"` to Cargo.toml.

**Behavior**:
- `put_blob()`: Read source → encrypt with AES-256-GCM (random 96-bit nonce prepended) → write encrypted to temp → `inner.put_blob(hash, encrypted_temp)`
- `get_blob_stream()`: `inner.get_blob_stream()` → decrypt stream → return plaintext stream
- **CRITICAL**: BLAKE3 hash is computed on the PLAINTEXT (before encryption), so dedup still works across encrypted backends
- **Nonce storage**: Prepend 12-byte nonce to each encrypted blob (total overhead: 28 bytes per blob — 12 nonce + 16 GCM tag)

**Configuration** (admin panel):
```
storage.encryption.enabled     → "true" | "false"
storage.encryption.key         → base64-encoded 32-byte key (is_secret: true)
```

**Key generation**: Provide an admin API endpoint `POST /api/admin/settings/storage/generate-key` that generates a cryptographically secure key and returns it once (user must save it).

**WARNING in admin UI**: "If you lose the encryption key, all data in the storage backend is IRRECOVERABLY LOST. Back up this key securely."

### Task 4.3: Azure Blob Storage backend

**File**: `src/infrastructure/services/azure_blob_backend.rs` (NEW)

**Dependency**: `azure_storage_blobs = "0.21"`, `azure_storage = "0.21"`

Same `BlobStorageBackend` trait implementation targeting Azure Blob Storage:
- Container = bucket equivalent
- Blob key scheme: `{prefix}/{hash}.blob` (same as S3/local)
- Authentication: Account Name + Account Key OR SAS token

**Configuration**:
```
OXICLOUD_AZURE_ACCOUNT_NAME
OXICLOUD_AZURE_ACCOUNT_KEY
OXICLOUD_AZURE_CONTAINER
OXICLOUD_AZURE_SAS_TOKEN         # alternative auth
```

### Task 4.4: Bandwidth throttling & retry policies

Apply to all remote backends (S3, Azure):

**Throttling**:
- Configurable upload/download bandwidth limit per-backend
- Implemented via `tokio::time::sleep` between chunks
- Admin configurable: `storage.s3.max_upload_bandwidth`, `storage.s3.max_download_bandwidth`

**Retry policy** (exponential backoff):
```rust
pub struct RetryPolicy {
    pub max_retries: u32,           // default: 3
    pub initial_backoff_ms: u64,    // default: 100
    pub max_backoff_ms: u64,        // default: 10_000
    pub backoff_multiplier: f64,    // default: 2.0
}
```

Wrap remote backend calls with retry logic for transient errors (network timeouts, 500s, 503s).

---

## Architecture Invariants (MUST be preserved)

1. **BLAKE3 hashing**: Always performed locally on temp files, never delegated to backend
2. **PostgreSQL dedup index**: `storage.blobs` table remains the source of truth for ref_count, metadata
3. **Write-first strategy**: Blob stored in backend BEFORE PostgreSQL upsert
4. **Remove-after-commit**: Blob deleted from backend AFTER PostgreSQL transaction commits
5. **Streaming reads**: All blob reads return `Pin<Box<dyn Stream>>`, never load full blob into memory
6. **Zero framework deps in domain**: `BlobStorageBackend` trait lives in `application/ports/`, not infrastructure
7. **DI via AppState**: All backends are `Arc`-wrapped and assembled in `common/di.rs`
8. **Env var precedence**: `OXICLOUD_*` env vars always override DB-stored admin settings
9. **Admin guard**: All storage admin endpoints require JWT with role `"admin"`
10. **Existing tests**: All ~208 tests must pass after Phase 1 refactor

## Backend Decorator Composition

The backends compose as decorators. In `di.rs`, the assembly looks like:

```rust
// Phase 1: Base backend
let base_backend: Arc<dyn BlobStorageBackend> = match config {
    Local => Arc::new(LocalBlobBackend::new(...)),
    S3 => Arc::new(S3BlobBackend::new(...).await?),
    Azure => Arc::new(AzureBlobBackend::new(...).await?),
};

// Phase 4: Optional encryption layer
let backend = if encryption_enabled {
    Arc::new(EncryptedBlobBackend::new(base_backend, key))
} else {
    base_backend
};

// Phase 4: Optional cache layer (only for remote backends)
let backend = if cache_enabled && !matches!(config, Local) {
    Arc::new(CachedBlobBackend::new(backend, cache_config))
} else {
    backend
};

// Phase 3: Optional migration wrapper
let backend = if migration_in_progress {
    Arc::new(MigrationBlobBackend::new(old_backend, backend, state))
} else {
    backend
};

// Finally: DedupService uses the composed backend
let dedup_service = Arc::new(DedupService::new(backend, pool, maintenance_pool));
```

This decorator pattern means each feature (encryption, caching, migration) is:
- Independently testable
- Independently toggleable
- Zero overhead when disabled
- Composable in any order
