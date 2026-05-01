//! Chunked Upload Handler - TUS-like Protocol Endpoints
//!
//! Provides HTTP endpoints for resumable, parallel chunk uploads:
//! - POST   /api/uploads          → Create upload session
//! - PATCH  /api/uploads/:id      → Upload a chunk
//! - HEAD   /api/uploads/:id      → Get upload status
//! - POST   /api/uploads/:id/complete → Assemble and finalize
//! - DELETE /api/uploads/:id      → Cancel upload

use axum::{
    Json,
    extract::{Path, Query, Request, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
};
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use std::sync::Arc;

use crate::application::ports::chunked_upload_ports::ChunkedUploadPort;
use crate::application::ports::chunked_upload_ports::DEFAULT_CHUNK_SIZE;
use crate::application::ports::file_ports::FileUploadUseCase;
use crate::application::ports::storage_ports::StorageUsagePort;
use crate::common::di::AppState;
use crate::interfaces::errors::AppError;
use crate::interfaces::middleware::auth::AuthUser;

/// Request body for creating an upload session
#[derive(Debug, Deserialize, ToSchema)]
pub struct CreateUploadRequest {
    pub filename: String,
    pub folder_id: Option<String>,
    pub content_type: Option<String>,
    pub total_size: u64,
    pub chunk_size: Option<usize>,
}

/// Query params for chunk upload
#[derive(Debug, Deserialize)]
pub struct ChunkUploadParams {
    pub chunk_index: usize,
    pub checksum: Option<String>,
}

/// Final response after completing upload
#[derive(Debug, Serialize, ToSchema)]
pub struct CompleteUploadResponse {
    pub file_id: String,
    pub filename: String,
    pub size: u64,
    pub path: String,
}

/// Chunked Upload Handler
///
/// The handler struct exists as a named grouping. All route functions are free
/// functions at module scope — see the section below the impl block for the reason.
pub struct ChunkedUploadHandler;

impl ChunkedUploadHandler {
    // ── Why no #[utoipa::path] here? ─────────────────────────────────────────────
    // utoipa 5.4.0's proc macro generates helper structs / impls inside its expansion.
    // Rust allows struct definitions at module scope but forbids them inside impl blocks,
    // so `#[utoipa::path]` fails on every method in this impl block regardless of HTTP
    // verb or annotation content. The same macro works fine on FileHandler / FolderHandler
    // (root cause in utoipa unknown — likely a 5.4.x bug). All five route handlers are
    // therefore declared as free functions below, which delegate to these `*_impl` methods.
    // TODO: try removing free-function indirection after a utoipa upgrade.
    pub(super) async fn create_upload_impl(
        State(state): State<Arc<AppState>>,
        auth_user: AuthUser,
        Json(request): Json<CreateUploadRequest>,
    ) -> impl IntoResponse {
        let chunked_service = &state.core.chunked_upload_service;

        // Validate request
        if request.filename.is_empty() {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "error": "Filename is required"
                })),
            )
                .into_response();
        }

        if request.total_size == 0 {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "error": "Total size must be greater than 0"
                })),
            )
                .into_response();
        }

        // ── Quota enforcement ────────────────────────────────────
        if let Some(storage_svc) = state.storage_usage_service.as_ref()
            && let Err(err) = storage_svc
                .check_storage_quota(auth_user.id, request.total_size)
                .await
        {
            tracing::warn!(
                "⛔ CHUNKED UPLOAD REJECTED (quota): user={}, file={}, size={} — {}",
                auth_user.username,
                request.filename,
                request.total_size,
                err.message
            );
            return (
                StatusCode::INSUFFICIENT_STORAGE,
                Json(serde_json::json!({
                    "error": err.message,
                    "error_type": "QuotaExceeded"
                })),
            )
                .into_response();
        }

        // Validate chunk size if provided
        let chunk_size = request.chunk_size.unwrap_or(DEFAULT_CHUNK_SIZE);
        if chunk_size < 1024 * 1024 {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "error": "Chunk size must be at least 1MB"
                })),
            )
                .into_response();
        }

        let content_type = request
            .content_type
            .unwrap_or_else(|| "application/octet-stream".to_string());

        match chunked_service
            .create_session(
                auth_user.id,
                request.filename,
                request.folder_id,
                content_type,
                request.total_size,
                Some(chunk_size),
            )
            .await
        {
            Ok(response) => (StatusCode::CREATED, Json(response)).into_response(),
            Err(e) => {
                tracing::error!("Failed to create upload session: {}", e);
                AppError::internal_error(format!("Failed to create upload session: {}", e))
                    .into_response()
            }
        }
    }

    pub(super) async fn upload_chunk_impl(
        State(state): State<Arc<AppState>>,
        auth_user: AuthUser,
        Path(upload_id): Path<String>,
        Query(params): Query<ChunkUploadParams>,
        headers: HeaderMap,
        body: Bytes,
    ) -> impl IntoResponse {
        let chunked_service = &state.core.chunked_upload_service;

        // Extract checksum from header or query param
        let checksum = params.checksum.or_else(|| {
            headers
                .get("Content-MD5")
                .and_then(|v| v.to_str().ok())
                .map(|s| s.to_string())
        });

        match chunked_service
            .upload_chunk(&upload_id, auth_user.id, params.chunk_index, body, checksum)
            .await
        {
            Ok(response) => {
                let mut resp = Response::builder()
                    .status(StatusCode::OK)
                    .header(header::CONTENT_TYPE, "application/json")
                    .header("Upload-Offset", response.bytes_received.to_string())
                    .header(
                        "Upload-Progress",
                        format!("{:.2}", response.progress * 100.0),
                    );

                if response.is_complete {
                    resp = resp.header("Upload-Complete", "true");
                }

                resp.body(axum::body::Body::from(
                    serde_json::to_string(&response).unwrap(),
                ))
                .unwrap()
                .into_response()
            }
            Err(e) => AppError::from(e).into_response(),
        }
    }

    pub(super) async fn get_upload_status_impl(
        State(state): State<Arc<AppState>>,
        auth_user: AuthUser,
        Path(upload_id): Path<String>,
    ) -> impl IntoResponse {
        let chunked_service = &state.core.chunked_upload_service;

        match chunked_service.get_status(&upload_id, auth_user.id).await {
            Ok(status) => Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, "application/json")
                .header("Upload-Offset", status.bytes_received.to_string())
                .header("Upload-Length", status.total_size.to_string())
                .header("Upload-Progress", format!("{:.2}", status.progress * 100.0))
                .header("Upload-Chunks-Total", status.total_chunks.to_string())
                .header(
                    "Upload-Chunks-Complete",
                    status.completed_chunks.to_string(),
                )
                .body(axum::body::Body::from(
                    serde_json::to_string(&status).unwrap(),
                ))
                .unwrap()
                .into_response(),
            Err(e) => AppError::from(e).into_response(),
        }
    }

    pub(super) async fn complete_upload_impl(
        State(state): State<Arc<AppState>>,
        auth_user: AuthUser,
        Path(upload_id): Path<String>,
    ) -> impl IntoResponse {
        let chunked_service = &state.core.chunked_upload_service;
        let upload_service = &state.applications.file_upload_service;

        // Assemble chunks (hash-on-write: SHA-256 computed during assembly)
        let (assembled_path, filename, folder_id, content_type, total_size, hash) =
            match chunked_service
                .complete_upload(&upload_id, auth_user.id)
                .await
            {
                Ok(result) => result,
                Err(e) => {
                    return AppError::from(e).into_response();
                }
            };

        // ── MIME detection (magic bytes + extension fallback) ─────
        let content_type = crate::common::mime_detect::refine_content_type_from_file(
            &assembled_path,
            &filename,
            &content_type,
        )
        .await;

        // Upload from assembled file on disk — zero extra RAM copies, hash pre-computed
        match upload_service
            .upload_file_from_path(
                filename.clone(),
                folder_id.clone(),
                content_type,
                &assembled_path,
                Some(hash),
            )
            .await
        {
            Ok(file) => {
                // Cleanup session
                let _ = chunked_service
                    .finalize_upload(&upload_id, auth_user.id)
                    .await;

                tracing::info!(
                    "✅ CHUNKED UPLOAD COMPLETE: {} (ID: {}, {} bytes)",
                    filename,
                    file.id,
                    total_size
                );

                (
                    StatusCode::CREATED,
                    Json(CompleteUploadResponse {
                        file_id: file.id,
                        filename: file.name,
                        size: total_size,
                        path: file.path,
                    }),
                )
                    .into_response()
            }
            Err(e) => {
                tracing::error!("Failed to create file from assembled upload: {:?}", e);
                AppError::internal_error(format!("Failed to create file: {}", e)).into_response()
            }
        }
    }

    pub(super) async fn cancel_upload_impl(
        State(state): State<Arc<AppState>>,
        auth_user: AuthUser,
        Path(upload_id): Path<String>,
    ) -> impl IntoResponse {
        let chunked_service = &state.core.chunked_upload_service;

        match chunked_service
            .cancel_upload(&upload_id, auth_user.id)
            .await
        {
            Ok(_) => StatusCode::NO_CONTENT.into_response(),
            Err(e) => {
                AppError::internal_error(format!("Failed to cancel upload: {}", e)).into_response()
            }
        }
    }
}

// ── Route handlers (free functions) ──────────────────────────────────────────
//
// All five route functions live here rather than as methods on ChunkedUploadHandler
// because utoipa 5.4.0's #[utoipa::path] macro generates helper structs inside its
// expansion. Rust allows struct definitions at module scope but forbids them inside
// impl blocks — so every #[utoipa::path] annotation on a ChunkedUploadHandler method
// fails to compile regardless of HTTP verb or annotation content.
//
// FileHandler and FolderHandler are not affected (root cause in utoipa unknown, likely
// a 5.4.x regression). All logic lives in the ChunkedUploadHandler::*_impl methods
// above; these thin wrappers exist solely to carry the OpenAPI annotation at a scope
// where utoipa can generate its helper types.
//
// routes.rs calls these free functions directly.
// TODO: collapse back into the impl block after a utoipa upgrade resolves the issue.

#[utoipa::path(
    post,
    path = "/api/uploads",
    request_body(content = CreateUploadRequest, content_type = "application/json", description = "Upload session parameters"),
    responses(
        (status = 201, description = "Upload session created", body = crate::application::ports::chunked_upload_ports::CreateUploadResponseDto),
        (status = 400, description = "Invalid request (empty filename, zero size, chunk too small)"),
        (status = 507, description = "Storage quota exceeded"),
    ),
    tag = "uploads",
    security(("bearerAuth" = []))
)]
pub async fn create_upload(
    state: State<Arc<AppState>>,
    auth_user: AuthUser,
    request: Json<CreateUploadRequest>,
) -> impl IntoResponse {
    ChunkedUploadHandler::create_upload_impl(state, auth_user, request).await
}

#[utoipa::path(
    patch,
    path = "/api/uploads/{upload_id}",
    params(
        ("upload_id" = String, Path, description = "Upload session ID"),
        ("chunk_index" = usize, Query, description = "Zero-based chunk index"),
        ("checksum" = Option<String>, Query, description = "Optional MD5 checksum for integrity verification"),
    ),
    request_body(content_type = "application/octet-stream", description = "Raw chunk bytes"),
    responses(
        (status = 200, description = "Chunk received", body = crate::application::ports::chunked_upload_ports::ChunkUploadResponseDto),
        (status = 400, description = "Invalid chunk or checksum mismatch"),
        (status = 404, description = "Upload session not found"),
    ),
    tag = "uploads",
    security(("bearerAuth" = []))
)]
pub async fn upload_chunk(
    state: State<Arc<AppState>>,
    auth_user: AuthUser,
    path: Path<String>,
    query: Query<ChunkUploadParams>,
    headers: HeaderMap,
    request: Request,
) -> impl IntoResponse {
    let body = axum::body::to_bytes(request.into_body(), usize::MAX)
        .await
        .unwrap_or_default();
    ChunkedUploadHandler::upload_chunk_impl(state, auth_user, path, query, headers, body).await
}

#[utoipa::path(
    head,
    path = "/api/uploads/{upload_id}",
    params(
        ("upload_id" = String, Path, description = "Upload session ID"),
    ),
    responses(
        (status = 200, description = "Upload status in response headers and body", body = crate::application::ports::chunked_upload_ports::UploadStatusResponseDto),
        (status = 404, description = "Upload session not found"),
    ),
    tag = "uploads",
    security(("bearerAuth" = []))
)]
pub async fn get_upload_status(
    state: State<Arc<AppState>>,
    auth_user: AuthUser,
    path: Path<String>,
) -> impl IntoResponse {
    ChunkedUploadHandler::get_upload_status_impl(state, auth_user, path).await
}

#[utoipa::path(
    post,
    path = "/api/uploads/{upload_id}/complete",
    params(
        ("upload_id" = String, Path, description = "Upload session ID"),
    ),
    responses(
        (status = 201, description = "File assembled and created", body = CompleteUploadResponse),
        (status = 404, description = "Upload session not found"),
        (status = 500, description = "Assembly or file creation failed"),
    ),
    tag = "uploads",
    security(("bearerAuth" = []))
)]
pub async fn complete_upload(
    state: State<Arc<AppState>>,
    auth_user: AuthUser,
    path: Path<String>,
) -> impl IntoResponse {
    ChunkedUploadHandler::complete_upload_impl(state, auth_user, path).await
}

#[utoipa::path(
    delete,
    path = "/api/uploads/{upload_id}",
    params(
        ("upload_id" = String, Path, description = "Upload session ID"),
    ),
    responses(
        (status = 204, description = "Upload cancelled and temp files cleaned up"),
        (status = 500, description = "Cancel failed"),
    ),
    tag = "uploads",
    security(("bearerAuth" = []))
)]
pub async fn cancel_upload(
    state: State<Arc<AppState>>,
    auth_user: AuthUser,
    path: Path<String>,
) -> impl IntoResponse {
    ChunkedUploadHandler::cancel_upload_impl(state, auth_user, path).await
}
