use std::sync::Arc;
use uuid::Uuid;

use crate::application::dtos::file_dto::FileDto;
use crate::application::ports::authorization_ports::AuthorizationEngine;
use crate::application::ports::file_lifecycle::FileLifecycleHook;
use crate::application::ports::file_ports::{FileUploadUseCase, StoredBlob};
use crate::application::ports::storage_ports::{FileReadPort, FileWritePort, StorageUsagePort};
use crate::application::services::storage_usage_service::StorageUsageService;
use crate::common::errors::DomainError;
use crate::domain::services::authorization::{Permission, Resource, Subject};
use crate::infrastructure::repositories::pg::FileBlobReadRepository;
use crate::infrastructure::repositories::pg::FileBlobWriteRepository;
use crate::infrastructure::services::dedup_service::DedupService;
use crate::infrastructure::services::file_content_cache::FileContentCache;
use crate::infrastructure::services::pg_acl_engine::PgAclEngine;
use tracing::{info, warn};

/// Service for file upload operations.
///
/// Content never passes through this service: the interface layer streams
/// the request body straight into the CDC chunk store (no spool file, no
/// full-body buffering) and hands over a [`StoredBlob`] reference. This
/// service registers the metadata row, keeps caches coherent and fires
/// lifecycle hooks. Blob-reference ownership is consumed by the write
/// port, which releases it on failure — callers never compensate.
pub struct FileUploadService {
    /// Write port — registers file rows against ingested blobs.
    file_write: Arc<FileBlobWriteRepository>,
    /// Read port — needed for WebDAV/WOPI update-by-path.
    file_read: Option<Arc<FileBlobReadRepository>>,
    /// Optional storage usage tracking
    storage_usage_service: Option<Arc<StorageUsageService>>,
    /// Content cache — invalidated on file update so stale content is never served.
    content_cache: Option<Arc<FileContentCache>>,
    /// Single lifecycle dispatcher — fires on_file_created / on_file_updated.
    file_lifecycle_hook: Option<Arc<dyn FileLifecycleHook>>,
    /// Dependencies of the instant-upload path
    /// (`create_file_from_owned_blob_with_perms`); `None` in minimal test
    /// wiring.
    instant_upload: Option<InstantUploadDeps>,
}

/// Everything the instant-upload path needs beyond the upload service's own
/// ports: permission checks, the dedup index, and quota enforcement.
struct InstantUploadDeps {
    authz: Arc<PgAclEngine>,
    dedup: Arc<DedupService>,
    quota: Arc<StorageUsageService>,
}

impl FileUploadService {
    /// Constructor with write port only (minimal).
    pub fn new(file_repository: Arc<FileBlobWriteRepository>) -> Self {
        Self {
            file_write: file_repository,
            file_read: None,
            storage_usage_service: None,
            content_cache: None,
            file_lifecycle_hook: None,
            instant_upload: None,
        }
    }

    /// Constructor for blob-storage model: write + read ports.
    pub fn new_with_read(
        file_write: Arc<FileBlobWriteRepository>,
        file_read: Arc<FileBlobReadRepository>,
    ) -> Self {
        Self {
            file_write,
            file_read: Some(file_read),
            storage_usage_service: None,
            content_cache: None,
            file_lifecycle_hook: None,
            instant_upload: None,
        }
    }

    /// Wires the authorization engine, dedup index and quota service that
    /// power the instant-upload path.
    pub fn with_instant_upload(
        mut self,
        authz: Arc<PgAclEngine>,
        dedup: Arc<DedupService>,
        quota: Arc<StorageUsageService>,
    ) -> Self {
        self.instant_upload = Some(InstantUploadDeps {
            authz,
            dedup,
            quota,
        });
        self
    }

    /// Configures the content cache for invalidation on file updates.
    pub fn with_content_cache(mut self, cache: Arc<FileContentCache>) -> Self {
        self.content_cache = Some(cache);
        self
    }

    /// Registers the lifecycle hook dispatcher (thumbnails, audio metadata, …).
    pub fn with_file_lifecycle_hook(mut self, hook: Arc<dyn FileLifecycleHook>) -> Self {
        self.file_lifecycle_hook = Some(hook);
        self
    }

    /// Configures the storage usage service
    pub fn with_storage_usage_service(
        mut self,
        storage_usage_service: Arc<StorageUsageService>,
    ) -> Self {
        self.storage_usage_service = Some(storage_usage_service);
        self
    }

    // ── Instant upload (zero content bytes) ──────────────────────

    /// Register a new file row pointing at a blob the caller **already
    /// owns** — the instant-upload path: the client proved it has the
    /// content by hash, so no bytes travel and no chunk is written. Pure
    /// metadata: one ref_count bump + one row INSERT.
    ///
    /// Security model (mirrors `GET /api/dedup/check/{hash}`):
    /// - The caller must have `Create` permission on the target folder.
    /// - The hash is only claimable when the caller owns at least one
    ///   non-trashed file referencing it — never a global content oracle.
    ///   A non-owned hash returns `NotFound` (anti-enumeration: same shape
    ///   as "no such blob") and emits an `instant_upload.rejected` audit
    ///   event with the real reason.
    /// - Quota is enforced on the logical size, exactly like a byte upload.
    pub async fn create_file_from_owned_blob_with_perms(
        &self,
        caller_id: Uuid,
        name: String,
        folder_id: String,
        hash: &str,
    ) -> Result<FileDto, DomainError> {
        let Some(InstantUploadDeps {
            authz,
            dedup,
            quota,
        }) = &self.instant_upload
        else {
            return Err(DomainError::internal_error(
                "FileUpload",
                "instant upload is not wired (authz/dedup/quota missing)",
            ));
        };

        // ── AuthZ: Create on the target folder ───────────────────
        let folder_uuid = Uuid::parse_str(&folder_id)
            .map_err(|_| DomainError::not_found("Folder", folder_id.clone()))?;
        authz
            .require(
                Subject::User(caller_id),
                Permission::Create,
                Resource::Folder(folder_uuid),
            )
            .await?;

        // ── Ownership: only blobs the caller can already read ────
        if !dedup
            .user_owns_blob_reference(hash, &caller_id.to_string())
            .await
        {
            tracing::info!(
                target: "audit",
                event = "instant_upload.rejected",
                reason = "hash_not_owned",
                caller_id = %caller_id,
                blob_hash = %hash,
                "👮🏻‍♂️ Instant upload rejected: caller owns no file referencing the claimed hash",
            );
            return Err(DomainError::not_found("Blob", hash));
        }

        let Some(metadata) = dedup.get_blob_metadata(hash).await else {
            // Lost a race with the last-reference delete — same shape as
            // "never existed".
            return Err(DomainError::not_found("Blob", hash));
        };

        // ── Quota on the logical size, before taking any reference ──
        quota.check_storage_quota(caller_id, metadata.size).await?;

        // The manifest knows the original content type; fall back to the
        // new name's extension when the stored one is generic.
        let claimed = metadata.content_type.as_deref().unwrap_or("");
        let content_type =
            match crate::common::mime_detect::refine_content_type(&[], &name, claimed) {
                ct if ct.is_empty() => "application/octet-stream".to_string(),
                ct => ct,
            };

        // Take the reference the row registration will consume (it releases
        // it again on any failure). A concurrent GC between the ownership
        // check and this bump surfaces as NotFound — the client falls back
        // to a normal byte upload.
        dedup.add_reference(hash).await?;

        let dto = self
            .upload_file_streaming(
                name,
                Some(folder_id),
                content_type,
                StoredBlob {
                    hash: hash.to_string(),
                    size: metadata.size,
                    is_new_blob: false,
                },
            )
            .await?;

        info!(
            "⚡ INSTANT UPLOAD: {} ({} bytes, 0 transferred, ID: {})",
            dto.name, metadata.size, dto.id
        );
        Ok(dto)
    }

    /// Swap an existing file's content to an already-ingested blob — the
    /// update mode of the delta-upload commit. The caller needs `Write`
    /// permission on the file; the blob reference is consumed (released on
    /// failure by the write port, like every other registration path).
    pub async fn update_file_content_by_id_with_perms(
        &self,
        caller_id: Uuid,
        file_id: &str,
        blob: StoredBlob,
    ) -> Result<FileDto, DomainError> {
        let Some(InstantUploadDeps { authz, .. }) = &self.instant_upload else {
            return Err(DomainError::internal_error(
                "FileUpload",
                "instant upload is not wired (authz/dedup/quota missing)",
            ));
        };
        let Some(file_read) = &self.file_read else {
            return Err(DomainError::internal_error(
                "FileUpload",
                "read port is not wired",
            ));
        };

        let file_uuid = Uuid::parse_str(file_id)
            .map_err(|_| DomainError::not_found("File", file_id.to_string()))?;
        authz
            .require(
                Subject::User(caller_id),
                Permission::Update,
                Resource::File(file_uuid),
            )
            .await?;

        let file = file_read.get_file(file_id).await?;
        let (new_hash, updated_at) = self
            .file_write
            .update_file_content_with_blob(file_id, &blob.hash, blob.size, None)
            .await?;
        // The file maps to a different blob now — stale cached content must
        // never be served for the rest of its TTI window.
        if let Some(cc) = &self.content_cache {
            cc.invalidate(file_id).await;
        }

        let parts = file.into_parts();
        let updated = crate::domain::entities::file::File::with_timestamps_and_blob_hash(
            parts.id,
            parts.name,
            parts.storage_path,
            blob.size,
            parts.mime_type,
            parts.folder_id,
            parts.created_at,
            updated_at as u64,
            parts.owner_id,
            new_hash,
        )
        .map_err(|e| DomainError::internal_error("FileUpload", format!("rebuild entity: {e}")))?;
        let dto = FileDto::from(updated);
        if let Some(hook) = &self.file_lifecycle_hook {
            hook.on_file_updated(file_id, &dto.content_hash, &dto.mime_type);
        }
        Ok(dto)
    }

    // ── private helpers ──────────────────────────────────────────

    /// Bump the owner's cached storage usage after a successful upload.
    ///
    /// Incremental (`+size`, O(1)) and fire-and-forget on a background task, so
    /// it adds neither latency nor a `SUM(size)` over the user's whole library
    /// to the upload path (the previous full recompute was O(N) per upload,
    /// O(N²) for a bulk upload). Keyed by the file's `owner_id`; drift — e.g.
    /// deletes, which don't decrement — is reconciled by the periodic sweep. A
    /// DTO without a resolvable owner is simply left to that sweep.
    fn maybe_update_storage_usage(&self, file: &FileDto) {
        let Some(storage_service) = &self.storage_usage_service else {
            return;
        };
        let Some(owner) = file
            .owner_id
            .as_deref()
            .and_then(|s| Uuid::parse_str(s).ok())
        else {
            return;
        };
        let delta = file.size as i64;
        let service_clone = Arc::clone(storage_service);
        tokio::spawn(async move {
            if let Err(e) = service_clone
                .add_user_storage_usage_delta(owner, delta)
                .await
            {
                warn!("Failed to bump storage usage for {owner}: {e}");
            }
        });
    }
}

impl FileUploadUseCase for FileUploadService {
    /// Register a new file row pointing at an already-ingested blob.
    async fn upload_file_streaming(
        &self,
        name: String,
        folder_id: Option<String>,
        content_type: String,
        blob: StoredBlob,
    ) -> Result<FileDto, DomainError> {
        let file = self
            .file_write
            .save_file_with_blob(name.clone(), folder_id, content_type, &blob.hash, blob.size)
            .await?;
        let dto = FileDto::from(file);
        info!(
            "📡 STREAMING UPLOAD: {} ({} bytes, ID: {})",
            name, blob.size, dto.id
        );
        self.maybe_update_storage_usage(&dto);
        if let Some(hook) = &self.file_lifecycle_hook {
            hook.on_file_created(&dto.id, &dto.content_hash, &dto.mime_type, blob.is_new_blob);
        }
        Ok(dto)
    }

    /// Swap the content of the file at `path` to an already-ingested blob,
    /// creating the file when it doesn't exist (WebDAV/NextCloud/WOPI PUT).
    async fn update_file_streaming(
        &self,
        path: &str,
        blob: StoredBlob,
        content_type: &str,
        modified_at: Option<i64>,
    ) -> Result<FileDto, DomainError> {
        // Try to find the existing file first
        if let Some(file_read) = &self.file_read
            && let Some(file) = file_read.find_file_by_path(path).await?
        {
            let file_id = file.id().to_string();
            let (new_hash, updated_at) = self
                .file_write
                .update_file_content_with_blob(&file_id, &blob.hash, blob.size, modified_at)
                .await?;
            // Invalidate content cache — file content has changed.
            if let Some(cc) = &self.content_cache {
                cc.invalidate(&file_id).await;
            }
            // Rebuild the fresh DTO from the entity already in hand plus the
            // values the UPDATE just returned — a re-read would only fetch
            // what we already know, at one extra round-trip per overwrite
            // (WebDAV sync clients overwrite constantly).
            let parts = file.into_parts();
            let updated = crate::domain::entities::file::File::with_timestamps_and_blob_hash(
                parts.id,
                parts.name,
                parts.storage_path,
                blob.size,
                parts.mime_type,
                parts.folder_id,
                parts.created_at,
                updated_at as u64,
                parts.owner_id,
                new_hash,
            )
            .map_err(|e| {
                DomainError::internal_error("FileUpload", format!("rebuild entity: {e}"))
            })?;
            let dto = FileDto::from(updated);
            if let Some(hook) = &self.file_lifecycle_hook {
                hook.on_file_updated(&file_id, &dto.content_hash, content_type);
            }
            return Ok(dto);
        }

        // File doesn't exist — create it via streaming upload
        let path_normalized = path.trim_start_matches('/').trim_end_matches('/');
        let (_, filename) = if let Some(idx) = path_normalized.rfind('/') {
            (&path_normalized[..idx], &path_normalized[idx + 1..])
        } else {
            ("", path_normalized)
        };

        // get_parent_folder_id expects the full file path — it strips the
        // last segment (filename) internally to find the parent folder.
        let parent_id = if path_normalized.contains('/') {
            if let Some(file_read) = &self.file_read {
                file_read.get_parent_folder_id(path_normalized).await.ok()
            } else {
                None
            }
        } else {
            None
        };

        let is_new_blob = blob.is_new_blob;
        let created = self
            .file_write
            .save_file_with_blob(
                filename.to_string(),
                parent_id,
                content_type.to_string(),
                &blob.hash,
                blob.size,
            )
            .await?;
        let dto = FileDto::from(created);
        if let Some(hook) = &self.file_lifecycle_hook {
            hook.on_file_created(&dto.id, &dto.content_hash, content_type, is_new_blob);
        }
        Ok(dto)
    }
}
