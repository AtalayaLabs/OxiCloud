use std::collections::HashMap;
use std::sync::Arc;
use uuid::Uuid;

use std::sync::atomic::{AtomicBool, Ordering};

use crate::application::dtos::settings_dto::{
    SaveStorageSettingsDto, StorageEntrySummaryDto, StorageSettingsDto, StorageTestResultDto,
    TestStorageConnectionDto,
};
use crate::application::ports::blob_storage_ports::BlobStorageBackend;
use crate::common::config::{
    NamedStorageEntry, S3StorageConfig, StorageBackendType, StorageConfig,
};
use crate::common::errors::{DomainError, ErrorKind};
use crate::domain::repositories::settings_repository::SettingsRepository;
use crate::infrastructure::repositories::pg::SettingsPgRepository;
use crate::infrastructure::services::azure_blob_backend::AzureBlobBackend;
use crate::infrastructure::services::dedup_service::DedupService;
use crate::infrastructure::services::local_blob_backend::LocalBlobBackend;
use crate::infrastructure::services::s3_blob_backend::S3BlobBackend;

/// Storage settings service — manages storage backend configuration via the admin panel.
///
/// Configuration priority: **env vars > DB settings > defaults**.
pub struct StorageSettingsService {
    settings_repo: Arc<SettingsPgRepository>,
    env_storage_config: StorageConfig,
    dedup_service: Arc<DedupService>,
    /// Multi-entry snapshot from `AppConfig.storage_entries`. Populated
    /// at DI time; immutable per-process (env can only change on
    /// restart, per `docs/plan/storage-multi-entry.md`). Empty when
    /// running in the pre-multi-entry legacy path.
    storage_entries: Vec<NamedStorageEntry>,
    /// Shared handle to `CoreServices.active_backend_name`. Reads
    /// snapshot the current value on each admin GET so a hot-swap
    /// cutover is immediately visible in the UI without waiting for
    /// a refresh. Empty string / "legacy" for the zero-entries path.
    active_entry_name: Arc<std::sync::RwLock<String>>,
    /// Shared readonly flag — read into the admin DTO so the UI can
    /// render a "server in migration read-only mode" banner. Same
    /// atomic as `AppState.migration_readonly`; changes made by the
    /// migration handler are visible without a DB round-trip.
    migration_readonly: Arc<AtomicBool>,
}

impl StorageSettingsService {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        settings_repo: Arc<SettingsPgRepository>,
        env_storage_config: StorageConfig,
        dedup_service: Arc<DedupService>,
        storage_entries: Vec<NamedStorageEntry>,
        active_entry_name: Arc<std::sync::RwLock<String>>,
        migration_readonly: Arc<AtomicBool>,
    ) -> Self {
        Self {
            settings_repo,
            env_storage_config,
            dedup_service,
            storage_entries,
            active_entry_name,
            migration_readonly,
        }
    }

    /// Apply environment variable overrides on top of a config.
    fn apply_env_overrides(&self, config: &mut StorageConfig) {
        let e = &self.env_storage_config;
        if std::env::var("OXICLOUD_STORAGE_BACKEND").is_ok() {
            config.backend = e.backend.clone();
        }
        // S3 env overrides — only apply if S3 config exists in env
        if let Some(env_s3) = &e.s3 {
            let s3 = config.s3.get_or_insert_with(|| S3StorageConfig {
                endpoint_url: None,
                bucket: String::new(),
                region: "us-east-1".to_string(),
                access_key: String::new(),
                secret_key: String::new(),
                force_path_style: false,
            });
            if std::env::var("OXICLOUD_S3_ENDPOINT_URL").is_ok() {
                s3.endpoint_url = env_s3.endpoint_url.clone();
            }
            if std::env::var("OXICLOUD_S3_BUCKET").is_ok() {
                s3.bucket = env_s3.bucket.clone();
            }
            if std::env::var("OXICLOUD_S3_REGION").is_ok() {
                s3.region = env_s3.region.clone();
            }
            if std::env::var("OXICLOUD_S3_ACCESS_KEY").is_ok() {
                s3.access_key = env_s3.access_key.clone();
            }
            if std::env::var("OXICLOUD_S3_SECRET_KEY").is_ok() {
                s3.secret_key = env_s3.secret_key.clone();
            }
            if std::env::var("OXICLOUD_S3_FORCE_PATH_STYLE").is_ok() {
                s3.force_path_style = env_s3.force_path_style;
            }
        }
    }

    /// Physical-storage identity string. Two configs that yield the
    /// same `storage_identity` point at the same physical location
    /// (same disk directory, same S3 bucket, same Azure container) —
    /// used by [`Self::is_source_target_identical`] to detect a no-op
    /// migration where source and target are the same backend.
    ///
    /// Credentials are deliberately excluded: two configs with
    /// different access keys pointing at the same bucket ARE the same
    /// storage; a migration between them would be a wasted walk. The
    /// same principle applies to fields that don't influence which
    /// bytes get read/written (chunk sizes, retention days, etc.).
    fn storage_identity(config: &StorageConfig) -> String {
        match config.backend {
            StorageBackendType::Local => format!("local:{}", config.root_dir),
            StorageBackendType::S3 => match config.s3.as_ref() {
                Some(s3) => format!(
                    "s3:{}/{}:path_style={}",
                    s3.endpoint_url.as_deref().unwrap_or("aws"),
                    s3.bucket,
                    s3.force_path_style,
                ),
                None => "s3:<missing-config>".to_string(),
            },
            StorageBackendType::Azure => match config.azure.as_ref() {
                Some(az) => format!(
                    "azure:{}/{}",
                    az.account_name.as_str(),
                    az.container.as_str(),
                ),
                None => "azure:<missing-config>".to_string(),
            },
        }
    }

    /// True iff the *effective* storage config points at the same
    /// physical location as the *boot* config — i.e. the migration
    /// would be a no-op that walks every blob and skips them all.
    ///
    /// The migration handler calls this at run start and refuses with
    /// `RunOutcome::Failed` if it's true — otherwise a misclick on an
    /// S3 deployment would issue one `HEAD` per blob for zero useful
    /// work (and real cost). "Legitimate" same-type migrations (e.g.
    /// `local:/data` → `local:/newdisk`, or S3 bucket A → S3 bucket B)
    /// return false and proceed normally.
    pub async fn is_source_target_identical(&self) -> Result<bool, DomainError> {
        let effective = self.load_effective_storage_config().await?;
        Ok(Self::storage_identity(&self.env_storage_config) == Self::storage_identity(&effective))
    }

    /// Build a `BlobStorageBackend` matching the current *effective*
    /// storage config (DB + env-var overrides + defaults).
    ///
    /// Distinct from `dedup_service.backend()`, which is the LIVE
    /// backend the app booted with — this method reflects what the
    /// admin has configured *now* and typically resolves to a
    /// different backend during a migration (source = live, target =
    /// effective). The returned handle is a fresh instance; the caller
    /// must `.initialize()` it before first use.
    pub async fn build_effective_backend(
        &self,
    ) -> Result<Arc<dyn BlobStorageBackend>, DomainError> {
        let effective = self.load_effective_storage_config().await?;
        match effective.backend {
            StorageBackendType::Local => Ok(Arc::new(LocalBlobBackend::new(std::path::Path::new(
                &effective.root_dir,
            )))),
            StorageBackendType::S3 => {
                let s3 = effective.s3.as_ref().ok_or_else(|| {
                    DomainError::new(
                        ErrorKind::InvalidInput,
                        "Storage",
                        "S3 backend selected but no S3 configuration is present",
                    )
                })?;
                Ok(Arc::new(S3BlobBackend::new(s3)))
            }
            StorageBackendType::Azure => {
                let az = effective.azure.as_ref().ok_or_else(|| {
                    DomainError::new(
                        ErrorKind::InvalidInput,
                        "Storage",
                        "Azure backend selected but no Azure configuration is present",
                    )
                })?;
                Ok(Arc::new(AzureBlobBackend::new(az)))
            }
        }
    }

    /// Load effective storage config: DB settings + env var overrides + defaults.
    pub async fn load_effective_storage_config(&self) -> Result<StorageConfig, DomainError> {
        let db: HashMap<String, String> = self.settings_repo.get_by_category("storage").await?;
        let d = StorageConfig::default();

        let backend = db
            .get("storage.backend")
            .map(|v| match v.as_str() {
                "s3" => crate::common::config::StorageBackendType::S3,
                "azure" => crate::common::config::StorageBackendType::Azure,
                _ => crate::common::config::StorageBackendType::Local,
            })
            .unwrap_or(d.backend);

        let s3 = {
            let bucket = db.get("storage.s3.bucket").cloned().unwrap_or_default();
            if bucket.is_empty() {
                None
            } else {
                Some(S3StorageConfig {
                    endpoint_url: db
                        .get("storage.s3.endpoint_url")
                        .cloned()
                        .filter(|s| !s.is_empty()),
                    bucket,
                    region: db
                        .get("storage.s3.region")
                        .cloned()
                        .unwrap_or_else(|| "us-east-1".to_string()),
                    access_key: db.get("storage.s3.access_key").cloned().unwrap_or_default(),
                    secret_key: db.get("storage.s3.secret_key").cloned().unwrap_or_default(),
                    force_path_style: db
                        .get("storage.s3.force_path_style")
                        .and_then(|v| v.parse().ok())
                        .unwrap_or(false),
                })
            }
        };

        let mut config = StorageConfig {
            backend,
            s3,
            ..self.env_storage_config.clone()
        };

        self.apply_env_overrides(&mut config);
        Ok(config)
    }

    /// Get storage settings for display in admin UI.
    ///
    /// Post-slice-6 the response is the multi-entry view + live stats
    /// only. The legacy `backend` / `s3_*` / `env_overrides` fields
    /// were retired — they duplicated `entries[]` and leaked stale
    /// admin_settings rows saved by the retired admin-panel form.
    pub async fn get_storage_settings(&self) -> Result<StorageSettingsDto, DomainError> {
        let stats = self.dedup_service.get_stats().await;
        let current_backend = self.dedup_service.backend().backend_type().to_string();

        // Project the multi-entry view. `is_active` is name-compared
        // against the boot-selected `active_entry_name` (matches
        // exactly one entry when we're in multi-entry mode; matches
        // nothing when running the zero-entries legacy path, which
        // is expected — the frontend hides the entries table then).
        // Snapshot the active name once; every entry below uses it
        // for the is_active flag AND we echo it on the top-level
        // DTO. RwLock is held only for the clone.
        let active_entry_name = self
            .active_entry_name
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let entries: Vec<StorageEntrySummaryDto> = self
            .storage_entries
            .iter()
            .map(|e| {
                // Render the pair-list summary — one row per pair,
                // head marked. Never emits key material; only
                // cipher + fingerprint. See `StorageEncryptionPairDto`
                // for the display contract.
                let pairs = e.encryption_pairs();
                let head_idx = pairs.len().saturating_sub(1);
                let encryption_pairs = pairs
                    .iter()
                    .enumerate()
                    .map(|(i, kp)| {
                        crate::application::dtos::settings_dto::StorageEncryptionPairDto {
                            cipher: kp.cipher.as_str().to_string(),
                            fingerprint: kp.fingerprint_short(),
                            is_head: i == head_idx,
                        }
                    })
                    .collect();
                StorageEntrySummaryDto {
                    name: e.name.clone(),
                    backend: match e.backend {
                        StorageBackendType::Local => "local".to_string(),
                        StorageBackendType::S3 => "s3".to_string(),
                        StorageBackendType::Azure => "azure".to_string(),
                    },
                    is_active: e.name == active_entry_name,
                    encryption_enabled: e.is_encrypted(),
                    location_hint: entry_location_hint(e),
                    encryption_pairs,
                }
            })
            .collect();

        Ok(StorageSettingsDto {
            current_backend,
            total_blobs: stats.total_blobs,
            total_bytes_stored: stats.total_bytes_stored,
            dedup_ratio: stats.dedup_ratio,
            entries,
            active_entry_name,
            migration_readonly: self.migration_readonly.load(Ordering::Relaxed),
        })
    }

    /// Save storage settings to DB.
    pub async fn save_storage_settings(
        &self,
        dto: SaveStorageSettingsDto,
        updated_by: Uuid,
    ) -> Result<(), DomainError> {
        let cat = "storage";
        let by = Some(updated_by);

        self.settings_repo
            .set("storage.backend", &dto.backend, cat, false, by)
            .await?;

        if let Some(ref v) = dto.s3_endpoint_url {
            self.settings_repo
                .set("storage.s3.endpoint_url", v, cat, false, by)
                .await?;
        }
        if let Some(ref v) = dto.s3_bucket {
            self.settings_repo
                .set("storage.s3.bucket", v, cat, false, by)
                .await?;
        }
        if let Some(ref v) = dto.s3_region {
            self.settings_repo
                .set("storage.s3.region", v, cat, false, by)
                .await?;
        }
        if let Some(ref v) = dto.s3_access_key
            && !v.is_empty()
        {
            self.settings_repo
                .set("storage.s3.access_key", v, cat, true, by)
                .await?;
        }
        if let Some(ref v) = dto.s3_secret_key
            && !v.is_empty()
        {
            self.settings_repo
                .set("storage.s3.secret_key", v, cat, true, by)
                .await?;
        }
        if let Some(v) = dto.s3_force_path_style {
            self.settings_repo
                .set(
                    "storage.s3.force_path_style",
                    &v.to_string(),
                    cat,
                    false,
                    by,
                )
                .await?;
        }

        tracing::info!("Storage settings saved by admin (backend={})", dto.backend);
        Ok(())
    }

    /// Test a storage backend: reachability (health-check) followed by
    /// a full read/write round-trip (see [`run_backend_roundtrip`]).
    ///
    /// Two-phase so the operator gets clean diagnosis: if the health-
    /// check fails they know it's an auth/endpoint/bucket problem
    /// (never even wrote a byte). If it passes but the round-trip
    /// fails, they know reachability is fine and the permissions are
    /// the gap. Round-trip fields on the result are `None` when we
    /// didn't attempt it (health-check failed early).
    pub async fn test_storage_connection(
        &self,
        dto: TestStorageConnectionDto,
    ) -> Result<StorageTestResultDto, DomainError> {
        // Multi-entry mode: `entry_name` present → look up the entry,
        // build via the shared factory, health-check + round-trip. The
        // legacy DTO fields (backend / s3_*) are ignored. Unknown
        // names return a `connected: false` result with a clear
        // message, mirroring the shape of the legacy path (no
        // exceptions for the client to catch — errors are inline).
        if let Some(name) = dto.entry_name.as_deref() {
            let entry = match self.storage_entries.iter().find(|e| e.name == name) {
                Some(e) => e,
                None => {
                    let available = if self.storage_entries.is_empty() {
                        "(none)".to_string()
                    } else {
                        self.storage_entries
                            .iter()
                            .map(|e| e.name.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    };
                    return Ok(StorageTestResultDto {
                        connected: false,
                        message: format!(
                            "entry `{name}` not declared in OXICLOUD_STORAGE_ENTRIES. \
                             Available: [{available}]"
                        ),
                        backend_type: "unknown".to_string(),
                        available_bytes: None,
                        roundtrip_passed: None,
                        phase_reached: None,
                        bytes_written: None,
                        bytes_read: None,
                        roundtrip_elapsed_ms: None,
                        cleanup_ok: None,
                    });
                }
            };
            let backend = crate::infrastructure::services::entry_backend::build_entry_backend(
                entry,
                std::path::Path::new(&self.env_storage_config.root_dir),
            );
            // Post-K2 (always-wrap): `backend.backend_type()` returns
            // the outer wrapper's kind ("v1-plaintext" / "encrypted"),
            // NOT the underlying storage backend. `health_check()`
            // formats the wrapper-inner combo as
            // `"<wrapper>(<inner>)"` — that's the more informative
            // string for the admin panel's Test-connection result.
            // We use the wrapper-only string as a fallback for the
            // health-check-failed branch, where there's no formatted
            // status to draw from.
            let fallback_backend_kind = backend.backend_type().to_string();
            let status = match backend.health_check().await {
                Ok(s) => s,
                Err(e) => {
                    return Ok(StorageTestResultDto {
                        connected: false,
                        message: format!("health-check failed: {e}"),
                        backend_type: fallback_backend_kind,
                        available_bytes: None,
                        roundtrip_passed: None,
                        phase_reached: None,
                        bytes_written: None,
                        bytes_read: None,
                        roundtrip_elapsed_ms: None,
                        cleanup_ok: None,
                    });
                }
            };
            let mut out = StorageTestResultDto {
                connected: status.connected,
                message: status.message,
                backend_type: status.backend_type,
                available_bytes: status.available_bytes,
                roundtrip_passed: None,
                phase_reached: None,
                bytes_written: None,
                bytes_read: None,
                roundtrip_elapsed_ms: None,
                cleanup_ok: None,
            };
            if out.connected {
                attach_roundtrip(&mut out, backend.as_ref()).await;
            }
            return Ok(out);
        }

        // Legacy path — DTO carries backend + s3 fields directly.
        // Retained for pre-multi-entry deployments (zero declared
        // entries) and for the admin form's on-form-values Test
        // button. Deprecated for new integrations.
        match dto.backend.as_str() {
            "local" => {
                // Local: no per-DTO override for the root_dir (the
                // form has no such field), so we test the live
                // backend the app is running on. health_check() reports
                // available_bytes via statfs; the round-trip validates
                // disk write + read + delete permissions.
                let backend = self.dedup_service.backend().clone();
                let status = backend.health_check().await?;
                let mut out = StorageTestResultDto {
                    connected: status.connected,
                    message: status.message,
                    backend_type: "local".to_string(),
                    available_bytes: status.available_bytes,
                    roundtrip_passed: None,
                    phase_reached: None,
                    bytes_written: None,
                    bytes_read: None,
                    roundtrip_elapsed_ms: None,
                    cleanup_ok: None,
                };
                if out.connected {
                    attach_roundtrip(&mut out, backend.as_ref()).await;
                }
                Ok(out)
            }
            "s3" => {
                let bucket = dto.s3_bucket.as_deref().unwrap_or_default();
                if bucket.is_empty() {
                    return Ok(StorageTestResultDto {
                        connected: false,
                        message: "S3 bucket name is required".to_string(),
                        backend_type: "s3".to_string(),
                        available_bytes: None,
                        roundtrip_passed: None,
                        phase_reached: None,
                        bytes_written: None,
                        bytes_read: None,
                        roundtrip_elapsed_ms: None,
                        cleanup_ok: None,
                    });
                }

                // Build a temporary S3 backend from the DTO values,
                // falling back to existing DB/env config for missing
                // fields — lets the admin test values entered but not
                // yet saved (matches the current UX).
                let effective = self.load_effective_storage_config().await.ok();
                let existing_s3 = effective.as_ref().and_then(|c| c.s3.as_ref());

                let config = S3StorageConfig {
                    endpoint_url: dto
                        .s3_endpoint_url
                        .clone()
                        .or_else(|| existing_s3.and_then(|s| s.endpoint_url.clone())),
                    bucket: bucket.to_string(),
                    region: dto.s3_region.clone().unwrap_or_else(|| {
                        existing_s3
                            .map(|s| s.region.clone())
                            .unwrap_or_else(|| "us-east-1".to_string())
                    }),
                    access_key: dto
                        .s3_access_key
                        .clone()
                        .filter(|s| !s.is_empty())
                        .unwrap_or_else(|| {
                            existing_s3
                                .map(|s| s.access_key.clone())
                                .unwrap_or_default()
                        }),
                    secret_key: dto
                        .s3_secret_key
                        .clone()
                        .filter(|s| !s.is_empty())
                        .unwrap_or_else(|| {
                            existing_s3
                                .map(|s| s.secret_key.clone())
                                .unwrap_or_default()
                        }),
                    force_path_style: dto
                        .s3_force_path_style
                        .unwrap_or_else(|| existing_s3.is_some_and(|s| s.force_path_style)),
                };

                let backend = S3BlobBackend::new(&config);
                let mut out = match backend.health_check().await {
                    Ok(status) => StorageTestResultDto {
                        connected: status.connected,
                        message: status.message,
                        backend_type: "s3".to_string(),
                        available_bytes: status.available_bytes,
                        roundtrip_passed: None,
                        phase_reached: None,
                        bytes_written: None,
                        bytes_read: None,
                        roundtrip_elapsed_ms: None,
                        cleanup_ok: None,
                    },
                    Err(e) => StorageTestResultDto {
                        connected: false,
                        message: format!("Connection failed: {}", e),
                        backend_type: "s3".to_string(),
                        available_bytes: None,
                        roundtrip_passed: None,
                        phase_reached: None,
                        bytes_written: None,
                        bytes_read: None,
                        roundtrip_elapsed_ms: None,
                        cleanup_ok: None,
                    },
                };
                if out.connected {
                    attach_roundtrip(&mut out, &backend).await;
                }
                Ok(out)
            }
            other => Err(DomainError::new(
                ErrorKind::InvalidInput,
                "Storage",
                format!("Unknown backend type: {}", other),
            )),
        }
    }
}

/// Populate the round-trip fields of `out` by executing a full
/// PUT → EXISTS → GET → VERIFY → DELETE cycle against `backend`. Only
/// called when reachability (`out.connected`) already passed —
/// keeping "wasn't even reachable" and "reachable but round-trip
/// failed" as distinct diagnoses.
///
/// On round-trip failure, `out.message` is REPLACED with the round-
/// trip diagnosis (the pre-round-trip message was just "connection
/// succeeded" — round-trip failure supersedes it). On round-trip
/// success, `out.message` is REPLACED with the success confirmation
/// so the admin sees the strong claim, not the weaker "reachable".
async fn attach_roundtrip(out: &mut StorageTestResultDto, backend: &dyn BlobStorageBackend) {
    let (passed, phase, written, read, elapsed_ms, cleanup_ok, message) =
        run_backend_roundtrip(backend).await;
    out.roundtrip_passed = Some(passed);
    out.phase_reached = Some(phase);
    out.bytes_written = Some(written);
    out.bytes_read = Some(read);
    out.roundtrip_elapsed_ms = Some(elapsed_ms);
    out.cleanup_ok = Some(cleanup_ok);
    if !passed {
        // Reachability was fine — the round-trip is the reason to
        // fail this test overall. Flip `connected` to false so the
        // UI shows the whole test as failed, and surface the
        // round-trip diagnosis in the message.
        out.connected = false;
    }
    out.message = message;
}

/// Full read/write round-trip against the given backend. PUT a tiny
/// unique object, verify existence, GET it back, check BLAKE3
/// matches, DELETE it. Validates the exact permissions the migration
/// job needs (`s3:PutObject` + `s3:GetObject` + `s3:DeleteObject` on
/// S3, disk write on Local) — a stronger check than `health_check`.
///
/// Returns the round-trip fields for [`StorageTestResultDto`]. Errors
/// are folded into the return value (not `Err`) so callers can
/// surface `phase_reached` diagnostics inline.
async fn run_backend_roundtrip(
    backend: &dyn BlobStorageBackend,
) -> (
    /* passed */ bool,
    /* phase_reached */ String,
    /* bytes_written */ u64,
    /* bytes_read */ u64,
    /* elapsed_ms */ u64,
    /* cleanup_ok */ bool,
    /* message */ String,
) {
    use bytes::Bytes;
    use futures::StreamExt;
    use std::time::Instant;

    let started = Instant::now();

    // Content-addressable — hash MUST be BLAKE3 of the payload. UUID
    // + timestamp guarantees a fresh key on every test so we never
    // collide with a real blob or stale test remnant.
    let payload = format!(
        "oxicloud-roundtrip-test-{}-{}",
        uuid::Uuid::new_v4(),
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0),
    );
    let payload_bytes = payload.into_bytes();
    let hash = blake3::hash(&payload_bytes).to_hex().to_string();
    let bytes_written = payload_bytes.len() as u64;

    if let Err(e) = backend
        .put_blob_from_bytes(&hash, Bytes::from(payload_bytes.clone()))
        .await
    {
        return (
            false,
            "initialize".to_string(),
            0,
            0,
            started.elapsed().as_millis() as u64,
            false,
            format!("put: {e}"),
        );
    }

    match backend.blob_exists(&hash).await {
        Ok(true) => {}
        Ok(false) => {
            let cleanup_ok = backend.delete_blob(&hash).await.is_ok();
            return (
                false,
                "put_ok".to_string(),
                bytes_written,
                0,
                started.elapsed().as_millis() as u64,
                cleanup_ok,
                "PUT reported success but blob_exists returned false".to_string(),
            );
        }
        Err(e) => {
            let cleanup_ok = backend.delete_blob(&hash).await.is_ok();
            return (
                false,
                "put_ok".to_string(),
                bytes_written,
                0,
                started.elapsed().as_millis() as u64,
                cleanup_ok,
                format!("exists: {e}"),
            );
        }
    }

    let mut got: Vec<u8> = Vec::with_capacity(payload_bytes.len());
    match backend.get_blob_stream(&hash).await {
        Ok(stream) => {
            let mut stream = std::pin::pin!(stream);
            while let Some(chunk) = stream.next().await {
                match chunk {
                    Ok(bytes) => got.extend_from_slice(&bytes),
                    Err(e) => {
                        let cleanup_ok = backend.delete_blob(&hash).await.is_ok();
                        return (
                            false,
                            "exists_ok".to_string(),
                            bytes_written,
                            got.len() as u64,
                            started.elapsed().as_millis() as u64,
                            cleanup_ok,
                            format!("get stream: {e}"),
                        );
                    }
                }
            }
        }
        Err(e) => {
            let cleanup_ok = backend.delete_blob(&hash).await.is_ok();
            return (
                false,
                "exists_ok".to_string(),
                bytes_written,
                0,
                started.elapsed().as_millis() as u64,
                cleanup_ok,
                format!("get: {e}"),
            );
        }
    }
    let bytes_read = got.len() as u64;

    // VERIFY — recompute BLAKE3 on the received bytes. A hash
    // mismatch means the backend returned different bytes than it
    // stored (silent corruption in the round-trip). Extremely rare
    // but the whole point of doing a byte-level test.
    let got_hash = blake3::hash(&got).to_hex().to_string();
    if got_hash != hash {
        let cleanup_ok = backend.delete_blob(&hash).await.is_ok();
        return (
            false,
            "get_ok".to_string(),
            bytes_written,
            bytes_read,
            started.elapsed().as_millis() as u64,
            cleanup_ok,
            format!(
                "byte mismatch: wrote {bytes_written} bytes hash {hash}, read back {bytes_read} bytes hash {got_hash}"
            ),
        );
    }

    // CLEANUP — failure here does NOT flip `passed`. Read/write
    // validation succeeded; the backend just left an orphan test
    // blob (harmless — content-addressed, ~100 B).
    let cleanup_ok = backend.delete_blob(&hash).await.is_ok();
    (
        true,
        if cleanup_ok {
            "cleanup_ok"
        } else {
            "verify_ok"
        }
        .to_string(),
        bytes_written,
        bytes_read,
        started.elapsed().as_millis() as u64,
        cleanup_ok,
        if cleanup_ok {
            "Round-trip OK: write + read + delete all succeeded".to_string()
        } else {
            format!(
                "Round-trip OK (write + read validated), but cleanup DELETE failed — one orphan test blob left at hash {hash}"
            )
        },
    )
}

/// Cosmetic human-readable identifier for an entry — the physical
/// location piece an admin uses to disambiguate two entries of the
/// same backend type. Never carries credentials. `None` when the
/// entry doesn't have a natural short label (S3 without a bucket,
/// which shouldn't happen because the parser rejects that shape at
/// boot).
fn entry_location_hint(entry: &NamedStorageEntry) -> Option<String> {
    match entry.backend {
        StorageBackendType::Local => entry.root_dir.clone(),
        // S3: show `<endpoint>/<bucket>` — bucket alone can collide
        // across providers (a "my-bucket" on AWS vs the same name on
        // MinIO / R2 look identical without the endpoint). `aws` is
        // the visual stand-in when no custom endpoint is configured
        // (i.e. talking to real AWS S3).
        StorageBackendType::S3 => entry.s3.as_ref().map(|s3| {
            // Trim trailing `/` so an env value of `https://host/`
            // doesn't render as `https://host//bucket`. Both shapes
            // are legitimate env inputs (some providers publish the
            // trailing slash in their docs).
            let endpoint = s3
                .endpoint_url
                .as_deref()
                .unwrap_or("aws")
                .trim_end_matches('/');
            format!("{endpoint}/{}", s3.bucket)
        }),
        // Azure: show `<endpoint_or_account>/<container>`. The
        // endpoint-URL override is uncommon on Azure (Azurite
        // emulator, private stamps), so fall back to the account
        // name when unset — matches what a reader would look for in
        // the portal. Same trailing-slash trim as S3.
        StorageBackendType::Azure => entry.azure.as_ref().map(|az| {
            let host = az
                .endpoint_url
                .as_deref()
                .unwrap_or(az.account_name.as_str())
                .trim_end_matches('/');
            format!("{host}/{}", az.container)
        }),
    }
}
