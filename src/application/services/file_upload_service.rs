use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::application::dtos::file_dto::FileDto;
use crate::application::ports::file_lifecycle::FileLifecycleHook;
use crate::application::ports::file_ports::FileUploadUseCase;
use crate::application::ports::storage_ports::{FileReadPort, FileWritePort};
use crate::application::services::storage_usage_service::StorageUsageService;
use crate::common::errors::DomainError;
use crate::infrastructure::repositories::pg::FileBlobReadRepository;
use crate::infrastructure::repositories::pg::FileBlobWriteRepository;
use crate::infrastructure::services::dedup_service::DedupService;
use crate::infrastructure::services::file_content_cache::FileContentCache;
use tracing::{debug, info, warn};

/// Helper function to extract username from folder path string.
/// e.g. "My Folder - user1/subfolder/file.txt" → "user1"
fn extract_username_from_path(path: &str) -> Option<String> {
    if !path.contains("My Folder - ") {
        return None;
    }
    let parts: Vec<&str> = path.split("My Folder - ").collect();
    if parts.len() <= 1 {
        return None;
    }
    let remainder = parts[1].trim();
    let username = remainder.split('/').next().unwrap_or(remainder);
    let username = username.trim();
    if username.is_empty() {
        return None;
    }
    Some(username.to_string())
}

/// Service for file upload operations.
///
/// **Every upload path converges on streaming-to-disk** — there is no
/// `Vec<u8>` buffer path.
///
/// - **Normal uploads**: handler spools multipart to temp file → `upload_file_streaming`
/// - **Chunked uploads**: chunks already on disk → `upload_file_from_path`
/// - **WebDAV PUT (large)**: handler streams body to temp file → `update_file_streaming`
/// - **WebDAV PUT (small / compat)**: `create_file` / `update_file` spool `&[u8]`
///   to a temp file internally, then call the streaming path.
///
/// Peak RAM usage during any upload: ~256 KB (streaming hash) regardless of file size.
pub struct FileUploadService {
    /// Write port — handles save, streaming, deferred registration
    file_write: Arc<FileBlobWriteRepository>,
    /// Read port — needed for WebDAV create_file / update_file
    file_read: Option<Arc<FileBlobReadRepository>>,
    /// Optional storage usage tracking
    storage_usage_service: Option<Arc<StorageUsageService>>,
    /// Content cache — invalidated on file update so stale content is never served.
    content_cache: Option<Arc<FileContentCache>>,
    /// Single lifecycle dispatcher — fires on_file_created / on_file_updated.
    file_lifecycle_hook: Option<Arc<dyn FileLifecycleHook>>,
    /// Directory for spool temp files (`&[u8]` upload variants). When `Some`,
    /// keeps spools off tmpfs/RAM so they don't count against the cgroup limit.
    upload_temp_dir: Option<PathBuf>,
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
            upload_temp_dir: None,
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
            upload_temp_dir: None,
        }
    }

    /// Configures the spool directory for the `&[u8]` upload variants.
    pub fn with_upload_temp_dir(mut self, dir: Option<PathBuf>) -> Self {
        self.upload_temp_dir = dir;
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

    // ── private helpers ──────────────────────────────────────────

    /// Create a spool temp file, honoring the configured upload temp dir.
    fn new_temp(&self) -> std::io::Result<tempfile::NamedTempFile> {
        crate::common::temp::new_spool_temp_file(self.upload_temp_dir.as_deref())
    }

    /// Optionally update storage usage after a successful upload.
    fn maybe_update_storage_usage(&self, file: &FileDto) {
        if let Some(storage_service) = &self.storage_usage_service {
            let file_path = file.path.clone();
            if let Some(username) = extract_username_from_path(&file_path) {
                let service_clone = Arc::clone(storage_service);
                tokio::spawn(async move {
                    match service_clone
                        .update_user_storage_usage_by_username(&username)
                        .await
                    {
                        Ok(usage) => debug!(
                            "Updated storage usage for user {} to {} bytes",
                            username, usage
                        ),
                        Err(e) => warn!("Failed to update storage usage for {}: {}", username, e),
                    }
                });
            }
        }
    }
}

impl FileUploadUseCase for FileUploadService {
    /// Streaming upload from a temp file on disk.
    ///
    /// Peak RAM: ~256 KB (hash calculation) regardless of file size.
    /// The temp file is consumed (moved/deleted) by the blob store.
    async fn upload_file_streaming(
        &self,
        name: String,
        folder_id: Option<String>,
        content_type: String,
        temp_path: &Path,
        size: u64,
        pre_computed_hash: Option<String>,
    ) -> Result<FileDto, DomainError> {
        let (file, is_new_blob) = self
            .file_write
            .save_file_from_temp_with_dedup(
                name.clone(),
                folder_id,
                content_type,
                temp_path,
                size,
                pre_computed_hash,
            )
            .await?;
        let dto = FileDto::from(file);
        info!(
            "📡 STREAMING UPLOAD: {} ({} bytes, ID: {})",
            name, size, dto.id
        );
        self.maybe_update_storage_usage(&dto);
        if let Some(hook) = &self.file_lifecycle_hook {
            hook.on_file_created(&dto.id, &dto.etag, &dto.mime_type, is_new_blob);
        }
        Ok(dto)
    }

    /// Upload from a file already on disk (chunked uploads).
    async fn upload_file_from_path(
        &self,
        name: String,
        folder_id: Option<String>,
        content_type: String,
        file_path: &Path,
        pre_computed_hash: Option<String>,
    ) -> Result<FileDto, DomainError> {
        let size = tokio::fs::metadata(file_path)
            .await
            .map_err(|e| {
                DomainError::internal_error(
                    "FileUpload",
                    format!("Failed to read file metadata: {}", e),
                )
            })?
            .len();

        self.upload_file_streaming(
            name,
            folder_id,
            content_type,
            file_path,
            size,
            pre_computed_hash,
        )
        .await
    }

    /// Creates a file at a specific path (for WebDAV PUT on new resource).
    ///
    /// Spools the in-memory `&[u8]` to a temp file with hash-on-write,
    /// then delegates to the streaming path.  Peak RAM: the caller's
    /// buffer + ~256 KB for the hasher.
    async fn create_file(
        &self,
        parent_path: &str,
        filename: &str,
        content: &[u8],
        content_type: &str,
    ) -> Result<FileDto, DomainError> {
        // Look up the folder ID by folder path
        let parent_id = if !parent_path.is_empty() {
            if let Some(file_read) = &self.file_read {
                file_read.get_folder_id_by_path(parent_path).await.ok()
            } else {
                None
            }
        } else {
            None
        };

        // Spool to temp file + hash
        let temp = self
            .new_temp()
            .map_err(|e| DomainError::internal_error("FileUpload", format!("temp file: {e}")))?;
        tokio::fs::write(temp.path(), content)
            .await
            .map_err(|e| DomainError::internal_error("FileUpload", format!("write temp: {e}")))?;
        let hash = DedupService::hash_file(temp.path())
            .await
            .map_err(|e| DomainError::internal_error("FileUpload", format!("hash: {e}")))?;

        let (file, is_new_blob) = self
            .file_write
            .save_file_from_temp_with_dedup(
                filename.to_string(),
                parent_id,
                content_type.to_string(),
                temp.path(),
                content.len() as u64,
                Some(hash),
            )
            .await?;
        let dto = FileDto::from(file);
        self.maybe_update_storage_usage(&dto);
        if let Some(hook) = &self.file_lifecycle_hook {
            hook.on_file_created(&dto.id, &dto.etag, &dto.mime_type, is_new_blob);
        }
        Ok(dto)
    }

    /// Updates an existing file's content, or creates it if not found (for WebDAV PUT).
    ///
    /// Spools the in-memory `&[u8]` to a temp file with hash-on-write,
    /// then delegates to the streaming update/create path.
    async fn update_file(
        &self,
        path: &str,
        content: &[u8],
        content_type: &str,
        modified_at: Option<i64>,
    ) -> Result<FileDto, DomainError> {
        // Spool to temp file + hash
        let temp = self
            .new_temp()
            .map_err(|e| DomainError::internal_error("FileUpload", format!("temp file: {e}")))?;
        tokio::fs::write(temp.path(), content)
            .await
            .map_err(|e| DomainError::internal_error("FileUpload", format!("write temp: {e}")))?;
        let hash = DedupService::hash_file(temp.path())
            .await
            .map_err(|e| DomainError::internal_error("FileUpload", format!("hash: {e}")))?;

        self.update_file_streaming(
            path,
            temp.path(),
            content.len() as u64,
            content_type,
            Some(hash),
            modified_at,
        )
        .await
    }

    /// Streaming update — replaces file content from a temp file on disk.
    ///
    /// Uses `update_file_content_from_temp` which passes the pre-computed hash
    /// to dedup, avoiding a second full read of the file.
    /// For new files (not found at `path`), falls back to `upload_file_streaming`.
    ///
    /// Peak RAM: ~256 KB regardless of file size.
    async fn update_file_streaming(
        &self,
        path: &str,
        temp_path: &Path,
        size: u64,
        content_type: &str,
        pre_computed_hash: Option<String>,
        modified_at: Option<i64>,
    ) -> Result<FileDto, DomainError> {
        // Try to find the existing file first
        if let Some(file_read) = &self.file_read
            && let Some(file) = file_read.find_file_by_path(path).await?
        {
            let file_id = file.id().to_string();
            let (new_hash, updated_at) = self
                .file_write
                .update_file_content_from_temp(
                    &file_id,
                    temp_path,
                    size,
                    Some(content_type.to_string()),
                    pre_computed_hash,
                    modified_at,
                )
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
                size,
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
                hook.on_file_updated(&file_id, &dto.etag, content_type);
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

        let (created, is_new_blob) = self
            .file_write
            .save_file_from_temp_with_dedup(
                filename.to_string(),
                parent_id,
                content_type.to_string(),
                temp_path,
                size,
                pre_computed_hash,
            )
            .await?;
        let dto = FileDto::from(created);
        if let Some(hook) = &self.file_lifecycle_hook {
            hook.on_file_created(&dto.id, &dto.etag, content_type, is_new_blob);
        }
        Ok(dto)
    }
}
