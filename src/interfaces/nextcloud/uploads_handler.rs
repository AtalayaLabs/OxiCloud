use axum::{
    body::{self, Body},
    http::{Request, StatusCode, header},
    response::Response,
};
use std::sync::Arc;

use crate::application::ports::file_ports::{FileRetrievalUseCase, FileUploadUseCase};
use crate::common::di::AppState;
use crate::common::mime_detect::{filename_from_path, refine_content_type_from_file};
use crate::infrastructure::services::audio_metadata_service::AudioMetadataService;
use crate::interfaces::errors::AppError;
use crate::interfaces::middleware::auth::{AuthUser, CurrentUser};

/// Dispatch Nextcloud chunked upload WebDAV requests.
///
/// Routes:
///   MKCOL  /remote.php/dav/uploads/{user}/{upload_id}             → create session
///   PUT    /remote.php/dav/uploads/{user}/{upload_id}/{chunk}      → store chunk
///   MOVE   /remote.php/dav/uploads/{user}/{upload_id}/.file        → assemble
///   DELETE /remote.php/dav/uploads/{user}/{upload_id}              → abort
pub async fn handle_nc_uploads(
    state: Arc<AppState>,
    req: Request<Body>,
    user: AuthUser,
    upload_id: String,
    rest: String, // chunk name or ".file" or empty
) -> Result<Response<Body>, AppError> {
    let method = req.method().clone();
    match method.as_str() {
        "MKCOL" => handle_mkcol(state, &user, &upload_id).await,
        "PUT" => handle_put_chunk(state, req, &user, &upload_id, &rest).await,
        "MOVE" => handle_assemble(state, req, &user, &upload_id).await,
        "DELETE" => handle_abort(state, &user, &upload_id).await,
        _ => Ok(Response::builder()
            .status(StatusCode::METHOD_NOT_ALLOWED)
            .body(Body::empty())
            .unwrap()),
    }
}

/// MKCOL — create upload session directory.
async fn handle_mkcol(
    state: Arc<AppState>,
    user: &CurrentUser,
    upload_id: &str,
) -> Result<Response<Body>, AppError> {
    let nc = state
        .nextcloud
        .as_ref()
        .ok_or_else(|| AppError::internal_error("Nextcloud services unavailable"))?;

    nc.chunked_uploads
        .create_session(&user.username, upload_id)
        .await
        .map_err(|e| AppError::internal_error(format!("Failed to create session: {}", e)))?;

    Ok(Response::builder()
        .status(StatusCode::CREATED)
        .body(Body::empty())
        .unwrap())
}

/// PUT — store a chunk.
async fn handle_put_chunk(
    state: Arc<AppState>,
    req: Request<Body>,
    user: &CurrentUser,
    upload_id: &str,
    chunk_name: &str,
) -> Result<Response<Body>, AppError> {
    let nc = state
        .nextcloud
        .as_ref()
        .ok_or_else(|| AppError::internal_error("Nextcloud services unavailable"))?;

    let chunk_name = chunk_name.trim_matches('/');
    if chunk_name.is_empty() {
        return Err(AppError::bad_request("Missing chunk name"));
    }

    let max_upload = state.core.config.storage.max_upload_size;
    let body_bytes = body::to_bytes(req.into_body(), max_upload)
        .await
        .map_err(|e| AppError::bad_request(format!("Failed to read chunk body: {}", e)))?;

    nc.chunked_uploads
        .store_chunk(&user.username, upload_id, chunk_name, &body_bytes)
        .await
        .map_err(|e| AppError::internal_error(format!("Failed to store chunk: {}", e)))?;

    Ok(Response::builder()
        .status(StatusCode::CREATED)
        .body(Body::empty())
        .unwrap())
}

/// MOVE — assemble chunks into final file.
///
/// The Destination header contains the final file path in the DAV files namespace.
async fn handle_assemble(
    state: Arc<AppState>,
    req: Request<Body>,
    user: &CurrentUser,
    upload_id: &str,
) -> Result<Response<Body>, AppError> {
    let nc = state
        .nextcloud
        .as_ref()
        .ok_or_else(|| AppError::internal_error("Nextcloud services unavailable"))?;

    // Parse Destination header to determine final file path.
    let destination = req
        .headers()
        .get("destination")
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| AppError::bad_request("Missing Destination header"))?
        .to_string();

    let oc_mtime = req
        .headers()
        .get("x-oc-mtime")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<i64>().ok());

    let dest_subpath = extract_files_subpath(&destination, &user.username)
        .ok_or_else(|| AppError::bad_request("Invalid Destination URL"))?;

    // Assemble chunks into a temp file (no full-file buffering in RAM).
    let (temp_path, size) = nc
        .chunked_uploads
        .assemble(&user.username, upload_id)
        .await
        .map_err(|e| AppError::internal_error(format!("Failed to assemble chunks: {}", e)))?;

    // Write assembled file to storage via the upload service.
    let upload_service = &state.applications.file_upload_service;
    let file_service = &state.applications.file_retrieval_service;

    let internal_path = format!(
        "My Folder - {}/{}",
        user.username,
        dest_subpath.trim_matches('/')
    );

    // Detect content type via magic bytes + extension fallback.
    let filename = filename_from_path(&dest_subpath);
    let content_type =
        refine_content_type_from_file(&temp_path, filename, "application/octet-stream").await;

    // Check if file exists (update vs create).
    let existing = file_service.get_file_by_path(&internal_path).await;

    let etag: Option<String> = if existing.is_ok() {
        let dto = upload_service
            .update_file_streaming(
                &internal_path,
                &temp_path,
                size,
                &content_type,
                None,
                oc_mtime,
            )
            .await
            .map_err(|e| AppError::internal_error(format!("Failed to update file: {}", e)))?;

        state.core.refresh_thumbnails_after_update(
            dto.id.clone(),
            dto.etag.clone(),
            &content_type,
        ).await;

        Some(dto.etag)
    } else {
        // For new files we still need to read the temp file since create_file takes &[u8].
        let assembled = tokio::fs::read(&temp_path).await.map_err(|e| {
            AppError::internal_error(format!("Failed to read assembled file: {}", e))
        })?;

        let (parent_sub, filename) = match dest_subpath.rsplit_once('/') {
            Some((p, n)) => (p, n),
            None => ("", dest_subpath.as_str()),
        };
        let parent_internal = format!(
            "My Folder - {}/{}",
            user.username,
            parent_sub.trim_matches('/')
        );
        let parent_internal = parent_internal.trim_end_matches('/');

        let dto = upload_service
            .create_file(parent_internal, filename, &assembled, &content_type)
            .await
            .map_err(|e| AppError::internal_error(format!("Failed to create file: {}", e)))?;

        // Extract audio metadata for supported audio files in background.
        if let Some(ref audio_service) = state.applications.audio_metadata_service
            && AudioMetadataService::is_audio_file(&dto.mime_type)
            && let Ok(file_id) = uuid::Uuid::parse_str(&dto.id)
        {
            let file_path = state.core.dedup_service.blob_path(&dto.etag);
            AudioMetadataService::spawn_extraction_background(
                audio_service.clone(),
                file_id,
                file_path,
            );
        }

        Some(dto.etag)
    };

    // Clean up temp file (session cleanup below removes the directory anyway).
    let _ = tokio::fs::remove_file(&temp_path).await;

    // Cleanup session.
    let _ = nc.chunked_uploads.cleanup(&user.username, upload_id).await;

    if let Some(tag) = etag {
        return Ok(Response::builder()
            .status(StatusCode::CREATED)
            .header(header::ETAG, format!("\"{}\"", tag))
            .header("oc-etag", format!("\"{}\"", tag))
            .body(Body::empty())
            .unwrap());
    }

    Ok(Response::builder()
        .status(StatusCode::CREATED)
        .body(Body::empty())
        .unwrap())
}

/// DELETE — abort an upload session.
async fn handle_abort(
    state: Arc<AppState>,
    user: &CurrentUser,
    upload_id: &str,
) -> Result<Response<Body>, AppError> {
    let nc = state
        .nextcloud
        .as_ref()
        .ok_or_else(|| AppError::internal_error("Nextcloud services unavailable"))?;

    nc.chunked_uploads
        .cleanup(&user.username, upload_id)
        .await
        .map_err(|e| AppError::internal_error(format!("Failed to abort upload: {}", e)))?;

    Ok(Response::builder()
        .status(StatusCode::NO_CONTENT)
        .body(Body::empty())
        .unwrap())
}

/// Extract the file subpath from a Destination header pointing to the files DAV namespace.
///
/// For full URLs the host is ignored — only the path component is used.
fn extract_files_subpath(dest: &str, username: &str) -> Option<String> {
    let prefix = format!("/remote.php/dav/files/{}/", username);
    let path = if dest.starts_with("http://") || dest.starts_with("https://") {
        let after_scheme = dest.split_once("://")?.1;
        let path_start = after_scheme.find('/').unwrap_or(after_scheme.len());
        &after_scheme[path_start..]
    } else {
        dest
    };
    let decoded = urlencoding::decode(path).ok()?;
    let decoded = decoded.trim_end_matches('/');
    decoded
        .strip_prefix(prefix.trim_end_matches('/'))
        .map(|s| s.trim_start_matches('/').to_string())
}
