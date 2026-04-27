use axum::{
    Json,
    body::Body,
    extract::{Multipart, Path, Query, State},
    http::{HeaderMap, Response, StatusCode, header},
    response::IntoResponse,
};
use bytes::Bytes;
use http_range_header::parse_range_header;
use serde::Deserialize;
use std::collections::HashMap;
use utoipa::ToSchema;

use crate::application::dtos::file_dto::FileDto;
use crate::application::ports::file_ports::OptimizedFileContent;
use crate::application::ports::file_ports::{
    FileManagementUseCase, FileRetrievalUseCase, FileUploadUseCase,
};
use crate::application::ports::storage_ports::{FileReadPort, StorageUsagePort};
use crate::application::ports::thumbnail_ports::ThumbnailPort;
use crate::common::di::AppState;
use crate::infrastructure::services::audio_metadata_service::AudioMetadataService;
use crate::interfaces::errors::AppError;
use crate::interfaces::middleware::auth::AuthUser;
use std::sync::Arc;

/**
 * Type aliases for dependency injection state.
 */
/// Global application state for dependency injection
type GlobalState = Arc<AppState>;

/**
 * API handler for file-related operations.
 *
 * Acts as a thin HTTP adapter in the hexagonal architecture: it parses requests,
 * delegates business logic to application services, and maps results to HTTP
 * responses.  No infrastructure or strategy logic lives here.
 */
pub struct FileHandler;

impl FileHandler {
    // ── Why no #[utoipa::path] here? ─────────────────────────────────────────────
    // utoipa 5.4.0's proc macro generates helper structs / impls inside its expansion.
    // Rust allows struct definitions at module scope but forbids them inside impl blocks,
    // so `#[utoipa::path]` fails on every method in this impl block regardless of HTTP
    // verb or annotation content. All route handlers are free functions below.
    // TODO: collapse after utoipa upgrade.

    // ═══════════════════════════════════════════════════════════════════════
    //  UPLOAD
    // ═══════════════════════════════════════════════════════════════════════

    /// Streaming file upload — constant ~64 KB RAM regardless of file size.
    ///
    /// **Hash-on-Write**: BLAKE3 is computed while spooling the multipart
    /// body to the temp file. This eliminates the second sequential read
    /// that dedup_service would otherwise need, cutting total I/O in half.
    pub async fn upload_file(
        State(state): State<GlobalState>,
        auth_user: AuthUser,
        multipart: Multipart,
    ) -> impl IntoResponse {
        match Self::upload_file_inner(&state, &auth_user, multipart).await {
            Ok((file, _blob_hash)) => Self::created_json_response(&file).into_response(),
            Err(response) => response.into_response(),
        }
    }

    /// Core upload logic shared by [`Self::upload_file`] and
    /// [`Self::upload_file_with_thumbnails`].
    ///
    /// Returns `(FileDto, blob_hash)` on success.  The blob hash is the
    /// BLAKE3 digest computed during the hash-on-write spool and is
    /// propagated without an extra database round-trip so that callers
    /// (e.g. thumbnail generation) can resolve the physical blob path
    /// immediately.
    async fn upload_file_inner(
        state: &GlobalState,
        auth_user: &AuthUser,
        mut multipart: Multipart,
    ) -> Result<(crate::application::dtos::file_dto::FileDto, String), Response<Body>> {
        let upload_service = &state.applications.file_upload_service;
        let mut folder_id: Option<String> = None;

        tracing::debug!("📤 Processing streaming file upload (hash-on-write)");

        while let Some(field) = multipart.next_field().await.unwrap_or(None) {
            let name = field.name().unwrap_or("").to_string();

            if name == "folder_id" {
                let v = field.text().await.unwrap_or_default();
                if !v.is_empty() {
                    folder_id = Some(v);
                }
                continue;
            }

            if name == "file" {
                let raw_filename = field.file_name().unwrap_or("unnamed").to_string();
                // Browsers send the full relative path (e.g. "Screenshots/file.png")
                // as the filename for folder uploads via webkitRelativePath.
                // Strip path components to get the basename only.
                // This also prevents path-traversal attacks.
                let filename = raw_filename
                    .rsplit('/')
                    .next()
                    .unwrap_or(&raw_filename)
                    .rsplit('\\')
                    .next()
                    .unwrap_or(&raw_filename)
                    .to_string();
                let content_type = field
                    .content_type()
                    .unwrap_or("application/octet-stream")
                    .to_string();

                // ── SECURITY: Verify folder ownership before upload (IDOR V-03 fix) ──
                if let Some(ref fid) = folder_id {
                    use crate::application::ports::inbound::FolderUseCase;
                    let folder_service = &state.applications.folder_service;
                    if folder_service
                        .get_folder_owned(fid, auth_user.id)
                        .await
                        .is_err()
                    {
                        tracing::warn!(
                            "⛔ UPLOAD REJECTED (IDOR): user='{}' attempted upload to folder '{}' owned by another user",
                            auth_user.username,
                            fid,
                        );
                        return Err(Self::domain_error_response(
                            crate::common::errors::DomainError::not_found("Folder", fid),
                        ));
                    }
                }

                // ── Early quota check (before spooling to disk) ──────
                if let Some(storage_svc) = state.storage_usage_service.as_ref() {
                    let estimated_size = field
                        .headers()
                        .get(header::CONTENT_LENGTH)
                        .and_then(|v| v.to_str().ok())
                        .and_then(|s| s.parse::<u64>().ok())
                        .unwrap_or(0);
                    if let Err(err) = storage_svc
                        .check_storage_quota(auth_user.id, estimated_size)
                        .await
                    {
                        tracing::warn!(
                            "⛔ UPLOAD REJECTED (early quota): user={}, file={}, est_size={}",
                            auth_user.username,
                            filename,
                            estimated_size
                        );
                        return Err(Self::quota_error_response(err));
                    }
                }

                // ── Spool multipart field to temp file + hash-on-write ──
                // .dedup_temp is created once by DedupService::initialize() at startup
                let temp_dir = state.core.path_service.get_root_path().join(".dedup_temp");
                let temp_path = temp_dir.join(format!("upload-{}", uuid::Uuid::new_v4()));

                let mut total_size: u64 = 0;
                let mut hasher = blake3::Hasher::new();
                let spool_result: Result<(), String> = async {
                    let file = tokio::fs::File::create(&temp_path)
                        .await
                        .map_err(|e| format!("Failed to create temp file: {}", e))?;

                    // Pre-allocate if Content-Length is known (reduces fragmentation)
                    let hint = field
                        .headers()
                        .get(axum::http::header::CONTENT_LENGTH)
                        .and_then(|v| v.to_str().ok())
                        .and_then(|s| s.parse::<u64>().ok());
                    if let Some(len) = hint {
                        let _ = file.set_len(len).await; // best-effort
                    }

                    // 512 KB buffer — 8× fewer write syscalls than 64 KB
                    let mut writer = tokio::io::BufWriter::with_capacity(524_288, file);
                    let mut field = field;
                    // IMPORTANT: use explicit match instead of `while let Ok(Some(..))`.
                    // The old pattern silently swallowed Err (client disconnect)
                    // and accepted partially received data as a complete upload.
                    loop {
                        match field.chunk().await {
                            Ok(Some(chunk)) => {
                                total_size += chunk.len() as u64;
                                hasher.update(&chunk);
                                tokio::io::AsyncWriteExt::write_all(&mut writer, &chunk)
                                    .await
                                    .map_err(|e| format!("Failed to write chunk: {}", e))?;
                            }
                            Ok(None) => break, // End of field — upload complete
                            Err(e) => {
                                return Err(format!(
                                    "Connection lost during upload (received {} bytes): {}",
                                    total_size, e
                                ));
                            }
                        }
                    }
                    tokio::io::AsyncWriteExt::flush(&mut writer)
                        .await
                        .map_err(|e| format!("Failed to flush temp file: {}", e))?;
                    Ok(())
                }
                .await;

                if let Err(e) = spool_result {
                    let _ = tokio::fs::remove_file(&temp_path).await;
                    tracing::error!("❌ UPLOAD SPOOL FAILED: {} - {}", filename, e);
                    return Err(Self::domain_error_response(
                        crate::common::errors::DomainError::internal_error("FileUpload", e),
                    ));
                }

                // Empty file — use streaming path with the (empty) temp file
                if total_size == 0 {
                    let hash = hasher.finalize().to_hex().to_string();
                    let dto = upload_service
                        .upload_file_streaming(
                            filename,
                            folder_id,
                            content_type,
                            &temp_path,
                            0,
                            Some(hash.clone()),
                        )
                        .await
                        .map_err(Self::domain_error_response)?;
                    return Ok((dto, hash));
                }

                // Finalize hash
                let hash = hasher.finalize().to_hex().to_string();

                // ── MIME detection (magic bytes + extension fallback) ─
                let content_type = crate::common::mime_detect::refine_content_type_from_file(
                    &temp_path,
                    &filename,
                    &content_type,
                )
                .await;

                // ── Quota enforcement ────────────────────────────────
                if let Some(storage_svc) = state.storage_usage_service.as_ref()
                    && let Err(err) = storage_svc
                        .check_storage_quota(auth_user.id, total_size)
                        .await
                {
                    let _ = tokio::fs::remove_file(&temp_path).await;
                    tracing::warn!(
                        "⛔ UPLOAD REJECTED (quota): user={}, file={}, size={}",
                        auth_user.username,
                        filename,
                        total_size
                    );
                    return Err(Self::quota_error_response(err));
                }

                // ── Streaming upload (temp file → blob store, hash pre-computed) ─
                match upload_service
                    .upload_file_streaming(
                        filename.clone(),
                        folder_id,
                        content_type,
                        &temp_path,
                        total_size,
                        Some(hash.clone()),
                    )
                    .await
                {
                    Ok(file) => {
                        tracing::info!(
                            "✅ STREAMING UPLOAD: {} ({} bytes, ID: {})",
                            filename,
                            total_size,
                            file.id
                        );
                        return Ok((file, hash));
                    }
                    Err(err) => {
                        let _ = tokio::fs::remove_file(&temp_path).await;
                        tracing::error!("❌ UPLOAD FAILED: {} - {}", filename, err);
                        return Err(Self::domain_error_response(err));
                    }
                }
            }
        }

        Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": "No file provided"
            })),
        )
            .into_response())
    }

    // ═══════════════════════════════════════════════════════════════════════
    //  THUMBNAILS
    // ═══════════════════════════════════════════════════════════════════════

    /// Get a thumbnail for a file (image or video).
    ///
    /// **Cache-first**: if the thumbnail already exists in the moka in-memory
    /// cache or on disk, serve it immediately — **zero DB queries**.  The
    /// ownership check was already performed when the thumbnail was first
    /// generated (at upload) or uploaded (PUT by the owner).  UUIDv4 file IDs
    /// have 122 bits of entropy, making enumeration infeasible.
    ///
    /// **ETag / 304**: responses carry an immutable ETag.  If the browser
    /// sends `If-None-Match` matching the ETag, we return 304 Not Modified
    /// without touching cache or DB — pure header round-trip.
    ///
    /// The DB path is only taken on a **cache miss for images** where the
    /// thumbnail hasn't been generated yet (first access after upload if
    /// background generation hasn't finished).
    pub(super) async fn get_thumbnail_impl(
        State(state): State<GlobalState>,
        auth_user: AuthUser,
        headers: HeaderMap,
        Path((id, size)): Path<(String, String)>,
    ) -> impl IntoResponse {
        use crate::application::ports::thumbnail_ports::ThumbnailSize;

        let thumbnail_service = &state.core.thumbnail_service;

        let thumb_size = match size.as_str() {
            "icon" => ThumbnailSize::Icon,
            "preview" => ThumbnailSize::Preview,
            "large" => ThumbnailSize::Large,
            _ => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({
                        "error": "Invalid thumbnail size. Use: icon, preview, or large"
                    })),
                )
                    .into_response();
            }
        };

        // ── ETag short-circuit (Solution C) ──────────────────────────
        // Thumbnails are immutable — the ETag never changes for a given
        // (file_id, size) pair.  If the browser already has it, return 304
        // with zero I/O or DB work.
        let etag = format!("\"thumb-{}-{:?}\"", id, thumb_size);
        if let Some(if_none_match) = headers.get(header::IF_NONE_MATCH)
            && let Ok(val) = if_none_match.to_str()
            && (val == etag || val == "*")
        {
            return Response::builder()
                .status(StatusCode::NOT_MODIFIED)
                .header(header::ETAG, &etag)
                .header(header::CACHE_CONTROL, "public, max-age=31536000, immutable")
                .body(Body::empty())
                .unwrap()
                .into_response();
        }

        // ── Cache-first path (Solution A) ────────────────────────────
        // Try moka (RAM) → disk before touching the database.
        // If the thumbnail exists it was authorized at creation time.
        if let Some(data) = thumbnail_service
            .get_cached_thumbnail(&id, None, thumb_size.into())
            .await
        {
            return Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, "image/jpeg")
                .header(header::CONTENT_LENGTH, data.len())
                .header(header::CACHE_CONTROL, "public, max-age=31536000, immutable")
                .header(header::ETAG, &etag)
                .body(Body::from(data))
                .unwrap()
                .into_response();
        }

        // ── Cache miss — need DB for ownership + blob resolution ─────
        let file_retrieval_service = &state.applications.file_retrieval_service;

        let file = match file_retrieval_service
            .get_file_owned(&id, auth_user.id)
            .await
        {
            Ok(f) => f,
            Err(err) => {
                return AppError::from(err).into_response();
            }
        };

        // Non-image (video, etc.) with no cached thumbnail → 204
        if !thumbnail_service.is_supported_image(&file.mime_type) {
            return Response::builder()
                .status(StatusCode::NO_CONTENT)
                .header(header::CACHE_CONTROL, "no-store")
                .body(Body::empty())
                .unwrap()
                .into_response();
        }

        // Resolve the blob hash (content-addressable storage).
        let blob_hash = match state
            .repositories
            .file_read_repository
            .get_blob_hash(&id)
            .await
        {
            Ok(hash) => hash,
            Err(_) => {
                return AppError::internal_error("File blob not found").into_response();
            }
        };
        if let Some(data) = thumbnail_service
            .get_cached_thumbnail(&id, Some(&blob_hash), thumb_size.into())
            .await
        {
            return Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, "image/jpeg")
                .header(header::CONTENT_LENGTH, data.len())
                .header(header::CACHE_CONTROL, "public, max-age=31536000, immutable")
                .header(header::ETAG, &etag)
                .body(Body::from(data))
                .unwrap()
                .into_response();
        }

        let original_bytes = match state.core.dedup_service.read_blob_bytes(&blob_hash).await {
            Ok(bytes) => bytes,
            Err(err) => {
                return AppError::internal_error(format!(
                    "Failed to load source image for thumbnail generation: {}",
                    err
                ))
                .into_response();
            }
        };

        match thumbnail_service
            .get_thumbnail_from_bytes(&id, &blob_hash, thumb_size.into(), original_bytes)
            .await
        {
            Ok(data) => Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, "image/jpeg")
                .header(header::CONTENT_LENGTH, data.len())
                .header(header::CACHE_CONTROL, "public, max-age=31536000, immutable")
                .header(header::ETAG, &etag)
                .body(Body::from(data))
                .unwrap()
                .into_response(),
            Err(err) => AppError::internal_error(format!("Thumbnail generation failed: {}", err))
                .into_response(),
        }
    }

    // ═══════════════════════════════════════════════════════════════════════
    //  UPLOAD THUMBNAIL (client-generated, e.g. video frames)
    // ═══════════════════════════════════════════════════════════════════════

    /// Accept a client-generated thumbnail (e.g. video frame extracted via
    /// `<video>` + `<canvas>` in the browser) and persist it in the server
    /// cache.  The image is validated, re-encoded to WebP, and stored so
    /// subsequent `GET …/thumbnail/{size}` requests are served instantly.
    ///
    /// **Max body: 512 KB** — thumbnails are small.
    pub(super) async fn upload_thumbnail_impl(
        State(state): State<GlobalState>,
        auth_user: AuthUser,
        Path((id, size)): Path<(String, String)>,
        body: Bytes,
    ) -> impl IntoResponse {
        use crate::application::ports::thumbnail_ports::ThumbnailSize;

        let thumbnail_service = &state.core.thumbnail_service;

        // Validate size
        let thumb_size = match size.as_str() {
            "icon" => ThumbnailSize::Icon,
            "preview" => ThumbnailSize::Preview,
            "large" => ThumbnailSize::Large,
            _ => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({
                        "error": "Invalid thumbnail size. Use: icon, preview, or large"
                    })),
                )
                    .into_response();
            }
        };

        // Reject oversized payloads (512 KB)
        if body.len() > 512 * 1024 {
            return (
                StatusCode::PAYLOAD_TOO_LARGE,
                Json(serde_json::json!({ "error": "Thumbnail exceeds 512 KB limit" })),
            )
                .into_response();
        }

        // Validate file ownership
        let file_retrieval_service = &state.applications.file_retrieval_service;
        if let Err(err) = file_retrieval_service
            .get_file_owned(&id, auth_user.id)
            .await
        {
            return AppError::from(err).into_response();
        }

        // Validate, re-encode to WebP, and store
        match thumbnail_service
            .store_external_thumbnail(&id, thumb_size.into(), body)
            .await
        {
            Ok(_) => StatusCode::CREATED.into_response(),
            Err(err) => AppError::internal_error(format!("Failed to store thumbnail: {}", err))
                .into_response(),
        }
    }

    // ═══════════════════════════════════════════════════════════════════════
    //  DOWNLOAD
    // ═══════════════════════════════════════════════════════════════════════

    /// Downloads a file with optimized multi-tier strategy.
    ///
    /// The tier selection (write-behind → hot cache → WebP transcode → mmap →
    /// streaming) is fully handled by `FileRetrievalUseCase::get_file_optimized`.
    /// This handler only deals with HTTP concerns: ETag, Range, Content-Disposition,
    /// and optional compression.
    pub(super) async fn download_file_impl(
        State(state): State<GlobalState>,
        auth_user: AuthUser,
        Path(id): Path<String>,
        Query(params): Query<HashMap<String, String>>,
        headers: HeaderMap,
    ) -> impl IntoResponse {
        let retrieval = &state.applications.file_retrieval_service;

        // ── Get file metadata (ownership-scoped) ────────────────────────
        let file_dto = match retrieval.get_file_owned(&id, auth_user.id).await {
            Ok(f) => f,
            Err(err) => {
                return AppError::from(err).into_response();
            }
        };

        // ── Metadata-only request ────────────────────────────────────
        if params
            .get("metadata")
            .is_some_and(|v| v == "true" || v == "1")
        {
            return (
                StatusCode::OK,
                Json(serde_json::json!({
                    "id": file_dto.id,
                    "name": file_dto.name,
                    "path": file_dto.path,
                    "size": file_dto.size,
                    "mime_type": file_dto.mime_type,
                    "folder_id": file_dto.folder_id,
                    "created_at": file_dto.created_at,
                    "modified_at": file_dto.modified_at
                })),
            )
                .into_response();
        }

        let etag = format!("\"{}-{}\"", id, file_dto.modified_at);

        // ── ETag (304 Not Modified) ──────────────────────────────────
        if let Some(inm) = headers.get(header::IF_NONE_MATCH)
            && let Ok(client_etag) = inm.to_str()
            && (client_etag == etag || client_etag == "*")
        {
            return Response::builder()
                .status(StatusCode::NOT_MODIFIED)
                .header(header::ETAG, &etag)
                .body(Body::empty())
                .unwrap()
                .into_response();
        }

        // ── Range Requests ───────────────────────────────────────────
        if let Some(range_header) = headers.get(header::RANGE)
            && let Ok(range_str) = range_header.to_str()
            && let Ok(ranges) = parse_range_header(range_str)
        {
            let validated = ranges.validate(file_dto.size);
            if let Ok(valid_ranges) = validated {
                if let Some(range) = valid_ranges.first() {
                    let start = *range.start();
                    let end = *range.end();
                    let range_length = end - start + 1;
                    let disposition =
                        Self::content_disposition(&file_dto.name, &file_dto.mime_type, &params);

                    match retrieval
                        .get_file_range_stream_owned(&id, auth_user.id, start, Some(end + 1))
                        .await
                    {
                        Ok(stream) => {
                            return Response::builder()
                                .status(StatusCode::PARTIAL_CONTENT)
                                .header(header::CONTENT_TYPE, &*file_dto.mime_type)
                                .header(header::CONTENT_DISPOSITION, &disposition)
                                .header(header::CONTENT_LENGTH, range_length)
                                .header(
                                    header::CONTENT_RANGE,
                                    format!("bytes {}-{}/{}", start, end, file_dto.size),
                                )
                                .header(header::ACCEPT_RANGES, "bytes")
                                .header(header::ETAG, &etag)
                                .header(
                                    header::CACHE_CONTROL,
                                    "private, max-age=3600, must-revalidate",
                                )
                                .body(Body::from_stream(Box::into_pin(stream)))
                                .unwrap()
                                .into_response();
                        }
                        Err(err) => {
                            tracing::error!("Error creating range stream: {}", err);
                            // fall through to normal download
                        }
                    }
                }
            } else {
                return Response::builder()
                    .status(StatusCode::RANGE_NOT_SATISFIABLE)
                    .header(header::CONTENT_RANGE, format!("bytes */{}", file_dto.size))
                    .body(Body::empty())
                    .unwrap()
                    .into_response();
            }
        }

        // ── Normal download (delegated to service) ───────────────────
        let disposition = Self::content_disposition(&file_dto.name, &file_dto.mime_type, &params);

        let accept_webp = headers
            .get(header::ACCEPT)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|a| a.contains("image/webp"));
        let prefer_original = params
            .get("original")
            .is_some_and(|v| v == "true" || v == "1");

        // Use the ownership-scoped optimized download.
        // Ownership was already verified by get_file_owned above,
        // so we can safely use the preloaded variant.
        match retrieval
            .get_file_optimized_preloaded(&id, file_dto.clone(), accept_webp, prefer_original)
            .await
        {
            Ok((_file, content)) => match content {
                OptimizedFileContent::Bytes {
                    data, mime_type, ..
                } => Self::build_cached_response(data, &mime_type, &disposition, &etag)
                    .into_response(),
                OptimizedFileContent::Mmap(mmap_data) => Response::builder()
                    .status(StatusCode::OK)
                    .header(header::CONTENT_TYPE, &*file_dto.mime_type)
                    .header(header::CONTENT_DISPOSITION, &disposition)
                    .header(header::CONTENT_LENGTH, mmap_data.len())
                    .header(header::ETAG, &etag)
                    .header(
                        header::CACHE_CONTROL,
                        "private, max-age=3600, must-revalidate",
                    )
                    .header(header::ACCEPT_RANGES, "bytes")
                    .body(Body::from(mmap_data))
                    .unwrap()
                    .into_response(),
                OptimizedFileContent::Stream(pinned_stream) => Response::builder()
                    .status(StatusCode::OK)
                    .header(header::CONTENT_TYPE, &*file_dto.mime_type)
                    .header(header::CONTENT_DISPOSITION, &disposition)
                    .header(header::CONTENT_LENGTH, file_dto.size)
                    .header(header::ETAG, &etag)
                    .header(
                        header::CACHE_CONTROL,
                        "private, max-age=3600, must-revalidate",
                    )
                    .header(header::ACCEPT_RANGES, "bytes")
                    .body(Body::from_stream(pinned_stream))
                    .unwrap()
                    .into_response(),
            },
            Err(err) => AppError::from(err).into_response(),
        }
    }

    // ═══════════════════════════════════════════════════════════════════════
    //  LIST
    // ═══════════════════════════════════════════════════════════════════════

    /// Lists files in a folder.
    pub(super) async fn list_files_query_impl(
        State(state): State<GlobalState>,
        auth_user: AuthUser,
        headers: HeaderMap,
        Query(params): Query<HashMap<String, String>>,
    ) -> impl IntoResponse {
        let folder_id = params.get("folder_id").map(|id| id.as_str());
        tracing::info!("API: Listing files with folder_id: {:?}", folder_id);

        let retrieval = &state.applications.file_retrieval_service;
        match retrieval.list_files_owned(folder_id, auth_user.id).await {
            Ok(files) => {
                // Compute lightweight ETag from max modified_at + count
                let max_mod = files.iter().map(|f| f.modified_at).max().unwrap_or(0);
                let count = files.len();
                let mut hasher = std::collections::hash_map::DefaultHasher::new();
                std::hash::Hash::hash(&max_mod, &mut hasher);
                std::hash::Hash::hash(&count, &mut hasher);
                let etag = format!("\"{:x}\"", std::hash::Hasher::finish(&hasher));

                // 304 Not Modified if client already has this version
                if let Some(inm) = headers.get(header::IF_NONE_MATCH)
                    && let Ok(client_etag) = inm.to_str()
                    && client_etag == etag
                {
                    return Response::builder()
                        .status(StatusCode::NOT_MODIFIED)
                        .header(header::ETAG, &etag)
                        .body(Body::empty())
                        .unwrap()
                        .into_response();
                }

                tracing::info!("Found {} files", files.len());
                let mut resp = (StatusCode::OK, Json(files)).into_response();
                resp.headers_mut()
                    .insert(header::ETAG, header::HeaderValue::from_str(&etag).unwrap());
                resp
            }
            Err(err) => AppError::from(err).into_response(),
        }
    }

    /// Uploads a file and generates thumbnails in the background for images.
    ///
    /// Delegates to [`Self::upload_file_inner`] and, on success, spawns
    /// a background task to generate all thumbnail sizes before serialising
    /// the `FileDto` once.
    pub(super) async fn upload_file_with_thumbnails_impl(
        State(state): State<GlobalState>,
        auth_user: AuthUser,
        multipart: Multipart,
    ) -> impl IntoResponse {
        let (file, blob_hash) = match Self::upload_file_inner(&state, &auth_user, multipart).await {
            Ok(pair) => pair,
            Err(response) => return response.into_response(),
        };

        // Generate thumbnails for supported images in background.
        // The blob_hash was already computed during the hash-on-write spool,
        // so we can reconstruct the source bytes directly from DedupService
        // without an extra DB round-trip.
        if state
            .core
            .thumbnail_service
            .is_supported_image(&file.mime_type)
        {
            let file_id = file.id.clone();
            let thumbnail_service = state.core.thumbnail_service.clone();
            let dedup_service = state.core.dedup_service.clone();
            let blob_hash_owned = blob_hash.clone();

            tokio::spawn(async move {
                tracing::info!("🖼️ Generating thumbnails for: {}", file_id);
                match dedup_service.read_blob_bytes(&blob_hash_owned).await {
                    Ok(original_bytes) => {
                        thumbnail_service.generate_all_sizes_background_from_bytes(
                            file_id,
                            blob_hash_owned,
                            original_bytes,
                        );
                    }
                    Err(err) => {
                        tracing::warn!(
                            "Failed to load source image for thumbnail generation {}: {}",
                            file_id,
                            err
                        );
                    }
                }
            });
        }

        // Extract audio metadata for supported audio files in background.
        if let Some(ref audio_service) = state.applications.audio_metadata_service
            && AudioMetadataService::is_audio_file(&file.mime_type)
            && let Ok(file_id) = uuid::Uuid::parse_str(&file.id)
        {
            let file_path = state.core.dedup_service.blob_path(&blob_hash);
            AudioMetadataService::spawn_extraction_background(
                audio_service.clone(),
                file_id,
                file_path,
            );
        }

        Self::created_json_response(&file).into_response()
    }

    // ═══════════════════════════════════════════════════════════════════════
    //  METADATA
    // ═══════════════════════════════════════════════════════════════════════

    /// Returns EXIF/media metadata for a file.
    pub(super) async fn get_file_metadata_impl(
        State(state): State<GlobalState>,
        auth_user: AuthUser,
        Path(file_id): Path<String>,
    ) -> impl IntoResponse {
        // Verify ownership
        let file_read = &state.repositories.file_read_repository;
        if let Err(e) = file_read.verify_file_owner(&file_id, auth_user.id).await {
            let msg = e.to_string();
            return (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "error": msg })),
            )
                .into_response();
        }

        let metadata_repo = &state.repositories.file_metadata_repository;
        match metadata_repo.get(&file_id).await {
            Ok(Some(meta)) => (StatusCode::OK, Json(meta)).into_response(),
            Ok(None) => (
                StatusCode::OK,
                Json(serde_json::json!({
                    "file_id": file_id,
                    "message": "No EXIF metadata available"
                })),
            )
                .into_response(),
            Err(e) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": e.to_string() })),
            )
                .into_response(),
        }
    }

    // ═══════════════════════════════════════════════════════════════════════
    //  DELETE
    // ═══════════════════════════════════════════════════════════════════════

    /// Deletes a file (trash-first with dedup cleanup).
    ///
    /// All logic (trash fallback, dedup ref-count, hash computation) is handled
    /// by `FileManagementUseCase::delete_with_cleanup`.
    ///
    /// When auth is available, uses trash-first deletion; otherwise falls back
    /// to permanent delete so the endpoint works with or without auth.
    pub(super) async fn delete_file_impl(
        State(state): State<GlobalState>,
        auth_user: AuthUser,
        Path(id): Path<String>,
    ) -> impl IntoResponse {
        let mgmt = &state.applications.file_management_service;

        // Auth required: trash-first with dedup cleanup + ownership verification
        let result = mgmt
            .delete_with_cleanup(&id, auth_user.id)
            .await
            .map(|was_trashed| {
                if was_trashed {
                    tracing::info!("File moved to trash: {}", id);
                } else {
                    tracing::info!("File permanently deleted: {}", id);
                }
            });

        match result {
            Ok(_) => StatusCode::NO_CONTENT.into_response(),
            Err(err) => AppError::from(err).into_response(),
        }
    }

    // ═══════════════════════════════════════════════════════════════════════
    //  MOVE
    // ═══════════════════════════════════════════════════════════════════════

    /// Renames a file (ownership-verified)
    pub(super) async fn rename_file_impl(
        State(state): State<GlobalState>,
        auth_user: AuthUser,
        Path(id): Path<String>,
        Json(payload): Json<serde_json::Value>,
    ) -> impl IntoResponse {
        let new_name = match payload.get("name").and_then(|v| v.as_str()) {
            Some(name) if !name.trim().is_empty() => name.trim().to_string(),
            _ => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({
                        "error": "Missing or empty 'name' field"
                    })),
                )
                    .into_response();
            }
        };

        tracing::info!("Renaming file {} to \"{}\"", id, new_name);
        let mgmt = &state.applications.file_management_service;
        match mgmt.rename_file_owned(&id, auth_user.id, &new_name).await {
            Ok(file_dto) => (StatusCode::OK, Json(file_dto)).into_response(),
            Err(err) => AppError::from(err).into_response(),
        }
    }

    /// Moves a file to a different folder (ownership-verified)
    pub async fn move_file(
        State(state): State<GlobalState>,
        auth_user: AuthUser,
        Path(id): Path<String>,
        Json(payload): Json<MoveFilePayload>,
    ) -> impl IntoResponse {
        tracing::info!("Moving file {} to folder {:?}", id, payload.folder_id);

        let mgmt = &state.applications.file_management_service;

        match mgmt
            .move_file_owned(&id, auth_user.id, payload.folder_id)
            .await
        {
            Ok(file) => (StatusCode::OK, Json(file)).into_response(),
            Err(err) => AppError::from(err).into_response(),
        }
    }

    /// Moves a file to a different folder (ownership-verified)
    pub(super) async fn move_file_simple_impl(
        State(state): State<GlobalState>,
        auth_user: AuthUser,
        Path(id): Path<String>,
        Json(payload): Json<serde_json::Value>,
    ) -> impl IntoResponse {
        let folder_id = payload
            .get("folder_id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        let mgmt = &state.applications.file_management_service;
        match mgmt.move_file_owned(&id, auth_user.id, folder_id).await {
            Ok(file_dto) => (StatusCode::OK, Json(file_dto)).into_response(),
            Err(err) => AppError::from(err).into_response(),
        }
    }

    // ═══════════════════════════════════════════════════════════════════════
    //  PRIVATE HELPERS
    // ═══════════════════════════════════════════════════════════════════════

    /// Build a Content-Disposition header value.
    ///
    /// Uses RFC 5987 `filename*=UTF-8''<percent-encoded>` to safely handle
    /// filenames with quotes, non-ASCII characters, or other special chars.
    /// A sanitised ASCII `filename=` fallback is included for legacy clients.
    fn content_disposition(name: &str, mime: &str, params: &HashMap<String, String>) -> String {
        let force_inline = params
            .get("inline")
            .is_some_and(|v| v == "true" || v == "1");
        let disposition = if force_inline
            || mime.starts_with("image/")
            || mime == "application/pdf"
            || mime.starts_with("video/")
            || mime.starts_with("audio/")
        {
            "inline"
        } else {
            "attachment"
        };

        // RFC 5987 percent-encode for filename* (attr-char safe set)
        use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
        // Characters that DON'T need encoding per RFC 5987 attr-char:
        //   ALPHA / DIGIT / "!" / "#" / "$" / "&" / "+" / "-" / "." /
        //   "^" / "_" / "`" / "|" / "~"
        const RFC5987_SET: &AsciiSet = &NON_ALPHANUMERIC
            .remove(b'!')
            .remove(b'#')
            .remove(b'$')
            .remove(b'&')
            .remove(b'+')
            .remove(b'-')
            .remove(b'.')
            .remove(b'^')
            .remove(b'_')
            .remove(b'`')
            .remove(b'|')
            .remove(b'~');
        let encoded = utf8_percent_encode(name, RFC5987_SET).to_string();

        // ASCII fallback: strip anything outside printable ASCII and
        // replace '"' and '\\' to prevent header injection.
        let ascii_safe: String = name
            .chars()
            .filter(|c| c.is_ascii_graphic() || *c == ' ')
            .map(|c| match c {
                '"' | '\\' => '_',
                _ => c,
            })
            .collect();

        format!("{disposition}; filename=\"{ascii_safe}\"; filename*=UTF-8''{encoded}")
    }

    /// Build a 201 Created JSON response.
    fn created_json_response(file: &crate::application::dtos::file_dto::FileDto) -> Response<Body> {
        Response::builder()
            .status(StatusCode::CREATED)
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::CACHE_CONTROL, "no-cache, no-store, must-revalidate")
            .body(Body::from(serde_json::to_string(file).unwrap()))
            .unwrap()
    }

    /// Build error response for DomainError.
    fn domain_error_response(err: crate::common::errors::DomainError) -> Response<Body> {
        AppError::from(err).into_response()
    }

    /// Build a quota-specific error response with 507 status and structured body.
    fn quota_error_response(err: crate::common::errors::DomainError) -> Response<Body> {
        AppError::from(err).into_response()
    }

    /// Build response for cached/small files.
    ///
    /// Compression is handled uniformly by `CompressionLayer` (tower-http)
    /// which negotiates `Accept-Encoding` and applies gzip/brotli in streaming
    /// mode. No manual compression is done here to avoid double-encoding.
    fn build_cached_response(
        content: Bytes,
        mime_type: &str,
        disposition: &str,
        etag: &str,
    ) -> Response<Body> {
        Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, mime_type)
            .header(header::CONTENT_DISPOSITION, disposition)
            .header(header::ETAG, etag)
            .header(
                header::CACHE_CONTROL,
                "private, max-age=3600, must-revalidate",
            )
            .header(header::VARY, "Accept-Encoding")
            .header(header::CONTENT_LENGTH, content.len())
            .body(Body::from(content))
            .unwrap()
    }
}

/// Payload for moving a file
#[derive(Debug, Deserialize, ToSchema)]
pub struct MoveFilePayload {
    /// Target folder ID (None means root)
    pub folder_id: Option<String>,
}

// ── Route handlers (free functions) ──────────────────────────────────────────
//
// All annotated route functions live here rather than as methods on FileHandler
// because utoipa 5.4.0's #[utoipa::path] macro generates helper structs inside
// its expansion. Rust allows struct definitions at module scope but forbids them
// inside impl blocks — so every #[utoipa::path] annotation on a FileHandler
// method fails to compile regardless of HTTP verb or annotation content.
//
// All logic lives in the FileHandler::*_impl methods above; these thin wrappers
// exist solely to carry the OpenAPI annotation at a scope where utoipa can
// generate its helper types.
//
// routes.rs calls these free functions directly.
// TODO: collapse back into the impl block after a utoipa upgrade resolves the issue.

#[utoipa::path(
    get,
    path = "/api/files",
    params(("folder_id" = Option<String>, Query, description = "Filter by folder ID")),
    responses(
        (status = 200, description = "List of files", body = Vec<FileDto>),
        (status = 304, description = "Not modified"),
    ),
    tag = "files"
)]
pub async fn list_files_query(
    state: State<GlobalState>,
    auth_user: AuthUser,
    headers: HeaderMap,
    query: Query<HashMap<String, String>>,
) -> impl IntoResponse {
    FileHandler::list_files_query_impl(state, auth_user, headers, query).await
}

#[utoipa::path(
    post,
    path = "/api/files/upload",
    request_body(content_type = "multipart/form-data", description = "File data + optional folder_id field"),
    responses(
        (status = 201, description = "File uploaded", body = FileDto),
        (status = 400, description = "Invalid request"),
        (status = 507, description = "Storage quota exceeded"),
    ),
    tag = "files"
)]
pub async fn upload_file_with_thumbnails(
    state: State<GlobalState>,
    auth_user: AuthUser,
    multipart: Multipart,
) -> impl IntoResponse {
    FileHandler::upload_file_with_thumbnails_impl(state, auth_user, multipart).await
}

#[utoipa::path(
    get,
    path = "/api/files/{id}",
    params(
        ("id" = String, Path, description = "File ID"),
        ("metadata" = Option<bool>, Query, description = "Return metadata JSON instead of file content"),
        ("original" = Option<bool>, Query, description = "Skip WebP transcoding"),
        ("inline" = Option<bool>, Query, description = "Content-Disposition: inline"),
    ),
    responses(
        (status = 200, description = "File content"),
        (status = 206, description = "Partial content (Range request)"),
        (status = 304, description = "Not modified"),
        (status = 404, description = "File not found"),
    ),
    tag = "files"
)]
pub async fn download_file(
    state: State<GlobalState>,
    auth_user: AuthUser,
    path: Path<String>,
    query: Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> impl IntoResponse {
    FileHandler::download_file_impl(state, auth_user, path, query, headers).await
}

#[utoipa::path(
    get,
    path = "/api/files/{id}/thumbnail/{size}",
    params(
        ("id" = String, Path, description = "File ID"),
        ("size" = String, Path, description = "Thumbnail size: icon | preview | large"),
    ),
    responses(
        (status = 200, description = "Thumbnail image (image/jpeg or image/webp)"),
        (status = 204, description = "No thumbnail available for this file type"),
        (status = 304, description = "Not modified"),
        (status = 404, description = "File not found"),
    ),
    tag = "files"
)]
pub async fn get_thumbnail(
    state: State<GlobalState>,
    auth_user: AuthUser,
    headers: HeaderMap,
    path: Path<(String, String)>,
) -> impl IntoResponse {
    FileHandler::get_thumbnail_impl(state, auth_user, headers, path).await
}

#[utoipa::path(
    put,
    path = "/api/files/{id}/thumbnail/{size}",
    params(
        ("id" = String, Path, description = "File ID"),
        ("size" = String, Path, description = "Thumbnail size: icon | preview | large"),
    ),
    request_body(content_type = "application/octet-stream", description = "Raw image bytes (max 512 KB)"),
    responses(
        (status = 201, description = "Thumbnail stored"),
        (status = 400, description = "Invalid image or size too large"),
        (status = 404, description = "File not found"),
    ),
    tag = "files"
)]
pub async fn upload_thumbnail(
    state: State<GlobalState>,
    auth_user: AuthUser,
    path: Path<(String, String)>,
    body: Bytes,
) -> impl IntoResponse {
    FileHandler::upload_thumbnail_impl(state, auth_user, path, body).await
}

#[utoipa::path(
    get,
    path = "/api/files/{id}/metadata",
    params(("id" = String, Path, description = "File ID")),
    responses(
        (status = 200, description = "File metadata (EXIF, dimensions, duration, etc.)"),
        (status = 404, description = "File not found"),
    ),
    tag = "files"
)]
pub async fn get_file_metadata(
    state: State<GlobalState>,
    auth_user: AuthUser,
    path: Path<String>,
) -> impl IntoResponse {
    FileHandler::get_file_metadata_impl(state, auth_user, path).await
}

#[utoipa::path(
    delete,
    path = "/api/files/{id}",
    params(("id" = String, Path, description = "File ID")),
    responses(
        (status = 204, description = "File deleted (moved to trash if enabled)"),
        (status = 404, description = "File not found"),
    ),
    tag = "files"
)]
pub async fn delete_file(
    state: State<GlobalState>,
    auth_user: AuthUser,
    path: Path<String>,
) -> impl IntoResponse {
    FileHandler::delete_file_impl(state, auth_user, path).await
}

#[utoipa::path(
    put,
    path = "/api/files/{id}/rename",
    params(("id" = String, Path, description = "File ID")),
    request_body(content_type = "application/json", description = r#"{"name": "new-name.txt"}"#),
    responses(
        (status = 200, description = "Renamed file", body = FileDto),
        (status = 404, description = "File not found"),
    ),
    tag = "files"
)]
pub async fn rename_file(
    state: State<GlobalState>,
    auth_user: AuthUser,
    path: Path<String>,
    json: Json<serde_json::Value>,
) -> impl IntoResponse {
    FileHandler::rename_file_impl(state, auth_user, path, json).await
}

#[utoipa::path(
    put,
    path = "/api/files/{id}/move",
    params(("id" = String, Path, description = "File ID")),
    request_body(content = MoveFilePayload, content_type = "application/json", description = "MoveFilePayload"),
    responses(
        (status = 200, description = "Moved file", body = FileDto),
        (status = 404, description = "File or destination not found"),
    ),
    tag = "files"
)]
pub async fn move_file_simple(
    state: State<GlobalState>,
    auth_user: AuthUser,
    path: Path<String>,
    json: Json<serde_json::Value>,
) -> impl IntoResponse {
    FileHandler::move_file_simple_impl(state, auth_user, path, json).await
}
