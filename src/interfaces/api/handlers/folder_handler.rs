use axum::{
    Json,
    extract::{Path, Query, State},
    http::{Response, StatusCode, header},
    response::IntoResponse,
};
use std::sync::Arc;

use crate::application::dtos::display_helpers::{
    category_for, classify_display, format_file_size, icon_class_for, icon_special_class_for,
    intern_display, intern_mime,
};
use crate::application::dtos::file_dto::FileDto;
use crate::application::dtos::folder_dto::{
    CreateFolderDto, FolderAncestorsDto, FolderDto, FolderResourceItemDto, FolderResourcesDto,
    FolderResourcesQuery, ListResourcesOptions, MoveFolderDto, RenameFolderDto,
};
use crate::application::dtos::grant_dto::{ResourceContentDto, ResourceTypeDto};
use crate::application::ports::external_mount_ports::MountEntry;
use crate::application::ports::folder_ports::FolderUseCase;
use crate::application::ports::trash_ports::TrashUseCase;
use crate::application::services::external_mount_router::ResolvedId;
use crate::application::services::folder_service::FolderService;
use crate::application::services::mount_registry::MountConfig;
use crate::common::di::AppState as GlobalAppState;
use crate::domain::entities::file::File;
use crate::domain::services::external_mount_id::{
    NodeId, encode_child_id, virtual_file_etag, virtual_folder_etag,
};
use crate::interfaces::errors::AppError;
use crate::interfaces::middleware::auth::AuthUser;

type AppState = Arc<FolderService>;

use crate::interfaces::api::handlers::caller_flags::enrich_folder_flags;

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
    /// When parent_id is not provided, the folder is created inside the
    /// authenticated user's home folder rather than at the storage root.
    pub(super) async fn create_folder_impl(
        State(state): State<Arc<GlobalAppState>>,
        auth_user: AuthUser,
        Json(mut dto): Json<CreateFolderDto>,
    ) -> impl IntoResponse {
        let service = &state.applications.folder_service_concrete;
        // If no parent_id was supplied, resolve the user's home folder as
        // the default parent so the new folder is nested correctly.
        if dto.parent_id.is_none() {
            tracing::info!(
                "create_folder: parent_id is None for user '{}', resolving home folder",
                auth_user.username
            );
            match service.list_folders_with_perms(None, auth_user.id).await {
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

        match service.create_folder_with_perms(dto, auth_user.id).await {
            Ok(mut folder) => {
                enrich_folder_flags(&state, &mut folder, auth_user.id).await;
                (StatusCode::CREATED, Json(folder)).into_response()
            }
            Err(err) => AppError::from(err).into_response(),
        }
    }

    /// Gets a folder by ID.
    /// Validates that the authenticated user owns the folder.
    pub(super) async fn get_folder_impl(
        State(state): State<Arc<GlobalAppState>>,
        auth_user: AuthUser,
        Path(id): Path<String>,
    ) -> impl IntoResponse {
        let service = &state.applications.folder_service_concrete;
        match service.get_folder_with_perms(&id, auth_user.id).await {
            Ok(mut folder) => {
                enrich_folder_flags(&state, &mut folder, auth_user.id).await;
                (StatusCode::OK, Json(folder)).into_response()
            }
            Err(err) => AppError::from(err).into_response(),
        }
    }

    /// `GET /api/folders/{id}/ancestors` — parent-chain + access-source
    /// for the shared breadcrumb component. See `FolderAncestorsDto`
    /// for the response shape. Anti-enum via `NotFound` on Read denial.
    pub(super) async fn get_folder_ancestors_impl(
        State(state): State<Arc<GlobalAppState>>,
        auth_user: AuthUser,
        Path(id): Path<String>,
    ) -> impl IntoResponse {
        let service = &state.applications.folder_service_concrete;
        match service.get_ancestors_with_perms(&id, auth_user.id).await {
            Ok(dto) => (StatusCode::OK, Json(dto)).into_response(),
            Err(err) => AppError::from(err).into_response(),
        }
    }

    /// Lists root folders for the authenticated user.
    /// Only returns folders owned by this user — no information disclosure.
    pub(super) async fn list_root_folders_impl(
        State(service): State<AppState>,
        auth_user: AuthUser,
    ) -> axum::response::Response {
        Self::list_folders_scoped(service, None, &auth_user).await
    }

    /// Internal helper: lists folders the authenticated caller can Read.
    /// Post-PR-B, `list_root_folders_for_caller` scopes via
    /// drive-membership grants (`role_grants` + group cascade via
    /// `storage.caller_group_ids`) instead of the legacy `folders.user_id`
    /// filter, so folders in shared drives the caller belongs to
    /// surface here too.
    async fn list_folders_scoped(
        service: AppState,
        parent_id: Option<&str>,
        auth_user: &AuthUser,
    ) -> axum::response::Response {
        match service
            .list_folders_with_perms(parent_id, auth_user.id)
            .await
        {
            Ok(folders) => (StatusCode::OK, Json(folders)).into_response(),
            Err(err) => AppError::from(err).into_response(),
        }
    }

    /// Renames a folder (ownership enforced).
    pub(super) async fn rename_folder_impl(
        State(state): State<Arc<GlobalAppState>>,
        auth_user: AuthUser,
        Path(id): Path<String>,
        Json(dto): Json<RenameFolderDto>,
    ) -> impl IntoResponse {
        let service = &state.applications.folder_service_concrete;
        match service
            .rename_folder_with_perms(&id, dto, auth_user.id)
            .await
        {
            Ok(mut folder) => {
                enrich_folder_flags(&state, &mut folder, auth_user.id).await;
                (StatusCode::OK, Json(folder)).into_response()
            }
            Err(err) => AppError::from(err).into_response(),
        }
    }

    /// Moves a folder to a new parent (ownership enforced).
    pub(super) async fn move_folder_impl(
        State(state): State<Arc<GlobalAppState>>,
        auth_user: AuthUser,
        Path(id): Path<String>,
        Json(dto): Json<MoveFolderDto>,
    ) -> impl IntoResponse {
        let service = &state.applications.folder_service_concrete;
        match service.move_folder_with_perms(&id, dto, auth_user.id).await {
            Ok(mut folder) => {
                enrich_folder_flags(&state, &mut folder, auth_user.id).await;
                (StatusCode::OK, Json(folder)).into_response()
            }
            Err(err) => AppError::from(err).into_response(),
        }
    }

    /// Deletes a folder (ownership enforced by service layer)
    pub async fn delete_folder(
        State(service): State<AppState>,
        auth_user: AuthUser,
        Path(id): Path<String>,
    ) -> impl IntoResponse {
        match service.delete_folder_with_perms(&id, auth_user.id).await {
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

        // External mounts have no trash — a permanent provider delete is the
        // only option. Route `ext:` ids straight to the mount-aware service
        // delete, skipping the (always-failing) trash attempt.
        if state.mount_router.is_mount_id(&id) {
            return match state
                .applications
                .folder_service
                .delete_folder_with_perms(&id, user_id)
                .await
            {
                Ok(_) => StatusCode::NO_CONTENT.into_response(),
                Err(err) => AppError::from(err).into_response(),
            };
        }

        // Check if trash service is available
        // FIXME: permissions !!
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
        match folder_service.delete_folder_with_perms(&id, user_id).await {
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
    ) -> impl IntoResponse {
        tracing::info!("Downloading folder as ZIP: {}", id);

        // Get folder information and verify ownership
        let folder_service = &state.applications.folder_service;

        match folder_service
            .get_folder_with_perms(&id, auth_user.id)
            .await
        {
            Ok(folder) => {
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

                // Stream the archive as it is built — the first byte reaches
                // the client after the first entry, not after the whole ZIP
                // exists on disk (benches/ZIP-STREAM.md). No Content-Length:
                // the final size isn't known up front (chunked encoding).
                match zip_service
                    .create_folder_zip_stream(&id, &folder.name)
                    .await
                {
                    Ok(stream) => {
                        let body = axum::body::Body::from_stream(stream);

                        // Setup headers for download
                        let filename = format!("{}.zip", folder.name);
                        let content_disposition = format!("attachment; filename=\"{}\"", filename);

                        Response::builder()
                            .status(StatusCode::OK)
                            .header(header::CONTENT_TYPE, "application/zip")
                            .header(header::CONTENT_DISPOSITION, content_disposition)
                            .body(body)
                            .unwrap()
                            .into_response()
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
    security(("bearerAuth" = [])),
    tag = "folders"
)]
pub async fn create_folder(
    state: State<Arc<GlobalAppState>>,
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
    security(("bearerAuth" = [])),
    tag = "folders"
)]
pub async fn get_folder(
    state: State<Arc<GlobalAppState>>,
    auth_user: AuthUser,
    path: Path<String>,
) -> impl IntoResponse {
    FolderHandler::get_folder_impl(state, auth_user, path).await
}

#[utoipa::path(
    get,
    path = "/api/folders/{id}/ancestors",
    params(("id" = String, Path, description = "Leaf folder ID — the walk starts here and climbs the parent chain up to the drive root or the caller's share/drive-membership boundary.")),
    responses(
        (status = 200, description = "Ancestor chain + access-source. `ancestors` is root-first, leaf-last (length ≥ 1). See `FolderAncestorsDto`.", body = FolderAncestorsDto),
        (status = 404, description = "Folder not found or caller lacks Read (anti-enum)"),
    ),
    security(("bearerAuth" = [])),
    tag = "folders"
)]
pub async fn get_folder_ancestors(
    state: State<Arc<GlobalAppState>>,
    auth_user: AuthUser,
    path: Path<String>,
) -> impl IntoResponse {
    FolderHandler::get_folder_ancestors_impl(state, auth_user, path).await
}

#[utoipa::path(
    get,
    path = "/api/folders",
    responses(
        (status = 200, description = "List of root folders", body = Vec<FolderDto>),
    ),
    security(("bearerAuth" = [])),
    tag = "folders"
)]
pub async fn list_root_folders(
    state: State<AppState>,
    auth_user: AuthUser,
) -> axum::response::Response {
    FolderHandler::list_root_folders_impl(state, auth_user).await
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
    security(("bearerAuth" = [])),
    tag = "folders"
)]
pub async fn rename_folder(
    state: State<Arc<GlobalAppState>>,
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
    security(("bearerAuth" = [])),
    tag = "folders"
)]
pub async fn move_folder(
    state: State<Arc<GlobalAppState>>,
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
    security(("bearerAuth" = [])),
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
    security(("bearerAuth" = [])),
    tag = "folders"
)]
pub async fn download_folder_zip(
    state: State<Arc<GlobalAppState>>,
    auth_user: AuthUser,
    path: Path<String>,
) -> impl IntoResponse {
    // No `Query` extractor: the handler reads only the path `id`. axum ignores
    // any query string when no extractor is present, so the response is
    // byte-identical while a per-request HashMap + owned key/value Strings are
    // no longer parsed and dropped (benches/ROUND25.md §M3).
    FolderHandler::download_folder_zip_impl(state, auth_user, path).await
}

// ── GET /api/folders/{id}/resources ─────────────────────────────────────────

#[utoipa::path(
    get,
    path = "/api/folders/{id}/resources",
    params(
        ("id" = String, Path, description = "Folder ID"),
        FolderResourcesQuery,
    ),
    responses(
        (status = 200,
         description = "Cursor-paginated files and folders inside the requested folder. \
                        Items arrive in `order_by` order (folders first when order_by=name). \
                        `next_cursor` is absent on the last page.",
         body = FolderResourcesDto),
        (status = 404, description = "Folder not found or access denied"),
    ),
    security(("bearerAuth" = [])),
    tag = "folders"
)]
pub async fn list_folder_resources(
    State(service): State<AppState>,
    auth_user: AuthUser,
    Path(id): Path<String>,
    Query(q): Query<FolderResourcesQuery>,
) -> impl IntoResponse {
    let order_by = q.order_by.clone().unwrap_or_else(|| "name".to_owned());
    let kinds = q.resource_kinds();
    let opts = ListResourcesOptions {
        limit: q.limit_clamped(),
        cursor: q.decode_cursor(),
        order_by: &order_by,
        kinds: kinds.as_deref(),
        reverse: q.reverse,
    };

    // External mount branch: a mount-root UUID or an `ext:` id lists live from
    // the provider instead of the PostgreSQL UNION. The parent of each entry is
    // the requested id itself.
    match service.mount_router().classify(&id) {
        ResolvedId::MountRoot { cfg } => {
            return list_mount_dir_response(
                &service,
                &cfg,
                &NodeId::default(),
                &id,
                auth_user.id,
                opts,
            )
            .await;
        }
        ResolvedId::MountChild { cfg, node_id } => {
            return list_mount_dir_response(&service, &cfg, &node_id, &id, auth_user.id, opts)
                .await;
        }
        ResolvedId::Regular => {}
    }

    match service
        .list_resources_paged_with_perms(&id, auth_user.id, opts)
        .await
    {
        Ok((rows, next_cursor)) => {
            let items: Vec<FolderResourceItemDto> = rows
                .into_iter()
                .map(|row| {
                    if row.resource_type == "folder" {
                        let resource_id = row.id.to_string();
                        let dto = FolderDto {
                            etag: resource_id.clone(),
                            id: resource_id,
                            // Folders use fixed icon classes (below), so `name`
                            // is never borrowed again — move it instead of cloning.
                            name: row.name,
                            path: String::new(), // cleared — share recipients must not see hierarchy
                            parent_id: row.parent_id.map(|u| u.to_string()),
                            drive_id: row.drive_id,
                            created_at: row.created_at.timestamp() as u64,
                            modified_at: row.modified_at.timestamp() as u64,
                            is_root: false,
                            icon_class: intern_display("fas fa-folder"),
                            icon_special_class: intern_display("folder-icon"),
                            category: intern_display("Folder"),
                            created_by: row.created_by,
                            updated_by: row.updated_by,
                            is_favorite: row.is_favorite,
                            is_shared: row.is_shared,
                        };
                        FolderResourceItemDto {
                            resource_type: ResourceTypeDto::Folder,
                            resource: ResourceContentDto::Folder(dto),
                        }
                    } else {
                        let mime = row
                            .mime_type
                            .as_deref()
                            .unwrap_or("application/octet-stream");
                        let size_bytes = row.size.max(0) as u64;
                        // `blob_hash` is `Some(_)` for file rows in the
                        // UNION ALL (`NULL` for folders). Route the
                        // ETag formula through `File::compute_etag` —
                        // the single source of truth shared with
                        // GET/HEAD/PROPFIND/PUT response — so this
                        // listing's `etag` byte-equals what a
                        // conditional request would compare against.
                        let modified_at_u = row.modified_at.timestamp() as u64;
                        let content_hash = row.blob_hash.unwrap_or_default();
                        let etag = if content_hash.is_empty() {
                            String::new()
                        } else {
                            File::compute_etag(&content_hash, modified_at_u)
                        };
                        // Compute the name-derived icon/category classes first
                        // (they borrow `&row.name`), so `name` can be moved into
                        // the DTO below instead of cloned — one fewer String
                        // alloc per file row (benches/ROUND7.md).
                        let classes = classify_display(&row.name, mime);
                        let icon_class = intern_display(classes.icon_class);
                        let icon_special_class = intern_display(classes.icon_special_class);
                        let category = intern_display(classes.category);
                        let dto = FileDto {
                            id: row.id.to_string(),
                            name: row.name,
                            path: String::new(),
                            size: size_bytes,
                            mime_type: intern_mime(mime),
                            folder_id: row.parent_id.map(|u| u.to_string()),
                            created_at: row.created_at.timestamp() as u64,
                            modified_at: row.modified_at.timestamp() as u64,
                            icon_class,
                            icon_special_class,
                            category,
                            size_formatted: format_file_size(size_bytes),
                            sort_date: None,
                            content_hash,
                            etag,
                            created_by: row.created_by,
                            updated_by: row.updated_by,
                            is_favorite: row.is_favorite,
                            is_shared: row.is_shared,
                        };
                        FolderResourceItemDto {
                            resource_type: ResourceTypeDto::File,
                            resource: ResourceContentDto::File(dto),
                        }
                    }
                })
                .collect();

            {
                // Pre-sized serialization (benches/ROUND12.md §M1).
                let body = FolderResourcesDto::with_cursor(items, next_cursor);
                crate::interfaces::api::sized_json::sized_json(
                    128 + body.items.len()
                        * crate::interfaces::api::sized_json::EST_WRAPPED_ROW_BYTES,
                    &body,
                )
            }
        }
        Err(e) => AppError::from(e).into_response(),
    }
}

/// List one directory inside an external mount and render the standard
/// `/resources` envelope, mapping each live provider entry to a
/// `FolderResourceItemDto` with a synthetic `ext:` id. `parent_id` is the
/// requested id (the directory being listed), which becomes each entry's parent.
async fn list_mount_dir_response(
    service: &FolderService,
    cfg: &MountConfig,
    node_id: &NodeId,
    parent_id: &str,
    caller_id: uuid::Uuid,
    opts: ListResourcesOptions<'_>,
) -> axum::response::Response {
    match service
        .list_mount_dir_with_perms(cfg, node_id, caller_id, opts)
        .await
    {
        Ok((entries, next_cursor)) => {
            let items: Vec<FolderResourceItemDto> = entries
                .into_iter()
                .map(|entry| mount_entry_to_item(cfg, parent_id, entry))
                .collect();
            (
                StatusCode::OK,
                Json(FolderResourcesDto::with_cursor(items, next_cursor)),
            )
                .into_response()
        }
        Err(e) => AppError::from(e).into_response(),
    }
}

/// Map a live mount entry to a `/resources` item with a synthetic `ext:` id and
/// virtual (size+mtime / mtime) etag. Mount entries have no blob hash.
fn mount_entry_to_item(
    cfg: &MountConfig,
    parent_id: &str,
    entry: MountEntry,
) -> FolderResourceItemDto {
    let id = encode_child_id(cfg.mount_id, entry.node_id.clone());
    if entry.is_dir {
        let dto = FolderDto {
            etag: virtual_folder_etag(entry.modified_at),
            id,
            name: entry.name.clone(),
            path: String::new(),
            parent_id: Some(parent_id.to_owned()),
            drive_id: cfg.drive_id,
            created_at: entry.created_at,
            modified_at: entry.modified_at,
            is_root: false,
            icon_class: Arc::from("fas fa-folder"),
            icon_special_class: Arc::from("folder-icon"),
            category: Arc::from("Folder"),
            created_by: Some(cfg.owner_id),
            updated_by: Some(cfg.owner_id),
            // Mount entries use synthetic `ext:*` ids — no favorites /
            // grants rows can ever key against them.
            is_favorite: false,
            is_shared: false,
        };
        FolderResourceItemDto {
            resource_type: ResourceTypeDto::Folder,
            resource: ResourceContentDto::Folder(dto),
        }
    } else {
        let mime = mime_guess::from_path(&entry.name)
            .first_or_octet_stream()
            .to_string();
        let dto = FileDto {
            id,
            name: entry.name.clone(),
            path: String::new(),
            size: entry.size,
            mime_type: Arc::from(mime.as_str()),
            folder_id: Some(parent_id.to_owned()),
            created_at: entry.created_at,
            modified_at: entry.modified_at,
            icon_class: Arc::from(icon_class_for(&entry.name, &mime)),
            icon_special_class: Arc::from(icon_special_class_for(&entry.name, &mime)),
            category: Arc::from(category_for(&entry.name, &mime)),
            size_formatted: format_file_size(entry.size),
            sort_date: None,
            content_hash: String::new(),
            etag: virtual_file_etag(entry.size, entry.modified_at),
            created_by: Some(cfg.owner_id),
            updated_by: Some(cfg.owner_id),
            is_favorite: false,
            is_shared: false,
        };
        FolderResourceItemDto {
            resource_type: ResourceTypeDto::File,
            resource: ResourceContentDto::File(dto),
        }
    }
}

#[cfg(test)]
mod mount_mapping_tests {
    use super::*;
    use crate::application::services::mount_registry::MountConfig;
    use crate::infrastructure::services::local_fs_mount_provider::LocalFsMountProvider;
    use uuid::Uuid;

    fn config() -> MountConfig {
        let dir = tempfile::tempdir().unwrap();
        // Leak the tempdir so the path stays valid for the provider's lifetime;
        // the provider is never exercised here (mapping is pure metadata).
        let path = dir.keep();
        MountConfig {
            mount_id: Uuid::new_v4(),
            kind: "local_fs".to_string(),
            name: "Media".to_string(),
            owner_id: Uuid::new_v4(),
            drive_id: Uuid::new_v4(),
            read_only: false,
            mount_path: "Personal/Media".to_string(),
            provider: Arc::new(LocalFsMountProvider::new(&path, false).unwrap()),
        }
    }

    fn mount_entry(name: &str, node_id: &str, is_dir: bool, size: u64, mtime: u64) -> MountEntry {
        MountEntry {
            name: name.to_string(),
            node_id: NodeId(node_id.to_string()),
            is_dir,
            size,
            modified_at: mtime,
            created_at: mtime,
        }
    }

    #[test]
    fn maps_folder_entry_to_item() {
        let cfg = config();
        let parent = cfg.mount_id.to_string();
        let item = mount_entry_to_item(&cfg, &parent, mount_entry("docs", "docs", true, 0, 1234));

        assert!(matches!(item.resource_type, ResourceTypeDto::Folder));
        let ResourceContentDto::Folder(dto) = item.resource else {
            panic!("expected folder");
        };
        assert_eq!(dto.name, "docs");
        // id is the synthetic ext: envelope for (mount_id, node_id).
        assert_eq!(dto.id, encode_child_id(cfg.mount_id, "docs"));
        assert_eq!(dto.parent_id.as_deref(), Some(parent.as_str()));
        assert_eq!(dto.etag, virtual_folder_etag(1234));
        assert_eq!(dto.drive_id, cfg.drive_id);
        assert!(!dto.is_root);
        // Hierarchy is intentionally cleared on this listing.
        assert_eq!(dto.path, "");
    }

    #[test]
    fn maps_file_entry_to_item_with_virtual_etag_and_no_hash() {
        let cfg = config();
        let parent = encode_child_id(cfg.mount_id, "docs");
        let item = mount_entry_to_item(
            &cfg,
            &parent,
            mount_entry("report.json", "docs/report.json", false, 42, 999),
        );

        assert!(matches!(item.resource_type, ResourceTypeDto::File));
        let ResourceContentDto::File(dto) = item.resource else {
            panic!("expected file");
        };
        assert_eq!(dto.id, encode_child_id(cfg.mount_id, "docs/report.json"));
        assert_eq!(dto.folder_id.as_deref(), Some(parent.as_str()));
        assert_eq!(dto.size, 42);
        assert_eq!(dto.etag, virtual_file_etag(42, 999));
        // Virtual files have no blob hash.
        assert_eq!(dto.content_hash, "");
        // Mime is sniffed from the name.
        assert_eq!(&*dto.mime_type, "application/json");
    }
}
