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

use crate::application::ports::external_mount_ports::MountStat;
use crate::application::ports::file_ports::{
    FileManagementUseCase, FileRetrievalUseCase, FileUploadUseCase, RangeContent,
};
use crate::application::ports::storage_ports::{FileReadPort, StorageUsagePort};
use crate::application::ports::thumbnail_ports::ThumbnailPort;
use crate::application::ports::{file_ports::OptimizedFileContent, folder_ports::FolderUseCase};
use crate::application::services::external_mount_router::ResolvedId;
use crate::application::services::mount_registry::MountConfig;
use crate::common::di::AppState;
use crate::domain::errors::DomainError;
use crate::domain::services::external_mount_id::{NodeId, virtual_file_etag};
use crate::interfaces::errors::AppError;
use crate::interfaces::middleware::auth::AuthUser;
use crate::interfaces::range_requests::not_modified_response;
use crate::interfaces::upload_ingest;
use crate::{application::dtos::file_dto::FileDto, domain::services::authorization::Permission};
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

    /// Streaming file upload — bounded RAM regardless of file size.
    ///
    /// The multipart body is streamed straight into the CDC chunk store:
    /// chunking, hashing and dedup checks happen while the bytes arrive.
    /// No spool file, no re-read — chunks the store already has are never
    /// written to disk at all.
    pub async fn upload_file(
        State(state): State<GlobalState>,
        auth_user: AuthUser,
        multipart: Multipart,
    ) -> impl IntoResponse {
        match Self::upload_file_inner(&state, &auth_user, multipart).await {
            Ok((mut file, _blob_hash)) => {
                crate::interfaces::api::handlers::caller_flags::enrich_file_flags(
                    &state,
                    &mut file,
                    auth_user.id,
                )
                .await;
                Self::created_json_response(&file).into_response()
            }
            Err(response) => response.into_response(),
        }
    }

    /// Instant upload: create a file from a blob the caller already owns.
    ///
    /// Zero content bytes travel — the client proved possession of the
    /// content by hash (it computed BLAKE3 locally and confirmed via
    /// `GET /api/dedup/check/{hash}`), so the server only bumps the blob's
    /// reference count and registers the metadata row.
    ///
    /// All authorization (folder Create permission, hash ownership with
    /// anti-enumeration, quota) lives in the application service.
    pub(super) async fn create_file_by_hash_impl(
        State(state): State<GlobalState>,
        auth_user: AuthUser,
        Json(request): Json<CreateFileByHashRequest>,
    ) -> impl IntoResponse {
        // Hash shape check — same contract as /api/dedup/check/{hash}.
        if request.hash.len() != 64 || !request.hash.chars().all(|c| c.is_ascii_hexdigit()) {
            return AppError::bad_request(
                "Invalid hash format. Expected BLAKE3 (64 hex characters)",
            )
            .into_response();
        }
        // Basename only — same path-traversal guard as the multipart upload.
        let filename = request
            .name
            .rsplit('/')
            .next()
            .unwrap_or(&request.name)
            .rsplit('\\')
            .next()
            .unwrap_or(&request.name)
            .to_string();
        if filename.is_empty() {
            return AppError::bad_request("File name must not be empty").into_response();
        }

        match state
            .applications
            .file_upload_service
            .create_file_from_owned_blob_with_perms(
                auth_user.id,
                filename,
                request.folder_id,
                &request.hash,
            )
            .await
        {
            Ok(mut file) => {
                crate::interfaces::api::handlers::caller_flags::enrich_file_flags(
                    &state,
                    &mut file,
                    auth_user.id,
                )
                .await;
                Self::created_json_response(&file).into_response()
            }
            Err(err) => {
                // Anti-enumeration shape: every "caller cannot reach this
                // hash" outcome collapses into the same 404 with an
                // `upload_path` hint, regardless of whether the hash exists
                // globally, is owned by another tenant, or got GC'd in a
                // race against trash-empty. Hides the cross-tenant content
                // existence oracle and tells the client where to fall back.
                //
                // Three NotFound("Blob", _) paths in the service map here:
                //   1. user_owns_blob_reference returned false
                //   2. get_blob_metadata returned None (blob row vanished)
                //   3. add_reference lost the race with GC (rows_affected==0)
                use crate::common::errors::ErrorKind;
                if err.kind == ErrorKind::NotFound && err.entity_type == "Blob" {
                    return Response::builder()
                        .status(StatusCode::NOT_FOUND)
                        .header(header::CONTENT_TYPE, "application/json")
                        .body(Body::from(
                            r#"{"error":"blob_not_owned_by_caller","upload_path":"/api/files/upload"}"#,
                        ))
                        .unwrap()
                        .into_response();
                }
                Self::domain_error_response(err).into_response()
            }
        }
    }

    /// Core upload logic shared by [`Self::upload_file`] and
    /// [`Self::upload_file_with_thumbnails`].
    ///
    /// Returns `(FileDto, blob_hash)` on success.  The blob hash is the
    /// BLAKE3 digest computed during the streaming ingest and is
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

        // caveat: if folder_id field is given after check can fails
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

                // ── Fail-fast pre-check: verify the caller can Create inside
                // the target folder BEFORE spooling the multipart body to disk.
                // The upload service re-checks at write time — this is a
                // UX/resource optimization, not the security boundary.
                if let Some(ref fid) = folder_id
                    && let Err(err) = state
                        .applications
                        .folder_service_concrete
                        .require_permission(auth_user.id, Permission::Create, fid)
                        .await
                {
                    tracing::warn!(
                        "⛔ UPLOAD REJECTED: user='{}' folder='{}' err='{}'",
                        auth_user.username,
                        fid,
                        err
                    );
                    return Err(Self::domain_error_response(err));
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

                // ── External mount destination? Stream to the provider ──
                // Detected BEFORE the CAS ingest so the bytes never touch
                // BLAKE3/dedup. Authorization happens inside the service.
                if let Some(ref fid) = folder_id {
                    let (mount_cfg, parent_node) = match state.mount_router.classify(fid) {
                        ResolvedId::MountRoot { cfg } => (Some(cfg), NodeId::default()),
                        ResolvedId::MountChild { cfg, node_id } => (Some(cfg), node_id),
                        ResolvedId::Regular => (None, NodeId::default()),
                    };
                    if let Some(cfg) = mount_cfg {
                        use futures::StreamExt;
                        let body: crate::application::ports::external_mount_ports::MountByteStream<
                            '_,
                        > = Box::pin(
                            upload_ingest::multipart_field_stream(field)
                                .map(|r| r.map_err(|e| std::io::Error::other(e.to_string()))),
                        );
                        return match state
                            .applications
                            .external_upload_service
                            .write_file(&cfg, &parent_node, &filename, body, auth_user.id)
                            .await
                        {
                            Ok(file) => Ok((file, String::new())),
                            Err(err) => Err(Self::domain_error_response(err)),
                        };
                    }
                }

                // ── Stream the field into the CDC chunk store ────────
                // Chunking (FastCDC) + hashing (BLAKE3) + dedup checks +
                // MIME sniffing all happen while the bytes arrive; chunks
                // the store already has never touch the disk. Size is
                // capped globally by DefaultBodyLimit.
                let dedup = &state.core.dedup_service;
                let source = upload_ingest::multipart_field_stream(field);
                let ingested = match upload_ingest::ingest_stream_to_cas(
                    source,
                    dedup,
                    &filename,
                    &content_type,
                    usize::MAX,
                    None,
                )
                .await
                {
                    Ok(ingested) => ingested,
                    Err(e) => {
                        tracing::error!("❌ UPLOAD INGEST FAILED: {} - {}", filename, e.message);
                        return Err(e.into_response());
                    }
                };

                // ── Quota enforcement (exact size now known) ─────────
                if let Some(storage_svc) = state.storage_usage_service.as_ref()
                    && let Err(err) = storage_svc
                        .check_storage_quota(auth_user.id, ingested.size)
                        .await
                {
                    upload_ingest::discard_ingested(dedup, &ingested).await;
                    tracing::warn!(
                        "⛔ UPLOAD REJECTED (user quota): user={}, file={}, size={}",
                        auth_user.username,
                        filename,
                        ingested.size
                    );
                    return Err(Self::quota_error_response(err));
                }

                // ── Per-drive quota enforcement (D4) ─────────────────
                // Sibling to the per-user check above: same read-only
                // SELECT shape, same discard-then-507 outcome. Skipped
                // when there's no folder_id (root-level upload — no
                // drive to charge; folder service refuses these
                // independently). Unlimited-quota drives (`NULL`)
                // short-circuit inside the service.
                if let Some(storage_svc) = state.storage_usage_service.as_ref()
                    && let Some(fid_str) = folder_id.as_deref()
                    && let Ok(fid) = uuid::Uuid::parse_str(fid_str)
                    && let Err(err) = storage_svc
                        .check_drive_quota_by_folder(fid, ingested.size)
                        .await
                {
                    upload_ingest::discard_ingested(dedup, &ingested).await;
                    tracing::warn!(
                        "⛔ UPLOAD REJECTED (drive quota): user={}, folder={}, file={}, size={}",
                        auth_user.username,
                        fid,
                        filename,
                        ingested.size
                    );
                    return Err(Self::quota_error_response(err));
                }

                // ── Register the file row against the ingested blob ──
                let hash = ingested.hash.clone();
                let size = ingested.size;
                match upload_service
                    .upload_file_streaming(
                        filename.clone(),
                        folder_id,
                        ingested.content_type.clone(),
                        ingested.stored(),
                        auth_user.id,
                    )
                    .await
                {
                    Ok(file) => {
                        tracing::info!(
                            "✅ STREAMING UPLOAD: {} ({} bytes, ID: {})",
                            filename,
                            size,
                            file.id
                        );
                        return Ok((file, hash));
                    }
                    Err(err) => {
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
    /// **Cache-first**: once past the hash lookup below, a thumbnail already
    /// in the moka in-memory cache or on disk is served without further DB
    /// work.  The ownership check was already performed when the thumbnail
    /// was first generated (at upload) or uploaded (PUT by the owner).
    /// UUIDv4 file IDs have 122 bits of entropy, making enumeration
    /// infeasible.
    ///
    /// **ETag / 304**: responses carry an immutable ETag keyed on the
    /// **content hash**, so it identifies the bytes rather than the file.
    /// Replacing a file's content changes it (correct invalidation), and two
    /// files with identical content share it (a copy revalidates to 304
    /// instead of refetching).  Costs one PK lookup on the 304 path, which an
    /// id-keyed ETag avoided at the price of never invalidating — see the
    /// comment at the ETag construction.
    ///
    /// Beyond that, the DB path is only taken on a **cache miss for images**
    /// where the thumbnail hasn't been generated yet (first access after
    /// upload if background generation hasn't finished).
    pub(super) async fn get_thumbnail_impl(
        State(state): State<GlobalState>,
        auth_user: AuthUser,
        headers: &HeaderMap,
        Path((id, size)): Path<(String, String)>,
    ) -> impl IntoResponse + use<> {
        use crate::application::ports::thumbnail_ports::{ThumbnailFormat, ThumbnailSize};

        // check first that user can access this resource
        if let Err(err) = state
            .applications
            .file_management_service
            .require_permission(auth_user.id, Permission::Read, &id)
            .await
        {
            return AppError::from(err).into_response();
        }

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

        // Content negotiation: WebP for clients that advertise it (~97%), JPEG
        // otherwise. `Vary: Accept` keeps shared/browser caches from handing a
        // WebP body to a JPEG-only client (or vice-versa).
        let format =
            ThumbnailFormat::from_accept(headers.get(header::ACCEPT).and_then(|v| v.to_str().ok()));

        // ── ETag short-circuit ───────────────────────────────────────
        // Keyed on the CONTENT hash, not the file id. A thumbnail is a pure
        // function of (source bytes, size, format), so that triple genuinely
        // identifies the response — which is what makes the `immutable`
        // directive below an honest claim.
        //
        // Keying on `file_id` was wrong in both directions. Replacing a
        // file's content preserves its id (`file_upload_service` rebuilds the
        // entity with `parts.id` and a new hash, then fires
        // `on_file_updated`, which regenerates the thumbnails), so the ETag
        // never changed — and since `immutable` tells a browser not to
        // revalidate at all inside the freshness window, clients kept the old
        // preview for up to a year. Conversely a copy, or any dedup twin, got
        // a *different* id and so refetched bytes it already held, even
        // though the server serves both from the same derived blob.
        //
        // Cost: one PK lookup, where the id-keyed version needed none. It
        // buys correct invalidation plus 304s shared across every file with
        // the same content. The lookup runs after the authz check above,
        // which has already hit the database.
        //
        // No new disclosure: `content_hash` is already on `FileDto` and
        // returned by `GET /api/files/{id}`, so any caller who reaches here
        // could read it anyway.
        let blob_hash = match state
            .repositories
            .file_read_repository
            .get_blob_hash(&id)
            .await
        {
            Ok(h) => h,
            Err(err) => return AppError::from(err).into_response(),
        };
        let etag = {
            let (s, f) = (thumb_size.as_str(), format.as_str());
            let mut e = String::with_capacity(9 + blob_hash.len() + s.len() + f.len());
            e.push_str("\"thumb-");
            e.push_str(&blob_hash);
            e.push('-');
            e.push_str(s);
            e.push('-');
            e.push_str(f);
            e.push('"');
            e
        };
        if let Some(if_none_match) = headers.get(header::IF_NONE_MATCH)
            && let Ok(val) = if_none_match.to_str()
            && (val == etag || val == "*")
        {
            return Response::builder()
                .status(StatusCode::NOT_MODIFIED)
                .header(header::ETAG, &etag)
                .header(header::VARY, header::ACCEPT.as_str())
                .header(header::CACHE_CONTROL, "public, max-age=31536000, immutable")
                .body(Body::empty())
                .unwrap()
                .into_response();
        }

        // ── Cache-first path (Solution A) ────────────────────────────
        // Try moka (RAM) → disk before touching the database.
        // If the thumbnail exists it was authorized at creation time.
        if let Some(data) = thumbnail_service
            .get_cached_thumbnail(
                &id,
                // Already resolved for the ETag above — hand it over rather
                // than let the service look it up a second time.
                Some(&blob_hash),
                thumb_size.into(),
                format,
                Some(&state.core.dedup_service),
            )
            .await
        {
            return Response::builder()
                .status(StatusCode::OK)
                .header(
                    header::CONTENT_TYPE,
                    crate::common::mime_detect::thumbnail_content_type(&data),
                )
                .header(header::CONTENT_LENGTH, data.len())
                .header(header::CACHE_CONTROL, "public, max-age=31536000, immutable")
                .header(header::ETAG, &etag)
                .header(header::VARY, header::ACCEPT.as_str())
                .body(Body::from(data))
                .unwrap()
                .into_response();
        }

        // ── Cache miss — need DB for ownership + blob resolution ─────
        let file_retrieval_service = &state.applications.file_retrieval_service;

        let file = match file_retrieval_service
            .get_file_or_trashed_with_perms(&id, auth_user.id)
            .await
        {
            Ok(f) => f,
            Err(err) => {
                return AppError::from(err).into_response();
            }
        };

        // Images and videos both store blob-hash thumbnails (videos via an
        // eagerly-extracted frame); anything else has nothing to thumbnail → 204.
        let is_image = thumbnail_service.is_supported_image(&file.mime_type);
        let is_video = file.mime_type.starts_with("video/");
        if !is_image && !is_video {
            return Response::builder()
                .status(StatusCode::NO_CONTENT)
                .header(header::CACHE_CONTROL, "no-store")
                .body(Body::empty())
                .unwrap()
                .into_response();
        }

        // `blob_hash` was resolved above to build the ETag — no second lookup.
        if let Some(data) = thumbnail_service
            .get_cached_thumbnail(
                &id,
                Some(&blob_hash),
                thumb_size.into(),
                format,
                Some(&state.core.dedup_service),
            )
            .await
        {
            return Response::builder()
                .status(StatusCode::OK)
                .header(
                    header::CONTENT_TYPE,
                    crate::common::mime_detect::thumbnail_content_type(&data),
                )
                .header(header::CONTENT_LENGTH, data.len())
                .header(header::CACHE_CONTROL, "public, max-age=31536000, immutable")
                .header(header::ETAG, &etag)
                .header(header::VARY, header::ACCEPT.as_str())
                .body(Body::from(data))
                .unwrap()
                .into_response();
        }

        // Videos: thumbnails are produced eagerly server-side (ffmpeg) on upload,
        // and persisted WebP-only. Serve that WebP regardless of the negotiated
        // format — a JPEG/`*/*`/no-Accept client still gets it, correctly labelled
        // via byte-sniffing — otherwise non-WebP clients would 204 forever despite
        // a valid thumbnail on disk. We never image-decode a video, so a genuine
        // miss (generation in flight or unavailable) returns 204.
        if is_video {
            if let Some(data) = thumbnail_service
                .get_cached_thumbnail(
                    &id,
                    Some(&blob_hash),
                    thumb_size.into(),
                    ThumbnailFormat::Webp,
                    Some(&state.core.dedup_service),
                )
                .await
            {
                return Response::builder()
                    .status(StatusCode::OK)
                    .header(
                        header::CONTENT_TYPE,
                        crate::common::mime_detect::thumbnail_content_type(&data),
                    )
                    .header(header::CONTENT_LENGTH, data.len())
                    .header(header::CACHE_CONTROL, "public, max-age=31536000, immutable")
                    .header(header::ETAG, &etag)
                    .header(header::VARY, header::ACCEPT.as_str())
                    .body(Body::from(data))
                    .unwrap()
                    .into_response();
            }
            return Response::builder()
                .status(StatusCode::NO_CONTENT)
                .header(header::CACHE_CONTROL, "no-store")
                .body(Body::empty())
                .unwrap()
                .into_response();
        }

        match thumbnail_service
            .get_thumbnail_from_blob(
                &id,
                &blob_hash,
                thumb_size.into(),
                format,
                state.core.dedup_service.clone(),
            )
            .await
        {
            Ok(data) => Response::builder()
                .status(StatusCode::OK)
                .header(
                    header::CONTENT_TYPE,
                    crate::common::mime_detect::thumbnail_content_type(&data),
                )
                .header(header::CONTENT_LENGTH, data.len())
                .header(header::CACHE_CONTROL, "public, max-age=31536000, immutable")
                .header(header::ETAG, &etag)
                .header(header::VARY, header::ACCEPT.as_str())
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

        // check first that user can access this resource
        if let Err(err) = state
            .applications
            .file_management_service
            .require_permission(auth_user.id, Permission::Update, &id)
            .await
        {
            return AppError::from(err).into_response();
        }

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
            .get_file_with_perms(&id, auth_user.id)
            .await
        {
            return AppError::from(err).into_response();
        }

        // Validate, re-encode, and store the per-file sidecar.
        let stored = match thumbnail_service
            .store_external_thumbnail(&id, thumb_size.into(), body)
            .await
        {
            Ok(bytes) => bytes,
            Err(err) => {
                return AppError::internal_error(format!("Failed to store thumbnail: {}", err))
                    .into_response();
            }
        };

        // Also record it as a file-keyed attachment.
        //
        // The sidecar above is `ext-{file_id}.jpg` on local disk, which no
        // copy path duplicates and no other instance can see. Without this
        // row a copied file loses the preview its owner uploaded — falling
        // back to a rendered thumbnail, or to nothing at all for a PDF, which
        // has no server-side render path. `copy_file_satellites` duplicates
        // the row, so the copy inherits the bytes.
        //
        // File-keyed, never content-keyed: these bytes are the uploader's
        // claim about THIS file, and sharing them across files with identical
        // content is the poisoning vector `storage.file_attached_blobs`
        // exists to prevent.
        //
        // Best-effort: the sidecar already succeeded, so the user has their
        // thumbnail. Failing the request here would report an error for an
        // operation that visibly worked.
        if let Err(e) = state
            .core
            .dedup_service
            .store_attached_blob(
                &id,
                "preview",
                thumb_size.dir_name(),
                "image/jpeg",
                stored,
                auth_user.id,
            )
            .await
        {
            // ERROR, not WARN: the sidecar keeps the feature looking healthy
            // on this box, so nothing else signals that copies are silently
            // losing the preview. A syntax error in the upsert hid behind a
            // warning for an entire test cycle exactly this way.
            tracing::error!(
                target: "oxicloud::dedup",
                error = %e,
                file_id = %id,
                "failed to record attached thumbnail; sidecar written, copies will NOT inherit it"
            );
        }

        StatusCode::CREATED.into_response()
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
        headers: &HeaderMap,
    ) -> impl IntoResponse + use<> {
        // External mount: download a file living on the provider's backend.
        // (A mount-root UUID is a folder and is not downloadable — it falls
        // through and 404s as a non-file.)
        if let ResolvedId::MountChild { cfg, node_id } = state.mount_router.classify(&id) {
            return Self::download_mount_file(
                &state,
                &cfg,
                &node_id,
                &id,
                auth_user.id,
                &params,
                headers,
            )
            .await;
        }

        let retrieval = &state.applications.file_retrieval_service;

        // ── Get file metadata (ownership-scoped) ────────────────────────
        let file_dto = match retrieval.get_file_with_perms(&id, auth_user.id).await {
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

        // Route through `FileDto::etag` so this REST download
        // endpoint, WebDAV/NextCloud GET, HEAD, PROPFIND, and PUT all
        // emit the same opaque token for the same file — see
        // `File::etag` for the formula.
        let etag = format!("\"{}\"", file_dto.etag);

        // ── ETag (304 Not Modified) ──────────────────────────────────
        if let Some(resp) = not_modified_response(headers, &etag) {
            return resp.into_response();
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

                    // `file_dto` was already Read-authorized (and the access
                    // recorded) by `get_file_with_perms` above — every seek in
                    // a media/PDF scrub is a separate Range request, so
                    // re-authorizing + re-notifying per seek doubled that work
                    // for nothing. Use the non-perms range read, matching the
                    // share-landing and WebDAV range paths which authorize once
                    // then stream (benches/ROUND7.md).
                    match retrieval
                        .get_file_range_preloaded(&file_dto, start, Some(end + 1))
                        .await
                    {
                        Ok(content) => {
                            let body = match content {
                                RangeContent::Bytes(b) => Body::from(b),
                                RangeContent::Stream(s) => Body::from_stream(Box::into_pin(s)),
                            };
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
                                .body(body)
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
        // so we can safely use the preloaded variant. Capture the two
        // fields the stream arm needs (one Arc bump + a u64 copy) and MOVE
        // the DTO in — the old `file_dto.clone()` deep-copied all 7 owned
        // Strings on every download, purely to read mime/size afterwards
        // (benches/ROUND11.md §1).
        let dto_mime = file_dto.mime_type.clone();
        let dto_size = file_dto.size;
        match retrieval
            .get_file_optimized_preloaded(&id, file_dto, accept_webp, prefer_original)
            .await
        {
            Ok((_file, content)) => match content {
                OptimizedFileContent::Bytes {
                    data, mime_type, ..
                } => Self::build_cached_response(data, &mime_type, &disposition, &etag)
                    .into_response(),
                OptimizedFileContent::Stream(pinned_stream) => Response::builder()
                    .status(StatusCode::OK)
                    .header(header::CONTENT_TYPE, &*dto_mime)
                    .header(header::CONTENT_DISPOSITION, &disposition)
                    .header(header::CONTENT_LENGTH, dto_size)
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

    /// Download a file living inside an external mount: stat via the provider
    /// (authorized against the mount root), then serve metadata / 304 / Range /
    /// full stream straight from the backend. No blob cache, dedup, or WebP
    /// transcode — mount content is served as-is.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn download_mount_file(
        state: &AppState,
        cfg: &MountConfig,
        node_id: &NodeId,
        id: &str,
        caller_id: uuid::Uuid,
        params: &HashMap<String, String>,
        headers: &HeaderMap,
    ) -> axum::response::Response {
        let retrieval = &state.applications.file_retrieval_service;

        let stat: MountStat = match retrieval
            .stat_mount_file_with_perms(cfg, node_id, caller_id)
            .await
        {
            Ok(s) => s,
            Err(err) => return AppError::from(err).into_response(),
        };
        if stat.is_dir {
            // Directories are not downloadable through this endpoint.
            return AppError::from(DomainError::not_found("File", id)).into_response();
        }

        let name = node_id
            .as_str()
            .rsplit('/')
            .next()
            .unwrap_or_else(|| node_id.as_str());

        // ── Metadata-only request ────────────────────────────────────
        if params
            .get("metadata")
            .is_some_and(|v| v == "true" || v == "1")
        {
            return (
                StatusCode::OK,
                Json(serde_json::json!({
                    "id": id,
                    "name": name,
                    "size": stat.size,
                    "mime_type": stat.mime_type,
                    "modified_at": stat.modified_at,
                })),
            )
                .into_response();
        }

        let etag = format!("\"{}\"", virtual_file_etag(stat.size, stat.modified_at));
        if let Some(resp) = not_modified_response(headers, &etag) {
            return resp.into_response();
        }

        // ── Range Requests ───────────────────────────────────────────
        let range_header = headers.get(header::RANGE).and_then(|v| v.to_str().ok());
        match plan_mount_range(stat.size, range_header) {
            MountRangePlan::Full => {}
            MountRangePlan::NotSatisfiable => {
                return Response::builder()
                    .status(StatusCode::RANGE_NOT_SATISFIABLE)
                    .header(header::CONTENT_RANGE, format!("bytes */{}", stat.size))
                    .body(Body::empty())
                    .unwrap()
                    .into_response();
            }
            MountRangePlan::Range { start, end } => {
                let range_length = end - start + 1;
                let disposition = Self::content_disposition(name, &stat.mime_type, params);
                match retrieval
                    .open_mount_file_with_perms(cfg, node_id, caller_id, Some((start, Some(end))))
                    .await
                {
                    Ok(stream) => {
                        return Response::builder()
                            .status(StatusCode::PARTIAL_CONTENT)
                            .header(header::CONTENT_TYPE, &stat.mime_type)
                            .header(header::CONTENT_DISPOSITION, &disposition)
                            .header(header::CONTENT_LENGTH, range_length)
                            .header(
                                header::CONTENT_RANGE,
                                format!("bytes {}-{}/{}", start, end, stat.size),
                            )
                            .header(header::ACCEPT_RANGES, "bytes")
                            .header(header::ETAG, &etag)
                            .header(
                                header::CACHE_CONTROL,
                                "private, max-age=3600, must-revalidate",
                            )
                            .body(Body::from_stream(stream))
                            .unwrap()
                            .into_response();
                    }
                    Err(err) => {
                        tracing::error!("Error creating mount range stream: {}", err);
                        // fall through to full download
                    }
                }
            }
        }

        // ── Normal download ──────────────────────────────────────────
        let disposition = Self::content_disposition(name, &stat.mime_type, params);
        match retrieval
            .open_mount_file_with_perms(cfg, node_id, caller_id, None)
            .await
        {
            Ok(stream) => Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, &stat.mime_type)
                .header(header::CONTENT_DISPOSITION, &disposition)
                .header(header::CONTENT_LENGTH, stat.size)
                .header(header::ETAG, &etag)
                .header(
                    header::CACHE_CONTROL,
                    "private, max-age=3600, must-revalidate",
                )
                .header(header::ACCEPT_RANGES, "bytes")
                .body(Body::from_stream(stream))
                .unwrap()
                .into_response(),
            Err(err) => AppError::from(err).into_response(),
        }
    }

    // ═══════════════════════════════════════════════════════════════════════
    //  LIST
    // ═══════════════════════════════════════════════════════════════════════

    /// Lists files in a folder, extracting `folder_id` from query parameters.
    pub(super) async fn list_files_query_impl(
        State(state): State<GlobalState>,
        auth_user: AuthUser,
        headers: &HeaderMap,
        Query(params): Query<HashMap<String, String>>,
    ) -> impl IntoResponse + use<> {
        let folder_id = params.get("folder_id").map(|id| id.as_str());
        tracing::info!("API: Listing files with folder_id: {:?}", folder_id);

        let retrieval = &state.applications.file_retrieval_service;
        match retrieval
            .list_files_with_perms(folder_id, auth_user.id)
            .await
        {
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
                // Pre-sized serialization — this listing is unbounded (no
                // page cap), the axum Json 128-byte seed reallocs ~11 times
                // on a big folder (benches/ROUND12.md §M1).
                let mut resp = crate::interfaces::api::sized_json::sized_json(
                    64 + files.len() * crate::interfaces::api::sized_json::EST_ROW_BYTES,
                    &files,
                );
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
    /// TODO: should move thumbnail generation to a generic hook ? (onfileUploaded, other services will beneficiate it)
    pub(super) async fn upload_file_with_thumbnails_impl(
        State(state): State<GlobalState>,
        auth_user: AuthUser,
        multipart: Multipart,
    ) -> impl IntoResponse {
        let (mut file, _) = match Self::upload_file_inner(&state, &auth_user, multipart).await {
            Ok(pair) => pair,
            Err(response) => return response.into_response(),
        };
        crate::interfaces::api::handlers::caller_flags::enrich_file_flags(
            &state,
            &mut file,
            auth_user.id,
        )
        .await;
        Self::created_json_response(&file).into_response()
    }

    // ═══════════════════════════════════════════════════════════════════════
    //  METADATA
    // ═══════════════════════════════════════════════════════════════════════

    /// Returns EXIF/media metadata for a file.
    ///
    /// Used by the Photos lightbox and for testing EXIF extraction.
    pub(super) async fn get_file_metadata_impl(
        State(state): State<GlobalState>,
        auth_user: AuthUser,
        Path(file_id): Path<String>,
    ) -> impl IntoResponse {
        // check first that user can access this resource
        if let Err(err) = state
            .applications
            .file_management_service
            .require_permission(auth_user.id, Permission::Read, &file_id)
            .await
        {
            return AppError::from(err).into_response();
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
            .delete_and_cleanup_with_perms(&id, auth_user.id)
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
        match mgmt
            .rename_file_with_perms(&id, auth_user.id, &new_name)
            .await
        {
            Ok(file_dto) => (StatusCode::OK, Json(file_dto)).into_response(),
            Err(err) => AppError::from(err).into_response(),
        }
    }

    /// Moves a file to a different folder (ownership-verified)
    /// TODO: dead function ?
    pub async fn move_file(
        State(state): State<GlobalState>,
        auth_user: AuthUser,
        Path(id): Path<String>,
        Json(payload): Json<MoveFilePayload>,
    ) -> impl IntoResponse {
        tracing::info!("Moving file {} to folder {:?}", id, payload.folder_id);

        let mgmt = &state.applications.file_management_service;

        match mgmt
            .move_file_with_perms(&id, auth_user.id, payload.folder_id)
            .await
        {
            Ok(mut file) => {
                crate::interfaces::api::handlers::caller_flags::enrich_file_flags(
                    &state,
                    &mut file,
                    auth_user.id,
                )
                .await;
                (StatusCode::OK, Json(file)).into_response()
            }
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
        match mgmt
            .move_file_with_perms(&id, auth_user.id, folder_id)
            .await
        {
            Ok(file_dto) => (StatusCode::OK, Json(file_dto)).into_response(),
            Err(err) => AppError::from(err).into_response(),
        }
    }

    // ═══════════════════════════════════════════════════════════════════════
    //  PRIVATE HELPERS
    // ═══════════════════════════════════════════════════════════════════════

    /// Build a Content-Disposition header value.
    ///
    /// Build a `Content-Disposition` header value for an authenticated download,
    /// honouring the `?inline=true|1` query param. Delegates to the shared
    /// `build_content_disposition` so the share-link path produces identical
    /// header values for the same `(name, mime)` pair.
    fn content_disposition(name: &str, mime: &str, params: &HashMap<String, String>) -> String {
        let force_inline = params
            .get("inline")
            .is_some_and(|v| v == "true" || v == "1");
        build_content_disposition(name, mime, force_inline)
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

/// RFC 5987-compliant `Content-Disposition` with both ASCII fallback and
/// `filename*=UTF-8''...` for non-ASCII filenames.
pub(super) fn build_content_disposition(name: &str, mime: &str, force_inline: bool) -> String {
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

    use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
    // RFC 5987 attr-char safe set (no encoding needed for these).
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
    // Fast path: a name whose every byte is an RFC 5987 attr-char needs neither
    // percent-encoding nor ASCII-fallback filtering ('"' and '\\' are not
    // attr-chars, so none is substituted), so `filename` and `filename*` are the
    // name verbatim — one allocation (the header) instead of three.
    let all_attr_char = name.bytes().all(|b| {
        b.is_ascii_alphanumeric()
            || matches!(
                b,
                b'!' | b'#' | b'$' | b'&' | b'+' | b'-' | b'.' | b'^' | b'_' | b'`' | b'|' | b'~'
            )
    });
    if all_attr_char {
        return format!("{disposition}; filename=\"{name}\"; filename*=UTF-8''{name}");
    }

    // Slow path: assemble the header in one pre-sized buffer, writing the ASCII
    // fallback and the percent-encoded form in place — no throwaway `ascii_safe`
    // / `encoded` Strings. Sized for the worst case (every byte → %XX) so it
    // never grows.
    let mut out = String::with_capacity(disposition.len() + name.len() * 4 + 32);
    out.push_str(disposition);
    out.push_str("; filename=\"");
    for c in name.chars().filter(|c| c.is_ascii_graphic() || *c == ' ') {
        out.push(match c {
            '"' | '\\' => '_',
            _ => c,
        });
    }
    out.push_str("\"; filename*=UTF-8''");
    for chunk in utf8_percent_encode(name, RFC5987_SET) {
        out.push_str(chunk);
    }
    out
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
    security(("bearerAuth" = [])),
    tag = "files"
)]
pub async fn list_files_query(
    state: State<GlobalState>,
    auth_user: AuthUser,
    query: Query<HashMap<String, String>>,
    req: axum::extract::Request,
) -> impl IntoResponse {
    // Read headers by borrow (`req.headers()`) instead of the `HeaderMap`
    // extractor, which clones the whole request header table (~2 allocs) just to
    // read one If-None-Match — the ROUND14 §A4 middleware pattern applied to the
    // hot listing handler (benches/ROUND22.md §H1).
    FileHandler::list_files_query_impl(state, auth_user, req.headers(), query).await
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
    security(("bearerAuth" = [])),
    tag = "files"
)]
pub async fn upload_file_with_thumbnails(
    state: State<GlobalState>,
    auth_user: AuthUser,
    multipart: Multipart,
) -> impl IntoResponse {
    FileHandler::upload_file_with_thumbnails_impl(state, auth_user, multipart).await
}

/// Request body for the instant-upload endpoint.
#[derive(Debug, Deserialize, ToSchema)]
pub struct CreateFileByHashRequest {
    /// File name to create (path components are stripped).
    pub name: String,
    /// Target folder ID (the caller needs Create permission on it).
    pub folder_id: String,
    /// BLAKE3 hash (64 hex chars) of content the caller already owns.
    pub hash: String,
}

#[utoipa::path(
    post,
    path = "/api/files/by-hash",
    request_body = CreateFileByHashRequest,
    responses(
        (status = 201, description = "File created from an already-owned blob — zero bytes transferred", body = FileDto),
        (status = 400, description = "Invalid hash format or empty name"),
        (status = 404, description = "No owned blob with this hash (anti-enumeration: same shape as unknown hash)"),
        (status = 409, description = "A file with this name already exists in the folder"),
        (status = 507, description = "Storage quota exceeded"),
    ),
    security(("bearerAuth" = [])),
    tag = "files"
)]
pub async fn create_file_by_hash(
    state: State<GlobalState>,
    auth_user: AuthUser,
    request: Json<CreateFileByHashRequest>,
) -> impl IntoResponse {
    FileHandler::create_file_by_hash_impl(state, auth_user, request).await
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
    security(("bearerAuth" = [])),
    tag = "files"
)]
pub async fn download_file(
    state: State<GlobalState>,
    auth_user: AuthUser,
    path: Path<String>,
    query: Query<HashMap<String, String>>,
    req: axum::extract::Request,
) -> impl IntoResponse {
    // Borrow the headers (`req.headers()`) instead of the `HeaderMap` extractor's
    // full clone — every download AND every media Range seek hit this path
    // (benches/ROUND22.md §H1).
    FileHandler::download_file_impl(state, auth_user, path, query, req.headers()).await
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
    security(("bearerAuth" = [])),
    tag = "files"
)]
pub async fn get_thumbnail(
    state: State<GlobalState>,
    auth_user: AuthUser,
    path: Path<(String, String)>,
    req: axum::extract::Request,
) -> impl IntoResponse {
    // Borrow the headers (`req.headers()`) instead of the `HeaderMap` extractor's
    // full clone — thumbnails are the highest-frequency GET (one per grid tile),
    // and this handler reads only Accept + If-None-Match (benches/ROUND22.md §H1).
    FileHandler::get_thumbnail_impl(state, auth_user, req.headers(), path).await
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
    security(("bearerAuth" = [])),
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
    security(("bearerAuth" = [])),
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
    security(("bearerAuth" = [])),
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
    security(("bearerAuth" = [])),
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
    security(("bearerAuth" = [])),
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

/// The download decision for a mount file given a `Range` header — the gnarly
/// parse-and-validate logic, extracted so it is unit-testable without I/O.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum MountRangePlan {
    /// Serve the whole file (no/invalid range header).
    Full,
    /// Serve `start..=end` (inclusive) as 206 Partial Content.
    Range { start: u64, end: u64 },
    /// The requested range is unsatisfiable for this size → 416.
    NotSatisfiable,
}

/// Decide how to serve a mount file for a given size + optional `Range` header.
/// A missing or unparseable range → `Full`; a valid range → `Range`; an
/// out-of-bounds range → `NotSatisfiable`.
pub(super) fn plan_mount_range(size: u64, range_header: Option<&str>) -> MountRangePlan {
    let Some(rh) = range_header else {
        return MountRangePlan::Full;
    };
    let Ok(ranges) = parse_range_header(rh) else {
        // Malformed range header: ignore it and serve the whole file (RFC 7233).
        return MountRangePlan::Full;
    };
    match ranges.validate(size) {
        Ok(valid) => match valid.first() {
            Some(r) => MountRangePlan::Range {
                start: *r.start(),
                end: *r.end(),
            },
            None => MountRangePlan::Full,
        },
        Err(_) => MountRangePlan::NotSatisfiable,
    }
}

#[cfg(test)]
mod mount_range_tests {
    use super::{MountRangePlan, plan_mount_range};

    #[test]
    fn no_range_header_is_full() {
        assert_eq!(plan_mount_range(100, None), MountRangePlan::Full);
    }

    #[test]
    fn malformed_range_falls_back_to_full() {
        assert_eq!(
            plan_mount_range(100, Some("not-a-range")),
            MountRangePlan::Full
        );
        assert_eq!(
            plan_mount_range(100, Some("bytes=abc")),
            MountRangePlan::Full
        );
    }

    #[test]
    fn valid_range_is_parsed_inclusive() {
        assert_eq!(
            plan_mount_range(100, Some("bytes=10-19")),
            MountRangePlan::Range { start: 10, end: 19 }
        );
    }

    #[test]
    fn open_ended_range_extends_to_eof() {
        assert_eq!(
            plan_mount_range(100, Some("bytes=90-")),
            MountRangePlan::Range { start: 90, end: 99 }
        );
    }

    #[test]
    fn suffix_range_counts_from_end() {
        // last 10 bytes of a 100-byte file => 90..=99
        assert_eq!(
            plan_mount_range(100, Some("bytes=-10")),
            MountRangePlan::Range { start: 90, end: 99 }
        );
    }

    #[test]
    fn out_of_bounds_range_is_not_satisfiable() {
        assert_eq!(
            plan_mount_range(100, Some("bytes=200-300")),
            MountRangePlan::NotSatisfiable
        );
    }
}
