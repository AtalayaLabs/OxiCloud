use sqlx::PgPool;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use uuid::Uuid;

use crate::application::ports::blob_storage_ports::BlobStorageBackend;
use crate::application::ports::storage_ports::StorageUsagePort;
use crate::common::config::StorageBackendType;
use crate::domain::entities::drive::DriveKind;
use crate::domain::repositories::drive_repository::DriveRepository;
use crate::infrastructure::db::DbPools;

use crate::application::services::admin_settings_service::AdminSettingsService;
use crate::application::services::auth_application_service::AuthApplicationService;
use crate::application::services::storage_settings_service::StorageSettingsService;

use crate::application::ports::file_ports::FileUseCaseFactory;
use crate::application::services::favorites_service::FavoritesService;
use crate::application::services::folder_service::FolderService;
use crate::application::services::i18n_application_service::I18nApplicationService;
use crate::application::services::nextcloud_file_id_service::NextcloudFileIdService;
use crate::application::services::nextcloud_login_flow_service::NextcloudLoginFlowService;
use crate::application::services::people_service::PeopleService;
use crate::application::services::places_service::PlacesService;
use crate::application::services::recent_service::RecentService;
use crate::application::services::search_service::SearchService;
use crate::application::services::share_browse_service::ShareBrowseService;
use crate::application::services::share_service::ShareService;
use crate::application::services::trash_service::TrashService;
use crate::application::services::{
    AppFileUseCaseFactory, FileManagementService, FileRetrievalService, FileUploadService,
};
use crate::common::config::AppConfig;
use crate::common::errors::DomainError;
use crate::common::locale::LocaleRegistry;
use crate::infrastructure::repositories::pg::SharePgRepository;
use crate::infrastructure::repositories::pg::{
    FileBlobReadRepository, FileBlobWriteRepository, FileMetadataRepository, FolderDbRepository,
    TrashDbRepository,
};
use crate::infrastructure::scheduler::{JobRegistry, SchedulerEngine};
use crate::infrastructure::services::file_content_cache::{
    FileContentCache, FileContentCacheConfig,
};
use crate::infrastructure::services::file_system_i18n_service::FileSystemI18nService;
use crate::infrastructure::services::nextcloud_chunked_upload_service::NextcloudChunkedUploadService;
use crate::infrastructure::services::path_service::PathService;
use crate::infrastructure::services::pg_acl_engine::PgAclEngine;
use crate::infrastructure::services::search_index::content_index_worker::ContentIndexWorker;
use crate::infrastructure::services::search_index::tantivy_content_index::TantivyContentIndex;
use crate::infrastructure::services::trash_cleanup_service::TrashCleanupService;

use crate::application::ports::video_frame_ports::VideoFramePort;
use crate::application::services::app_password_service::AppPasswordService;
use crate::application::services::blob_lifecycle_service::BlobLifecycleService;
use crate::application::services::calendar_service::CalendarService;
use crate::application::services::contact_service::ContactService;
use crate::application::services::device_auth_service::DeviceAuthService;
use crate::application::services::file_lifecycle_service::FileLifecycleService;
use crate::application::services::music_service::MusicService;
use crate::application::services::storage_usage_service::StorageUsageService;
use crate::application::services::wopi_lock_service::WopiLockService;
use crate::application::services::wopi_token_service::WopiTokenService;
use crate::infrastructure::repositories::AppPasswordPgRepository;
use crate::infrastructure::repositories::DeviceCodePgRepository;
use crate::infrastructure::repositories::pg::{
    AddressBookPgRepository, AudioMetadataPgRepository, CalendarEventPgRepository,
    CalendarPgRepository, ContactGroupPgRepository, ContactPgRepository, PlaylistItemPgRepository,
    PlaylistPgRepository, SessionPgRepository, UserPgRepository,
};
use crate::infrastructure::services::audio_metadata_service::AudioMetadataService;
use crate::infrastructure::services::chunked_upload_service::ChunkedUploadService;
use crate::infrastructure::services::dedup_service::DedupService;
use crate::infrastructure::services::ffmpeg_video_frame_service::{
    FfmpegVideoFrameService, NoopVideoFrameService,
};
use crate::infrastructure::services::image_transcode_service::ImageTranscodeService;
use crate::infrastructure::services::jwt_service::JwtTokenService;
use crate::infrastructure::services::media_metadata_service::MediaMetadataService;
use crate::infrastructure::services::password_hasher::Argon2PasswordHasher;
use crate::infrastructure::services::path_resolver_service::PathResolverService;
use crate::infrastructure::services::thumbnail_service::{ThumbnailRefreshHook, ThumbnailService};
use crate::infrastructure::services::wopi_discovery_service::WopiDiscoveryService;
use crate::infrastructure::services::zip_service::ZipService;

/// Factory for the different application components
///
/// This factory centralizes the creation of all application services,
/// ensuring the correct initialization order and resolving circular dependencies.
pub struct AppServiceFactory {
    storage_path: PathBuf,
    locales_path: PathBuf,
    config: AppConfig,
    /// Validated set of locales discovered under `locales_path`. Built
    /// once at factory construction time; consumed by the I18n service
    /// and the `Accept-Language` extractor. See
    /// [`crate::common::locale::LocaleRegistry`] for the discovery rules.
    locale_registry: Arc<LocaleRegistry>,
}

impl AppServiceFactory {
    /// Creates a new service factory
    pub fn new(storage_path: PathBuf, locales_path: PathBuf) -> Self {
        let config = AppConfig::default();
        let locale_registry = Self::build_registry(&locales_path, &config);
        Self {
            storage_path,
            locales_path,
            config,
            locale_registry,
        }
    }

    /// Creates a new service factory with custom configuration
    pub fn with_config(storage_path: PathBuf, locales_path: PathBuf, config: AppConfig) -> Self {
        let locale_registry = Self::build_registry(&locales_path, &config);
        Self {
            storage_path,
            locales_path,
            config,
            locale_registry,
        }
    }

    /// Discover locales from disk at boot. A misconfigured default or
    /// an empty locale directory is treated as a fatal config error —
    /// fail fast so the operator notices at startup rather than when
    /// the first magic-link mail is queued.
    fn build_registry(locales_path: &Path, config: &AppConfig) -> Arc<LocaleRegistry> {
        let registry = LocaleRegistry::discover(locales_path, &config.i18n.default_locale)
            .unwrap_or_else(|e| {
                panic!(
                    "Failed to build locale registry from {}: {}. \
                     Check OXICLOUD_DEFAULT_LOCALE and that static/locales/ \
                     contains valid *.json files.",
                    locales_path.display(),
                    e
                )
            });
        Arc::new(registry)
    }

    /// Gets the configuration
    pub fn config(&self) -> &AppConfig {
        &self.config
    }

    /// Gets the storage path
    pub fn storage_path(&self) -> &PathBuf {
        &self.storage_path
    }

    /// Initializes the core system services.
    ///
    /// Requires a `PgPool` because `DedupService` stores its index in PostgreSQL.
    /// The `maintenance_pool` is given to `DedupService` for long-running
    /// operations (verify_integrity, garbage_collect) so they cannot starve
    /// the primary pool.
    pub async fn create_core_services(
        &self,
        db_pool: &Arc<PgPool>,
        maintenance_pool: &Arc<PgPool>,
    ) -> Result<CoreServices, DomainError> {
        // Path service (still needed for blob storage root + thumbnails)
        let path_service = Arc::new(PathService::new(self.storage_path.clone()));

        // File content cache for ultra-fast file serving (hot files in RAM)
        let file_content_cache = Arc::new(FileContentCache::new(FileContentCacheConfig {
            max_file_size: 10 * 1024 * 1024,   // 10MB max per file
            max_total_size: 512 * 1024 * 1024, // 512MB total cache
            max_entries: 10000,                // Up to 10k files
        }));
        tracing::info!("FileContentCache initialized: max 10MB/file, 512MB total, 10k entries");

        // Thumbnail service for thumbnail generation with timeout protection
        let thumbnail_service = Arc::new(
            crate::infrastructure::services::thumbnail_service::ThumbnailService::new(
                &self.storage_path,
                5000,              // max 5000 thumbnails in cache
                100 * 1024 * 1024, // max 100MB cache
                Some(self.config.timeouts.thumbnail_timeout()),
            ),
        );
        // Initialize thumbnail directories
        thumbnail_service.initialize().await?;

        // Chunked upload service for large files (>10MB).
        // Root for both REST (`/api/uploads/...`) and NC (`/dav/uploads/...`)
        // chunked sessions: honour `OXICLOUD_CHUNK_DIR` when set so sysadmins
        // can put session directories on fast storage (NVMe) or on the same
        // filesystem as `.blobs/` (turns the final blob promotion into an
        // atomic rename instead of a cross-FS copy). Falls back to
        // `{storage_path}/.uploads/` when unset — backwards-compatible with
        // every existing deployment.
        let chunk_root = self
            .config
            .storage
            .chunk_dir
            .clone()
            .unwrap_or_else(|| std::path::PathBuf::from(&self.storage_path).join(".uploads"));
        let chunked_upload_service = Arc::new(
            crate::infrastructure::services::chunked_upload_service::ChunkedUploadService::new(
                chunk_root.clone(),
            )
            .await,
        );

        // Image transcoding service for automatic WebP conversion
        let image_transcode_service = Arc::new(
            crate::infrastructure::services::image_transcode_service::ImageTranscodeService::new(
                &self.storage_path,
                2000,             // max 2000 transcoded images in cache
                50 * 1024 * 1024, // max 50MB in-memory cache
            ),
        );
        image_transcode_service.initialize().await?;

        // Build blob storage backend.
        //
        // Two paths, chosen by whether `_ENTRIES` (or the legacy
        // synthesis) populated `storage_entries` at parse time:
        //
        // * `storage_entries` non-empty — multi-entry mode
        //   (`docs/plan/storage-multi-entry.md`). Look up the active
        //   entry name in `auth.admin_settings.storage.active_backend_name`,
        //   fall back to the first entry when unset (fresh install,
        //   no admin has picked one yet), fail-fast when the DB
        //   points at a name that isn't declared. Build via the
        //   shared `entry_backend::build_entry_backend` factory so
        //   the encryption decorator is applied uniformly here and
        //   in the migration handler (slice 3).
        //
        // * `storage_entries` empty — no explicit storage config at
        //   all (fresh install without env vars). Use the framework
        //   defaults captured on `config.storage` — matches today's
        //   behaviour so a bare `cargo run` in a dev workspace keeps
        //   working. Encryption never applies here (legacy synthesis
        //   would have created an entry if any legacy var was set).
        let active_backend_kind: StorageBackendType;
        // Track which named entry the LIVE backend was built from so
        // the migration handler can name-compare `target != active`
        // without re-reading the DB. `"legacy"` sentinel for the
        // no-entries branch — the migration handler refuses that
        // target name anyway (no entry exists), which is the correct
        // behaviour for the zero-config path.
        let active_backend_name: String;
        let base_backend: Arc<dyn BlobStorageBackend> = if self.config.storage_entries.is_empty() {
            active_backend_kind = self.config.storage.backend.clone();
            active_backend_name = "legacy".to_string();
            tracing::info!(
                "Storage: no OXICLOUD_STORAGE_ENTRIES declared and no legacy vars — using \
                 framework default (backend={:?}, path={:?})",
                active_backend_kind,
                self.storage_path,
            );
            match active_backend_kind {
                StorageBackendType::S3 => {
                    let s3_config = self
                        .config
                        .storage
                        .s3
                        .as_ref()
                        .expect("S3 config required when OXICLOUD_STORAGE_BACKEND=s3");
                    Arc::new(
                        crate::infrastructure::services::s3_blob_backend::S3BlobBackend::new(
                            s3_config,
                        ),
                    )
                }
                StorageBackendType::Azure => {
                    let az_config = self
                        .config
                        .storage
                        .azure
                        .as_ref()
                        .expect("Azure config required when OXICLOUD_STORAGE_BACKEND=azure");
                    Arc::new(
                        crate::infrastructure::services::azure_blob_backend::AzureBlobBackend::new(
                            az_config,
                        ),
                    )
                }
                StorageBackendType::Local => Arc::new(
                    crate::infrastructure::services::local_blob_backend::LocalBlobBackend::new(
                        &self.storage_path,
                    ),
                ),
            }
        } else {
            use crate::infrastructure::services::entry_backend::{
                ActiveEntry, build_entry_backend, resolve_active_entry,
            };
            let active = resolve_active_entry(db_pool, &self.config.storage_entries)
                .await
                .unwrap_or_else(|e| {
                    panic!("Storage boot failed: {e}");
                });
            let entry = match active {
                ActiveEntry::Explicit(e) => {
                    tracing::info!(
                        "Storage: booting on entry `{}` (from auth.admin_settings.storage.active_backend_name)",
                        e.name,
                    );
                    e
                }
                ActiveEntry::Unset => {
                    // No admin pick yet. Fall back to the first entry
                    // in `_ENTRIES` order. `storage_entries` is
                    // guaranteed non-empty in this branch, so [0] is
                    // safe. Loud info-level log so operators see
                    // which entry was chosen for them.
                    let first = &self.config.storage_entries[0];
                    tracing::info!(
                        "Storage: no active_backend_name set in DB — defaulting to first entry \
                         `{}` (declared first in OXICLOUD_STORAGE_ENTRIES). Set explicitly via \
                         the admin storage tab or `oxicloud --select-storage <name>` to pin.",
                        first.name,
                    );
                    first
                }
            };
            active_backend_kind = entry.backend.clone();
            active_backend_name = entry.name.clone();
            build_entry_backend(entry, &self.storage_path)
        };
        // Shared mutable handle to the active-entry name. Wrapped
        // here rather than at the field type so the two String
        // literal writes above stay simple; wrapping happens once
        // just before the struct init.
        let active_backend_name = Arc::new(std::sync::RwLock::new(active_backend_name));

        // Stack decorators: retry → encryption → cache (inner-to-outer).
        //
        // Encryption is applied INSIDE build_entry_backend (per-entry
        // key), so it's already on the base returned above when the
        // entry declared one. The legacy-vars-no-entries branch skips
        // encryption (that path exists only for zero-storage-config
        // installs); if legacy synthesis fired it produced an entry
        // and we're on the entry branch instead.
        //
        // Retry + cache are app-level (config.storage.retry/cache), not
        // per-entry, so they still apply here. `active_backend_kind`
        // gates the "remote-only" decorators the same as before.
        let mut blob_backend: Arc<dyn BlobStorageBackend> = base_backend;

        // Retry decorator (for remote backends)
        if self.config.storage.retry.enabled && active_backend_kind != StorageBackendType::Local {
            use crate::infrastructure::services::retry_blob_backend::{
                RetryBlobBackend, RetryPolicy,
            };
            let policy = RetryPolicy {
                max_retries: self.config.storage.retry.max_retries,
                initial_backoff: std::time::Duration::from_millis(
                    self.config.storage.retry.initial_backoff_ms,
                ),
                max_backoff: std::time::Duration::from_millis(
                    self.config.storage.retry.max_backoff_ms,
                ),
                backoff_multiplier: self.config.storage.retry.backoff_multiplier,
            };
            blob_backend = Arc::new(RetryBlobBackend::new(blob_backend, policy));
            tracing::info!("Blob storage retry decorator enabled");
        }

        // Encryption decorator — legacy path only.
        //
        // When `storage_entries` is non-empty, encryption is already
        // applied inside `build_entry_backend` from the entry's own
        // pair-list (head-pair key). This block is the
        // pre-multi-entry fallback that reads the flat
        // `OXICLOUD_STORAGE_ENCRYPTION_*` vars — reachable only for
        // fresh installs with no explicit storage config at all
        // (legacy synthesis would have created an entry if any legacy
        // var, including the encryption ones, was present).
        if self.config.storage_entries.is_empty() && self.config.storage.encryption.enabled {
            use crate::infrastructure::services::encrypted_blob_backend::EncryptedBlobBackend;
            let key_b64 = self
                .config
                .storage
                .encryption
                .key_base64
                .as_ref()
                .expect("OXICLOUD_STORAGE_ENCRYPTION_KEY required when encryption is enabled");
            let key_bytes =
                base64::Engine::decode(&base64::engine::general_purpose::STANDARD, key_b64)
                    .expect("OXICLOUD_STORAGE_ENCRYPTION_KEY must be valid base64");
            let key: [u8; 32] = key_bytes.try_into().expect(
                "OXICLOUD_STORAGE_ENCRYPTION_KEY must be exactly 32 bytes (base64 of 32 bytes)",
            );
            blob_backend = Arc::new(EncryptedBlobBackend::new(blob_backend, &key));
            tracing::info!("Blob storage encryption decorator enabled (AES-256-GCM) — legacy path");
        }

        // Cache decorator (for remote backends only)
        if self.config.storage.cache.enabled && active_backend_kind != StorageBackendType::Local {
            use crate::infrastructure::services::cached_blob_backend::{
                BlobCacheConfig as CacheCfg, CachedBlobBackend,
            };
            let cache_path = self
                .config
                .storage
                .cache
                .cache_path
                .as_ref()
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|| self.storage_path.join(".blob-cache"));
            let cfg = CacheCfg {
                cache_dir: cache_path,
                max_cache_bytes: self.config.storage.cache.max_size_bytes,
            };
            blob_backend = Arc::new(CachedBlobBackend::new(blob_backend, &cfg));
            tracing::info!("Blob storage LRU disk cache enabled");
        }

        // Wrap the fully-decorated stack in the hot-swap wrapper.
        // Every downstream consumer holds `Arc<dyn BlobStorageBackend>`
        // as before; the wrapper is transparent from their point of
        // view. The `Arc<SwappableBlobBackend>` reference we retain
        // here (stored on `AppState.blob_backend_hot_swap`) is what
        // the migration handler calls `.swap()` on when cutover
        // completes — no restart needed. See
        // `swappable_blob_backend.rs` for the delegation contract.
        let blob_backend_hot_swap = Arc::new(
            crate::infrastructure::services::swappable_blob_backend::SwappableBlobBackend::new(
                blob_backend,
            ),
        );
        let blob_backend: Arc<dyn BlobStorageBackend> = blob_backend_hot_swap.clone();

        // Blob lifecycle — thumbnail disk-file cleanup when blob ref_count hits zero.
        // ThumbnailService (not ThumbnailRefreshHook) is used here to avoid a circular
        // Arc: DedupService→BlobLifecycleService→ThumbnailRefreshHook→DedupService.
        let blob_lifecycle =
            Arc::new(BlobLifecycleService::new().with_hook(thumbnail_service.clone()));

        // Hold a clone of the fully-decorated blob backend for use by
        // `blobs_consistency` (physical-existence + bit-rot probes)
        // further down the DI chain. Must be captured BEFORE the
        // `dedup_service` construction below because that call moves
        // `blob_backend` into DedupService.
        let blob_backend_for_consistency = blob_backend.clone();

        // Deduplication service — PRIMARY blob storage engine (PostgreSQL-backed index)
        let dedup_service = Arc::new(
            crate::infrastructure::services::dedup_service::DedupService::new(
                blob_backend,
                db_pool.clone(),
                maintenance_pool.clone(),
            )
            .with_blob_lifecycle(blob_lifecycle),
        );
        dedup_service.initialize().await?;

        // One-time background migration: re-chunk pre-CDC whole-file blobs
        // into chunk manifests so Range reads (and, with encryption, partial
        // decrypts) stop paying for the entire blob. No-op once converged.
        if self.config.storage.legacy_rechunk_enabled {
            dedup_service.spawn_legacy_rechunk();
        } else {
            tracing::info!(
                "Legacy re-chunk migration disabled (OXICLOUD_LEGACY_RECHUNK=false) — \
                 pre-CDC whole-file blobs, if any, will keep using the legacy read path"
            );
        }

        tracing::info!(
            "Core services initialized: path service, file content cache, thumbnails, chunked upload, image transcode, dedup (PRIMARY blob storage)"
        );

        // Audio metadata service — created here so it can be wired into file_lifecycle.
        let audio_metadata_service = self.create_audio_metadata_service(db_pool);

        // Image/video capture-metadata service — extracts EXIF/container capture
        // dates so the Photos timeline groups by real capture time, not upload time.
        let media_metadata_service = self.create_media_metadata_service(db_pool);

        // ThumbnailRefreshHook: handles FileLifecycleHook events (create/update/delete).
        // Implemented on ThumbnailRefreshHook (not ThumbnailService) to avoid circular Arc:
        //   DedupService → BlobLifecycleService → ThumbnailRefreshHook → DedupService.
        // Video frame extractor for thumbnails. Detect ffmpeg once at startup so
        // the choice (real extractor vs. no-op) is logged here instead of failing
        // per upload.
        let video_frame: Arc<dyn VideoFramePort> = {
            let ffmpeg_path =
                std::env::var("OXICLOUD_FFMPEG_PATH").unwrap_or_else(|_| "ffmpeg".to_string());
            if self.config.features.enable_video_thumbnails
                && FfmpegVideoFrameService::is_available(&ffmpeg_path)
            {
                let cpus = std::thread::available_parallelism()
                    .map(|n| n.get())
                    .unwrap_or(4);
                let concurrency = std::env::var("OXICLOUD_VIDEO_THUMBNAIL_CONCURRENCY")
                    .ok()
                    .and_then(|v| v.parse::<usize>().ok())
                    .unwrap_or((cpus / 2).max(1));
                let timeout = std::time::Duration::from_secs(
                    std::env::var("OXICLOUD_VIDEO_THUMBNAIL_TIMEOUT_SECS")
                        .ok()
                        .and_then(|v| v.parse().ok())
                        .unwrap_or(30),
                );
                tracing::info!(
                    "🎬 Video thumbnails enabled (ffmpeg '{}', concurrency {})",
                    ffmpeg_path,
                    concurrency
                );
                Arc::new(FfmpegVideoFrameService::new(
                    ffmpeg_path,
                    concurrency,
                    timeout,
                ))
            } else {
                if self.config.features.enable_video_thumbnails {
                    tracing::warn!(
                        "🎬 Video thumbnails enabled but ffmpeg not found at '{}' \
                         (set OXICLOUD_FFMPEG_PATH) — videos will have no thumbnail",
                        ffmpeg_path
                    );
                } else {
                    tracing::info!("🎬 Video thumbnails disabled");
                }
                Arc::new(NoopVideoFrameService)
            }
        };
        // Cap on bytes streamed to a temp file for frame extraction (default 2 GB).
        // saturating_mul so an absurd MB value can't silently wrap to a tiny cap.
        let video_max_bytes: u64 = std::env::var("OXICLOUD_VIDEO_THUMBNAIL_MAX_MB")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(2048)
            .saturating_mul(1024 * 1024);

        let thumbnail_refresh_hook = Arc::new(ThumbnailRefreshHook::new(
            thumbnail_service.clone(),
            dedup_service.clone(),
            video_frame,
            video_max_bytes,
        ));

        // Build the unified FileLifecycleService dispatcher.
        let mut fls = FileLifecycleService::new().with_hook(thumbnail_refresh_hook);
        if let Some(audio) = &audio_metadata_service {
            fls = fls.with_hook(audio.clone());
        }
        fls = fls.with_hook(media_metadata_service.clone());
        if self.config.features.enable_faces {
            fls = fls.with_hook(self.create_face_indexing_service(db_pool));
        }
        let file_lifecycle = Arc::new(fls);

        // Empty periodic-job registry; services register themselves
        // downstream during their own creation. `SchedulerEngine::start`
        // fires at the end of `build_app_state` once all registrations
        // have landed.
        let job_registry = Arc::new(JobRegistry::new());

        // Recoverable-run engine's PG provider (`jobs.recoverable_runs`).
        // Runs on the maintenance pool so a long-running scan's cursor
        // updates never contend with the request-path pool. Boot-time
        // crash-recovery sweep fires in `build_app_state` after the
        // provider is placed on `AppState`.
        let job_store_provider = Arc::new(
            crate::infrastructure::scheduler::PgJobStoreProvider::new(maintenance_pool.clone()),
        );

        Ok(CoreServices {
            path_service,
            file_content_cache,
            thumbnail_service,
            file_lifecycle,
            audio_metadata_service,
            media_metadata_service,
            chunked_upload_service,
            image_transcode_service,
            dedup_service,
            zip_service: None, // Placeholder - replaced after app services init
            config: self.config.clone(),
            job_registry,
            job_store_provider,
            blob_backend: blob_backend_for_consistency,
            blob_backend_hot_swap,
            active_backend_name,
        })
    }

    /// Initializes the repository services (blob-storage model).
    ///
    /// Requires a PgPool since all metadata lives in PostgreSQL.
    pub fn create_repository_services(
        &self,
        core: &CoreServices,
        db_pool: &Arc<PgPool>,
    ) -> RepositoryServices {
        // Folder repository — PostgreSQL-backed virtual folders
        let folder_repo_concrete = Arc::new(FolderDbRepository::new(db_pool.clone()));
        let folder_repository: Arc<FolderDbRepository> = folder_repo_concrete.clone();

        // File repositories — PostgreSQL metadata + blob content via DedupService
        let file_read_repository: Arc<FileBlobReadRepository> =
            Arc::new(FileBlobReadRepository::new(
                db_pool.clone(),
                core.dedup_service.clone(),
                folder_repo_concrete.clone(),
            ));

        let file_write_repository: Arc<FileBlobWriteRepository> =
            Arc::new(FileBlobWriteRepository::new(
                db_pool.clone(),
                core.dedup_service.clone(),
                // Shared blob-hash cache: the write side invalidates entries
                // on content swaps/deletes so reads never serve stale blobs.
                file_read_repository.blob_hash_cache(),
            ));

        // I18n repository — file-system backed, gated by the locale
        // registry built at factory construction.
        let i18n_repository = Arc::new(FileSystemI18nService::new(
            self.locales_path.clone(),
            self.locale_registry.clone(),
        ));

        // Trash repository — reads soft-delete flags from storage.files/folders
        let trash_repository = if core.config.features.enable_trash {
            Some(Arc::new(TrashDbRepository::new(
                db_pool.clone(),
                core.config.storage.trash_retention_days,
            )) as Arc<TrashDbRepository>)
        } else {
            None
        };

        // File metadata repository — EXIF/media metadata for images
        let file_metadata_repository = Arc::new(FileMetadataRepository::new(db_pool.clone()));

        tracing::info!(
            "Repository services initialized with 100% blob storage model (PG metadata + DedupService blobs)"
        );

        RepositoryServices {
            folder_repository,
            folder_repo_concrete,
            file_read_repository,
            file_write_repository,
            file_metadata_repository,
            i18n_repository,
            trash_repository,
        }
    }

    /// Initializes the application services
    #[allow(clippy::too_many_arguments)] // DI composition root — params are services, not a smell
    pub fn create_application_services(
        &self,
        core: &CoreServices,
        repos: &RepositoryServices,
        trash_service: Option<Arc<TrashService>>,
        authz: &Arc<PgAclEngine>,
        drive_repo: &Arc<crate::infrastructure::repositories::pg::DrivePgRepository>,
        storage_usage: &Arc<StorageUsageService>,
        content_index: Option<Arc<TantivyContentIndex>>,
        plugin_dispatch: Option<
            Arc<dyn crate::application::ports::plugin_ports::PluginDispatchPort>,
        >,
        mount_router: Arc<crate::application::services::external_mount_router::MountRouter>,
        resource_access_hook: Option<
            Arc<dyn crate::application::ports::resource_access_hook::ResourceAccessHook>,
        >,
    ) -> ApplicationServices {
        // Main services
        let folder_service = Arc::new(
            FolderService::new(
                repos.folder_repository.clone(),
                authz.clone(),
                // Same dispatcher TrashService uses, so the cascade hook in
                // `delete_folder_with_perms` fans out to the same handlers
                // (thumbnails, metadata, …) as a single-file delete.
                core.file_lifecycle.clone(),
                mount_router.clone(),
            )
            // D5 cross-drive move gate reads policies via the same
            // drive repo every other policy uses. Wired here so
            // `move_folder_with_perms` can enforce
            // `forbid_cross_drive_move` without a separate construction path.
            .with_drive_repo(drive_repo.clone())
            // Destination-drive quota pre-check on cross-drive folder
            // MOVE. Reuses the `check_drive_quota` the upload path
            // already runs. Without this, a Move that would push the
            // destination past its cap succeeds silently.
            .with_storage_usage(storage_usage.clone()),
        );

        // Built before the upload/management services so the plugin lifecycle
        // bridge (which looks file metadata up by id) can be wired into the
        // dispatcher they receive. It depends only on repos + core, never on
        // the upload service, so the reorder is safe.
        let file_retrieval_service = {
            let mut svc = FileRetrievalService::new_with_cache(
                repos.file_read_repository.clone(),
                core.file_content_cache.clone(),
                core.image_transcode_service.clone(),
                authz.clone(),
            )
            .with_mount_router(mount_router.clone());
            if let Some(hook) = resource_access_hook.clone() {
                svc = svc.with_resource_access_hook(hook);
            }
            Arc::new(svc)
        };

        // Effective lifecycle dispatcher: the core hooks (thumbnails, metadata)
        // plus, when the plugins feature is enabled, the WASM plugin bridge.
        let file_lifecycle =
            self.effective_file_lifecycle(core, &file_retrieval_service, plugin_dispatch);

        let file_upload_service = Arc::new({
            let mut svc = FileUploadService::new_with_read(
                repos.file_write_repository.clone(),
                repos.file_read_repository.clone(),
            )
            .with_content_cache(core.file_content_cache.clone())
            .with_file_lifecycle_hook(file_lifecycle.clone())
            // `with_storage_usage_service` wires the post-write delta
            // hook (`maybe_update_storage_usage`). Without this the
            // hook is dead code — both per-user and per-drive
            // `used_bytes` deltas would silently no-op and the
            // counters drift until the next reconciliation sweep
            // (default 10 min). `with_instant_upload` below stashes
            // the same service under a different field used only by
            // the dedup-instant-upload check, so they're not
            // interchangeable.
            .with_storage_usage_service(storage_usage.clone())
            .with_instant_upload(
                authz.clone(),
                core.dedup_service.clone(),
                storage_usage.clone(),
            );
            if let Some(hook) = resource_access_hook.clone() {
                svc = svc.with_resource_access_hook(hook);
            }
            svc
        });

        // Delta-upload protocol — chunk negotiation over the same dedup
        // store. Bounded by the same whole-file ceiling as byte uploads.
        let delta_upload_service = Arc::new(
            crate::application::services::delta_upload_service::DeltaUploadService::new(
                core.dedup_service.clone(),
                file_upload_service.clone(),
                repos.file_read_repository.clone(),
                storage_usage.clone(),
                authz.clone(),
                self.config.storage.max_upload_size as u64,
                self.config.storage.chunk_max_bytes as u64,
            ),
        );

        // FileManagementService — ref_count handled by PG trigger, no dedup port needed
        let file_management_service = Arc::new({
            let mut svc = FileManagementService::with_trash(
                repos.file_write_repository.clone(),
                trash_service.clone(),
                Some(repos.file_read_repository.clone()),
                Some(repos.folder_repository.clone()),
                Some(core.file_content_cache.clone()),
                authz.clone(),
            )
            .with_file_lifecycle_hook(file_lifecycle.clone())
            .with_mount_router(mount_router.clone())
            // D5 cross-drive move gate reads policies via the same
            // drive repo every other policy uses. Wired here so
            // `move_file_with_perms` can enforce `forbid_cross_drive_move`
            // without a separate construction path.
            .with_drive_repo(drive_repo.clone())
            // Destination-drive quota pre-check on cross-drive file
            // MOVE. Same rationale as the folder side above.
            .with_storage_usage(storage_usage.clone());
            if let Some(hook) = resource_access_hook.clone() {
                svc = svc.with_resource_access_hook(hook);
            }
            svc
        });

        // Streams uploads to external mount providers (bypasses the CAS).
        let external_upload_service = Arc::new(
            crate::application::services::external_upload_service::ExternalUploadService::new(
                authz.clone(),
            ),
        );

        let file_use_case_factory = Arc::new(AppFileUseCaseFactory::new(
            repos.file_read_repository.clone(),
            repos.file_write_repository.clone(),
            authz.clone(),
        ));

        let i18n_service = Arc::new(I18nApplicationService::new(repos.i18n_repository.clone()));

        // Search service with cache. The optional content index widens the
        // same `/api/search` endpoint to full-text content matches.
        let content_index_port: Option<
            Arc<dyn crate::application::ports::content_index_ports::ContentIndexPort>,
        > = content_index.map(|idx| idx as _);
        let search_service: Option<Arc<SearchService>> = Some(Arc::new(SearchService::new(
            repos.file_read_repository.clone(),
            repos.folder_repository.clone(),
            content_index_port,
            Some(authz.clone()),
            Some(drive_repo.clone()),
            300, // Cache TTL in seconds (5 minutes)
            // Byte budget for cached result pages (weigher-bounded, 32 MiB
            // default; env OXICLOUD_SEARCH_CACHE_MAX_BYTES). Replaces the old
            // entry-count capacity, which let 500-row pages keyed by
            // user×query×offset×limit pin hundreds of MB for the TTL.
            self.config.search_cache.max_bytes,
        )));

        tracing::info!("Application services initialized");

        ApplicationServices {
            // Concrete types for handlers that need them
            folder_service_concrete: folder_service.clone(),
            // Traits for abstraction
            folder_service,
            file_upload_service,
            delta_upload_service,
            file_retrieval_service,
            file_management_service,
            external_upload_service,
            file_use_case_factory,
            i18n_service,
            trash_service, // Already set via parameter
            search_service,
            share_service: None,     // Configured later with create_share_service
            favorites_service: None, // Configured later with create_favorites_service
            recent_service: None,    // Configured later with create_recent_service
            audio_metadata_service: core.audio_metadata_service.clone(),
            media_metadata_service: core.media_metadata_service.clone(),
        }
    }

    /// Builds the file lifecycle dispatcher handed to the upload/management
    /// services. By default this is just the core dispatcher (thumbnails,
    /// metadata). When a plugin dispatch is present, it wraps the core dispatcher
    /// together with the WASM plugin bridge so plugins observe `file.uploaded`
    /// events without any of the core hooks being aware of them.
    fn effective_file_lifecycle(
        &self,
        core: &CoreServices,
        file_retrieval: &Arc<FileRetrievalService>,
        plugin_dispatch: Option<
            Arc<dyn crate::application::ports::plugin_ports::PluginDispatchPort>,
        >,
    ) -> Arc<dyn crate::application::ports::file_lifecycle::FileLifecycleHook> {
        if let Some(dispatch) = plugin_dispatch {
            use crate::application::adapters::plugin_lifecycle_hook::PluginLifecycleHook;

            let bridge = Arc::new(PluginLifecycleHook::new(dispatch, file_retrieval.clone()));
            let composite = FileLifecycleService::new()
                .with_hook(core.file_lifecycle.clone())
                .with_hook(bridge);
            return Arc::new(composite);
        }

        core.file_lifecycle.clone()
    }

    /// The single plugin manager, exposed as the two ports it serves: the
    /// dispatch port (shared by every event bridge — file, user, …) and the
    /// management port (stored on `AppState` for the admin API). Both wrap the
    /// *same* `Arc`, so an install or toggle through the management port takes
    /// effect on the live dispatch path with no restart.
    ///
    /// Returns trait objects so call sites stay feature-agnostic; the
    /// `#[cfg(feature = "plugins")]` is confined to this body. Both are `None`
    /// when the feature is off or `OXICLOUD_ENABLE_PLUGINS` is false.
    #[allow(clippy::type_complexity)]
    fn create_plugin_ports(
        &self,
    ) -> (
        Option<Arc<dyn crate::application::ports::plugin_ports::PluginDispatchPort>>,
        Option<Arc<dyn crate::application::ports::plugin_ports::PluginManagementPort>>,
    ) {
        #[cfg(feature = "plugins")]
        {
            if self.config.plugins.enabled {
                let dir = self
                    .config
                    .plugins
                    .plugins_dir
                    .clone()
                    .unwrap_or_else(|| self.config.storage_path.join(".plugins"));

                // Resolve the log root to a sibling of the plugins dir by default
                // so an uninstall never wipes another plugin's logs, and pass it
                // into the manager via the config it owns.
                let mut plugin_config = self.config.plugins.clone();
                if plugin_config.log_dir.is_none() {
                    plugin_config.log_dir = Some(self.config.storage_path.join(".plugin-logs"));
                }

                let manager = Arc::new(
                    crate::infrastructure::services::plugins::ExtismPluginManager::load_from_dir(
                        plugin_config,
                        &dir,
                    ),
                );
                tracing::info!(
                    target: "oxicloud::plugins",
                    loaded = manager.loaded_count(),
                    "plugin manager initialized"
                );

                let dispatch: Arc<dyn crate::application::ports::plugin_ports::PluginDispatchPort> =
                    manager.clone();
                let management: Arc<
                    dyn crate::application::ports::plugin_ports::PluginManagementPort,
                > = manager.clone();

                // Background maintenance: prune each plugin's rotated log
                // segments by age + aggregate size on a schedule. Depends only on
                // the log store + the management port, so no special ordering.
                crate::infrastructure::services::plugins::PluginLogMaintenanceService::new(
                    manager.log_store(),
                    management.clone(),
                    6, // hours between sweeps
                )
                .start();

                // Periodic idle-eviction of cached compiled modules: frees the
                // memory of plugins not invoked within the configured TTL; the
                // next event recompiles transparently. Cheap, so it ticks often.
                {
                    let evictor = manager.clone();
                    tokio::spawn(async move {
                        let mut tick = tokio::time::interval(std::time::Duration::from_secs(60));
                        loop {
                            tick.tick().await;
                            evictor.evict_idle_compiled();
                        }
                    });
                }

                return (Some(dispatch), Some(management));
            }
        }
        (None, None)
    }

    /// Creates the audio metadata service (extracts ID3 tags from audio files)
    pub fn create_audio_metadata_service(
        &self,
        db_pool: &Arc<PgPool>,
    ) -> Option<Arc<AudioMetadataService>> {
        if !self.config.features.enable_music {
            tracing::info!("Audio metadata service is disabled (music feature disabled)");
            return None;
        }
        let blob_root = self.storage_path.join(".blobs");
        Some(Arc::new(AudioMetadataService::new(
            db_pool.clone(),
            blob_root,
        )))
    }

    /// Creates the image/video capture-metadata service (EXIF + container
    /// creation dates). Always enabled — the Photos timeline relies on it.
    pub fn create_media_metadata_service(
        &self,
        db_pool: &Arc<PgPool>,
    ) -> Arc<MediaMetadataService> {
        let blob_root = self.storage_path.join(".blobs");
        Arc::new(MediaMetadataService::new(db_pool.clone(), blob_root))
    }

    /// Creates the trash service
    pub async fn create_trash_service(
        &self,
        repos: &RepositoryServices,
        core: &CoreServices,
        authz: &Arc<PgAclEngine>,
        drive_repo: &Arc<crate::infrastructure::repositories::pg::DrivePgRepository>,
    ) -> Option<Arc<TrashService>> {
        if !self.config.features.enable_trash {
            tracing::info!("Trash service is disabled in configuration");
            return None;
        }

        let trash_repo = repos.trash_repository.as_ref()?;

        // Wire ports directly to TrashService — no adapter layer needed
        let service = Arc::new(
            TrashService::new(
                trash_repo.clone(),
                repos.file_write_repository.clone(),
                repos.folder_repository.clone(),
                core.dedup_service.clone(),
                Some(core.file_content_cache.clone()),
                authz.clone(),
                drive_repo.clone(),
            )
            .with_file_deleted_hook(core.file_lifecycle.clone()),
        );

        // Initialize cleanup service (bulk-deletes expired items in 2 SQL
        // queries, then GCs zero-reference blobs — including chunks orphaned
        // by aborted streaming uploads).
        //
        // Registers with the periodic-job scheduler
        // (`docs/plan/job-registry.md` Part 1) instead of spawning its own
        // tokio interval loop. `SchedulerEngine::start` fires the actual
        // supervisor task at the end of `build_app_state`.
        // Self-registering constructor chain — TrashCleanupService owns
        // its interval + timeout shape; DI only supplies deps.
        // `.register(&reg)` fires the uniform `job.registered` log line
        // and panics on wiring error (duplicate name = boot must fail
        // loud). See `docs/plan/job-registry.md` Part 1.
        let _ = Arc::new(TrashCleanupService::new(
            trash_repo.clone(),
            core.dedup_service.clone(),
            24, // Run cleanup every 24 hours
        ))
        .register(&core.job_registry)
        .await;

        Some(service as Arc<TrashService>)
    }

    /// Creates the sharing service. `search_service` is threaded through
    /// so create/delete of a share can flush the caller's cached search
    /// pages (2026-07-26 — per-user is_shared invalidation). `None` when
    /// search is disabled; the flush becomes a no-op.
    pub fn create_share_service(
        &self,
        repos: &RepositoryServices,
        db_pool: &Arc<PgPool>,
        authorization: &Arc<crate::infrastructure::services::pg_acl_engine::PgAclEngine>,
        drive_repo: &Arc<crate::infrastructure::repositories::pg::DrivePgRepository>,
        search_service: Option<Arc<SearchService>>,
    ) -> Option<Arc<ShareService>> {
        if !self.config.features.enable_file_sharing {
            tracing::info!("File sharing service is disabled in configuration");
            return None;
        }

        let share_repository = Arc::new(SharePgRepository::new(db_pool.clone()));

        // Build a password hasher for share password verification
        let password_hasher: Arc<Argon2PasswordHasher> = Arc::new(
            crate::infrastructure::services::password_hasher::Argon2PasswordHasher::new(
                self.config.auth.hash_memory_cost,
                self.config.auth.hash_time_cost,
                self.config.auth.hash_parallelism,
            ),
        );

        let service = Arc::new(ShareService::new(
            Arc::new(self.config.clone()),
            share_repository,
            repos.file_read_repository.clone(),
            repos.folder_repository.clone(),
            drive_repo.clone(),
            password_hasher,
            authorization.clone(),
            // Optional per-user search-cache invalidator — set here so
            // create/delete of a share drops the sharer's cached search
            // pages (2026-07-26). `None` when search is disabled.
            search_service.clone(),
        ));

        tracing::info!("File sharing service initialized");
        Some(service)
    }

    /// Creates the favorites service (requires database + authz engine
    /// for the Read gate on `add_to_favorites` — see the post-Drive
    /// AuthZ audit). `search_service` is threaded through so add/remove
    /// can flush the caller's cached search pages (2026-07-26 — per-user
    /// is_favorite invalidation). `None` when search is disabled.
    pub fn create_favorites_service(
        &self,
        db_pool: &Arc<PgPool>,
        authorization: &Arc<PgAclEngine>,
        search_service: Option<Arc<SearchService>>,
    ) -> Arc<FavoritesService> {
        let repo = Arc::new(
            crate::infrastructure::repositories::pg::FavoritesPgRepository::new(db_pool.clone()),
        );
        let service = Arc::new(FavoritesService::new(
            repo,
            authorization.clone(),
            search_service,
        ));
        tracing::info!("Favorites service initialized");
        service
    }

    /// Creates the recent items service (requires database + authz
    /// engine for the Read gate on `record_item_access` — see the
    /// post-Drive AuthZ audit).
    pub fn create_recent_service(
        &self,
        db_pool: &Arc<PgPool>,
        authorization: &Arc<PgAclEngine>,
    ) -> Arc<RecentService> {
        let repo = Arc::new(
            crate::infrastructure::repositories::pg::RecentItemsPgRepository::new(db_pool.clone()),
        );
        let service = Arc::new(RecentService::new(
            repo,
            authorization.clone(),
            50, // Maximum recent items per user
        ));
        tracing::info!("Recent items service initialized");
        service
    }

    /// Creates the Places (photo map) service. Reuses the existing file-read
    /// repository — the data is the caller's Photos-scope geotagged photos
    /// (§15: default personal drive + drives with
    /// `include_in_photo_index = true` AND caller has Read).
    /// Group-membership expansion is inline in the SQL via
    /// `storage.caller_group_ids`, so the service needs no AuthZ engine
    /// handle.
    pub fn create_places_service(
        &self,
        file_read: &Arc<FileBlobReadRepository>,
    ) -> Arc<PlacesService> {
        let service = Arc::new(PlacesService::new(file_read.clone()));
        tracing::info!("Places service initialized");
        service
    }

    /// Creates the face-indexing lifecycle hook (People feature). Picks the
    /// real ONNX analyzer when the `faces-onnx` feature is compiled in and the
    /// operator has configured the runtime + models; otherwise the inert no-op
    /// analyzer (see [`Self::build_face_analyzer`]).
    pub fn create_face_indexing_service(
        &self,
        db_pool: &Arc<PgPool>,
    ) -> Arc<crate::infrastructure::services::face_indexing_service::FaceIndexingService> {
        let blob_root = self.storage_path.join(".blobs");
        let analyzer = self.build_face_analyzer();
        Arc::new(
            crate::infrastructure::services::face_indexing_service::FaceIndexingService::new(
                db_pool.clone(),
                blob_root,
                analyzer,
            ),
        )
    }

    /// Selects the face analyzer. With the `faces-onnx` feature and a fully
    /// configured runtime + models, loads the real ONNX analyzer; any missing
    /// piece or load failure degrades gracefully to the no-op analyzer (logged)
    /// so startup never fails on biometric configuration.
    fn build_face_analyzer(
        &self,
    ) -> Arc<dyn crate::application::ports::face_ports::FaceAnalyzerPort> {
        #[cfg(feature = "faces-onnx")]
        {
            let f = &self.config.faces;
            if let (Some(dylib), Some(detector), Some(embedder)) = (
                f.ort_dylib.as_ref(),
                f.detector_model.as_ref(),
                f.embedder_model.as_ref(),
            ) {
                use crate::infrastructure::services::onnx_face_analyzer::{
                    OnnxFaceAnalyzer, OnnxLoadConfig,
                };
                let cfg = OnnxLoadConfig {
                    dylib,
                    detector,
                    embedder,
                    det_size: f.det_size,
                    det_threshold: f.det_threshold,
                    nms_threshold: f.nms_threshold,
                    intra_threads: f.intra_threads,
                };
                match OnnxFaceAnalyzer::load(&cfg) {
                    Ok(analyzer) => {
                        tracing::info!("Face analyzer: ONNX models loaded");
                        return Arc::new(analyzer);
                    }
                    Err(e) => {
                        tracing::warn!(
                            "Face analyzer: failed to load ONNX models ({e}); \
                             falling back to no-op analyzer"
                        );
                    }
                }
            } else {
                tracing::info!(
                    "Face analyzer: faces-onnx compiled but runtime/models not fully \
                     configured; using no-op analyzer"
                );
            }
        }
        Arc::new(crate::infrastructure::services::noop_face_analyzer::NoopFaceAnalyzer)
    }

    /// Creates the People (faces) read/clustering service.
    pub fn create_people_service(&self, db_pool: &Arc<PgPool>) -> Arc<PeopleService> {
        let repo = Arc::new(
            crate::infrastructure::repositories::pg::FacePgRepository::new(db_pool.clone()),
        );
        let service = Arc::new(PeopleService::new(repo));
        tracing::info!("People service initialized");
        service
    }

    /// Preloads translations for every locale in the registry. Build
    /// the registry at startup via `LocaleRegistry::discover` and pass
    /// the resulting list here.
    pub async fn preload_translations(&self, i18n_service: &I18nApplicationService) {
        let locales = i18n_service.available_locales().await;
        for locale in locales {
            let code = locale.as_str().to_string();
            if let Err(e) = i18n_service.load_translations(locale).await {
                tracing::warn!("Failed to load translations for {}: {}", code, e);
            }
        }
        tracing::info!("Translations preloaded");
    }

    /// Creates the storage usage service (requires database)
    ///
    /// Uses the `maintenance_pool` for batch operations
    /// (`update_all_users_storage_usage`) to avoid starving user requests.
    ///
    /// Note: this is `async` (unlike the pre-migration version) because
    /// registration with `core.job_registry` requires an `await`.
    pub async fn create_storage_usage_service(
        &self,
        _repos: &RepositoryServices,
        db_pool: &Arc<PgPool>,
        maintenance_pool: &Arc<PgPool>,
        drive_repo: Arc<crate::infrastructure::repositories::pg::DrivePgRepository>,
        core: &CoreServices,
    ) -> Arc<StorageUsageService> {
        let user_repository = Arc::new(
            crate::infrastructure::repositories::pg::UserPgRepository::new(db_pool.clone()),
        );
        // The `drive_repo` passed in is the SAME instance held on
        // `AppState`, so its `readable_cache` / `default_drive_cache`
        // are the caches the request path reads from. A separately
        // constructed `DrivePgRepository` would have its OWN caches
        // and invalidation would be a no-op observed by nobody —
        // this is the trap that regressed the used_bytes freshness
        // after perf commit `12dc648c`.
        // Keep cached storage usage fresh off the request path: GET
        // /api/auth/me no longer recomputes the O(N) SUM per call; a
        // periodic sweep does it instead (on the maintenance pool).
        // Self-registering constructor chain — StorageUsageService owns
        // its interval-clamping via `Self::reconciliation_interval`.
        Arc::new(
            crate::application::services::storage_usage_service::StorageUsageService::new(
                maintenance_pool.clone(),
                user_repository,
            )
            .with_drive_repo(
                drive_repo
                    as Arc<dyn crate::domain::repositories::drive_repository::DriveRepository>,
            ),
        )
        .register(&core.job_registry, self.config.storage.usage_reconcile_secs)
        .await
    }

    /// Starts the tree-ETag flush job (requires database).
    ///
    /// The statement triggers on `storage.files`/`storage.folders` only
    /// enqueue bump requests into `storage.tree_etag_dirty` (so user-facing
    /// writes take no folder-row locks); this job is the single drainer that
    /// turns them into `tree_modified_at` updates. It must run whenever the
    /// database is up — the triggers are always installed, and an undrained
    /// queue grows unboundedly while folder ETags freeze. Fire-and-forget on
    /// the maintenance pool, like the trash cleanup job.
    fn start_tree_etag_flush_job(&self, maintenance_pool: &Arc<PgPool>) {
        let service =
            crate::infrastructure::services::tree_etag_flush_service::TreeEtagFlushService::new(
                maintenance_pool.clone(),
                self.config.storage.tree_etag_flush_ms,
            );
        service.start_flush_job();
        tracing::info!("Tree-ETag flush service initialized");
    }

    /// Start the primary-pool saturation watchdog (Finding #3). Logs a WARN as
    /// the user-facing pool approaches exhaustion — the early signal for raising
    /// `max_connections` or chasing a slow query before tail latency cliffs.
    /// Skipped when `pool_monitor_interval_secs == 0`.
    fn start_db_pool_monitor(&self, primary_pool: &Arc<PgPool>) {
        let interval = self.config.database.pool_monitor_interval_secs;
        if interval == 0 {
            return;
        }
        crate::infrastructure::services::db_pool_monitor::DbPoolMonitor::new(
            primary_pool.as_ref().clone(),
            "primary",
            self.config.database.max_connections,
            interval,
        )
        .start();
    }

    /// Opens (or rebuilds) the embedded Tantivy content index. Returns the
    /// index plus a reseed flag (true when the on-disk index was missing or
    /// version-stale and must be repopulated from `storage.files`). Any
    /// failure degrades to name-only search instead of failing startup.
    fn create_content_index(&self) -> Option<(Arc<TantivyContentIndex>, bool)> {
        if !self.config.content_search.enabled {
            tracing::info!("Content search is disabled in configuration");
            return None;
        }
        let dir = self
            .config
            .content_search
            .index_dir
            .clone()
            .unwrap_or_else(|| self.storage_path.join(".search-index"));

        match TantivyContentIndex::open_or_rebuild(&dir) {
            Ok((index, needs_reseed)) => {
                tracing::info!(
                    "Content index ready at {} ({} doc(s), reseed: {})",
                    dir.display(),
                    index.num_docs(),
                    needs_reseed
                );
                Some((Arc::new(index), needs_reseed))
            }
            Err(e) => {
                tracing::error!("Content index unavailable — search will be name-only: {e}");
                None
            }
        }
    }

    /// Starts the content-index pipeline on the maintenance pool. The
    /// `storage.files` triggers enqueue unconditionally, so when the feature
    /// is off (or the index failed to open) a discard-only janitor keeps the
    /// dirty queue bounded instead.
    fn start_content_index_job(
        &self,
        maintenance_pool: &Arc<PgPool>,
        core: &CoreServices,
        content_index: Option<(Arc<TantivyContentIndex>, bool)>,
    ) {
        match content_index {
            Some((index, needs_reseed)) => {
                ContentIndexWorker::new(
                    maintenance_pool.clone(),
                    core.dedup_service.clone(),
                    index,
                    self.config.content_search.flush_interval_ms,
                    self.config.content_search.max_extract_file_bytes,
                    self.config.content_search.max_text_bytes,
                )
                .start(needs_reseed);
                tracing::info!("Content-index worker initialized");
            }
            None => ContentIndexWorker::start_drain_only_janitor(maintenance_pool.clone()),
        }
    }

    /// Builds the complete AppState using all factory services.
    ///
    /// This is the main entry point that replaces all manual logic in `main.rs`.
    pub async fn build_app_state(
        &self,
        db_pools: Option<DbPools>,
    ) -> Result<AppState, DomainError> {
        // Database is REQUIRED in 100% blob storage model
        let pools = db_pools.ok_or_else(|| {
            DomainError::internal_error(
                "Database",
                "PostgreSQL database is required for blob storage model",
            )
        })?;

        let pool = Arc::new(pools.primary);
        let maintenance_pool = Arc::new(pools.maintenance);

        // 1. Core services (PgPool needed for DedupService index)
        let core = self.create_core_services(&pool, &maintenance_pool).await?;

        // Register on-demand-only jobs whose owning service lives on
        // CoreServices. Dedup GC has NO periodic tick — trash cleanup's
        // sweep already runs GC as its tail step, so a periodic dedup
        // schedule would double the work. The `register()` method
        // encapsulates the on-demand shape.
        let _ = core
            .dedup_service
            .clone()
            .register(&core.job_registry)
            .await;

        // First recoverable-run tenant (`docs/plan/job-registry.md`
        // Part 2). Iterates `storage.drives` and reports each drive
        // whose cached `used_bytes` differs from `SUM(files.size)`.
        // On-demand only — read-only diagnostic, not periodic.
        // Runs on the maintenance pool alongside the other sweeps.
        let job_store_provider_dyn: Arc<dyn crate::infrastructure::scheduler::JobStoreProvider> =
            core.job_store_provider.clone();
        let _ = Arc::new(
            crate::infrastructure::services::drives_consistency_service::DrivesConsistencyCheck::new(
                maintenance_pool.clone(),
            ),
        )
        .register_recoverable_job(&core.job_registry, &job_store_provider_dyn)
        .await;

        // Second recoverable-run tenant. Iterates `storage.folders`
        // and reports each row whose materialised path/lpath or
        // parent-trashed state has drifted from the parent-chain
        // reconstruction — same subject-iteration pattern as drives.
        // On-demand only; findings surface via the
        // `oxicloud::consistency` tracing target until the
        // `jobs.run_findings` table lands.
        let _ = Arc::new(
            crate::infrastructure::services::folders_consistency_service::FoldersConsistencyCheck::new(
                maintenance_pool.clone(),
            ),
        )
        .register_recoverable_job(&core.job_registry, &job_store_provider_dyn)
        .await;

        // Third recoverable-run tenant. Iterates `storage.files`
        // and reports parent-folder-trashed cascade misses,
        // `missing_blob` (data-loss indicator — file references
        // absent blob row), and `blob_size_mismatch` (denormalised
        // size drift). One SQL round-trip loads folder + blob via
        // two LEFT JOINs; per-row branches key off the join
        // results.
        let _ = Arc::new(
            crate::infrastructure::services::files_consistency_service::FilesConsistencyCheck::new(
                maintenance_pool.clone(),
            ),
        )
        .register_recoverable_job(&core.job_registry, &job_store_provider_dyn)
        .await;

        // Fourth recoverable-run tenant. Iterates `storage.blobs`
        // and verifies each row against the physical backend AND
        // against the reference-counting invariants that `dedup_gc`
        // relies on. Three per-row checks (subject-iteration in
        // action): `blob_missing_from_backend` (data_loss, bytes
        // gone from disk), `refcount_mismatch` (inconsistent,
        // dedup counter drift), and `blob_corrupted` (data_loss,
        // deep mode only — bit-rot). Complements
        // `files_consistency` without doubling work: probing
        // per-unique-blob preserves dedup savings vs probing
        // per-file-chunk. See memory
        // `project_cdc_dual_storage_registries` for the rationale.
        let _ = Arc::new(
            crate::infrastructure::services::blobs_consistency_service::BlobsConsistencyCheck::new(
                maintenance_pool.clone(),
                core.blob_backend.clone(),
                core.config.storage_entries.clone(),
                self.storage_path.clone(),
            ),
        )
        .register_recoverable_job(&core.job_registry, &job_store_provider_dyn)
        .await;

        // Fifth recoverable-run tenant. Iterates the storage
        // backend (via `BlobStorageBackend::list_blob_hashes`) and
        // reports every physical blob that has no matching row in
        // `storage.blobs`. Closes the reference graph together with
        // `blobs_consistency`: this tenant walks backend→DB, that
        // one walks DB→backend. Enumeration is backend-specific but
        // the tenant is fully backend-agnostic — each backend owns
        // its own layout knowledge (local walks `.blobs/`, S3 uses
        // ListObjectsV2, migration wrapper refuses mid-migration).
        let _ = Arc::new(
            crate::infrastructure::services::backend_consistency_service::BackendConsistencyCheck::new(
                maintenance_pool.clone(),
                core.blob_backend.clone(),
                core.config.storage_entries.clone(),
                self.storage_path.clone(),
            ),
        )
        .register_recoverable_job(&core.job_registry, &job_store_provider_dyn)
        .await;

        // "Run all consistency checks" coordinator. Plain JobHandler
        // (not RecoverableJobHandler) — it dispatches, doesn't scan.
        // MUST register AFTER every `*_consistency` tenant so the
        // snapshot ordering in `GET /api/admin/jobs` shows children
        // then wrapper; snapshot filtering happens at run time so
        // late registration is fine. Weak<JobRegistry> internally
        // breaks the Arc cycle.
        let _ = Arc::new(
            crate::infrastructure::services::consistency_batch_service::ConsistencyBatch::new(
                &core.job_registry,
            ),
        )
        .register_job(&core.job_registry)
        .await;

        // 2. Repository services (requires PgPool for all metadata)
        let repos = self.create_repository_services(&core, &pool);

        // 3a. Authorization engine — must exist before application services
        // because services hold an Arc<PgAclEngine> for ReBAC checks.
        // SubjectGroupPgRepository is constructed here too so the engine can
        // expand a user's transitive group set on cache misses.
        //
        // Moved above the eager recent-service build so `create_recent_service`
        // can receive an `Arc<PgAclEngine>` — the Read gate on
        // `record_item_access` (post-Drive AuthZ audit fix) needs it.
        let subject_group_repo = Arc::new(
            crate::infrastructure::repositories::pg::SubjectGroupPgRepository::new(pool.clone()),
        );
        // Migration-readonly atomic. Seeded from
        // `admin_settings.storage.migration_readonly` so the flag
        // survives restart (an operator won't see writes accidentally
        // re-enabled between a crash mid-migration and the retrigger).
        // Shared with the AuthZ engine so it can short-circuit write
        // permissions without a per-check DB round-trip. The boot
        // clear rule (§Read-only mode) runs after this seeding, after
        // the boot recovery sweep — enough for the runtime state
        // machine to decide whether to keep or clear.
        let migration_readonly = Arc::new(std::sync::atomic::AtomicBool::new(
            crate::infrastructure::services::entry_backend::load_migration_readonly(&pool).await,
        ));
        if migration_readonly.load(std::sync::atomic::Ordering::Relaxed) {
            tracing::warn!(
                target: "oxicloud::scheduler",
                event = "storage.migration_readonly.loaded_true_at_boot",
                "Server booted with migration_readonly=true — writes will be refused by AuthZ \
                 until the flag is cleared (either by the boot-clear rule or via the admin \
                 storage tab)."
            );
        }

        let authorization = build_authorization_engine(
            pool.clone(),
            repos.folder_repository.clone(),
            repos.file_read_repository.clone(),
            subject_group_repo.clone(),
            migration_readonly.clone(),
        );

        // Recent service + recording hook are built up-front so the
        // hook can be threaded into `create_application_services` below.
        // The file services hold the hook directly so every authorised
        // `_with_perms` read/write fires into `auth.user_recent_files`
        // without per-handler wiring.
        //
        // The back-edge `recent_service_eager.set_resource_access_hook`
        // closes the loop so the clear/remove handlers can drop the
        // hook's in-memory throttle entries — without it a freshly
        // cleared Recent list refuses to re-record the same file for a
        // full TTL window, surfacing as "I cleared, opened the file,
        // and Recent is still empty" (caught by tests/api/recent.hurl
        // step 8).
        let recent_service_eager = self.create_recent_service(&pool, &authorization);
        let resource_access_hook: Arc<
            dyn crate::application::ports::resource_access_hook::ResourceAccessHook,
        > = Arc::new(
            crate::infrastructure::services::recent_recording_hook::RecentRecordingHook::new(
                recent_service_eager.clone(),
            ),
        );
        recent_service_eager.set_resource_access_hook(resource_access_hook.clone());

        // Drive repository — needed both by the lifecycle hook (when auth
        // is enabled) and by `GET /api/drives` on the final `AppState`,
        // so declared at the outer scope.
        let drive_repo =
            Arc::new(crate::infrastructure::repositories::pg::DrivePgRepository::new(pool.clone()));

        // 3b. Trash service (needed before application services)
        let trash_service = self
            .create_trash_service(&repos, &core, &authorization, &drive_repo)
            .await;

        // 3c. Storage usage / quota service (needed by the instant-upload
        // path inside the application services, and re-exposed on AppState
        // for the handler-side quota checks of the byte-upload paths).
        let storage_usage = self
            .create_storage_usage_service(
                &repos,
                &pool,
                &maintenance_pool,
                drive_repo.clone(),
                &core,
            )
            .await;

        // 3d. Content index (embedded Tantivy) — opened before application
        // services so SearchService can hold the query port; the feeding
        // worker starts further down with the maintenance pool.
        let content_index = self.create_content_index();

        // Single plugin manager, surfaced as its two ports: the dispatch port
        // (shared by every event bridge — file uploads here, user logins at the
        // auth-services wiring below) and the management port (stored on
        // AppState for the admin API). Created once so plugins load exactly once
        // regardless of how many events they observe.
        let (plugin_dispatch, plugin_management) = self.create_plugin_ports();

        // 4. Application services (with trash + authz already wired)
        // External mount registry + router. Built before the application
        // services because `FolderService` holds the router to branch listing
        // onto the provider. The router is always present (an empty registry is
        // a cheap no-op); when the feature is enabled we load the configured
        // mounts and build their providers up front. The registry is
        // interior-mutable (arc-swap), so reloading here is visible to every
        // holder of the shared router.
        let mount_registry =
            Arc::new(crate::application::services::mount_registry::MountRegistry::empty());
        if self.config.features.enable_external_mounts {
            let repo = crate::infrastructure::repositories::pg::ExternalMountPgRepository::new(
                pool.clone(),
            );
            let factory =
                crate::infrastructure::services::mount_provider_factory::DefaultMountProviderFactory::new();
            mount_registry.reload(&repo, &factory).await;
        }
        let mount_router = Arc::new(
            crate::application::services::external_mount_router::MountRouter::new(mount_registry),
        );

        let mut apps = self.create_application_services(
            &core,
            &repos,
            trash_service.clone(),
            &authorization,
            &drive_repo,
            &storage_usage,
            content_index.as_ref().map(|(idx, _)| idx.clone()),
            plugin_dispatch.clone(),
            mount_router.clone(),
            Some(resource_access_hook.clone()),
        );

        // 5. Share service
        let share_service = self.create_share_service(
            &repos,
            &pool,
            &authorization,
            &drive_repo,
            apps.search_service.clone(),
        );
        apps.share_service = share_service.clone();

        let share_browse_service = share_service.as_ref().map(|s| {
            Arc::new(ShareBrowseService::new(
                s.clone(),
                apps.folder_service.clone(),
                apps.file_retrieval_service.clone(),
                repos.folder_repository.clone(),
            ))
        });

        // 6. Database-dependent services (PgPool always available in blob model)
        let favorites_service: Option<Arc<FavoritesService>>;
        let recent_service: Option<Arc<RecentService>>;
        let places_service: Option<Arc<PlacesService>>;
        let people_service: Option<Arc<PeopleService>>;
        let storage_usage_service: Option<Arc<StorageUsageService>>;
        let grant_cleanup_service: Option<
            Arc<crate::infrastructure::services::grant_cleanup_service::GrantCleanupService>,
        >;
        let mut auth_services: Option<crate::common::di::AuthServices> = None;
        let mut nextcloud_services: Option<NextcloudServices> = None;
        // Lifted out of the database-services block so PR 9's invite
        // orchestrator (built at AppState-assembly time below) can share
        // the same lifecycle dispatcher. The inner block at line ~682
        // is unconditional and always assigns; the `#[allow]` silences
        // the rustc warning that the `None` initialiser is never read.
        #[allow(unused_assignments)]
        let mut user_lifecycle_handle: Option<
            Arc<crate::application::services::user_lifecycle_service::UserLifecycleService>,
        > = None;

        {
            let favs =
                self.create_favorites_service(&pool, &authorization, apps.search_service.clone());
            favorites_service = Some(favs.clone());
            apps.favorites_service = Some(favs);

            // Already built up-front so the file services could hold the
            // RecentRecordingHook — reuse the same Arc here so AppState and
            // the recording hook share one service instance.
            recent_service = Some(recent_service_eager.clone());
            apps.recent_service = Some(recent_service_eager.clone());

            places_service = if core.config.features.enable_places {
                Some(self.create_places_service(&repos.file_read_repository))
            } else {
                None
            };

            people_service = if core.config.features.enable_faces {
                Some(self.create_people_service(&pool))
            } else {
                None
            };

            storage_usage_service = Some(storage_usage.clone());

            self.start_tree_etag_flush_job(&maintenance_pool);

            self.start_db_pool_monitor(&pool);

            self.start_content_index_job(&maintenance_pool, &core, content_index);

            grant_cleanup_service = if core.config.features.grant_cleanup.enabled {
                // Self-registering constructor chain. Grant-cleanup owns
                // its interval + on `?force=true` handling; DI only decides
                // whether to instantiate at all (feature-gated).
                let svc = Arc::new(
                    crate::infrastructure::services::grant_cleanup_service::GrantCleanupService::new(
                        authorization.clone(),
                        core.config.features.grant_cleanup.grace_days,
                        core.config.features.grant_cleanup.interval_hours,
                    ),
                )
                .register(&core.job_registry)
                .await;
                Some(svc)
            } else {
                tracing::info!(
                    "Grant-cleanup daemon disabled by OXICLOUD_GRANT_CLEANUP_ENABLED=false"
                );
                None
            };

            // User-lifecycle dispatcher. Hook order is registration order;
            // document dependencies inline if/when any arise. Today:
            //   1. AuditLifecycleHook             — fires first so the
            //                                       audit event is recorded
            //                                       even if a later hook
            //                                       errors out.
            //   2. PersonalDriveLifecycleHook        — provisions the user's
            //                                       home folder on
            //                                       created/login (no-op
            //                                       for external users).
            //   3. AuthzCacheLifecycleHook        — invalidates the
            //                                       Moka group-expansion
            //                                       cache on logout/delete
            //                                       so a re-login sees fresh
            //                                       membership immediately.
            //   4. SessionRevocationLifecycleHook — explicit per-user
            //                                       session revocation on
            //                                       delete (with audit) —
            //                                       replaces the silent FK
            //                                       CASCADE.
            //   5. ExternalIdentityLifecycleHook  — audit + magic-link
            //                                       token cleanup. Logs an
            //                                       audit event for any
            //                                       external user that gets
            //                                       created or logs in;
            //                                       transactionally clears
            //                                       outstanding magic-link
            //                                       tokens on delete (so a
            //                                       new user reusing the
            //                                       same id can never
            //                                       inherit an old token).
            //                                       Last in the chain so it
            //                                       observes the latest
            //                                       user state before the
            //                                       chain commits.
            let session_repo_for_hook = Arc::new(SessionPgRepository::new(pool.clone()));
            let magic_link_repo: Arc<
                dyn crate::domain::repositories::magic_link_token_repository::MagicLinkTokenRepository,
            > = Arc::new(
                crate::infrastructure::repositories::pg::MagicLinkTokenPgRepository::new(
                    pool.clone(),
                ),
            );

            // CalDAV / CardDAV storage — constructed here (rather than in
            // block #10 below) so the two default-provisioning lifecycle
            // hooks can be wired into `user_lifecycle_builder` with the
            // rest of the chain. The Arcs are cloned into both the hooks
            // and, later, into their respective services — cheap and
            // matches the pattern used for `drive_repo` above.
            let calendar_repo_for_hook: Arc<
                crate::infrastructure::repositories::pg::CalendarPgRepository,
            > = Arc::new(
                crate::infrastructure::repositories::pg::CalendarPgRepository::new(pool.clone()),
            );
            let event_repo_for_hook: Arc<
                crate::infrastructure::repositories::pg::CalendarEventPgRepository,
            > = Arc::new(
                crate::infrastructure::repositories::pg::CalendarEventPgRepository::new(
                    pool.clone(),
                ),
            );
            let calendar_storage_for_hook = Arc::new(
                crate::infrastructure::adapters::calendar_storage_adapter::CalendarStorageAdapter::new(
                    calendar_repo_for_hook.clone(),
                    event_repo_for_hook.clone(),
                )
            );
            let address_book_repo_for_hook: Arc<AddressBookPgRepository> = Arc::new(
                crate::infrastructure::repositories::pg::AddressBookPgRepository::new(pool.clone()),
            );
            let contact_repo_for_hook: Arc<ContactPgRepository> = Arc::new(
                crate::infrastructure::repositories::pg::ContactPgRepository::new(pool.clone()),
            );
            let group_repo_for_hook: Arc<ContactGroupPgRepository> = Arc::new(
                crate::infrastructure::repositories::pg::ContactGroupPgRepository::new(
                    pool.clone(),
                ),
            );
            let contact_storage_for_hook = Arc::new(
                crate::infrastructure::adapters::contact_storage_adapter::ContactStorageAdapter::new(
                    address_book_repo_for_hook.clone(),
                    contact_repo_for_hook.clone(),
                    group_repo_for_hook.clone(),
                ),
            );

            let mut user_lifecycle_builder =
                crate::application::services::user_lifecycle_service::UserLifecycleService::new()
                    .with_hook(Arc::new(
                        crate::application::services::user_lifecycle_service::AuditLifecycleHook,
                    ))
                    .with_hook(Arc::new(
                        crate::application::services::folder_service::PersonalDriveLifecycleHook::new(
                            drive_repo.clone(),
                            authorization.clone(),
                        ),
                    ))
                    .with_hook(Arc::new(
                        crate::application::services::calendar_service::DefaultCalendarLifecycleHook::new(
                            calendar_storage_for_hook.clone(),
                            authorization.clone(),
                        ),
                    ))
                    .with_hook(Arc::new(
                        crate::application::services::contact_service::DefaultAddressBookLifecycleHook::new(
                            address_book_repo_for_hook.clone(),
                            contact_storage_for_hook.clone(),
                            authorization.clone(),
                        ),
                    ))
                    .with_hook(Arc::new(
                        crate::infrastructure::services::pg_acl_engine::AuthzCacheLifecycleHook::new(
                            authorization.clone(),
                        ),
                    ))
                    .with_hook(Arc::new(
                        crate::application::services::user_lifecycle_service::SessionRevocationLifecycleHook::new(
                            session_repo_for_hook,
                        ),
                    ))
                    .with_hook(Arc::new(
                        crate::application::services::external_identity_service::ExternalIdentityLifecycleHook::new()
                            .with_magic_link_repo(magic_link_repo.clone()),
                    ));

            // Plugin user.login bridge — shares the single plugin dispatch with
            // the file-upload bridge. Registered only when plugins are active;
            // inert until auth is enabled (the dispatcher is never fired otherwise).
            if let Some(dispatch) = &plugin_dispatch {
                user_lifecycle_builder = user_lifecycle_builder.with_hook(Arc::new(
                    crate::application::adapters::plugin_user_lifecycle_hook::PluginUserLifecycleHook::new(
                        dispatch.clone(),
                    ),
                ));
            }

            let user_lifecycle = Arc::new(user_lifecycle_builder);

            // Auth services. Folder service no longer threaded here —
            // PR 3 moved home-folder provisioning into
            // PersonalDriveLifecycleHook, which already holds an Arc to the
            // folder service via the user_lifecycle dispatcher.
            if self.config.features.enable_auth {
                let services = crate::infrastructure::auth_factory::create_auth_services(
                    &self.config,
                    pool.clone(),
                    user_lifecycle.clone(),
                )
                .await
                .map_err(|e| {
                    // SECURITY: fail-closed. If auth is required but the auth
                    // services cannot be created, propagate the error so the
                    // server refuses to start — never degrade to public mode.
                    tracing::error!(
                        "FATAL: enable_auth=true but auth services failed to initialize: {}",
                        e
                    );
                    DomainError::internal_error(
                        "AuthInit",
                        format!(
                            "Authentication is enabled but auth services failed: {}. \
                             Refusing to start without authentication.",
                            e
                        ),
                    )
                })?;

                tracing::info!("Authentication services initialized successfully");
                auth_services = Some(services);
            }

            user_lifecycle_handle = Some(user_lifecycle);
        }

        // Shared App Password service — created once, used by both NC routes and native API
        let shared_app_pw_svc: Option<Arc<AppPasswordService>> =
            if self.config.nextcloud.enabled || self.config.features.enable_auth {
                let app_pw_repo: Arc<AppPasswordPgRepository> =
                    Arc::new(AppPasswordPgRepository::new(pool.clone()));
                let hasher: Arc<Argon2PasswordHasher> = Arc::new(
                    crate::infrastructure::services::password_hasher::Argon2PasswordHasher::new(
                        self.config.auth.hash_memory_cost,
                        self.config.auth.hash_time_cost,
                        self.config.auth.hash_parallelism,
                    ),
                );
                let user_repo: Arc<UserPgRepository> = Arc::new(
                    crate::infrastructure::repositories::pg::UserPgRepository::new(pool.clone()),
                );
                let svc = Arc::new(AppPasswordService::new(
                    app_pw_repo,
                    hasher,
                    user_repo,
                    self.config.base_url(),
                ));
                tracing::info!("App Password service initialized (shared)");
                Some(svc)
            } else {
                None
            };

        // Nextcloud compatibility services
        if self.config.nextcloud.enabled {
            if !self.config.features.enable_auth {
                tracing::warn!(
                    "Nextcloud compatibility enabled but auth is disabled; Nextcloud routes will be unusable"
                );
            }

            // NC chunked-upload sessions root. Honour `OXICLOUD_CHUNK_DIR`
            // (same env var that the REST chunked service uses) so a single
            // value covers both surfaces and they stay co-located on one
            // filesystem; fall back to `{storage_path}/.uploads/` to match
            // the legacy layout.
            let chunk_root = self
                .config
                .storage
                .chunk_dir
                .clone()
                .unwrap_or_else(|| self.storage_path.join(".uploads"));
            let chunk_base = chunk_root.join("nextcloud");
            let chunked_uploads = Arc::new(NextcloudChunkedUploadService::new(chunk_base));

            let file_id_repo = Arc::new(
                crate::infrastructure::repositories::pg::NextcloudObjectIdRepository::new(
                    pool.clone(),
                ),
            );
            let file_ids = Arc::new(NextcloudFileIdService::new(
                file_id_repo,
                self.config.nextcloud.instance_id.clone(),
            ));

            nextcloud_services = Some(NextcloudServices {
                login_flow: Arc::new(NextcloudLoginFlowService::new(
                    std::time::Duration::from_secs(self.config.nextcloud.login_flow_ttl_secs),
                )),
                app_passwords: shared_app_pw_svc
                    .clone()
                    .expect("AppPasswordService must be available when NC is enabled"),
                file_ids,
                chunked_uploads,
            });
        }

        // 7. Preload translations
        self.preload_translations(&apps.i18n_service).await;

        // 8. Build the ZipService with real application services
        let zip_service: Arc<ZipService> = Arc::new(
            crate::infrastructure::services::zip_service::ZipService::new(
                apps.file_retrieval_service.clone(),
                apps.folder_service.clone(),
            ),
        );
        let mut core = core;
        core.zip_service = Some(zip_service);

        // 9. Assemble final AppState
        let mut app_state = AppState {
            core,
            repositories: repos,
            applications: apps,
            locale_registry: self.locale_registry.clone(),
            db_pool: Some(pool.clone()),
            maintenance_pool: Some(maintenance_pool),
            mount_router,
            auth_service: auth_services,
            nextcloud: nextcloud_services,
            admin_settings_service: None,
            storage_settings_service: None,
            plugin_management,
            trash_service,
            share_service,
            share_browse_service,
            favorites_service,
            recent_service,
            places_service,
            people_service,
            storage_usage_service,
            grant_cleanup_service,
            calendar_service: None,
            calendar_use_case: None,
            addressbook_use_case: None,
            contact_use_case: None,
            music_service: None,
            wopi_token_service: None,
            wopi_lock_service: None,
            wopi_discovery_service: None,
            device_auth_service: None,
            app_password_service: None,
            path_resolver: None,
            webdav_lock_store:
                crate::infrastructure::services::webdav_lock_service::create_webdav_lock_store(),
            webdav_dead_props:
                crate::infrastructure::services::webdav_dead_property_store::create_dead_property_store(pool.clone()),
            authorization: authorization.clone(),
            migration_readonly: migration_readonly.clone(),
            migration_progress: Arc::new(std::sync::RwLock::new(None)),
            drive_repo: drive_repo.clone(),
            drive_management_service: Arc::new(
                crate::application::services::drive_management_service::DriveManagementService::new(
                    drive_repo.clone(),
                    authorization.clone(),
                    subject_group_repo.clone(),
                    Arc::new(
                        crate::infrastructure::repositories::pg::UserPgRepository::new(
                            pool.clone(),
                        ),
                    ),
                ),
            ),
            subject_group_service: Some(Arc::new(
                crate::application::services::subject_group_service::SubjectGroupService::new(
                    subject_group_repo.clone(),
                    pool.clone(),
                    Arc::new(
                        crate::infrastructure::repositories::pg::UserPgRepository::new(
                            pool.clone(),
                        ),
                    ),
                    authorization.clone(),
                    drive_repo.clone(),
                ),
            )),
            email_sender: None,                   // populated below
            mock_email_sender: None,              // populated below
            magic_link_invite_service: None,      // populated below
            recipient_notification_service: None, // populated below alongside magic_link_invite_service
            // 60 lookups / minute / caller; cap at 50 000 tracked
            // callers to bound memory. The same limiter instance is
            // shared by every clone of AppState since it lives in an
            // Arc.
            user_profile_rate_limiter: Arc::new(
                crate::interfaces::middleware::rate_limit::RateLimiter::new(60, 60, 50_000),
            ),
            // Delta upload: 240 requests / minute / caller. Generous for a
            // real client (chunk PUTs carry up to 100 MB each) while
            // stopping pin/negotiate floods; 50 000 tracked callers bound
            // the memory like the other limiters.
            delta_upload_rate_limiter: Arc::new(
                crate::interfaces::middleware::rate_limit::RateLimiter::new(240, 60, 50_000),
            ),
            // PR 12 — per-sharer email-invite ceiling: caller_id-keyed.
            // Defends against a compromised account spamming external
            // invites (each invite mints a new external user + email).
            // Limits come from MagicLinkConfig so tests / operators can
            // tune them via OXICLOUD_MAGIC_LINK_INVITE_PER_CALLER_PER_HOUR.
            email_invite_rate_limiter: Arc::new(
                crate::interfaces::middleware::rate_limit::RateLimiter::new(
                    self.config.magic_link.invite_per_caller_per_hour,
                    3_600,
                    50_000,
                ),
            ),
            // PR 12 — per-target-email send ceiling on
            // /api/auth/magic-link/send. Stops the endpoint from being
            // an email-bombing primitive against a known address.
            magic_link_send_per_email_rate_limiter: Arc::new(
                crate::interfaces::middleware::rate_limit::RateLimiter::new(
                    self.config.magic_link.send_per_email_per_hour,
                    3_600,
                    50_000,
                ),
            ),
            // PR 12 — per-IP backstop on /api/auth/magic-link/send.
            // Bounds the damage if an attacker spreads a low per-email
            // rate across many target addresses.
            magic_link_send_per_ip_rate_limiter: Arc::new(
                crate::interfaces::middleware::rate_limit::RateLimiter::new(
                    self.config.magic_link.send_per_ip_per_hour,
                    3_600,
                    50_000,
                ),
            ),
            // Populated below once every service has finished registering
            // with `core.job_registry`. Starting the engine before all
            // registrations land would race the first tick against
            // late-registered jobs.
            scheduler_engine: None,
        };
        let email_bundle = build_email_sender(&self.config.smtp);
        app_state.email_sender = email_bundle.sender;
        app_state.mock_email_sender = email_bundle.mock;

        // Magic-link invite orchestrator: only when SMTP wired AND the
        // user-lifecycle dispatcher exists (i.e. auth is enabled).
        if let (Some(email_sender), Some(lifecycle)) = (
            app_state.email_sender.clone(),
            user_lifecycle_handle.clone(),
        ) {
            let invite_user_storage = Arc::new(
                crate::infrastructure::repositories::pg::UserPgRepository::new(pool.clone()),
            );
            let invite_magic_link_repo: Arc<
                dyn crate::domain::repositories::magic_link_token_repository::MagicLinkTokenRepository,
            > = Arc::new(
                crate::infrastructure::repositories::pg::MagicLinkTokenPgRepository::new(
                    pool.clone(),
                ),
            );
            app_state.magic_link_invite_service = Some(Arc::new(
                crate::application::services::magic_link_invite_service::MagicLinkInviteService::new(
                    invite_user_storage.clone(),
                    invite_magic_link_repo,
                    email_sender.clone(),
                    lifecycle,
                    app_state.applications.i18n_service.clone(),
                    app_state.locale_registry.clone(),
                    self.config.magic_link.clone(),
                    self.config.base_url(),
                ),
            ));

            // PR N1: wire the unified RecipientNotificationService.
            // Only constructed when MagicLinkInviteService is also
            // available — the magic-link path delegates to it.
            // SubjectGroupService is built earlier in this factory; the
            // notification service needs it for the Group subject arm.
            if let (Some(magic_link_svc), Some(subject_groups)) = (
                app_state.magic_link_invite_service.clone(),
                app_state.subject_group_service.clone(),
            ) {
                app_state.recipient_notification_service = Some(Arc::new(
                    crate::application::services::recipient_notification_service::RecipientNotificationService::new(
                        invite_user_storage,
                        magic_link_svc,
                        email_sender,
                        app_state.applications.i18n_service.clone(),
                        app_state.locale_registry.clone(),
                        subject_groups,
                        app_state.magic_link_send_per_email_rate_limiter.clone(),
                        self.config.magic_link.clone(),
                        self.config.base_url(),
                    ),
                ));
            }
        }

        // 9b. Wire admin settings service when auth is available
        if let Some(auth_svc) = &app_state.auth_service {
            let settings_repo = Arc::new(
                crate::infrastructure::repositories::pg::SettingsPgRepository::new(pool.clone()),
            );
            let server_base_url = self.config.base_url();

            // Load OIDC config from env vars (the snapshot from startup)
            let env_oidc = crate::common::config::OidcConfig::from_env();

            let admin_svc = Arc::new(AdminSettingsService::new(
                settings_repo.clone(),
                env_oidc,
                auth_svc.auth_application_service.clone(),
                server_base_url,
            ));

            // Hot-reload OIDC from DB settings if configured
            match admin_svc.load_effective_oidc_config().await {
                Ok(eff)
                    if eff.enabled
                        && !eff.issuer_url.is_empty()
                        && !eff.client_id.is_empty()
                        && !eff.client_secret.is_empty() =>
                {
                    let oidc_svc = Arc::new(
                        crate::infrastructure::services::oidc_service::OidcService::new(
                            eff.clone(),
                        ),
                    );
                    auth_svc.auth_application_service.reload_oidc(oidc_svc, eff);
                    tracing::info!("OIDC config loaded from admin settings (database)");
                }
                Ok(_) => {
                    tracing::info!(
                        "No active OIDC config in admin settings — using env vars or defaults"
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        "Failed to load OIDC settings from database (table may not exist yet): {}",
                        e
                    );
                }
            }

            app_state.admin_settings_service = Some(admin_svc.clone());

            // 9b-1b. Wire storage settings service (reuses same settings_repo).
            // Multi-entry view fields (entries + active_entry_name +
            // migration_readonly) are populated from the same sources
            // the migration handler and AuthZ engine read from — one
            // snapshot at DI, shared atomic for the readonly flag so
            // changes are visible without a DB round-trip.
            let storage_settings_svc = Arc::new(StorageSettingsService::new(
                settings_repo.clone(),
                self.config.storage.clone(),
                app_state.core.dedup_service.clone(),
                app_state.core.config.storage_entries.clone(),
                app_state.core.active_backend_name.clone(),
                app_state.migration_readonly.clone(),
            ));
            app_state.storage_settings_service = Some(storage_settings_svc.clone());
            tracing::info!("Storage settings service initialized");

            // 9b-1c. Register the storage-backend migration tenant on
            // the recoverable-run engine. Target is resolved by NAME
            // from `params.target_name` on each run — plumbed from
            // the trigger endpoint. Constructor takes the ambient
            // entries snapshot + active-name + storage_path fallback
            // so no DB read is needed per run for target lookup.
            let job_store_provider_dyn: Arc<
                dyn crate::infrastructure::scheduler::JobStoreProvider,
            > = app_state.core.job_store_provider.clone();
            let _ = Arc::new(
                crate::infrastructure::services::storage_migration_service::StorageMigrationService::new(
                    app_state
                        .maintenance_pool
                        .clone()
                        .expect("maintenance_pool set above"),
                    app_state.core.blob_backend.clone(),
                    app_state.core.active_backend_name.clone(),
                    app_state.core.config.storage_entries.clone(),
                    self.storage_path.clone(),
                    app_state.migration_readonly.clone(),
                    app_state.core.blob_backend_hot_swap.clone(),
                    app_state.migration_progress.clone(),
                ),
            )
            .register_recoverable_job(&app_state.core.job_registry, &job_store_provider_dyn)
            .await;

            // 9b-2. Log whether system needs first-time admin setup
            if !admin_svc.is_system_initialized().await {
                tracing::warn!("╔══════════════════════════════════════════════════════════╗");
                tracing::warn!("║  SYSTEM NOT INITIALIZED — first admin setup required     ║");
                tracing::warn!("║                                                          ║");
                tracing::warn!("║  Open the web UI to create the first admin account.      ║");
                tracing::warn!("║  The setup page is available until an admin is created.  ║");
                tracing::warn!("╚══════════════════════════════════════════════════════════╝");
            } else {
                tracing::info!("System already initialized — setup endpoint disabled");
            }

            // 9c. Wire Device Authorization Grant (RFC 8628) service
            {
                let device_code_repo = Arc::new(DeviceCodePgRepository::new(pool.clone()));
                let user_repo: Arc<UserPgRepository> = Arc::new(
                    crate::infrastructure::repositories::UserPgRepository::new(pool.clone()),
                );
                let session_repo: Arc<SessionPgRepository> = Arc::new(
                    crate::infrastructure::repositories::SessionPgRepository::new(pool.clone()),
                );
                let base_url = self.config.base_url();

                let device_auth_svc = Arc::new(DeviceAuthService::new(
                    device_code_repo,
                    auth_svc.token_service.clone(),
                    user_repo,
                    session_repo,
                    base_url,
                ));
                app_state.device_auth_service = Some(device_auth_svc);
                tracing::info!("Device Authorization Grant (RFC 8628) service initialized");
            }

            // 9d. Wire App Password service (reuse shared instance)
            app_state.app_password_service = shared_app_pw_svc.clone();
        }

        // 9e. Wire PathResolver for single-query WebDAV path resolution
        {
            app_state.path_resolver = Some(Arc::new(PathResolverService::new(pool.clone())));
            tracing::info!("PathResolver service initialized");
        }

        // 10. Wire CalDAV/CardDAV services. Note: the `*_for_hook`
        //     adapters constructed inside the enable-auth block above
        //     are out of scope here (that block ends before AppState
        //     assembly). Re-constructing local adapters over the same
        //     `pool` is cheap — the pool itself is shared via Arc, and
        //     adapters are stateless delegators. Both instances end up
        //     talking to the same rows.
        {
            // CalDAV
            let calendar_repo: Arc<CalendarPgRepository> = Arc::new(
                crate::infrastructure::repositories::pg::CalendarPgRepository::new(pool.clone()),
            );
            let event_repo: Arc<CalendarEventPgRepository> = Arc::new(
                crate::infrastructure::repositories::pg::CalendarEventPgRepository::new(
                    pool.clone(),
                ),
            );
            let calendar_storage = Arc::new(
                crate::infrastructure::adapters::calendar_storage_adapter::CalendarStorageAdapter::new(
                    calendar_repo,
                    event_repo,
                )
            );
            let calendar_service = Arc::new(
                crate::application::services::calendar_service::CalendarService::new(
                    calendar_storage,
                    authorization.clone(),
                ),
            );
            app_state.calendar_use_case = Some(calendar_service as Arc<CalendarService>);

            // CardDAV
            let address_book_repo: Arc<AddressBookPgRepository> = Arc::new(
                crate::infrastructure::repositories::pg::AddressBookPgRepository::new(pool.clone()),
            );
            let contact_repo: Arc<ContactPgRepository> = Arc::new(
                crate::infrastructure::repositories::pg::ContactPgRepository::new(pool.clone()),
            );
            let group_repo: Arc<ContactGroupPgRepository> = Arc::new(
                crate::infrastructure::repositories::pg::ContactGroupPgRepository::new(
                    pool.clone(),
                ),
            );
            let contact_storage = Arc::new(
                crate::infrastructure::adapters::contact_storage_adapter::ContactStorageAdapter::new(
                    address_book_repo,
                    contact_repo,
                    group_repo,
                ),
            );
            let contact_service =
                Arc::new(ContactService::new(contact_storage, authorization.clone()));
            app_state.addressbook_use_case = Some(contact_service.clone());
            app_state.contact_use_case = Some(contact_service);

            tracing::info!("CalDAV and CardDAV services initialized with PostgreSQL repositories");
        }

        // Music service
        {
            let playlist_repo: Arc<PlaylistPgRepository> =
                Arc::new(PlaylistPgRepository::new(pool.clone()));
            let item_repo: Arc<PlaylistItemPgRepository> =
                Arc::new(PlaylistItemPgRepository::new(pool.clone()));
            let audio_metadata_repo: Arc<AudioMetadataPgRepository> =
                Arc::new(AudioMetadataPgRepository::new(pool.clone()));
            let music_storage = Arc::new(
                crate::infrastructure::adapters::music_storage_adapter::MusicStorageAdapter::new(
                    playlist_repo,
                    item_repo,
                    audio_metadata_repo,
                ),
            );
            let music_svc = Arc::new(MusicService::new(music_storage, authorization.clone()));
            app_state.music_service = Some(music_svc);
            tracing::info!("Music service initialized");
        }

        // 11. Wire WOPI services if enabled
        if self.config.wopi.enabled {
            let discovery_url = &self.config.wopi.discovery_url;
            if discovery_url.is_empty() {
                tracing::error!(
                    "WOPI is enabled but WOPI_DISCOVERY_URL is empty — WOPI services will NOT be available"
                );
            } else {
                let wopi_secret = if self.config.wopi.secret.is_empty() {
                    self.config.auth.jwt_secret.clone()
                } else {
                    self.config.wopi.secret.clone()
                };

                let wopi_token_service = Arc::new(WopiTokenService::new(
                    wopi_secret,
                    self.config.wopi.token_ttl_secs,
                ));

                let wopi_lock_service =
                    Arc::new(WopiLockService::new(self.config.wopi.lock_ttl_secs));
                wopi_lock_service.start_cleanup_task();

                let wopi_discovery_service = Arc::new(WopiDiscoveryService::new(
                    discovery_url.clone(),
                    86400, // 24 hour cache TTL
                ));

                app_state.wopi_token_service = Some(wopi_token_service);
                app_state.wopi_lock_service = Some(wopi_lock_service);
                app_state.wopi_discovery_service = Some(wopi_discovery_service);

                tracing::info!("WOPI services initialized (discovery: {})", discovery_url);
            }
        }

        // Recoverable-run engine crash recovery. Any row still marked
        // Running or CancelRequested when the previous process died gets
        // flipped to Paused with `error_message = 'server restart mid-run'`.
        // We do NOT auto-resume — operators explicitly re-trigger via
        // `POST /api/admin/jobs/{name}/trigger`, which resumes from the
        // persisted cursor. Runs BEFORE the scheduler starts so a
        // periodic-triggered recoverable job's first tick sees a clean
        // slate. See `docs/plan/job-registry.md` Part 2.
        use crate::infrastructure::scheduler::JobStoreProvider as _;
        match app_state
            .core
            .job_store_provider
            .boot_recovery_sweep()
            .await
        {
            Ok(0) => tracing::debug!(
                target: "oxicloud::scheduler",
                event = "recoverable.boot_recovery",
                flipped = 0,
                "no orphaned recoverable runs found at boot"
            ),
            Ok(n) => tracing::warn!(
                target: "oxicloud::scheduler",
                event = "recoverable.boot_recovery",
                flipped = n,
                "flipped {n} orphaned recoverable run(s) Running/CancelRequested → Paused (previous process died mid-run)"
            ),
            Err(e) => tracing::error!(
                target: "oxicloud::scheduler",
                event = "recoverable.boot_recovery.failed",
                error = %e,
                "boot recovery sweep failed — orphaned runs may remain in Running state"
            ),
        }

        // Migration-readonly boot-clear rule. See
        // `docs/plan/storage-multi-entry.md` §"Read-only mode".
        //
        // If the flag was set true at boot AND no storage_migration
        // run is currently non-terminal AND active_backend_name
        // matches the entry the app actually booted onto — that means
        // the cutover completed on a prior boot (the run reached
        // Completed, the pointer flipped, the operator restarted).
        // Safe to clear now: no in-flight migration means no one
        // still needs writes-off, and matching active_backend_name
        // means we're already on the target the run was pointing at.
        //
        // If ANY of those conditions fails (flag was false at boot;
        // there's still a Paused/Running/CancelRequested run in the
        // way; active doesn't match booted — mismatch means someone
        // manually edited the pointer while readonly was on) we
        // leave the flag alone. Operator has to decide.
        if app_state
            .migration_readonly
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            use crate::infrastructure::services::storage_migration_service::STORAGE_MIGRATION_JOB_NAME;
            let has_in_flight = match app_state
                .core
                .job_store_provider
                .list_runs(STORAGE_MIGRATION_JOB_NAME, 5)
                .await
            {
                Ok(runs) => runs.iter().any(|r| {
                    matches!(
                        r.status,
                        crate::infrastructure::scheduler::RunStatus::Running
                            | crate::infrastructure::scheduler::RunStatus::Paused
                            | crate::infrastructure::scheduler::RunStatus::CancelRequested
                    )
                }),
                Err(e) => {
                    tracing::warn!(
                        target: "oxicloud::scheduler",
                        event = "storage.migration_readonly.clear_check_failed",
                        error = %e,
                        "failed to list storage_migration runs during readonly-clear check; \
                         leaving migration_readonly flag as-is"
                    );
                    // Play it safe: assume in-flight to avoid clearing prematurely.
                    true
                }
            };

            // Look up the DB pointer to compare against the booted
            // active_backend_name. Absence (Unset) is treated as "no
            // mismatch to complain about" — the boot fallback already
            // picked the first entry.
            let booted_active = app_state
                .core
                .active_backend_name
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            let db_active_matches = {
                use crate::infrastructure::services::entry_backend::{
                    ActiveEntry, resolve_active_entry,
                };
                match resolve_active_entry(&pool, &app_state.core.config.storage_entries).await {
                    Ok(ActiveEntry::Explicit(e)) => e.name == booted_active,
                    Ok(ActiveEntry::Unset) => true,
                    Err(_) => false,
                }
            };

            if !has_in_flight && db_active_matches {
                use crate::infrastructure::services::entry_backend::persist_migration_readonly;
                match persist_migration_readonly(&pool, false).await {
                    Ok(()) => {
                        app_state
                            .migration_readonly
                            .store(false, std::sync::atomic::Ordering::Relaxed);
                        tracing::info!(
                            target: "audit",
                            event = "storage.migration_readonly.cleared_at_boot",
                            active = %booted_active,
                            "🧊 migration_readonly cleared at boot: no in-flight migration + \
                             active_backend_name matches booted entry (cutover complete on prior boot)"
                        );
                    }
                    Err(e) => tracing::warn!(
                        target: "oxicloud::scheduler",
                        event = "storage.migration_readonly.clear_persist_failed",
                        error = %e,
                        "cleared migration_readonly in memory would have been safe, but the DB \
                         write failed — leaving the DB row alone; will re-check next boot"
                    ),
                }
            } else {
                tracing::info!(
                    target: "oxicloud::scheduler",
                    event = "storage.migration_readonly.retained_at_boot",
                    has_in_flight = has_in_flight,
                    db_active_matches = db_active_matches,
                    "migration_readonly retained at boot (in-flight run and/or active-name \
                     mismatch prevents auto-clear)"
                );
            }
        }

        // Start the periodic-job scheduler AFTER every native service has
        // finished registering its jobs on `core.job_registry`. Starting
        // it earlier would race the first tick against late registrations.
        // See `docs/plan/job-registry.md` Part 1.
        let registered = app_state.core.job_registry.len().await;
        let engine = SchedulerEngine::start(app_state.core.job_registry.clone());
        app_state.scheduler_engine = Some(Arc::new(engine));
        tracing::info!(
            target: "oxicloud::scheduler",
            event = "scheduler.ready",
            registered = registered,
            "periodic scheduler ready ({} job(s) registered)",
            registered
        );

        Ok(app_state)
    }
}

/// Container for core services
#[derive(Clone)]
pub struct CoreServices {
    pub path_service: Arc<PathService>,
    pub file_content_cache: Arc<FileContentCache>,
    pub thumbnail_service: Arc<ThumbnailService>,
    /// Composite lifecycle dispatcher — wires thumbnails + audio metadata for all file events.
    pub file_lifecycle: Arc<FileLifecycleService>,
    pub audio_metadata_service: Option<Arc<AudioMetadataService>>,
    /// Image/video capture-metadata extractor (EXIF + container dates).
    pub media_metadata_service: Arc<MediaMetadataService>,
    pub chunked_upload_service: Arc<ChunkedUploadService>,
    pub image_transcode_service: Arc<ImageTranscodeService>,
    pub dedup_service: Arc<DedupService>,
    pub zip_service: Option<Arc<ZipService>>,
    pub config: AppConfig,
    /// Periodic-job scheduler registry. Services that satisfy the
    /// migration criterion (`docs/plan/job-registry.md`) `register()`
    /// themselves here during their creation; `SchedulerEngine::start`
    /// spins up the supervisor loop at the end of `build_app_state`.
    pub job_registry: Arc<JobRegistry>,
    /// PG-backed provider for `jobs.recoverable_runs`. Recoverable
    /// tenants (storage migration, reextract, consistency checks —
    /// Part 2 of `docs/plan/job-registry.md`) plug into this via
    /// `svc.register_recoverable_job(&registry, &job_store_provider).await`.
    /// Boot-time crash-recovery sweep is run in `build_app_state` right
    /// after this provider is created.
    pub job_store_provider: Arc<crate::infrastructure::scheduler::PgJobStoreProvider>,
    /// Fully-decorated blob backend (retry → encryption → cache
    /// stack applied). Exposed here so tenants outside
    /// `create_core_services` — notably `blobs_consistency` in
    /// `build_app_state` — can probe `blob_exists()` / re-hash bytes
    /// through the same stack DedupService uses.
    ///
    /// Concretely this is the hot-swap wrapper coerced to
    /// `Arc<dyn ...>`; a migration cutover replaces the inner
    /// backend via [`Self::blob_backend_hot_swap`] and every future
    /// call through this `blob_backend` sees the new inner.
    pub blob_backend: Arc<dyn BlobStorageBackend>,
    /// Typed handle to the hot-swap wrapper. Distinct from
    /// [`Self::blob_backend`] only in its declared type: the raw
    /// wrapper struct instead of `dyn BlobStorageBackend`. Same
    /// underlying instance, so a call to `.swap(new)` here is
    /// immediately visible through the trait-object handle above.
    /// The migration handler is the only intended caller — it flips
    /// the pointer on `RunOutcome::Completed`, so restart is no
    /// longer required for cutover.
    pub blob_backend_hot_swap:
        Arc<crate::infrastructure::services::swappable_blob_backend::SwappableBlobBackend>,
    /// Name of the storage entry the LIVE `blob_backend` was built
    /// from. Populated at boot: either from
    /// `admin_settings.storage.active_backend_name` when set, or the
    /// first entry in `OXICLOUD_STORAGE_ENTRIES` when unset. For the
    /// no-entries legacy path this is `"default"` (the synthesized
    /// name) or `"legacy"` (framework-defaults case with zero storage
    /// config at all). Migration handler consumes this to enforce the
    /// "target != active" no-op guard by name; without needing to
    /// re-read DB on every trigger.
    ///
    /// Wrapped in `Arc<RwLock<String>>` so the migration handler can
    /// update it on hot-swap — subsequent name-based guards (a
    /// second migration triggered by the admin after the first cut
    /// over) see the new active without a restart. Read pattern:
    /// acquire the read lock, clone the inner String, release the
    /// lock, use the clone across await points.
    pub active_backend_name: Arc<std::sync::RwLock<String>>,
}

/// Container for repository services
#[derive(Clone)]
pub struct RepositoryServices {
    pub folder_repository: Arc<FolderDbRepository>,
    pub folder_repo_concrete: Arc<FolderDbRepository>,
    pub file_read_repository: Arc<FileBlobReadRepository>,
    pub file_write_repository: Arc<FileBlobWriteRepository>,
    pub file_metadata_repository: Arc<FileMetadataRepository>,
    pub i18n_repository: Arc<FileSystemI18nService>,
    pub trash_repository: Option<Arc<TrashDbRepository>>,
}

/// Container for application services
#[derive(Clone)]
pub struct ApplicationServices {
    // Concrete types for compatibility with existing handlers
    pub folder_service_concrete: Arc<FolderService>,
    // Traits for abstraction
    pub folder_service: Arc<FolderService>,
    pub file_upload_service: Arc<FileUploadService>,
    pub delta_upload_service:
        Arc<crate::application::services::delta_upload_service::DeltaUploadService>,
    pub file_retrieval_service: Arc<FileRetrievalService>,
    pub file_management_service: Arc<FileManagementService>,
    /// Streams uploads straight to an external mount provider (bypasses the CAS).
    pub external_upload_service:
        Arc<crate::application::services::external_upload_service::ExternalUploadService>,
    pub file_use_case_factory: Arc<dyn FileUseCaseFactory>,
    pub i18n_service: Arc<I18nApplicationService>,
    pub trash_service: Option<Arc<TrashService>>,
    pub search_service: Option<Arc<SearchService>>,
    pub share_service: Option<Arc<ShareService>>,
    pub favorites_service: Option<Arc<FavoritesService>>,
    pub recent_service: Option<Arc<RecentService>>,
    pub audio_metadata_service: Option<Arc<AudioMetadataService>>,
    pub media_metadata_service: Arc<MediaMetadataService>,
}

/// Container for authentication services
#[derive(Clone)]
pub struct AuthServices {
    pub token_service: Arc<JwtTokenService>,
    pub auth_application_service: Arc<AuthApplicationService>,
    pub login_lockout:
        Arc<crate::infrastructure::services::login_lockout_service::LoginLockoutService>,
}

/// Container for Nextcloud compatibility services
#[derive(Clone)]
pub struct NextcloudServices {
    pub login_flow: Arc<NextcloudLoginFlowService>,
    pub app_passwords: Arc<AppPasswordService>,
    pub file_ids: Arc<NextcloudFileIdService>,
    pub chunked_uploads: Arc<NextcloudChunkedUploadService>,
}

/// Global application state for dependency injection
#[derive(Clone)]
pub struct AppState {
    pub core: CoreServices,
    pub repositories: RepositoryServices,
    pub applications: ApplicationServices,
    /// Validated set of locales the server knows about. Surfaced to
    /// handlers so the `Accept-Language` extractor and any
    /// locale-validation code (OIDC JIT, profile-edit) can consult one
    /// canonical list.
    pub locale_registry: Arc<LocaleRegistry>,
    pub db_pool: Option<Arc<PgPool>>,
    /// Isolated pool for background / batch operations.
    pub maintenance_pool: Option<Arc<PgPool>>,
    /// External-mount classifier + registry. Always present; an empty registry
    /// (feature disabled or no mounts configured) makes `classify` a cheap no-op
    /// that routes every id to native handling. Handlers consult this before
    /// parsing an id as a UUID, then call the matching service-layer mount
    /// method (which still owns the authorization check).
    pub mount_router:
        Arc<crate::application::services::external_mount_router::MountRouter>,
    pub auth_service: Option<AuthServices>,
    pub nextcloud: Option<NextcloudServices>,
    pub admin_settings_service: Option<Arc<AdminSettingsService>>,
    /// WASM plugin management (list/install/toggle/remove), backing the admin
    /// Plugins tab. `None` when the `plugins` feature is compiled out or
    /// `OXICLOUD_ENABLE_PLUGINS` is false — the admin endpoints return 503 then.
    pub plugin_management:
        Option<Arc<dyn crate::application::ports::plugin_ports::PluginManagementPort>>,
    pub storage_settings_service: Option<Arc<StorageSettingsService>>,
    pub trash_service: Option<Arc<TrashService>>,
    pub share_service: Option<Arc<ShareService>>,
    pub share_browse_service: Option<Arc<ShareBrowseService>>,
    pub favorites_service: Option<Arc<FavoritesService>>,
    pub recent_service: Option<Arc<RecentService>>,
    pub places_service: Option<Arc<PlacesService>>,
    pub people_service: Option<Arc<PeopleService>>,
    pub storage_usage_service: Option<Arc<StorageUsageService>>,
    /// Handle to the service that purges expired `storage.role_grants`
    /// rows. Registered with the periodic-job scheduler on the
    /// configured cadence; `None` when disabled via
    /// `OXICLOUD_GRANT_CLEANUP_ENABLED=false`. Exposed on `AppState`
    /// so the admin trigger endpoint can invoke `purge(Some(0))` for
    /// the `?force=true` grace-override path.
    pub grant_cleanup_service: Option<
        Arc<crate::infrastructure::services::grant_cleanup_service::GrantCleanupService>,
    >,
    pub calendar_service: Option<Arc<CalendarService>>,
    pub calendar_use_case: Option<Arc<CalendarService>>,
    pub addressbook_use_case: Option<Arc<ContactService>>,
    pub contact_use_case: Option<Arc<ContactService>>,
    pub music_service: Option<Arc<MusicService>>,
    pub wopi_token_service:
        Option<Arc<crate::application::services::wopi_token_service::WopiTokenService>>,
    pub wopi_lock_service:
        Option<Arc<crate::application::services::wopi_lock_service::WopiLockService>>,
    pub wopi_discovery_service:
        Option<Arc<crate::infrastructure::services::wopi_discovery_service::WopiDiscoveryService>>,
    pub device_auth_service:
        Option<Arc<crate::application::services::device_auth_service::DeviceAuthService>>,
    pub app_password_service:
        Option<Arc<crate::application::services::app_password_service::AppPasswordService>>,
    pub path_resolver:
        Option<Arc<crate::infrastructure::services::path_resolver_service::PathResolverService>>,
    pub webdav_lock_store:
        Arc<crate::infrastructure::services::webdav_lock_service::WebDavLockStore>,
    pub webdav_dead_props:
        Arc<crate::infrastructure::services::webdav_dead_property_store::DeadPropertyStore>,
    /// ReBAC authorization engine — all service-layer permission checks go
    /// through this. Concrete type today is `PgAclEngine`; the
    /// `AuthorizationEngine` trait describes the contract. When alternate
    /// implementations land (OpenFGA, cached decorator), swap this field for
    /// an enum dispatcher or `Arc<dyn AuthorizationEngine>` (with
    /// `async_trait` boxing).
    pub authorization: Arc<crate::infrastructure::services::pg_acl_engine::PgAclEngine>,
    /// Global "server is in migration read-only mode" flag, shared
    /// with [`Self::authorization`] so it can short-circuit write
    /// permissions. Backed by
    /// `admin_settings.storage.migration_readonly` for restart
    /// survival. Slice 5's cutover state machine flips this atomic
    /// (via `Ordering::Relaxed`) and calls
    /// `entry_backend::persist_migration_readonly` to keep DB and
    /// memory in sync. See `docs/plan/storage-multi-entry.md`
    /// §"Read-only mode".
    pub migration_readonly: Arc<std::sync::atomic::AtomicBool>,
    /// Live progress snapshot for the storage-migration handler.
    /// `Some(_)` while a migration is running; `None` otherwise.
    /// Updated by the handler on every batch checkpoint (cheap
    /// in-memory write, no DB read on the request path). The
    /// server-status header middleware reads it to inform every
    /// user's session banner about maintenance progress without
    /// polling. See `MigrationProgress` for the field shape.
    pub migration_progress: Arc<std::sync::RwLock<Option<crate::common::migration_progress::MigrationProgress>>>,
    /// Drive entity repository — `GET /api/drives`, the personal-drive
    /// lifecycle hook, and (post-D2) shared-drive creation flow all read
    /// through this. Backing table is `storage.drives`; membership is
    /// resolved through `role_grants` not a separate `drive_members`
    /// table (see `docs/plan/drive.md` §3).
    pub drive_repo: Arc<crate::infrastructure::repositories::pg::DrivePgRepository>,
    /// D2 — drive membership management service. Translates the membership
    /// API (`POST/PATCH/DELETE /api/drives/{id}/members`) into role-grant
    /// writes on `resource_type='drive'`, with the personal-drive guard
    /// and shared-drive last-owner protection layered in.
    pub drive_management_service: Arc<
        crate::application::services::drive_management_service::DriveManagementService,
    >,
    /// ReBAC subject-group management (CRUD + membership). `None` when the
    /// auth subsystem is not configured.
    pub subject_group_service:
        Option<Arc<crate::application::services::subject_group_service::SubjectGroupService>>,
    /// Outbound transactional email — `None` when `OXICLOUD_SMTP_HOST` is
    /// empty. Endpoints that need email (magic-link invite, login-via-email)
    /// must return 503 when this is `None` rather than silently dropping
    /// the message.
    pub email_sender: Option<Arc<dyn crate::application::ports::email_sender::EmailSender>>,
    /// Set alongside `email_sender` when the test harness flag
    /// `OXICLOUD_SMTP_MOCK=true` is on. Used by the
    /// `GET /api/admin/smtp/test/captured` test-only endpoint to look up
    /// recently captured messages. Always `None` in production.
    pub mock_email_sender:
        Option<Arc<crate::infrastructure::services::mock_email_sender::MockEmailSender>>,
    /// Invite-by-email orchestrator — `None` when SMTP isn't configured
    /// (no `email_sender`). `POST /api/grants` with `subject.type=email`
    /// returns 503 when this is `None`.
    pub magic_link_invite_service: Option<
        Arc<crate::application::services::magic_link_invite_service::MagicLinkInviteService>,
    >,
    /// Unified share-notification dispatcher (PR N1) — used by both
    /// `create_grant` and the future `POST /api/grants/{id}/notify` to
    /// route share emails through coalesce + rate-limit + per-recipient
    /// dispatch. `None` when SMTP / magic-link / subject-group services
    /// aren't all configured; callers degrade to silent no-op in that
    /// case (no mail sent, grant still created).
    pub recipient_notification_service: Option<
        Arc<crate::application::services::recipient_notification_service::RecipientNotificationService>,
    >,
    /// Per-caller sliding-window limiter for `GET /api/users/{id}`. The
    /// endpoint's primary defense is the visibility check, but a stale
    /// JWT could in theory iterate UUIDs against the related-by-grant
    /// branch of that check. 60 lookups per minute keyed on the
    /// authenticated caller covers any legitimate UI rendering while
    /// throttling enumeration.
    pub user_profile_rate_limiter: Arc<crate::interfaces::middleware::rate_limit::RateLimiter>,
    /// Per-caller flood guard for the delta-upload endpoints
    /// (negotiate / chunks / commit share one budget).
    pub delta_upload_rate_limiter: Arc<crate::interfaces::middleware::rate_limit::RateLimiter>,
    /// Per-sharer ceiling on `POST /api/grants` invitations whose
    /// subject is `{ type: "email" }`. 50 per hour keyed on
    /// `caller_id`. Anonymous attackers can't reach this code path
    /// (the route is auth-protected); this defends against a
    /// compromised internal account or a malicious admin.
    pub email_invite_rate_limiter: Arc<crate::interfaces::middleware::rate_limit::RateLimiter>,
    /// Per-target-email ceiling on `POST /api/auth/magic-link/send`. 5
    /// per hour keyed on the **normalised** target email. Exceeding
    /// the cap is silently absorbed: the handler still returns the
    /// uniform 200 anti-enumeration response, but no new mail is
    /// dispatched. Authenticated callers bypass this limit.
    pub magic_link_send_per_email_rate_limiter:
        Arc<crate::interfaces::middleware::rate_limit::RateLimiter>,
    /// Per-source-IP backstop on `POST /api/auth/magic-link/send`. 200
    /// per hour keyed on the trusted client IP (respects
    /// `OXICLOUD_TRUST_PROXY_CIDR`). Bounds the cost of a single
    /// attacker spreading 5/hr requests over a wide email list.
    /// Authenticated callers bypass this limit.
    pub magic_link_send_per_ip_rate_limiter:
        Arc<crate::interfaces::middleware::rate_limit::RateLimiter>,
    /// Handle to the periodic-job scheduler's supervisor task, spawned
    /// at the end of `build_app_state` after every native service has
    /// registered its jobs on `core.job_registry`. `Option` because
    /// tests that assemble a partial `AppState` (no full DI) skip the
    /// scheduler; production always populates it. Held here purely so
    /// the tokio task isn't dropped — the supervisor loop runs off its
    /// internal `JoinHandle`, not off this reference.
    pub scheduler_engine: Option<Arc<SchedulerEngine>>,
}

// All AppState construction is done via struct literal in build_app_state().

impl AppState {
    /// Drive-aware RFC 4331 quota resolution — shared by the native and
    /// NextCloud-compatible WebDAV PROPFIND handlers so both surfaces
    /// report the same numbers for the same drive.
    ///
    /// - `drive_id == Uuid::nil()`: synthetic drive-listing pseudo-root —
    ///   no single drive, so the account envelope is the only defensible
    ///   answer.
    /// - Personal drives carry no quota of their own (`Drive::quota_bytes`
    ///   is NULL post-migration) — the account envelope in `auth.users`
    ///   caps them.
    /// - Shared drives carry their own finite quota on `storage.drives` —
    ///   report that, not the owner's unrelated personal envelope.
    ///
    /// `available` is `None` for unlimited accounts/drives (quota <= 0 or
    /// unset) — RFC 4331 §3 lets a server omit `quota-available-bytes`
    /// rather than disclose a made-up value. Any lookup failure (quota
    /// subsystem disabled, drive gone) is treated the same way: quota is
    /// silently omitted rather than failing the whole PROPFIND.
    pub async fn resolve_webdav_quota(
        &self,
        user_id: Uuid,
        drive_id: Uuid,
    ) -> Option<(i64, Option<i64>)> {
        let storage_svc = self.storage_usage_service.as_ref()?;

        if drive_id.is_nil() {
            let (used, quota) = storage_svc.get_user_storage_info(user_id).await.ok()?;
            return Some((used, (quota > 0).then(|| (quota - used).max(0))));
        }

        let drive = self.drive_repo.get_by_id(drive_id).await.ok()?.drive;
        match drive.kind {
            DriveKind::Personal => {
                let (used, quota) = storage_svc.get_user_storage_info(user_id).await.ok()?;
                Some((used, (quota > 0).then(|| (quota - used).max(0))))
            }
            DriveKind::Shared => {
                let used = drive.used_bytes;
                Some((used, drive.quota_bytes.map(|q| (q - used).max(0))))
            }
        }
    }
}

/// Builds the authorization engine. Today this only constructs `PgAclEngine`;
/// the `OXICLOUD_AUTHZ_ENGINE` env var is reserved for future alternate
/// implementations (e.g. `openfga`).
fn build_authorization_engine(
    pool: Arc<PgPool>,
    folder_repo: Arc<
        crate::infrastructure::repositories::pg::folder_db_repository::FolderDbRepository,
    >,
    file_repo: Arc<
        crate::infrastructure::repositories::pg::file_blob_read_repository::FileBlobReadRepository,
    >,
    group_repo: Arc<crate::infrastructure::repositories::pg::SubjectGroupPgRepository>,
    migration_readonly: Arc<std::sync::atomic::AtomicBool>,
) -> Arc<crate::infrastructure::services::pg_acl_engine::PgAclEngine> {
    use crate::infrastructure::services::pg_acl_engine::PgAclEngine;

    if let Ok(other) = std::env::var("OXICLOUD_AUTHZ_ENGINE")
        && other != "postgres"
        && !other.is_empty()
    {
        panic!(
            "OXICLOUD_AUTHZ_ENGINE={other:?} is not yet supported. Only 'postgres' is implemented; leave the variable unset to use the default."
        );
    }
    Arc::new(PgAclEngine::new(
        pool,
        folder_repo,
        file_repo,
        group_repo,
        migration_readonly,
    ))
}

/// Pair returned by [`build_email_sender`] when wiring DI: the
/// `EmailSender` trait object used by the rest of the application, plus
/// (in mock mode only) a typed handle to the same `MockEmailSender` so
/// the test-only capture endpoint can introspect it without downcasting.
struct EmailSenderBundle {
    sender: Option<Arc<dyn crate::application::ports::email_sender::EmailSender>>,
    mock: Option<Arc<crate::infrastructure::services::mock_email_sender::MockEmailSender>>,
}

/// Construct the SMTP email sender from config, or return `None` when
/// SMTP is disabled (`OXICLOUD_SMTP_HOST` empty). Construction errors
/// (unparseable `From:` mailbox, bad TLS settings) downgrade to `None`
/// with a `WARN` log — the server still starts, but every magic-link
/// endpoint will return 503 until the operator fixes the config.
///
/// When `OXICLOUD_SMTP_MOCK=true` (test harness only — never in
/// production), construction returns an in-process `MockEmailSender`
/// that captures every message instead of sending it. The harness
/// retrieves captured messages via `GET /api/admin/smtp/test/captured`.
fn build_email_sender(cfg: &crate::common::config::SmtpConfig) -> EmailSenderBundle {
    if std::env::var("OXICLOUD_SMTP_MOCK")
        .map(|v| v == "true" || v == "1")
        .unwrap_or(false)
    {
        tracing::warn!(
            target: "oxicloud",
            event = "smtp.mock_enabled",
            "OXICLOUD_SMTP_MOCK=true — outbound mail is being captured in-process. \
             Test harness only; never set this in production.",
        );
        let mock =
            Arc::new(crate::infrastructure::services::mock_email_sender::MockEmailSender::new());
        return EmailSenderBundle {
            sender: Some(mock.clone()),
            mock: Some(mock),
        };
    }

    if !cfg.is_enabled() {
        tracing::info!(
            "SMTP disabled (OXICLOUD_SMTP_HOST empty); magic-link endpoints will return 503"
        );
        return EmailSenderBundle {
            sender: None,
            mock: None,
        };
    }
    match crate::infrastructure::services::smtp_email_sender::SmtpEmailSender::new(cfg) {
        Ok(sender) => {
            tracing::info!(
                target: "oxicloud",
                event = "smtp.configured",
                host = %cfg.host,
                port = cfg.port,
                tls = ?cfg.tls,
                from = %cfg.from,
                user = if cfg.user.is_empty() { "<anon>" } else { "<set>" },
                "SMTP sender configured",
            );
            EmailSenderBundle {
                sender: Some(Arc::new(sender)),
                mock: None,
            }
        }
        Err(e) => {
            tracing::warn!(
                target: "oxicloud",
                event = "smtp.config_invalid",
                error = %e,
                "SMTP configuration is invalid; magic-link endpoints will return 503",
            );
            EmailSenderBundle {
                sender: None,
                mock: None,
            }
        }
    }
}
