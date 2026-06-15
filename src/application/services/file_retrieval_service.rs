use bytes::{Bytes, BytesMut};
use futures::{Stream, StreamExt};
use std::pin::Pin;
use std::sync::Arc;

use crate::application::dtos::file_dto::FileDto;
use crate::application::ports::authorization_ports::AuthorizationEngine;
use crate::application::ports::file_ports::{FileRetrievalUseCase, OptimizedFileContent};
use crate::application::ports::storage_ports::FileReadPort;
use crate::common::errors::DomainError;
use crate::domain::services::authorization::{Permission, Resource, Subject};
use crate::infrastructure::repositories::pg::file_blob_read_repository::FileBlobReadRepository;
use crate::infrastructure::services::file_content_cache::FileContentCache;
use crate::infrastructure::services::image_transcode_service::{
    ImageTranscodeService, OutputFormat,
};
use crate::infrastructure::services::pg_acl_engine::PgAclEngine;
use tracing::{debug, info};
use uuid::Uuid;

/// Threshold below which files are served from RAM cache (10 MB).
const CACHE_THRESHOLD: u64 = 10 * 1024 * 1024;

/// Service for file retrieval operations
///
/// Implements a multi-tier download strategy:
/// - Tier 0: Write-behind cache (just-uploaded files still in RAM)
/// - Tier 1: Hot cache + optional WebP transcoding (<10 MB)
/// - Tier 2: Memory-mapped I/O (10–100 MB)
/// - Tier 3: Streaming (≥100 MB)
pub struct FileRetrievalService {
    file_read: Arc<FileBlobReadRepository>,
    content_cache: Option<Arc<FileContentCache>>,
    transcode: Option<Arc<ImageTranscodeService>>,
    authz: Option<Arc<PgAclEngine>>,
}

impl FileRetrievalService {
    /// Backward-compatible constructor (simple pass-through). Without the
    /// authorization engine, the `*_owned`/`*_with_perms` methods fail closed.
    /// Use `new_with_cache` in production.
    pub fn new(file_repository: Arc<FileBlobReadRepository>) -> Self {
        Self {
            file_read: file_repository,
            content_cache: None,
            transcode: None,
            authz: None,
        }
    }

    /// Constructor for blob-storage model: read + content cache + transcode +
    /// ReBAC authorization.
    pub fn new_with_cache(
        file_read: Arc<FileBlobReadRepository>,
        content_cache: Arc<FileContentCache>,
        transcode: Arc<ImageTranscodeService>,
        authz: Arc<PgAclEngine>,
    ) -> Self {
        Self {
            file_read,
            content_cache: Some(content_cache),
            transcode: Some(transcode),
            authz: Some(authz),
        }
    }

    // ── private helpers ──────────────────────────────────────────

    /// Helper: require the caller has `perm` on the given file id.
    /// Fail-closed if no engine was injected (stub/test path).
    async fn require_file(
        &self,
        file_id: &str,
        perm: Permission,
        caller_id: Uuid,
    ) -> Result<(), DomainError> {
        let authz = self.authz.as_ref().ok_or_else(|| {
            DomainError::internal_error("FileRetrieval", "Authorization engine unavailable")
        })?;
        let uuid = Uuid::parse_str(file_id).map_err(|_| DomainError::not_found("File", file_id))?;
        authz
            .require(Subject::User(caller_id), perm, Resource::File(uuid))
            .await
    }

    /// Engine check for a target folder. `None` is allowed (root namespace,
    /// implicitly owned by the caller).
    async fn require_target_folder_perm(
        &self,
        folder_id: Option<&str>,
        perm: Permission,
        caller_id: Uuid,
    ) -> Result<(), DomainError> {
        let Some(target) = folder_id else {
            return Ok(());
        };
        let authz = self.authz.as_ref().ok_or_else(|| {
            DomainError::internal_error("FileRetrieval", "Authorization engine unavailable")
        })?;
        let uuid = Uuid::parse_str(target).map_err(|_| DomainError::not_found("Folder", target))?;
        authz
            .require(Subject::User(caller_id), perm, Resource::Folder(uuid))
            .await
    }

    /// Try to transcode image content to WebP and return transcoded variant.
    async fn try_transcode(
        &self,
        id: &str,
        content: &Bytes,
        mime: &str,
        file_size: u64,
        accept_webp: bool,
    ) -> Option<(Bytes, Arc<str>)> {
        if !accept_webp {
            return None;
        }
        let transcode = self.transcode.as_ref()?;
        if !ImageTranscodeService::should_transcode(mime, file_size) {
            return None;
        }
        let format = OutputFormat::WebP;
        match transcode
            .get_transcoded(id, content.clone(), mime, format)
            .await
        {
            Ok((transcoded, webp_mime, true)) => {
                debug!(
                    "🖼️ WebP transcode: {} -> {} bytes ({:.0}% smaller)",
                    content.len(),
                    transcoded.len(),
                    (1.0 - transcoded.len() as f64 / content.len().max(1) as f64) * 100.0
                );
                Some((transcoded, Arc::from(&*webp_mime)))
            }
            _ => None,
        }
    }

    /// Core multi-tier download logic shared by `get_file_optimized` and
    /// `get_file_optimized_preloaded`.
    async fn optimized_inner(
        &self,
        id: &str,
        dto: FileDto,
        accept_webp: bool,
        prefer_original: bool,
    ) -> Result<(FileDto, OptimizedFileContent), DomainError> {
        let mime_type = dto.mime_type.clone();
        let file_size = dto.size;
        let file_name = dto.name.clone();
        // The content cache is content-addressed: keyed by the blob hash, not
        // the file id. Identical content deduplicated to one blob on disk is
        // then cached ONCE in RAM and shared by every file/user that references
        // it — the cache benefits from dedup, not just the disk. Immutable by
        // construction, so entries never go stale (no invalidation needed). A
        // stub DTO without a hash disables caching for that request rather than
        // colliding every hash-less file on the key "".
        let cache_key = dto.content_hash.clone();
        let cacheable = !cache_key.is_empty();
        let do_transcode = accept_webp && !prefer_original;

        // ── Tier 1: Hot cache + transcode (<10 MB) ──────────
        if file_size < CACHE_THRESHOLD {
            // Check content cache first (keyed by blob hash — see above)
            if cacheable
                && let Some(cache) = &self.content_cache
                && let Some((cached, _etag, _ct)) = cache.get(&cache_key).await
            {
                debug!(
                    "🔥 TIER 1 Cache HIT: {} ({} bytes)",
                    file_name,
                    cached.len()
                );
                if do_transcode
                    && let Some((t, m)) = self
                        .try_transcode(id, &cached, &mime_type, file_size, true)
                        .await
                {
                    return Ok((
                        dto,
                        OptimizedFileContent::Bytes {
                            data: t,
                            mime_type: m,
                            was_transcoded: true,
                        },
                    ));
                }
                return Ok((
                    dto,
                    OptimizedFileContent::Bytes {
                        data: cached,
                        mime_type: mime_type.clone(),
                        was_transcoded: false,
                    },
                ));
            }

            // Cache miss – load from disk via streaming (constant 64 KB memory)
            debug!("💾 TIER 1 Cache MISS: {} – loading from disk", file_name);
            let stream = self.file_read.get_file_stream(id).await?;
            let mut stream = std::pin::Pin::from(stream);
            let mut buf = BytesMut::with_capacity(file_size as usize);
            while let Some(chunk) = stream.next().await {
                buf.extend_from_slice(&chunk.map_err(|e| {
                    DomainError::internal_error("File", format!("Stream read error: {}", e))
                })?);
            }
            let content_bytes = buf.freeze();

            // Store in cache (keyed by blob hash; ETag = the immutable hash)
            if cacheable && let Some(cache) = &self.content_cache {
                let etag: Arc<str> = format!("\"{}\"", cache_key).into();
                let ct: Arc<str> = mime_type.clone();
                cache
                    .put(cache_key.clone(), content_bytes.clone(), etag, ct)
                    .await;
            }

            if do_transcode
                && let Some((t, m)) = self
                    .try_transcode(id, &content_bytes, &mime_type, file_size, true)
                    .await
            {
                return Ok((
                    dto,
                    OptimizedFileContent::Bytes {
                        data: t,
                        mime_type: m,
                        was_transcoded: true,
                    },
                ));
            }
            return Ok((
                dto,
                OptimizedFileContent::Bytes {
                    data: content_bytes,
                    mime_type: mime_type.clone(),
                    was_transcoded: false,
                },
            ));
        }

        // ── Tier 2 + 3: Streaming (≥10 MB) ──────────────────
        info!(
            "📡 TIER 2 STREAMING: {} ({} MB)",
            file_name,
            file_size / (1024 * 1024)
        );
        let stream = self.file_read.get_file_stream(id).await?;
        Ok((dto, OptimizedFileContent::Stream(Box::into_pin(stream))))
    }
}

impl FileRetrievalUseCase for FileRetrievalService {
    async fn get_file(&self, id: &str) -> Result<FileDto, DomainError> {
        let file = self.file_read.get_file(id).await?;
        Ok(FileDto::from(file))
    }

    async fn get_file_with_perms(&self, id: &str, caller_id: Uuid) -> Result<FileDto, DomainError> {
        self.require_file(id, Permission::Read, caller_id).await?;
        let file = self.file_read.get_file(id).await?;
        Ok(FileDto::from(file))
    }

    async fn get_file_or_trashed_with_perms(
        &self,
        id: &str,
        caller_id: Uuid,
    ) -> Result<FileDto, DomainError> {
        self.require_file(id, Permission::Read, caller_id).await?;
        let file = self.file_read.get_file_or_trashed(id).await?;
        Ok(FileDto::from(file))
    }

    // FIXME no authorisation at all
    async fn get_file_by_path(&self, path: &str) -> Result<FileDto, DomainError> {
        // Direct SQL lookup — O(folder_depth) queries instead of O(total_files)
        // NOTE: This method does NOT perform any authorization check. Callers
        // that surface its result to a user-driven request MUST resolve the
        // file via get_file_owned afterwards, or call authz.require directly.
        // (Tracked in the audit punch-list under "path-based lookups".)
        if let Some(file) = self.file_read.find_file_by_path(path).await? {
            return Ok(FileDto::from(file));
        }

        Err(DomainError::not_found(
            "File",
            format!("not found at path: {}", path),
        ))
    }

    async fn list_files(&self, folder_id: Option<&str>) -> Result<Vec<FileDto>, DomainError> {
        let files = self.file_read.list_files(folder_id).await?;
        Ok(files.into_iter().map(FileDto::from).collect())
    }

    async fn list_files_with_perms(
        &self,
        folder_id: Option<&str>,
        owner_id: Uuid,
    ) -> Result<Vec<FileDto>, DomainError> {
        if folder_id.is_some() {
            // folder id is defined, check permissions
            self.require_target_folder_perm(folder_id, Permission::Read, owner_id)
                .await?;
            self.list_files(folder_id).await
        } else {
            // no folder id, get owners's files' root
            let files = self
                .file_read
                .list_files_for_owner(folder_id, owner_id)
                .await?;
            Ok(files.into_iter().map(FileDto::from).collect())
        }
    }

    async fn get_file_stream(
        &self,
        id: &str,
    ) -> Result<Box<dyn Stream<Item = Result<Bytes, std::io::Error>> + Send>, DomainError> {
        self.file_read.get_file_stream(id).await
    }

    async fn get_file_stream_with_perms(
        &self,
        id: &str,
        caller_id: Uuid,
    ) -> Result<Box<dyn Stream<Item = Result<Bytes, std::io::Error>> + Send>, DomainError> {
        self.require_file(id, Permission::Read, caller_id).await?;
        self.file_read.get_file_stream(id).await
    }

    /// Multi-tier optimized download.
    async fn get_file_optimized(
        &self,
        id: &str,
        accept_webp: bool,
        prefer_original: bool,
    ) -> Result<(FileDto, OptimizedFileContent), DomainError> {
        let file = self.file_read.get_file(id).await?;
        let dto = FileDto::from(file);
        self.optimized_inner(id, dto, accept_webp, prefer_original)
            .await
    }

    async fn get_file_optimized_with_perms(
        &self,
        id: &str,
        caller_id: Uuid,
        accept_webp: bool,
        prefer_original: bool,
    ) -> Result<(FileDto, OptimizedFileContent), DomainError> {
        self.require_file(id, Permission::Read, caller_id).await?;
        let file = self.file_read.get_file(id).await?;
        let dto = FileDto::from(file);
        self.optimized_inner(id, dto, accept_webp, prefer_original)
            .await
    }

    /// Like `get_file_optimized` but skips the metadata re-fetch.
    async fn get_file_optimized_preloaded(
        &self,
        id: &str,
        file_dto: FileDto,
        accept_webp: bool,
        prefer_original: bool,
    ) -> Result<(FileDto, OptimizedFileContent), DomainError> {
        self.optimized_inner(id, file_dto, accept_webp, prefer_original)
            .await
    }

    /// Range-based streaming for HTTP Range Requests.
    async fn get_file_range_stream(
        &self,
        id: &str,
        start: u64,
        end: Option<u64>,
    ) -> Result<Box<dyn Stream<Item = Result<Bytes, std::io::Error>> + Send>, DomainError> {
        self.file_read.get_file_range_stream(id, start, end).await
    }

    async fn get_file_range_stream_with_perms(
        &self,
        id: &str,
        caller_id: Uuid,
        start: u64,
        end: Option<u64>,
    ) -> Result<Box<dyn Stream<Item = Result<Bytes, std::io::Error>> + Send>, DomainError> {
        self.require_file(id, Permission::Read, caller_id).await?;
        self.file_read.get_file_range_stream(id, start, end).await
    }

    // TODO: check: no permission check
    async fn stream_files_in_subtree(
        &self,
        folder_id: &str,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<FileDto, DomainError>> + Send>>, DomainError> {
        let inner = self.file_read.stream_files_in_subtree(folder_id).await?;
        let mapped = inner.map(|r| r.map(FileDto::from));
        Ok(Box::pin(mapped))
    }

    async fn list_files_batch(
        &self,
        folder_id: Option<&str>,
        offset: i64,
        limit: i64,
    ) -> Result<Vec<FileDto>, DomainError> {
        let files = self
            .file_read
            .list_files_batch(folder_id, offset, limit)
            .await?;
        Ok(files.into_iter().map(FileDto::from).collect())
    }

    async fn list_files_batch_with_perms(
        &self,
        folder_id: Option<&str>,
        owner_id: Uuid,
        offset: i64,
        limit: i64,
    ) -> Result<Vec<FileDto>, DomainError> {
        if folder_id.is_some() {
            // folder id is defined, check permissions
            self.require_target_folder_perm(folder_id, Permission::Read, owner_id)
                .await?;
            let files = self
                .file_read
                .list_files_batch(folder_id, offset, limit)
                .await?;
            return Ok(files.into_iter().map(FileDto::from).collect());
        }

        let files = self
            .file_read
            .list_files_batch_for_owner(folder_id, owner_id, offset, limit)
            .await?;
        Ok(files.into_iter().map(FileDto::from).collect())
    }
}
