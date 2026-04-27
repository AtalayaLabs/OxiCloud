use axum::{
    Json,
    body::Body,
    extract::{Path, Query, State},
    http::{HeaderMap, Response, StatusCode, header},
    response::IntoResponse,
};
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use tokio_util::io::ReaderStream;

use crate::application::dtos::folder_dto::{
    CreateFolderDto, FolderDto, MoveFolderDto, RenameFolderDto,
};
use crate::application::dtos::folder_listing_dto::FolderListingDto;
use crate::application::dtos::pagination::PaginationRequestDto;
use crate::application::ports::file_ports::FileRetrievalUseCase;
use crate::application::ports::inbound::FolderUseCase;
use crate::application::ports::trash_ports::TrashUseCase;
use crate::application::services::folder_service::FolderService;
use crate::common::di::AppState as GlobalAppState;
use crate::interfaces::errors::AppError;
use crate::interfaces::middleware::auth::AuthUser;

type AppState = Arc<FolderService>;

/// Handler for folder-related API endpoints
pub struct FolderHandler;

impl FolderHandler {
    // ── Why no #[utoipa::path] here? ─────────────────────────────────────────────
    // utoipa 5.4.0's proc macro generates helper structs / impls inside its expansion.
    // Rust allows struct definitions at module scope but forbids them inside impl blocks,
    // so `#[utoipa::path]` fails on every method in this impl block regardless of HTTP
    // verb or annotation content. All route handlers are free functions below.
    // TODO: collapse after utoipa upgrade.

    /// Creates a new folder.
    pub(super) async fn create_folder_impl(
        State(service): State<AppState>,
        auth_user: AuthUser,
        Json(mut dto): Json<CreateFolderDto>,
    ) -> impl IntoResponse {
        // If no parent_id was supplied, resolve the user's home folder as
        // the default parent so the new folder is nested correctly.
        if dto.parent_id.is_none() {
            tracing::info!(
                "create_folder: parent_id is None for user '{}', resolving home folder",
                auth_user.username
            );
            match service.list_folders_for_owner(None, auth_user.id).await {
                Ok(folders) => {
                    if let Some(home) = folders.first() {
                        tracing::info!(
                            "create_folder: resolved home folder ID '{}' for user '{}'",
                            home.id,
                            auth_user.username
                        );
                        dto.parent_id = Some(home.id.clone());
                    } else {
                        tracing::warn!(
                            "create_folder: home folder not found for user '{}', folder will be created at root",
                            auth_user.username
                        );
                    }
                }
                Err(e) => {
                    tracing::error!(
                        "create_folder: failed to list folders for home resolution: {}",
                        e
                    );
                }
            }
        }

        // ── SECURITY: Verify parent folder ownership (IDOR V-04 fix) ──
        if let Some(ref parent_id) = dto.parent_id {
            use crate::application::ports::inbound::FolderUseCase;
            if service
                .get_folder_owned(parent_id, auth_user.id)
                .await
                .is_err()
            {
                tracing::warn!(
                    "create_folder: user '{}' attempted to create folder in parent '{}' owned by another user",
                    auth_user.username,
                    parent_id,
                );
                return AppError::not_found(format!("Parent folder not found: {}", parent_id))
                    .into_response();
            }
        }

        match service.create_folder(dto).await {
            Ok(folder) => (StatusCode::CREATED, Json(folder)).into_response(),
            Err(err) => AppError::from(err).into_response(),
        }
    }

    /// Gets a folder by ID.
    pub(super) async fn get_folder_impl(
        State(service): State<AppState>,
        auth_user: AuthUser,
        Path(id): Path<String>,
    ) -> impl IntoResponse {
        match service.get_folder(&id).await {
            Ok(folder) => {
                // Access check: folder must belong to the requesting user
                if let Some(ref owner) = folder.owner_id
                    && owner != &auth_user.id.to_string()
                {
                    tracing::warn!(
                        "get_folder: user '{}' attempted to access folder '{}' owned by '{}'",
                        auth_user.id,
                        id,
                        owner
                    );
                    return AppError::not_found("Folder not found").into_response();
                }
                (StatusCode::OK, Json(folder)).into_response()
            }
            Err(err) => AppError::from(err).into_response(),
        }
    }

    /// Lists root folders for the authenticated user.
    pub(super) async fn list_root_folders_impl(
        State(service): State<AppState>,
        auth_user: AuthUser,
    ) -> axum::response::Response {
        Self::list_folders_scoped(service, None, &auth_user).await
    }

    /// Lists sub-folders inside a specific folder.
    pub(super) async fn list_folder_contents_impl(
        State(service): State<AppState>,
        auth_user: AuthUser,
        Path(id): Path<String>,
    ) -> axum::response::Response {
        Self::list_folders_scoped(service, Some(&id), &auth_user).await
    }

    /// Lists root folders with pagination.
    pub(super) async fn list_root_folders_paginated_impl(
        State(service): State<AppState>,
        auth_user: AuthUser,
        _pagination: Query<PaginationRequestDto>,
    ) -> axum::response::Response {
        Self::list_folders_scoped(service, None, &auth_user).await
    }

    /// Lists sub-folders inside a folder with pagination.
    pub(super) async fn list_folder_contents_paginated_impl(
        State(service): State<AppState>,
        auth_user: AuthUser,
        Path(id): Path<String>,
        pagination: Query<PaginationRequestDto>,
    ) -> axum::response::Response {
        match service
            .list_folders_for_owner_paginated(Some(&id), auth_user.id, &pagination)
            .await
        {
            Ok(paginated_result) => (StatusCode::OK, Json(paginated_result)).into_response(),
            Err(err) => AppError::from(err).into_response(),
        }
    }

    /// Internal helper: lists folders scoped to the authenticated user.
    /// Uses `list_folders_for_owner` — the DB query filters by `user_id`,
    /// so no data from other users ever leaves the database.
    async fn list_folders_scoped(
        service: AppState,
        parent_id: Option<&str>,
        auth_user: &AuthUser,
    ) -> axum::response::Response {
        match service
            .list_folders_for_owner(parent_id, auth_user.id)
            .await
        {
            Ok(folders) => (StatusCode::OK, Json(folders)).into_response(),
            Err(err) => AppError::from(err).into_response(),
        }
    }

    /// Compute a lightweight ETag from the maximum `modified_at` timestamp
    /// and item count. No body buffering required.
    fn compute_listing_etag(
        folders: &[crate::application::dtos::folder_dto::FolderDto],
        files: &[crate::application::dtos::file_dto::FileDto],
    ) -> String {
        let max_mod = folders
            .iter()
            .map(|f| f.modified_at)
            .chain(files.iter().map(|f| f.modified_at))
            .max()
            .unwrap_or(0);
        let count = folders.len() + files.len();
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        max_mod.hash(&mut hasher);
        count.hash(&mut hasher);
        format!("\"{:x}\"", hasher.finish())
    }

    /// Returns sub-folders and files for a folder in one request (ETag-cached).
    pub(super) async fn list_folder_listing_impl(
        State(state): State<Arc<GlobalAppState>>,
        auth_user: AuthUser,
        headers: HeaderMap,
        Path(id): Path<String>,
    ) -> axum::response::Response {
        let folder_service = &state.applications.folder_service;
        let file_service = &state.applications.file_retrieval_service;

        // Run both queries concurrently — no sequential wait.
        let (folders_result, files_result) = tokio::join!(
            folder_service.list_folders_for_owner(Some(&id), auth_user.id),
            file_service.list_files_owned(Some(&id), auth_user.id)
        );

        match (folders_result, files_result) {
            (Ok(folders), Ok(files)) => {
                let etag = Self::compute_listing_etag(&folders, &files);

                // 304 Not Modified if the client already has this version
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

                let listing = FolderListingDto { folders, files };
                let mut resp = (StatusCode::OK, Json(listing)).into_response();
                resp.headers_mut()
                    .insert(header::ETAG, header::HeaderValue::from_str(&etag).unwrap());
                resp
            }
            (Err(err), _) | (_, Err(err)) => AppError::from(err).into_response(),
        }
    }

    /// Renames a folder (ownership enforced).
    pub(super) async fn rename_folder_impl(
        State(service): State<AppState>,
        auth_user: AuthUser,
        Path(id): Path<String>,
        Json(dto): Json<RenameFolderDto>,
    ) -> impl IntoResponse {
        match service.rename_folder(&id, dto, auth_user.id).await {
            Ok(folder) => (StatusCode::OK, Json(folder)).into_response(),
            Err(err) => AppError::from(err).into_response(),
        }
    }

    /// Moves a folder to a new parent (ownership enforced).
    pub(super) async fn move_folder_impl(
        State(service): State<AppState>,
        auth_user: AuthUser,
        Path(id): Path<String>,
        Json(dto): Json<MoveFolderDto>,
    ) -> impl IntoResponse {
        match service.move_folder(&id, dto, auth_user.id).await {
            Ok(folder) => (StatusCode::OK, Json(folder)).into_response(),
            Err(err) => AppError::from(err).into_response(),
        }
    }

    /// Deletes a folder (ownership enforced by service layer)
    pub async fn delete_folder(
        State(service): State<AppState>,
        auth_user: AuthUser,
        Path(id): Path<String>,
    ) -> impl IntoResponse {
        match service.delete_folder(&id, auth_user.id).await {
            Ok(_) => StatusCode::NO_CONTENT.into_response(),
            Err(err) => AppError::from(err).into_response(),
        }
    }

    /// Deletes a folder (moves to trash if enabled, otherwise permanent).
    pub(super) async fn delete_folder_with_trash_impl(
        State(state): State<Arc<GlobalAppState>>,
        auth_user: AuthUser,
        Path(id): Path<String>,
    ) -> impl IntoResponse {
        let user_id = auth_user.id;
        // Check if trash service is available
        if let Some(trash_service) = &state.trash_service {
            tracing::info!("Moving folder to trash: {}", id);

            // Try to move to trash first
            match trash_service.move_to_trash(&id, "folder", user_id).await {
                Ok(_) => {
                    tracing::info!("Folder successfully moved to trash: {}", id);
                    return StatusCode::NO_CONTENT.into_response();
                }
                Err(err) => {
                    tracing::warn!(
                        "Could not move folder to trash, falling back to permanent delete: {}",
                        err
                    );
                    // Fall through to regular delete if trash fails
                }
            }
        }

        // Fallback to permanent delete if trash is unavailable or failed
        let folder_service = &state.applications.folder_service;
        match folder_service.delete_folder(&id, user_id).await {
            Ok(_) => {
                tracing::info!("Folder permanently deleted: {}", id);
                StatusCode::NO_CONTENT.into_response()
            }
            Err(err) => AppError::from(err).into_response(),
        }
    }

    /// Downloads a folder and all its contents as a ZIP archive.
    pub(super) async fn download_folder_zip_impl(
        State(state): State<Arc<GlobalAppState>>,
        auth_user: AuthUser,
        Path(id): Path<String>,
        Query(_params): Query<HashMap<String, String>>,
    ) -> impl IntoResponse {
        tracing::info!("Downloading folder as ZIP: {}", id);

        // Get folder information and verify ownership
        let folder_service = &state.applications.folder_service;

        match folder_service.get_folder(&id).await {
            Ok(folder) => {
                // Access check: folder must belong to the requesting user
                if folder.owner_id.as_deref() != Some(&auth_user.id.to_string()) {
                    tracing::warn!(
                        "download_folder_zip: user '{}' attempted to download folder '{}' owned by '{:?}'",
                        auth_user.id,
                        id,
                        folder.owner_id
                    );
                    return (
                        StatusCode::NOT_FOUND,
                        Json(serde_json::json!({ "error": "Folder not found" })),
                    )
                        .into_response();
                }
                tracing::info!("Preparing ZIP for folder: {} ({})", folder.name, id);

                // Use ZIP service from DI container
                let zip_service = match &state.core.zip_service {
                    Some(svc) => svc,
                    None => {
                        tracing::error!("ZipService not initialized");
                        return (
                            StatusCode::INTERNAL_SERVER_ERROR,
                            Json(serde_json::json!({ "error": "ZipService not initialized" })),
                        )
                            .into_response();
                    }
                };

                // Create the ZIP archive (written to a temp file, O(1) RAM)
                match zip_service.create_folder_zip(&id, &folder.name).await {
                    Ok(temp_file) => {
                        // Get the file size for Content-Length
                        let file_size = match temp_file.as_file().metadata() {
                            Ok(m) => m.len(),
                            Err(e) => {
                                tracing::error!("Error reading temp file metadata: {}", e);
                                return (
                                    StatusCode::INTERNAL_SERVER_ERROR,
                                    Json(serde_json::json!({
                                        "error": "Error creating ZIP file"
                                    })),
                                )
                                    .into_response();
                            }
                        };

                        tracing::info!("ZIP file created successfully, size: {} bytes", file_size);

                        // Split the NamedTempFile into the already-open std File
                        // and the TempPath (auto-deletes on drop).  This reuses
                        // the existing fd instead of opening a second one.
                        let (std_file, temp_path) = temp_file.into_parts();
                        let tokio_file = tokio::fs::File::from_std(std_file);

                        // Stream the file to the client in chunks
                        let stream = ReaderStream::new(tokio_file);
                        let body = axum::body::Body::from_stream(stream);

                        // Setup headers for download
                        let filename = format!("{}.zip", folder.name);
                        let content_disposition = format!("attachment; filename=\"{}\"", filename);

                        let mut response = Response::builder()
                            .status(StatusCode::OK)
                            .header(header::CONTENT_TYPE, "application/zip")
                            .header(header::CONTENT_DISPOSITION, content_disposition)
                            .header(header::CONTENT_LENGTH, file_size)
                            .body(body)
                            .unwrap();

                        // Keep TempPath alive in the response extensions so the
                        // file is only deleted AFTER the body stream finishes.
                        response.extensions_mut().insert(Arc::new(temp_path));

                        response.into_response()
                    }
                    Err(err) => {
                        tracing::error!("Error creating ZIP file: {}", err);
                        AppError::internal_error(format!("Error creating ZIP file: {}", err))
                            .into_response()
                    }
                }
            }
            Err(err) => {
                tracing::error!("Folder not found: {}", err);
                AppError::from(err).into_response()
            }
        }
    }
}

// ── Route handlers (free functions) ──────────────────────────────────────────
//
// All annotated route functions live here rather than as methods on FolderHandler
// because utoipa 5.4.0's #[utoipa::path] macro generates helper structs inside
// its expansion. Rust allows struct definitions at module scope but forbids them
// inside impl blocks — so every #[utoipa::path] annotation on a FolderHandler
// method fails to compile regardless of HTTP verb or annotation content.
//
// All logic lives in the FolderHandler::*_impl methods above; these thin wrappers
// exist solely to carry the OpenAPI annotation at a scope where utoipa can
// generate its helper types.
//
// routes.rs calls these free functions directly.
// TODO: collapse back into the impl block after a utoipa upgrade resolves the issue.

#[utoipa::path(
    post,
    path = "/api/folders",
    request_body(content = CreateFolderDto, content_type = "application/json", description = "Folder creation payload"),
    responses(
        (status = 201, description = "Folder created", body = FolderDto),
        (status = 400, description = "Invalid request"),
    ),
    tag = "folders"
)]
pub async fn create_folder(
    state: State<AppState>,
    auth_user: AuthUser,
    json: Json<CreateFolderDto>,
) -> impl IntoResponse {
    FolderHandler::create_folder_impl(state, auth_user, json).await
}

#[utoipa::path(
    get,
    path = "/api/folders/{id}",
    params(("id" = String, Path, description = "Folder ID")),
    responses(
        (status = 200, description = "Folder", body = FolderDto),
        (status = 404, description = "Folder not found"),
    ),
    tag = "folders"
)]
pub async fn get_folder(
    state: State<AppState>,
    auth_user: AuthUser,
    path: Path<String>,
) -> impl IntoResponse {
    FolderHandler::get_folder_impl(state, auth_user, path).await
}

#[utoipa::path(
    get,
    path = "/api/folders",
    responses(
        (status = 200, description = "List of root folders", body = Vec<FolderDto>),
    ),
    tag = "folders"
)]
pub async fn list_root_folders(
    state: State<AppState>,
    auth_user: AuthUser,
) -> axum::response::Response {
    FolderHandler::list_root_folders_impl(state, auth_user).await
}

#[utoipa::path(
    get,
    path = "/api/folders/{id}/contents",
    params(("id" = String, Path, description = "Folder ID")),
    responses(
        (status = 200, description = "List of sub-folders", body = Vec<FolderDto>),
        (status = 404, description = "Folder not found"),
    ),
    tag = "folders"
)]
pub async fn list_folder_contents(
    state: State<AppState>,
    auth_user: AuthUser,
    path: Path<String>,
) -> axum::response::Response {
    FolderHandler::list_folder_contents_impl(state, auth_user, path).await
}

#[utoipa::path(
    get,
    path = "/api/folders/paginated",
    params(PaginationRequestDto),
    responses(
        (status = 200, description = "Paginated list of root folders"),
    ),
    tag = "folders"
)]
pub async fn list_root_folders_paginated(
    state: State<AppState>,
    auth_user: AuthUser,
    pagination: Query<PaginationRequestDto>,
) -> axum::response::Response {
    FolderHandler::list_root_folders_paginated_impl(state, auth_user, pagination).await
}

#[utoipa::path(
    get,
    path = "/api/folders/{id}/contents/paginated",
    params(
        ("id" = String, Path, description = "Folder ID"),
        PaginationRequestDto,
    ),
    responses(
        (status = 200, description = "Paginated list of sub-folders"),
        (status = 404, description = "Folder not found"),
    ),
    tag = "folders"
)]
pub async fn list_folder_contents_paginated(
    state: State<AppState>,
    auth_user: AuthUser,
    path: Path<String>,
    pagination: Query<PaginationRequestDto>,
) -> axum::response::Response {
    FolderHandler::list_folder_contents_paginated_impl(state, auth_user, path, pagination).await
}

#[utoipa::path(
    get,
    path = "/api/folders/{id}/listing",
    params(("id" = String, Path, description = "Folder ID")),
    responses(
        (status = 200, description = "Folder listing (sub-folders + files)", body = FolderListingDto),
        (status = 304, description = "Not modified"),
        (status = 404, description = "Folder not found"),
    ),
    tag = "folders"
)]
pub async fn list_folder_listing(
    state: State<Arc<GlobalAppState>>,
    auth_user: AuthUser,
    headers: HeaderMap,
    path: Path<String>,
) -> axum::response::Response {
    FolderHandler::list_folder_listing_impl(state, auth_user, headers, path).await
}

#[utoipa::path(
    put,
    path = "/api/folders/{id}/rename",
    params(("id" = String, Path, description = "Folder ID")),
    request_body(content = RenameFolderDto, content_type = "application/json", description = "Rename payload"),
    responses(
        (status = 200, description = "Renamed folder", body = FolderDto),
        (status = 404, description = "Folder not found"),
    ),
    tag = "folders"
)]
pub async fn rename_folder(
    state: State<AppState>,
    auth_user: AuthUser,
    path: Path<String>,
    json: Json<RenameFolderDto>,
) -> impl IntoResponse {
    FolderHandler::rename_folder_impl(state, auth_user, path, json).await
}

#[utoipa::path(
    put,
    path = "/api/folders/{id}/move",
    params(("id" = String, Path, description = "Folder ID")),
    request_body(content = MoveFolderDto, content_type = "application/json", description = "Move payload"),
    responses(
        (status = 200, description = "Moved folder", body = FolderDto),
        (status = 404, description = "Folder or destination not found"),
    ),
    tag = "folders"
)]
pub async fn move_folder(
    state: State<AppState>,
    auth_user: AuthUser,
    path: Path<String>,
    json: Json<MoveFolderDto>,
) -> impl IntoResponse {
    FolderHandler::move_folder_impl(state, auth_user, path, json).await
}

#[utoipa::path(
    delete,
    path = "/api/folders/{id}",
    params(("id" = String, Path, description = "Folder ID")),
    responses(
        (status = 204, description = "Folder deleted"),
        (status = 404, description = "Folder not found"),
    ),
    tag = "folders"
)]
pub async fn delete_folder_with_trash(
    state: State<Arc<GlobalAppState>>,
    auth_user: AuthUser,
    path: Path<String>,
) -> impl IntoResponse {
    FolderHandler::delete_folder_with_trash_impl(state, auth_user, path).await
}

#[utoipa::path(
    get,
    path = "/api/folders/{id}/download",
    params(("id" = String, Path, description = "Folder ID")),
    responses(
        (status = 200, description = "ZIP archive stream (application/zip)"),
        (status = 404, description = "Folder not found"),
        (status = 501, description = "ZIP service not available"),
    ),
    tag = "folders"
)]
pub async fn download_folder_zip(
    state: State<Arc<GlobalAppState>>,
    auth_user: AuthUser,
    path: Path<String>,
    query: Query<HashMap<String, String>>,
) -> impl IntoResponse {
    FolderHandler::download_folder_zip_impl(state, auth_user, path, query).await
}
