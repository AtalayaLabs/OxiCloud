use axum::{
    extract::{Json, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use crate::application::dtos::file_dto::FileDto;
use crate::application::dtos::folder_dto::FolderDto;
use crate::application::services::batch_operations::{
    BatchOperationService, BatchResult, BatchStats,
};
use crate::interfaces::api::deserializer;
use crate::interfaces::api::handlers::ApiResult;
use crate::interfaces::middleware::auth::AuthUser;

/// Maximum number of items allowed in a single batch request.
/// Prevents fan-out amplification attacks and database connection exhaustion.
const MAX_BATCH_SIZE: usize = 1_000;

/// Shared state for the batch handler
#[derive(Clone)]
pub struct BatchHandlerState {
    pub batch_service: Arc<BatchOperationService>,
}

/// DTO for batch file operation requests
#[derive(Debug, Deserialize)]
pub struct BatchFileOperationRequest {
    /// IDs of the files to process
    pub file_ids: Vec<String>,
    /// Target folder ID (optional)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_folder_id: Option<String>,
}

/// DTO for batch folder operation requests
#[derive(Debug, Deserialize)]
pub struct BatchFolderOperationRequest {
    /// IDs of the folders to process
    pub folder_ids: Vec<String>,
    /// Whether the operation should be recursive
    #[serde(default)]
    pub recursive: bool,
    /// Target folder ID (optional)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_folder_id: Option<String>,
}

/// DTO for batch folder creation requests
#[derive(Debug, Deserialize)]
pub struct BatchCreateFoldersRequest {
    /// Details of the folders to create
    pub folders: Vec<CreateFolderDetail>,
}

/// Detail for folder creation
#[derive(Debug, Deserialize)]
pub struct CreateFolderDetail {
    /// Folder name
    pub name: String,
    /// Parent folder ID (optional)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
}

/// DTO for batch operation results
#[derive(Debug, Serialize)]
pub struct BatchOperationResponse<T> {
    /// Successfully processed entities
    pub successful: Vec<T>,
    /// Failed operations with their error messages
    pub failed: Vec<FailedOperation>,
    /// Operation statistics
    pub stats: BatchOperationStats,
}

/// Failed operation in a batch
#[derive(Debug, Serialize)]
pub struct FailedOperation {
    /// Identifier of the entity that failed
    pub id: String,
    /// Error message
    pub error: String,
}

/// Statistics for a batch operation
#[derive(Debug, Serialize)]
pub struct BatchOperationStats {
    /// Total number of operations
    pub total: usize,
    /// Number of successful operations
    pub successful: usize,
    /// Number of failed operations
    pub failed: usize,
    /// Total execution time in milliseconds
    pub execution_time_ms: u128,
}

/// Converts domain BatchStats to DTO
impl From<BatchStats> for BatchOperationStats {
    fn from(stats: BatchStats) -> Self {
        Self {
            total: stats.total,
            successful: stats.successful,
            failed: stats.failed,
            execution_time_ms: stats.execution_time_ms,
        }
    }
}

/// Converts domain BatchResult<T> to DTO
impl<T, U> From<BatchResult<T>> for BatchOperationResponse<U>
where
    U: From<T>,
{
    fn from(result: BatchResult<T>) -> Self {
        let successful = result.successful.into_iter().map(U::from).collect();

        let failed = result
            .failed
            .into_iter()
            .map(|(id, error)| FailedOperation { id, error })
            .collect();

        Self {
            successful,
            failed,
            stats: result.stats.into(),
        }
    }
}

/// Handler for moving multiple files in batch
pub async fn move_files_batch(
    State(state): State<BatchHandlerState>,
    auth_user: AuthUser,
    Json(request): Json<BatchFileOperationRequest>,
) -> ApiResult<impl IntoResponse> {
    // Verify there are files to process
    if request.file_ids.is_empty() {
        return Ok((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": "No file IDs provided"
            })),
        )
            .into_response());
    }
    if request.file_ids.len() > MAX_BATCH_SIZE {
        return Ok((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": format!("Batch size {} exceeds maximum of {}", request.file_ids.len(), MAX_BATCH_SIZE)
            })),
        )
            .into_response());
    }

    // Execute batch operation
    let result = state
        .batch_service
        .move_files(request.file_ids, request.target_folder_id, auth_user.id)
        .await
        .map_err(|e| {
            tracing::error!("Batch move_files failed: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Batch operation failed".to_string(),
            )
        })?;

    // Convert result to DTO
    let response: BatchOperationResponse<FileDto> = result.into();

    // Determine status code based on results
    let status_code = if response.stats.failed > 0 {
        if response.stats.successful > 0 {
            StatusCode::PARTIAL_CONTENT // Some operations successful, others failed
        } else {
            StatusCode::BAD_REQUEST // All failed
        }
    } else {
        StatusCode::OK // All successful
    };

    Ok((status_code, Json(response)).into_response())
}

/// Handler for copying multiple files in batch
pub async fn copy_files_batch(
    State(state): State<BatchHandlerState>,
    auth_user: AuthUser,
    Json(request): Json<BatchFileOperationRequest>,
) -> ApiResult<impl IntoResponse> {
    // Verify there are files to process
    if request.file_ids.is_empty() {
        return Ok((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": "No file IDs provided"
            })),
        )
            .into_response());
    }
    if request.file_ids.len() > MAX_BATCH_SIZE {
        return Ok((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": format!("Batch size {} exceeds maximum of {}", request.file_ids.len(), MAX_BATCH_SIZE)
            })),
        )
            .into_response());
    }

    // Execute batch operation
    let result = state
        .batch_service
        .copy_files(request.file_ids, request.target_folder_id, auth_user.id)
        .await
        .map_err(|e| {
            tracing::error!("Batch copy_files failed: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Batch operation failed".to_string(),
            )
        })?;

    // Convert result to DTO
    let response: BatchOperationResponse<FileDto> = result.into();

    // Determine status code based on results
    let status_code = if response.stats.failed > 0 {
        if response.stats.successful > 0 {
            StatusCode::PARTIAL_CONTENT // Some operations successful, others failed
        } else {
            StatusCode::BAD_REQUEST // All failed
        }
    } else {
        StatusCode::OK // All successful
    };

    Ok((status_code, Json(response)).into_response())
}

/// Handler for deleting multiple files in batch
pub async fn delete_files_batch(
    State(state): State<BatchHandlerState>,
    auth_user: AuthUser,
    Json(request): Json<BatchFileOperationRequest>,
) -> ApiResult<impl IntoResponse> {
    // Verify there are files to process
    if request.file_ids.is_empty() {
        return Ok((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": "No file IDs provided"
            })),
        )
            .into_response());
    }
    if request.file_ids.len() > MAX_BATCH_SIZE {
        return Ok((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": format!("Batch size {} exceeds maximum of {}", request.file_ids.len(), MAX_BATCH_SIZE)
            })),
        )
            .into_response());
    }

    // Execute batch operation
    let result = state
        .batch_service
        .delete_files(request.file_ids, auth_user.id)
        .await
        .map_err(|e| {
            tracing::error!("Batch delete_files failed: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Batch operation failed".to_string(),
            )
        })?;

    // Create custom response for string IDs
    let response = BatchOperationResponse {
        successful: result.successful,
        failed: result
            .failed
            .into_iter()
            .map(|(id, error)| FailedOperation { id, error })
            .collect(),
        stats: result.stats.into(),
    };

    // Determine status code based on results
    let status_code = if response.stats.failed > 0 {
        if response.stats.successful > 0 {
            StatusCode::PARTIAL_CONTENT // Some operations successful, others failed
        } else {
            StatusCode::BAD_REQUEST // All failed
        }
    } else {
        StatusCode::OK // All successful
    };

    Ok((status_code, Json(response)).into_response())
}

/// Handler for deleting multiple folders in batch
pub async fn delete_folders_batch(
    State(state): State<BatchHandlerState>,
    auth_user: AuthUser,
    Json(request): Json<BatchFolderOperationRequest>,
) -> ApiResult<impl IntoResponse> {
    // Verify there are folders to process
    if request.folder_ids.is_empty() {
        return Ok((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": "No folder IDs provided"
            })),
        )
            .into_response());
    }
    if request.folder_ids.len() > MAX_BATCH_SIZE {
        return Ok((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": format!("Batch size {} exceeds maximum of {}", request.folder_ids.len(), MAX_BATCH_SIZE)
            })),
        )
            .into_response());
    }

    // Execute batch operation
    let result = state
        .batch_service
        .delete_folders(request.folder_ids, request.recursive, auth_user.id)
        .await
        .map_err(|e| {
            tracing::error!("Batch delete_folders failed: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Batch operation failed".to_string(),
            )
        })?;

    // Create custom response for string IDs
    let response = BatchOperationResponse {
        successful: result.successful,
        failed: result
            .failed
            .into_iter()
            .map(|(id, error)| FailedOperation { id, error })
            .collect(),
        stats: result.stats.into(),
    };

    // Determine status code based on results
    let status_code = if response.stats.failed > 0 {
        if response.stats.successful > 0 {
            StatusCode::PARTIAL_CONTENT // Some operations successful, others failed
        } else {
            StatusCode::BAD_REQUEST // All failed
        }
    } else {
        StatusCode::OK // All successful
    };

    Ok((status_code, Json(response)).into_response())
}

/// Handler for creating multiple folders in batch
pub async fn create_folders_batch(
    State(state): State<BatchHandlerState>,
    auth_user: AuthUser,
    Json(request): Json<BatchCreateFoldersRequest>,
) -> ApiResult<impl IntoResponse> {
    // Verify there are folders to process
    if request.folders.is_empty() {
        return Ok((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": "No folders provided"
            })),
        )
            .into_response());
    }
    if request.folders.len() > MAX_BATCH_SIZE {
        return Ok((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": format!("Batch size {} exceeds maximum of {}", request.folders.len(), MAX_BATCH_SIZE)
            })),
        )
            .into_response());
    }

    // Transform the format for the service
    let folders = request
        .folders
        .into_iter()
        .map(|detail| (detail.name, detail.parent_id))
        .collect();

    // Execute batch operation
    let result = state
        .batch_service
        .create_folders(folders, auth_user.id)
        .await
        .map_err(|e| {
            tracing::error!("Batch create_folders failed: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Batch operation failed".to_string(),
            )
        })?;

    // Convert result to DTO
    let response: BatchOperationResponse<FolderDto> = result.into();

    // Determine status code based on results
    let status_code = if response.stats.failed > 0 {
        if response.stats.successful > 0 {
            StatusCode::PARTIAL_CONTENT // Some operations successful, others failed
        } else {
            StatusCode::BAD_REQUEST // All failed
        }
    } else {
        StatusCode::CREATED // All successful
    };

    Ok((status_code, Json(response)).into_response())
}

/// Handler for getting multiple files in batch
pub async fn get_files_batch(
    State(state): State<BatchHandlerState>,
    auth_user: AuthUser,
    Json(request): Json<BatchFileOperationRequest>,
) -> ApiResult<impl IntoResponse> {
    // Verify there are files to process
    if request.file_ids.is_empty() {
        return Ok((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": "No file IDs provided"
            })),
        )
            .into_response());
    }
    if request.file_ids.len() > MAX_BATCH_SIZE {
        return Ok((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": format!("Batch size {} exceeds maximum of {}", request.file_ids.len(), MAX_BATCH_SIZE)
            })),
        )
            .into_response());
    }

    // Execute batch operation
    let result = state
        .batch_service
        .get_multiple_files(request.file_ids, auth_user.id)
        .await
        .map_err(|e| {
            tracing::error!("Batch get_files failed: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Batch operation failed".to_string(),
            )
        })?;

    // Convert result to DTO
    let response: BatchOperationResponse<FileDto> = result.into();

    // Determine status code based on results
    let status_code = if response.stats.failed > 0 {
        if response.stats.successful > 0 {
            StatusCode::PARTIAL_CONTENT // Some operations successful, others failed
        } else {
            StatusCode::BAD_REQUEST // All failed
        }
    } else {
        StatusCode::OK // All successful
    };

    Ok((status_code, Json(response)).into_response())
}

/// Handler for getting multiple folders in batch
pub async fn get_folders_batch(
    State(state): State<BatchHandlerState>,
    auth_user: AuthUser,
    Json(request): Json<BatchFolderOperationRequest>,
) -> ApiResult<impl IntoResponse> {
    // Verify there are folders to process
    if request.folder_ids.is_empty() {
        return Ok((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": "No folder IDs provided"
            })),
        )
            .into_response());
    }
    if request.folder_ids.len() > MAX_BATCH_SIZE {
        return Ok((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": format!("Batch size {} exceeds maximum of {}", request.folder_ids.len(), MAX_BATCH_SIZE)
            })),
        )
            .into_response());
    }

    // Execute batch operation
    let result = state
        .batch_service
        .get_multiple_folders(request.folder_ids, auth_user.id)
        .await
        .map_err(|e| {
            tracing::error!("Batch get_folders failed: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Batch operation failed".to_string(),
            )
        })?;

    // Convert result to DTO
    let response: BatchOperationResponse<FolderDto> = result.into();

    // Determine status code based on results
    let status_code = if response.stats.failed > 0 {
        if response.stats.successful > 0 {
            StatusCode::PARTIAL_CONTENT // Some operations successful, others failed
        } else {
            StatusCode::BAD_REQUEST // All failed
        }
    } else {
        StatusCode::OK // All successful
    };

    Ok((status_code, Json(response)).into_response())
}

/// DTO for batch trash operation requests
#[derive(Debug, Deserialize)]
pub struct BatchTrashRequest {
    /// IDs of the files to move to trash
    #[serde(default)]
    pub file_ids: Vec<String>,
    /// IDs of the folders to move to trash
    #[serde(default)]
    pub folder_ids: Vec<String>,
}

/// DTO for batch download requests
#[derive(Debug, Deserialize)]
pub struct BatchDownloadRequest {
    /// IDs of the files to include in the ZIP
    #[serde(default)]
    pub file_ids: Vec<String>,
    /// IDs of the folders to include in the ZIP
    #[serde(default)]
    pub folder_ids: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct BatchDownloadQuery {
    #[serde(default, deserialize_with = "deserializer::deserialize_csv")]
    pub file_ids: Vec<String>, // will deserialize query string "1,2,3" into Vec<String>
    #[serde(default, deserialize_with = "deserializer::deserialize_csv")]
    pub folder_ids: Vec<String>, // will deserialize query string "1,2,3" into Vec<String>
}

// convert BatchDownloadQuery into BatchDownloadRequest
impl From<BatchDownloadQuery> for BatchDownloadRequest {
    fn from(q: BatchDownloadQuery) -> Self {
        Self {
            file_ids: q.file_ids,
            folder_ids: q.folder_ids,
        }
    }
}

/// Handler for moving multiple files and folders to trash in batch
pub async fn trash_batch(
    State(state): State<BatchHandlerState>,
    auth_user: AuthUser,
    Json(request): Json<BatchTrashRequest>,
) -> ApiResult<impl IntoResponse> {
    if request.file_ids.is_empty() && request.folder_ids.is_empty() {
        return Ok((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": "No file or folder IDs provided"
            })),
        )
            .into_response());
    }
    let combined_size = request.file_ids.len() + request.folder_ids.len();
    if combined_size > MAX_BATCH_SIZE {
        return Ok((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": format!("Batch size {} exceeds maximum of {}", combined_size, MAX_BATCH_SIZE)
            })),
        )
            .into_response());
    }

    let mut all_successful: Vec<String> = Vec::new();
    let mut all_failed: Vec<FailedOperation> = Vec::new();
    let total = request.file_ids.len() + request.folder_ids.len();
    let start_time = std::time::Instant::now();

    // Trash files
    if !request.file_ids.is_empty() {
        match state
            .batch_service
            .trash_files(request.file_ids, auth_user.id)
            .await
        {
            Ok(result) => {
                all_successful.extend(result.successful);
                all_failed.extend(
                    result
                        .failed
                        .into_iter()
                        .map(|(id, error)| FailedOperation { id, error }),
                );
            }
            Err(e) => {
                tracing::error!("Batch trash_files failed: {}", e);
                return Ok((
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({ "error": "Batch trash operation failed" })),
                )
                    .into_response());
            }
        }
    }

    // Trash folders
    if !request.folder_ids.is_empty() {
        match state
            .batch_service
            .trash_folders(request.folder_ids, auth_user.id)
            .await
        {
            Ok(result) => {
                all_successful.extend(result.successful);
                all_failed.extend(
                    result
                        .failed
                        .into_iter()
                        .map(|(id, error)| FailedOperation { id, error }),
                );
            }
            Err(e) => {
                tracing::error!("Batch trash_folders failed: {}", e);
                return Ok((
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({ "error": "Batch trash operation failed" })),
                )
                    .into_response());
            }
        }
    }

    let successful_count = all_successful.len();
    let failed_count = all_failed.len();

    let response = BatchOperationResponse {
        successful: all_successful,
        failed: all_failed,
        stats: BatchOperationStats {
            total,
            successful: successful_count,
            failed: failed_count,
            execution_time_ms: start_time.elapsed().as_millis(),
        },
    };

    let status_code = if failed_count > 0 {
        if successful_count > 0 {
            StatusCode::PARTIAL_CONTENT
        } else {
            StatusCode::BAD_REQUEST
        }
    } else {
        StatusCode::OK
    };

    Ok((status_code, Json(response)).into_response())
}

/// Handler for moving multiple folders in batch
pub async fn move_folders_batch(
    State(state): State<BatchHandlerState>,
    auth_user: AuthUser,
    Json(request): Json<BatchFolderOperationRequest>,
) -> ApiResult<impl IntoResponse> {
    if request.folder_ids.is_empty() {
        return Ok((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": "No folder IDs provided"
            })),
        )
            .into_response());
    }
    if request.folder_ids.len() > MAX_BATCH_SIZE {
        return Ok((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": format!("Batch size {} exceeds maximum of {}", request.folder_ids.len(), MAX_BATCH_SIZE)
            })),
        )
            .into_response());
    }

    let result = state
        .batch_service
        .move_folders(request.folder_ids, request.target_folder_id, auth_user.id)
        .await
        .map_err(|e| {
            tracing::error!("Batch move_folders failed: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Batch operation failed".to_string(),
            )
        })?;

    let response: BatchOperationResponse<FolderDto> = result.into();

    let status_code = if response.stats.failed > 0 {
        if response.stats.successful > 0 {
            StatusCode::PARTIAL_CONTENT
        } else {
            StatusCode::BAD_REQUEST
        }
    } else {
        StatusCode::OK
    };

    Ok((status_code, Json(response)).into_response())
}

// Hander as a workarround for drag & drop (does not support POST requests)
pub async fn download_batch_querystring(
    State(state): State<BatchHandlerState>,
    auth_user: AuthUser,
    Query(params): Query<BatchDownloadQuery>,
) -> Result<Response, (StatusCode, String)> {
    process_download_batch(state, auth_user, params.into()).await
}
/// Handler for downloading multiple files and folders as a single ZIP.
///
/// The ZIP is written to a temporary file and streamed to the client,
/// so RAM usage is O(buffer_size) regardless of archive size.
pub async fn download_batch_post(
    State(state): State<BatchHandlerState>,
    auth_user: AuthUser,
    Json(request): Json<BatchDownloadRequest>,
) -> Result<Response, (StatusCode, String)> {
    process_download_batch(state, auth_user, request).await
}

async fn process_download_batch(
    state: BatchHandlerState,
    auth_user: AuthUser,
    request: BatchDownloadRequest,
) -> Result<Response, (StatusCode, String)> {
    if request.file_ids.is_empty() && request.folder_ids.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            "No file or folder IDs provided".to_string(),
        ));
    }
    let combined_size = request.file_ids.len() + request.folder_ids.len();
    if combined_size > MAX_BATCH_SIZE {
        return Err((
            StatusCode::BAD_REQUEST,
            format!(
                "Batch size {} exceeds maximum of {}",
                combined_size, MAX_BATCH_SIZE
            ),
        ));
    }

    let temp_file = state
        .batch_service
        .download_zip(request.file_ids, request.folder_ids, auth_user.id)
        .await
        .map_err(|e| {
            tracing::error!("Batch download ZIP failed: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Batch download failed".to_string(),
            )
        })?;

    // Read file size for Content-Length before splitting ownership
    let file_size = temp_file
        .as_file()
        .metadata()
        .map(|m| m.len())
        .map_err(|e| {
            tracing::error!("Failed to read temp file metadata: {}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to prepare download".to_string(),
            )
        })?;

    // Split into the already-open fd + auto-delete path
    let (std_file, temp_path) = temp_file.into_parts();
    let tokio_file = tokio::fs::File::from_std(std_file);

    // Stream to client — O(64 KB) RAM regardless of ZIP size
    let stream = tokio_util::io::ReaderStream::new(tokio_file);
    let body = axum::body::Body::from_stream(stream);

    let filename = format!("oxicloud-download-{}.zip", chrono::Utc::now().timestamp());

    let mut response = Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "application/zip")
        .header(
            "Content-Disposition",
            format!("attachment; filename=\"{}\"", filename),
        )
        .header("Content-Length", file_size)
        .body(body)
        .unwrap();

    // Keep TempPath alive in response extensions so the file is only
    // deleted AFTER the body stream finishes sending.
    response
        .extensions_mut()
        .insert(std::sync::Arc::new(temp_path));

    Ok(response)
}
