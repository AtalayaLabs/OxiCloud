//! Chunked Upload Handler - TUS-like Protocol Endpoints
//!
//! Provides HTTP endpoints for resumable, parallel chunk uploads:
//! - POST   /api/uploads          → Create upload session
//! - PATCH  /api/uploads/:id      → Upload a chunk
//! - HEAD   /api/uploads/:id      → Get upload status
//! - POST   /api/uploads/:id/complete → Stream parts into the blob store
//! - DELETE /api/uploads/:id      → Cancel upload

use axum::{
    Json,
    extract::{Path, Query, Request, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use utoipa::ToSchema;

use crate::application::ports::chunked_upload_ports::ChecksumAlg;
use crate::application::ports::chunked_upload_ports::ChunkedUploadPort;
use crate::application::ports::chunked_upload_ports::DEFAULT_CHUNK_SIZE;
use crate::application::ports::file_ports::FileUploadUseCase;
use crate::application::ports::folder_ports::FolderUseCase;
use crate::application::ports::storage_ports::StorageUsagePort;
use crate::common::di::AppState;
use crate::domain::services::authorization::{Permission, Subject};
use crate::interfaces::errors::AppError;
use crate::interfaces::middleware::auth::AuthUser;
use crate::interfaces::upload_ingest::{self, stream_body_to_path};

/// Request body for creating an upload session
#[derive(Debug, Deserialize, ToSchema)]
pub struct CreateUploadRequest {
    pub filename: String,
    pub folder_id: Option<String>,
    pub content_type: Option<String>,
    pub total_size: u64,
    pub chunk_size: Option<usize>,
}

/// Query params for chunk upload.
///
/// `checksumalg` is parsed via [`ChecksumAlg::parse`] and defaults to
/// `Md5` when absent — matching the legacy `Content-MD5` contract that
/// older clients rely on. Unknown algorithm names produce a 400 with the
/// offending value echoed back.
#[derive(Debug, Deserialize)]
pub struct ChunkUploadParams {
    pub chunk_index: usize,
    pub checksum: Option<String>,
    pub checksumalg: Option<String>,
}

/// Final response after completing upload
#[derive(Debug, Serialize, ToSchema)]
pub struct CompleteUploadResponse {
    pub file_id: String,
    pub filename: String,
    pub size: u64,
    pub path: String,
}

/// Optional body for `POST /api/uploads/{id}/complete`.
///
/// When the client supplies `checksum`, the server compares it against
/// the streamed content's hash BEFORE the file row is created — failure
/// releases the blob reference and returns 400, with the chunk parts
/// kept on disk for a retry. This is the end-to-end integrity check:
/// per-chunk MD5 proves each chunk arrived intact, but only the final
/// hash catches mis-ordered or corrupted assemblies.
///
/// **`blake3` is highly recommended** — it's the content-addressing
/// algorithm of the blob store itself, so verification is a string
/// comparison against the hash the store already computed, and the
/// value the client sends equals the `content_hash` they'd later read
/// back from `GET /api/files/{id}`. `md5` and `sha256` are accepted
/// for legacy client tooling; they are computed by an in-flight tee
/// during the same streaming pass — no extra disk read either way.
///
/// `Default` keeps the existing wire shape: clients that POST with no
/// body get today's behavior (no verification, server just returns
/// what it computed).
#[derive(Debug, Default, Deserialize, ToSchema)]
pub struct CompleteUploadRequest {
    /// Lowercase hex digest the client expects the streamed content to
    /// hash to. Compared case-insensitively. Omit to skip verification.
    pub checksum: Option<String>,
    /// Algorithm name. `blake3` is the recommended choice (default —
    /// matches the blob store's content-addressing algorithm). `md5`,
    /// `sha256` / `sha-256` are accepted and computed in-flight.
    /// Unknown values return 400.
    pub checksumalg: Option<String>,
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

    /// POST /api/uploads - Create a new upload session
    ///
    /// Request body:
    /// ```json
    /// {
    ///   "filename": "large-video.mp4",
    ///   "folder_id": "optional-folder-id",
    ///   "content_type": "video/mp4",
    ///   "total_size": 104857600,
    ///   "chunk_size": 5242880
    /// }
    /// ```
    ///
    /// Response:
    /// ```json
    /// {
    ///   "upload_id": "uuid",
    ///   "chunk_size": 5242880,
    ///   "total_chunks": 20,
    ///   "expires_at": 86400
    /// }
    /// ```
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

        // ── Whole-file cap ──────────────────────────────────────────
        // Reject upfront, before any chunk is uploaded — wasting
        // bandwidth + server disk on an upload that's going to be
        // rejected at /complete is the worst-of-both-worlds outcome.
        // `max_upload_size` is the same ceiling that bounds direct
        // PUTs (per-byte during streaming there; declared per-session
        // here). When quotas are disabled, this is the only whole-file
        // limit for chunked uploads — without it a hostile client
        // could declare `total_size: 1 TB` and accumulate chunks
        // until disk fills.
        let max_upload = state.core.config.storage.max_upload_size as u64;
        if request.total_size > max_upload {
            tracing::warn!(
                "⛔ CHUNKED UPLOAD REJECTED (total_size cap): user={}, file={}, declared={}, max={}",
                auth_user.username,
                request.filename,
                request.total_size,
                max_upload
            );
            return AppError::payload_too_large(format!(
                "Declared total_size {} exceeds the server's `max_upload_size` cap ({} bytes). \
                 Raise OXICLOUD_MAX_UPLOAD_SIZE on the server if larger uploads are expected.",
                request.total_size, max_upload
            ))
            .into_response();
        }

        // ── Permission pre-check: caller must have Create on the target
        // folder BEFORE we allocate a session and accept chunks. The
        // upload service re-checks at finalize via
        // `upload_file_streaming_with_perms` (AuthZ audit #17 fix,
        // 2026-07-16) so a grant revoked mid-session is caught. This
        // pre-check is the fail-fast: it avoids wasting client+server
        // resources on chunks that will be rejected anyway. `None`
        // means the write lands at drive-root — that path is currently
        // unchecked (session doesn't carry `drive_id`; tracked with the
        // folder-id-walking follow-up).
        if let Some(ref fid) = request.folder_id
            && let Err(err) = state
                .applications
                .folder_service_concrete
                .require_permission(&[Subject::User(auth_user.id)], Permission::Create, fid)
                .await
        {
            tracing::warn!(
                "⛔ CHUNKED UPLOAD REJECTED (no perm): user='{}' folder='{}' err='{}'",
                auth_user.username,
                fid,
                err
            );
            return AppError::from(err).into_response();
        }

        // ── Quota enforcement ────────────────────────────────────
        if let Some(storage_svc) = state.storage_usage_service.as_ref()
            && let Err(err) = storage_svc
                .check_storage_quota(auth_user.id, request.total_size)
                .await
        {
            tracing::warn!(
                "⛔ CHUNKED UPLOAD REJECTED (user quota): user={}, file={}, size={} — {}",
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

        // ── Per-drive quota (D4) ─────────────────────────────────
        // Native-chunked declares `total_size` at session creation,
        // so we can refuse here before any chunk is accepted — same
        // wasted-bandwidth optimisation the multipart path has via
        // the post-ingest check. No folder_id means root-level which
        // the folder-permission check above already rejects.
        if let Some(storage_svc) = state.storage_usage_service.as_ref()
            && let Some(fid_str) = request.folder_id.as_deref()
            && let Ok(fid) = uuid::Uuid::parse_str(fid_str)
            && let Err(err) = storage_svc
                .check_drive_quota_by_folder(fid, request.total_size)
                .await
        {
            tracing::warn!(
                "⛔ CHUNKED UPLOAD REJECTED (drive quota): user={}, folder={}, file={}, size={} — {}",
                auth_user.username,
                fid,
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

    // PATCH /api/uploads/:upload_id — moved entirely to the free
    // function `upload_chunk` below so the body can be streamed
    // (axum::body::Body) instead of materialised as `Bytes` here.
    // The port-level `ChunkedUploadPort::upload_chunk` (Bytes-based)
    // remains for tests and any future caller that genuinely has the
    // bytes already in memory.

    /// HEAD /api/uploads/:upload_id - Get upload status
    ///
    /// Returns upload progress and pending chunks
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

    /// POST /api/uploads/:upload_id/complete - Finalize upload
    ///
    /// Streams the uploaded chunk parts, in order, straight into the CDC
    /// chunk store and creates the file record — no assembled temp file is
    /// ever written. When `body.checksum` is supplied it is verified from
    /// the same streaming pass (BLAKE3 comes from the store itself;
    /// MD5/SHA-256 are computed by an in-flight tee) — mismatch returns 400
    /// with the blob reference released, and the chunk parts stay on disk
    /// so the client can re-issue complete after diagnosing.
    pub(super) async fn complete_upload_impl(
        State(state): State<Arc<AppState>>,
        auth_user: AuthUser,
        Path(upload_id): Path<String>,
        body: CompleteUploadRequest,
    ) -> impl IntoResponse {
        let chunked_service = &state.core.chunked_upload_service;
        let upload_service = &state.applications.file_upload_service;
        let dedup = &state.core.dedup_service;

        // ── Parse the optional algorithm BEFORE completion so a bad
        //    `checksumalg` doesn't waste any work on a request we'll
        //    reject anyway.
        let alg = match body.checksumalg.as_deref() {
            Some(name) => match ChecksumAlg::parse(name) {
                Some(a) => Some(a),
                None => {
                    return AppError::bad_request(format!(
                        "Unsupported checksumalg: {name} (supported: md5, sha256, blake3)"
                    ))
                    .into_response();
                }
            },
            None => None,
        };
        let expected_checksum = body.checksum.as_deref();

        // Validate completion and get the chunk parts in assembly order.
        let parts = match chunked_service
            .complete_upload(&upload_id, auth_user.id)
            .await
        {
            Ok(result) => result,
            Err(e) => {
                return AppError::from(e).into_response();
            }
        };

        // MD5/SHA-256 verification taps the stream while it is ingested;
        // BLAKE3 needs no tee — the store's own content hash IS BLAKE3.
        let alg = expected_checksum.map(|_| alg.unwrap_or(ChecksumAlg::Blake3));
        let tee = match alg {
            Some(ChecksumAlg::Md5) | Some(ChecksumAlg::Sha256) => {
                Some(upload_ingest::checksum_tee(alg.unwrap()))
            }
            _ => None,
        };

        // ── Stream the parts into the CDC chunk store ───────────────
        let ingested = match upload_ingest::ingest_stream_to_cas(
            upload_ingest::stream_from_files(parts.chunk_paths),
            dedup,
            &parts.filename,
            &parts.content_type,
            usize::MAX,
            tee.clone(),
        )
        .await
        {
            Ok(ingested) => ingested,
            Err(e) => return e.into_response(),
        };

        // ── End-to-end integrity verification ───────────────────────
        if let (Some(expected), Some(alg)) = (expected_checksum, alg) {
            let computed = match alg {
                ChecksumAlg::Blake3 => Some(ingested.hash.clone()),
                _ => tee.as_ref().and_then(upload_ingest::finalize_checksum_tee),
            };
            let Some(computed) = computed else {
                upload_ingest::discard_ingested(dedup, &ingested).await;
                return AppError::internal_error("Checksum tee produced no digest").into_response();
            };
            if !computed.eq_ignore_ascii_case(expected) {
                upload_ingest::discard_ingested(dedup, &ingested).await;
                tracing::warn!(
                    target: "audit",
                    event = "chunked_upload.checksum_mismatch",
                    reason = "final_checksum_mismatch",
                    upload_id = %upload_id,
                    user_id = %auth_user.id,
                    alg = alg.as_str(),
                    expected = %expected,
                    actual = %computed,
                    "👮🏻‍♂️ Chunked upload complete: client checksum mismatch — blob not promoted"
                );
                return AppError::bad_request(format!(
                    "Checksum mismatch ({}): expected {}, got {}",
                    alg.as_str(),
                    expected,
                    computed
                ))
                .into_response();
            }
        }

        // Register the file row against the ingested blob.
        //
        // AuthZ audit #17 (2026-07-12): swapped `upload_file_streaming` →
        // `upload_file_streaming_with_perms` so `Create` on the target
        // folder is re-verified at finalize. Session creation already
        // pre-checked (line ~198), but that was potentially hours or
        // days ago; app-passwords keep sessions valid indefinitely.
        // Without the finalize re-check, a grant revoked mid-session
        // stayed effective until the last chunk landed.
        let size = ingested.size;
        match upload_service
            .upload_file_streaming_with_perms(
                parts.filename.clone(),
                parts.folder_id.clone(),
                ingested.content_type.clone(),
                ingested.stored(),
                auth_user.id,
            )
            .await
        {
            Ok(file) => {
                // Cleanup session (removes the chunk part files)
                let _ = chunked_service
                    .finalize_upload(&upload_id, auth_user.id)
                    .await;

                tracing::info!(
                    "✅ CHUNKED UPLOAD COMPLETE: {} (ID: {}, {} bytes)",
                    parts.filename,
                    file.id,
                    size
                );

                (
                    StatusCode::CREATED,
                    Json(CompleteUploadResponse {
                        file_id: file.id,
                        filename: file.name,
                        size,
                        path: file.path,
                    }),
                )
                    .into_response()
            }
            Err(e) => {
                tracing::error!("Failed to create file from chunked upload: {:?}", e);
                // AuthZ audit #2 (2026-07-12) — route DomainError through
                // `AppError::from` so graduated denial from
                // `upload_file_streaming_with_perms` keeps the 403/404
                // shape instead of collapsing into a 500. Sibling
                // `cancel_upload_impl` at :514 already uses this pattern.
                AppError::from(e).into_response()
            }
        }
    }

    /// DELETE /api/uploads/:upload_id - Cancel upload
    ///
    /// Cancels an in-progress upload and cleans up temp files
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
            Err(e) => AppError::from(e).into_response(),
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

/// **Deprecated.** Prefer `/api/files/delta/*` — hash-first negotiation,
/// resumable, chunked. The `/api/uploads/*` family stays for backward
/// compatibility with existing clients but receives no new features.
#[utoipa::path(
    post,
    path = "/api/uploads",
    description = "**Deprecated.** Prefer the delta-upload surface at `/api/files/delta/*` \
(hash-first negotiation, resumable, chunked). The `/api/uploads/*` family is kept for \
backward compatibility with existing clients but is no longer receiving new features.",
    request_body(content = CreateUploadRequest, content_type = "application/json", description = "Upload session parameters"),
    responses(
        (status = 201, description = "Upload session created", body = crate::application::ports::chunked_upload_ports::CreateUploadResponseDto),
        (status = 400, description = "Invalid request (empty filename, zero size, chunk too small)"),
        (status = 507, description = "Storage quota exceeded"),
    ),
    tag = "uploads",
    security(("bearerAuth" = []))
)]
#[deprecated(note = "prefer /api/files/delta/*")]
pub async fn create_upload(
    state: State<Arc<AppState>>,
    auth_user: AuthUser,
    request: Json<CreateUploadRequest>,
) -> impl IntoResponse {
    ChunkedUploadHandler::create_upload_impl(state, auth_user, request).await
}

/// **Deprecated.** Prefer `/api/files/delta/*` — see `create_upload`.
#[utoipa::path(
    patch,
    path = "/api/uploads/{upload_id}",
    description = "**Deprecated.** See `POST /api/uploads` for the migration note.",
    params(
        ("upload_id" = String, Path, description = "Upload session ID"),
        ("chunk_index" = usize, Query, description = "Zero-based chunk index"),
        (
            "checksum" = Option<String>,
            Query,
            description = "Optional hex-encoded checksum for integrity verification. \
                Computed incrementally during the streaming write. \
                Algorithm is selected by `checksumalg` (default `md5`). \
                Also accepted via the legacy `Content-MD5` request header."
        ),
        (
            "checksumalg" = Option<String>,
            Query,
            description = "Algorithm used by `checksum`. One of: `md5` (default, legacy), `sha256` / `sha-256`, `blake3`. \
                Unknown values return 400."
        ),
    ),
    request_body(content_type = "application/octet-stream", description = "Raw chunk bytes"),
    responses(
        (status = 200, description = "Chunk received", body = crate::application::ports::chunked_upload_ports::ChunkUploadResponseDto),
        (status = 400, description = "Invalid chunk, size mismatch, checksum mismatch, or unknown `checksumalg`"),
        (status = 404, description = "Upload session not found"),
        (status = 413, description = "Chunk exceeds `storage.chunk_max_bytes` cap"),
    ),
    tag = "uploads",
    security(("bearerAuth" = []))
)]
#[deprecated(note = "prefer /api/files/delta/*")]
pub async fn upload_chunk(
    State(state): State<Arc<AppState>>,
    auth_user: AuthUser,
    Path(upload_id): Path<String>,
    Query(params): Query<ChunkUploadParams>,
    headers: HeaderMap,
    request: Request,
) -> impl IntoResponse {
    let chunked_service = &state.core.chunked_upload_service;
    let max_chunk = state.core.config.storage.chunk_max_bytes;

    // ── Resolve the client's checksum + algorithm ────────────────────
    // Wire shape: `?checksum=<hex>&checksumalg=<name>` (or `Content-MD5`
    // header for older clients). When `checksumalg` is omitted we
    // default to MD5, matching the legacy contract — switching the
    // default would silently break any client still relying on
    // `Content-MD5` semantics.
    let expected_checksum = params.checksum.clone().or_else(|| {
        headers
            .get("Content-MD5")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string())
    });
    let alg = match params.checksumalg.as_deref() {
        Some(name) => match ChecksumAlg::parse(name) {
            Some(a) => a,
            None => {
                return AppError::bad_request(format!(
                    "Unsupported checksumalg: {name} (supported: md5, sha256, blake3)"
                ))
                .into_response();
            }
        },
        None => ChecksumAlg::Md5,
    };
    // Only compute the hash when the client supplied an `expected_checksum`
    // to verify against — saves ~30 ms per chunk for clients that don't.
    let alg_to_compute = expected_checksum.as_ref().map(|_| alg);

    // ── Phase 1: prepare ─────────────────────────────────────────────
    // Validates session ownership + chunk index, returns the on-disk
    // path and the chunk's declared size. The handler streams the body
    // to that path; service finalises bookkeeping after the write.
    let (chunk_path, _expected_size) = match chunked_service
        .prepare_chunk(&upload_id, auth_user.id, params.chunk_index)
        .await
    {
        Ok(p) => p,
        Err(e) => return AppError::from(e).into_response(),
    };

    // ── Phase 2: stream the body straight to disk ────────────────────
    // Peak heap ~one HTTP frame (~64 KB) regardless of chunk size or
    // `chunk_max_bytes`. Optional incremental hashing happens here so
    // verification doesn't require reading the chunk file back.
    let streamed = match stream_body_to_path(
        request.into_body(),
        &chunk_path,
        max_chunk,
        alg_to_compute,
    )
    .await
    {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(
                error = ?e,
                upload_id = %upload_id,
                chunk_index = params.chunk_index,
                max_chunk,
                "Chunked upload PATCH rejected — streaming write failed (cap, transport, or IO)"
            );
            return e.into_response();
        }
    };

    // ── Phase 3: commit ──────────────────────────────────────────────
    // Size + checksum verification + session state update. Same RAM-only
    // DashMap shard ownership pattern as the legacy `upload_chunk_inner`
    // (held only for ~µs; bitmask persist done after release).
    let response = match chunked_service
        .commit_chunk(
            &upload_id,
            auth_user.id,
            params.chunk_index,
            streamed.bytes_written,
            streamed.checksum_hex,
            expected_checksum,
        )
        .await
    {
        Ok(r) => r,
        Err(e) => return AppError::from(e).into_response(),
    };

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

/// **Deprecated.** Prefer `/api/files/delta/*` — see `create_upload`.
#[utoipa::path(
    head,
    path = "/api/uploads/{upload_id}",
    description = "**Deprecated.** See `POST /api/uploads` for the migration note.",
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
#[deprecated(note = "prefer /api/files/delta/*")]
pub async fn get_upload_status(
    state: State<Arc<AppState>>,
    auth_user: AuthUser,
    path: Path<String>,
) -> impl IntoResponse {
    ChunkedUploadHandler::get_upload_status_impl(state, auth_user, path).await
}

/// **Deprecated.** Prefer `/api/files/delta/*` — see `create_upload`.
#[utoipa::path(
    post,
    path = "/api/uploads/{upload_id}/complete",
    description = "**Deprecated.** See `POST /api/uploads` for the migration note.",
    params(
        ("upload_id" = String, Path, description = "Upload session ID"),
    ),
    request_body(
        content = CompleteUploadRequest,
        content_type = "application/json",
        description = "Optional. End-to-end integrity verification of the assembled file. \
            **`blake3` is highly recommended** as the `checksumalg` value — the server already \
            computes BLAKE3 over the assembled file during hash-on-write assembly, so \
            verification is a string comparison with zero extra CPU/IO. \
            Picking `md5` or `sha256` is supported for legacy client tooling but triggers a \
            second full hash pass over the assembled file. \
            Clients that POST with no body (or with an empty JSON object) get today's \
            behavior: no verification, server returns the BLAKE3 it computed."
    ),
    responses(
        (status = 201, description = "File assembled and created", body = CompleteUploadResponse),
        (status = 400, description = "Unknown `checksumalg` or final-checksum mismatch"),
        (status = 404, description = "Upload session not found"),
        (status = 500, description = "Assembly, hashing, or file creation failed"),
    ),
    tag = "uploads",
    security(("bearerAuth" = []))
)]
#[deprecated(note = "prefer /api/files/delta/*")]
pub async fn complete_upload(
    state: State<Arc<AppState>>,
    auth_user: AuthUser,
    path: Path<String>,
    // Empty body → `None` → default `CompleteUploadRequest`, preserving the
    // pre-checksum wire shape. Clients that DO send a body get strict
    // parsing (a malformed JSON returns 400 via the Json extractor).
    body: Option<Json<CompleteUploadRequest>>,
) -> impl IntoResponse {
    let req = body.map(|Json(r)| r).unwrap_or_default();
    ChunkedUploadHandler::complete_upload_impl(state, auth_user, path, req).await
}

/// **Deprecated.** Prefer `/api/files/delta/*` — see `create_upload`.
#[utoipa::path(
    delete,
    path = "/api/uploads/{upload_id}",
    description = "**Deprecated.** See `POST /api/uploads` for the migration note.",
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
#[deprecated(note = "prefer /api/files/delta/*")]
pub async fn cancel_upload(
    state: State<Arc<AppState>>,
    auth_user: AuthUser,
    path: Path<String>,
) -> impl IntoResponse {
    ChunkedUploadHandler::cancel_upload_impl(state, auth_user, path).await
}
