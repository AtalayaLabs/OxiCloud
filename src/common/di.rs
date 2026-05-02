use sqlx::PgPool;
use std::path::PathBuf;
use std::sync::Arc;

use crate::application::ports::blob_storage_ports::BlobStorageBackend;
use crate::common::config::StorageBackendType;
use crate::infrastructure::db::DbPools;

use crate::application::services::admin_settings_service::AdminSettingsService;
use crate::application::services::auth_application_service::AuthApplicationService;
use crate::application::services::storage_settings_service::StorageSettingsService;
use crate::infrastructure::services::migration_blob_backend::MigrationState;

use crate::application::ports::file_ports::FileUseCaseFactory;
use crate::application::services::favorites_service::FavoritesService;
use crate::application::services::folder_service::FolderService;
use crate::application::services::i18n_application_service::I18nApplicationService;
use crate::application::services::nextcloud_file_id_service::NextcloudFileIdService;
use crate::application::services::nextcloud_login_flow_service::NextcloudLoginFlowService;
use crate::application::services::recent_service::RecentService;
use crate::application::services::search_service::SearchService;
use crate::application::services::share_service::ShareService;
use crate::application::services::trash_service::TrashService;
use crate::application::services::{
    AppFileUseCaseFactory, FileManagementService, FileRetrievalService, FileUploadService,
};
use crate::common::config::AppConfig;
use crate::common::errors::DomainError;
use crate::infrastructure::repositories::pg::SharePgRepository;
use crate::infrastructure::repositories::pg::{
    FileBlobReadRepository, FileBlobWriteRepository, FileMetadataRepository, FolderDbRepository,
    TrashDbRepository,
};
use crate::infrastructure::services::file_content_cache::{
    FileContentCache, FileContentCacheConfig,
};
use crate::infrastructure::services::file_system_i18n_service::FileSystemI18nService;
use crate::infrastructure::services::nextcloud_chunked_upload_service::NextcloudChunkedUploadService;
use crate::infrastructure::services::path_service::PathService;
use crate::infrastructure::services::trash_cleanup_service::TrashCleanupService;

use crate::application::services::app_password_service::AppPasswordService;
use crate::application::services::calendar_service::CalendarService;
use crate::application::services::device_auth_service::DeviceAuthService;
use crate::application::services::music_service::MusicService;
use crate::application::services::storage_usage_service::StorageUsageService;
use crate::application::services::wopi_lock_service::WopiLockService;
use crate::application::services::wopi_token_service::WopiTokenService;
use crate::infrastructure::adapters::contact_storage_adapter::ContactStorageAdapter;
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
use crate::infrastructure::services::image_transcode_service::ImageTranscodeService;
use crate::infrastructure::services::jwt_service::JwtTokenService;
use crate::infrastructure::services::password_hasher::Argon2PasswordHasher;
use crate::infrastructure::services::path_resolver_service::PathResolverService;
use crate::infrastructure::services::thumbnail_service::ThumbnailService;
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
}

impl AppServiceFactory {
    /// Creates a new service factory
    pub fn new(storage_path: PathBuf, locales_path: PathBuf) -> Self {
        Self {
            storage_path,
            locales_path,
            config: AppConfig::default(),
        }
    }

    /// Creates a new service factory with custom configuration
    pub fn with_config(storage_path: PathBuf, locales_path: PathBuf, config: AppConfig) -> Self {
        Self {
            storage_path,
            locales_path,
            config,
        }
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

        // Chunked upload service for large files (>10MB)
        let chunked_temp_dir = std::path::PathBuf::from(&self.storage_path).join(".uploads");
        let chunked_upload_service = Arc::new(
            crate::infrastructure::services::chunked_upload_service::ChunkedUploadService::new(
                chunked_temp_dir,
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

        // Build blob storage backend based on configuration
        let base_backend: Arc<dyn BlobStorageBackend> = match self.config.storage.backend {
            StorageBackendType::S3 => {
                let s3_config = self
                    .config
                    .storage
                    .s3
                    .as_ref()
                    .expect("S3 config required when OXICLOUD_STORAGE_BACKEND=s3");
                Arc::new(
                    crate::infrastructure::services::s3_blob_backend::S3BlobBackend::new(s3_config),
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
        };

        // Stack decorators: retry → encryption → cache (inner-to-outer)
        let mut blob_backend: Arc<dyn BlobStorageBackend> = base_backend;

        // Retry decorator (for remote backends)
        if self.config.storage.retry.enabled
            && self.config.storage.backend != StorageBackendType::Local
        {
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

        // Encryption decorator
        if self.config.storage.encryption.enabled {
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
            tracing::info!("Blob storage encryption decorator enabled (AES-256-GCM)");
        }

        // Cache decorator (for remote backends only)
        if self.config.storage.cache.enabled
            && self.config.storage.backend != StorageBackendType::Local
        {
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

        // Deduplication service — PRIMARY blob storage engine (PostgreSQL-backed index)
        let dedup_service = Arc::new(
            crate::infrastructure::services::dedup_service::DedupService::new(
                blob_backend,
                db_pool.clone(),
                maintenance_pool.clone(),
            ),
        );
        dedup_service.initialize().await?;

        tracing::info!(
            "Core services initialized: path service, file content cache, thumbnails, chunked upload, image transcode, dedup (PRIMARY blob storage)"
        );

        Ok(CoreServices {
            path_service,
            file_content_cache,
            thumbnail_service,
            chunked_upload_service,
            image_transcode_service,
            dedup_service,
            zip_service: None, // Placeholder - replaced after app services init
            config: self.config.clone(),
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
                folder_repo_concrete.clone(),
            ));

        // I18n repository
        let i18n_repository = Arc::new(FileSystemI18nService::new(self.locales_path.clone()));

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
    pub fn create_application_services(
        &self,
        core: &CoreServices,
        repos: &RepositoryServices,
        trash_service: Option<Arc<TrashService>>,
        db_pool: &Arc<PgPool>,
        path_resolver: Arc<PathResolverService>,
    ) -> ApplicationServices {
        let folder_service = Arc::new(FolderService::new(
            repos.folder_repository.clone(),
            Some(path_resolver),
        ));

        // Refactored services with all infrastructure ports
        // In blob model, dedup is handled by the repository — no separate write-behind needed
        let file_upload_service = Arc::new(
            FileUploadService::new_with_read(
                repos.file_write_repository.clone(),
                repos.file_read_repository.clone(),
            )
            .with_content_cache(core.file_content_cache.clone()),
        );

        let file_retrieval_service = Arc::new(FileRetrievalService::new_with_cache(
            repos.file_read_repository.clone(),
            core.file_content_cache.clone(),
            core.image_transcode_service.clone(),
        ));

        // FileManagementService — ref_count handled by PG trigger, no dedup port needed
        let file_management_service = Arc::new(FileManagementService::with_trash(
            repos.file_write_repository.clone(),
            trash_service.clone(),
            Some(repos.file_read_repository.clone()),
            Some(repos.folder_repository.clone()),
            Some(core.thumbnail_service.clone()),
            Some(core.file_content_cache.clone()),
        ));

        let file_use_case_factory = Arc::new(AppFileUseCaseFactory::new(
            repos.file_read_repository.clone(),
            repos.file_write_repository.clone(),
        ));

        let i18n_service = Arc::new(I18nApplicationService::new(repos.i18n_repository.clone()));

        // Search service with cache
        let search_service: Option<Arc<SearchService>> = Some(Arc::new(SearchService::new(
            repos.file_read_repository.clone(),
            repos.folder_repository.clone(),
            300,  // Cache TTL in seconds (5 minutes)
            1000, // Maximum cache entries
        )));

        tracing::info!("Application services initialized");

        ApplicationServices {
            // Concrete types for handlers that need them
            folder_service_concrete: folder_service.clone(),
            // Traits for abstraction
            folder_service,
            file_upload_service,
            file_retrieval_service,
            file_management_service,
            file_use_case_factory,
            i18n_service,
            trash_service, // Already set via parameter
            search_service,
            share_service: None,     // Configured later with create_share_service
            favorites_service: None, // Configured later with create_favorites_service
            recent_service: None,    // Configured later with create_recent_service
            audio_metadata_service: self.create_audio_metadata_service(db_pool),
        }
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

    /// Creates the trash service
    pub async fn create_trash_service(
        &self,
        repos: &RepositoryServices,
        core: &CoreServices,
    ) -> Option<Arc<TrashService>> {
        if !self.config.features.enable_trash {
            tracing::info!("Trash service is disabled in configuration");
            return None;
        }

        let trash_repo = repos.trash_repository.as_ref()?;

        // Wire ports directly to TrashService — no adapter layer needed
        let service = Arc::new(TrashService::new(
            trash_repo.clone(),
            repos.file_read_repository.clone(),
            repos.file_write_repository.clone(),
            repos.folder_repository.clone(),
            self.config.storage.trash_retention_days,
            Some(core.thumbnail_service.clone()),
            Some(core.file_content_cache.clone()),
        ));

        // Initialize cleanup service (bulk-deletes expired items in 2 SQL queries)
        let cleanup_service = TrashCleanupService::new(
            trash_repo.clone(),
            24, // Run cleanup every 24 hours
        );

        cleanup_service.start_cleanup_job().await;
        tracing::info!("Trash service initialized with daily cleanup schedule");

        Some(service as Arc<TrashService>)
    }

    /// Creates the sharing service
    pub fn create_share_service(
        &self,
        repos: &RepositoryServices,
        db_pool: &Arc<PgPool>,
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
            password_hasher,
        ));

        tracing::info!("File sharing service initialized");
        Some(service)
    }

    /// Creates the favorites service (requires database)
    pub fn create_favorites_service(&self, db_pool: &Arc<PgPool>) -> Arc<FavoritesService> {
        let repo = Arc::new(
            crate::infrastructure::repositories::pg::FavoritesPgRepository::new(db_pool.clone()),
        );
        let service = Arc::new(FavoritesService::new(repo));
        tracing::info!("Favorites service initialized");
        service
    }

    /// Creates the recent items service (requires database)
    pub fn create_recent_service(&self, db_pool: &Arc<PgPool>) -> Arc<RecentService> {
        let repo = Arc::new(
            crate::infrastructure::repositories::pg::RecentItemsPgRepository::new(db_pool.clone()),
        );
        let service = Arc::new(RecentService::new(
            repo, 50, // Maximum recent items per user
        ));
        tracing::info!("Recent items service initialized");
        service
    }

    /// Preloads translations
    pub async fn preload_translations(&self, i18n_service: &I18nApplicationService) {
        use crate::domain::services::i18n_service::Locale;

        if let Err(e) = i18n_service.load_translations(Locale::English).await {
            tracing::warn!("Failed to load English translations: {}", e);
        }
        if let Err(e) = i18n_service.load_translations(Locale::Spanish).await {
            tracing::warn!("Failed to load Spanish translations: {}", e);
        }
        if let Err(e) = i18n_service.load_translations(Locale::French).await {
            tracing::warn!("Failed to load French translations: {}", e);
        }
        if let Err(e) = i18n_service.load_translations(Locale::German).await {
            tracing::warn!("Failed to load German translations: {}", e);
        }
        if let Err(e) = i18n_service.load_translations(Locale::Portuguese).await {
            tracing::warn!("Failed to load Portuguese translations: {}", e);
        }
        tracing::info!("Translations preloaded");
    }

    /// Creates the storage usage service (requires database)
    ///
    /// Uses the `maintenance_pool` for batch operations
    /// (`update_all_users_storage_usage`) to avoid starving user requests.
    pub fn create_storage_usage_service(
        &self,
        _repos: &RepositoryServices,
        db_pool: &Arc<PgPool>,
        maintenance_pool: &Arc<PgPool>,
    ) -> Arc<StorageUsageService> {
        let user_repository = Arc::new(
            crate::infrastructure::repositories::pg::UserPgRepository::new(db_pool.clone()),
        );
        let service = Arc::new(
            crate::application::services::storage_usage_service::StorageUsageService::new(
                maintenance_pool.clone(),
                user_repository,
            ),
        );
        tracing::info!("Storage usage service initialized");
        service
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

        // PathResolver — used by both FolderService (for ensure_path /
        // owner-scoped MKCOL) and the WebDAV PROPFIND/GET path. Single
        // shared instance; both consumers get the same Arc.
        let path_resolver = Arc::new(PathResolverService::new(pool.clone()));

        // 1. Core services (PgPool needed for DedupService index)
        let core = self.create_core_services(&pool, &maintenance_pool).await?;

        // 2. Repository services (requires PgPool for all metadata)
        let repos = self.create_repository_services(&core, &pool);

        // 3. Trash service (needed before application services)
        let trash_service = self.create_trash_service(&repos, &core).await;

        // 4. Application services (with trash already wired)
        let mut apps = self.create_application_services(
            &core,
            &repos,
            trash_service.clone(),
            &pool,
            path_resolver.clone(),
        );

        // 5. Share service
        let share_service = self.create_share_service(&repos, &pool);
        apps.share_service = share_service.clone();

        // 6. Database-dependent services (PgPool always available in blob model)
        let favorites_service: Option<Arc<FavoritesService>>;
        let recent_service: Option<Arc<RecentService>>;
        let storage_usage_service: Option<Arc<StorageUsageService>>;
        let mut auth_services: Option<crate::common::di::AuthServices> = None;
        let mut nextcloud_services: Option<NextcloudServices> = None;

        {
            let favs = self.create_favorites_service(&pool);
            favorites_service = Some(favs.clone());
            apps.favorites_service = Some(favs);

            let recent = self.create_recent_service(&pool);
            recent_service = Some(recent.clone());
            apps.recent_service = Some(recent);

            storage_usage_service =
                Some(self.create_storage_usage_service(&repos, &pool, &maintenance_pool));

            // Auth services
            if self.config.features.enable_auth {
                let services = crate::infrastructure::auth_factory::create_auth_services(
                    &self.config,
                    pool.clone(),
                    Some(apps.folder_service_concrete.clone()),
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

            let chunk_base = self.storage_path.join(".uploads/nextcloud");
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
            db_pool: Some(pool.clone()),
            maintenance_pool: Some(maintenance_pool),
            auth_service: auth_services,
            nextcloud: nextcloud_services,
            admin_settings_service: None,
            storage_settings_service: None,
            migration_state: Arc::new(tokio::sync::RwLock::new(MigrationState::default())),
            trash_service,
            share_service,
            favorites_service,
            recent_service,
            storage_usage_service,
            calendar_service: None,
            contact_service: None,
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
        };

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

            // 9b-1b. Wire storage settings service (reuses same settings_repo)
            let storage_settings_svc = Arc::new(StorageSettingsService::new(
                settings_repo.clone(),
                self.config.storage.clone(),
                app_state.core.dedup_service.clone(),
            ));
            app_state.storage_settings_service = Some(storage_settings_svc);
            tracing::info!("Storage settings service initialized");

            // 9b-2. Log whether system needs first-time admin setup
            if !admin_svc.is_system_initialized().await {
                tracing::warn!("╔══════════════════════════════════════════════════════════╗");
                tracing::warn!("║  SYSTEM NOT INITIALIZED — first admin setup required     ║");
                tracing::warn!("║                                                          ║");
                tracing::warn!("║  Open the web UI to create the first admin account.       ║");
                tracing::warn!("║  The setup page is available until an admin is created.    ║");
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

        // 9e. Wire PathResolver for single-query WebDAV path resolution.
        // Same instance already injected into FolderService at step 4.
        app_state.path_resolver = Some(path_resolver);
        tracing::info!("PathResolver service initialized");

        // 10. Wire CalDAV/CardDAV services
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
                )
            );
            app_state.addressbook_use_case = Some(contact_storage.clone());
            app_state.contact_use_case = Some(contact_storage);

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
            let music_svc = Arc::new(MusicService::new(music_storage));
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

        Ok(app_state)
    }
}

/// Container for core services
#[derive(Clone)]
pub struct CoreServices {
    pub path_service: Arc<PathService>,
    pub file_content_cache: Arc<FileContentCache>,
    pub thumbnail_service: Arc<ThumbnailService>,
    pub chunked_upload_service: Arc<ChunkedUploadService>,
    pub image_transcode_service: Arc<ImageTranscodeService>,
    pub dedup_service: Arc<DedupService>,
    pub zip_service: Option<Arc<ZipService>>,
    pub config: AppConfig,
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
    pub file_retrieval_service: Arc<FileRetrievalService>,
    pub file_management_service: Arc<FileManagementService>,
    pub file_use_case_factory: Arc<dyn FileUseCaseFactory>,
    pub i18n_service: Arc<I18nApplicationService>,
    pub trash_service: Option<Arc<TrashService>>,
    pub search_service: Option<Arc<SearchService>>,
    pub share_service: Option<Arc<ShareService>>,
    pub favorites_service: Option<Arc<FavoritesService>>,
    pub recent_service: Option<Arc<RecentService>>,
    pub audio_metadata_service: Option<Arc<AudioMetadataService>>,
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
    pub db_pool: Option<Arc<PgPool>>,
    /// Isolated pool for background / batch operations.
    pub maintenance_pool: Option<Arc<PgPool>>,
    pub auth_service: Option<AuthServices>,
    pub nextcloud: Option<NextcloudServices>,
    pub admin_settings_service: Option<Arc<AdminSettingsService>>,
    pub storage_settings_service: Option<Arc<StorageSettingsService>>,
    pub migration_state: Arc<tokio::sync::RwLock<MigrationState>>,
    pub trash_service: Option<Arc<TrashService>>,
    pub share_service: Option<Arc<ShareService>>,
    pub favorites_service: Option<Arc<FavoritesService>>,
    pub recent_service: Option<Arc<RecentService>>,
    pub storage_usage_service: Option<Arc<StorageUsageService>>,
    pub calendar_service: Option<Arc<CalendarService>>,
    pub contact_service: Option<Arc<ContactStorageAdapter>>,
    pub calendar_use_case: Option<Arc<CalendarService>>,
    pub addressbook_use_case: Option<Arc<ContactStorageAdapter>>,
    pub contact_use_case: Option<Arc<ContactStorageAdapter>>,
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
}

// All AppState construction is done via struct literal in build_app_state().
