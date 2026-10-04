use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
};
use serde::Deserialize;
use std::sync::Arc;
use tracing::info;
use utoipa::ToSchema;

use crate::application::dtos::display_helpers::{
    classify_display, format_file_size, intern_display, intern_mime,
};
use crate::application::dtos::favorites_dto::{
    FavoritesResourceItemDto, FavoritesResourcesDto, FavoritesResourcesQuery,
};
use crate::application::dtos::file_dto::FileDto;
use crate::application::dtos::folder_dto::FolderDto;
use crate::application::dtos::grant_dto::{ResourceContentDto, ResourceTypeDto};
use crate::application::ports::favorites_ports::FavoritesUseCase;
use crate::application::services::favorites_service::FavoritesService;
use crate::domain::entities::file::File;
use crate::interfaces::errors::AppError;
use crate::interfaces::middleware::auth::AuthUser;

/// Single item in a batch-add-favorites request.
#[derive(Debug, Deserialize, ToSchema)]
pub struct BatchFavoriteItem {
    pub item_id: String,
    pub item_type: String,
}

/// Request body for POST /api/favorites/batch
#[derive(Debug, Deserialize, ToSchema)]
pub struct BatchFavoritesRequest {
    pub items: Vec<BatchFavoriteItem>,
}

/// Add an item to user's favorites
#[utoipa::path(
    post,
    path = "/api/favorites/{item_type}/{item_id}",
    params(
        ("item_type" = String, Path, description = "Item type (file or folder)"),
        ("item_id" = String, Path, description = "Item ID")
    ),
    responses(
        (status = 201, body = Object, description = "Item added to favorites"),
        (status = 400, description = "Invalid item type")
    ),
    security(("bearerAuth" = [])),
    tag = "favorites"
)]
pub async fn add_favorite(
    State(favorites_service): State<Arc<FavoritesService>>,
    auth_user: AuthUser,
    Path((item_type, item_id)): Path<(String, String)>,
) -> impl IntoResponse {
    let user_id = auth_user.id;

    // Validate item_type
    if item_type != "file" && item_type != "folder" {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": "Item type must be 'file' or 'folder'"
            })),
        )
            .into_response();
    }

    match favorites_service
        .add_to_favorites(user_id, &item_id, &item_type)
        .await
    {
        Ok(_) => {
            info!("Added {} '{}' to favorites", item_type, item_id);
            (
                StatusCode::CREATED,
                Json(serde_json::json!({
                    "message": "Item added to favorites"
                })),
            )
                .into_response()
        }
        // Route through AppError so the `DomainError::kind` maps to the
        // right status code (NotFound → 404 anti-enum for the pre-write
        // authz gate, InvalidInput → 400 for a malformed UUID, etc.).
        // A hardcoded 500 here would mask the 404 the Round 1 AuthZ
        // fix relies on.
        Err(err) => AppError::from(err).into_response(),
    }
}

/// Remove an item from user's favorites
#[utoipa::path(
    delete,
    path = "/api/favorites/{item_type}/{item_id}",
    params(
        ("item_type" = String, Path, description = "Item type (file or folder)"),
        ("item_id" = String, Path, description = "Item ID")
    ),
    responses(
        (status = 200, body = Object, description = "Item removed from favorites"),
        (status = 404, description = "Item not in favorites")
    ),
    security(("bearerAuth" = [])),
    tag = "favorites"
)]
pub async fn remove_favorite(
    State(favorites_service): State<Arc<FavoritesService>>,
    auth_user: AuthUser,
    Path((item_type, item_id)): Path<(String, String)>,
) -> impl IntoResponse {
    let user_id = auth_user.id;

    match favorites_service
        .remove_from_favorites(user_id, &item_id, &item_type)
        .await
    {
        Ok(removed) => {
            if removed {
                info!("Removed {} '{}' from favorites", item_type, item_id);
                (
                    StatusCode::OK,
                    Json(serde_json::json!({
                        "message": "Item removed from favorites"
                    })),
                )
                    .into_response()
            } else {
                info!("Item {} '{}' was not in favorites", item_type, item_id);
                (
                    StatusCode::NOT_FOUND,
                    Json(serde_json::json!({
                        "message": "Item was not in favorites"
                    })),
                )
                    .into_response()
            }
        }
        // Same rationale as `add_favorite` — preserve DomainError→HTTP
        // status mapping instead of collapsing every error to 500.
        Err(err) => AppError::from(err).into_response(),
    }
}

/// Cursor-paginated list of a user's favorited resources.
///
/// Supports sorting by `name`, `type`, `favorited_at`, `modified_at`, `size`, or `owner`.
/// Items that have been deleted/trashed are silently excluded.
/// `path` is cleared when the resource is not owned by the requesting user.
#[utoipa::path(
    get,
    path = "/api/favorites/resources",
    params(FavoritesResourcesQuery),
    responses(
        (status = 200, description = "Paginated list of favorited resources",
         body = crate::application::dtos::favorites_dto::FavoritesResourcesDto),
        (status = 400, description = "Invalid cursor or query parameters"),
    ),
    security(("bearerAuth" = [])),
    tag = "favorites"
)]
pub async fn list_favorites_resources(
    State(favorites_service): State<Arc<FavoritesService>>,
    auth_user: AuthUser,
    Query(q): Query<FavoritesResourcesQuery>,
) -> impl IntoResponse {
    let user_id = auth_user.id;

    let order_by = q.order_by.as_deref().unwrap_or("name").to_owned();

    // If a cursor exists, validate that it matches the requested sort/direction.
    let cursor = q
        .decode_cursor()
        .filter(|c| c.order_by == order_by && c.reverse == q.reverse);

    let kinds = q.resource_kinds();

    match favorites_service
        .list_resources_paged(
            user_id,
            q.limit_clamped(),
            cursor,
            &order_by,
            kinds.as_deref(),
            q.reverse,
        )
        .await
    {
        Ok((rows, next_cursor)) => {
            let items: Vec<FavoritesResourceItemDto> = rows
                .into_iter()
                .map(|row| {
                    // Path is only shown to the owner; non-owners see ""
                    // to avoid leaking another user's folder hierarchy.
                    let path = if row.is_owner {
                        row.path.unwrap_or_default()
                    } else {
                        String::new()
                    };

                    if row.resource_type == "folder" {
                        let resource_id = row.resource_id.to_string();
                        let dto = FolderDto {
                            etag: resource_id.clone(),
                            id: resource_id,
                            name: row.name,
                            path,
                            parent_id: row.parent_id.map(|u| u.to_string()),
                            drive_id: row.drive_id,
                            created_at: row.resource_created_at.timestamp() as u64,
                            modified_at: row.modified_at.timestamp() as u64,
                            is_root: false,
                            icon_class: intern_display("fas fa-folder"),
                            icon_special_class: intern_display("folder-icon"),
                            category: intern_display("Folder"),
                            created_by: row.created_by,
                            updated_by: row.updated_by,
                            // Every row on this listing is a favorite by
                            // construction; the listing repo returns
                            // `TRUE AS is_favorite` unconditionally.
                            is_favorite: row.is_favorite,
                            is_shared: row.is_shared,
                        };
                        FavoritesResourceItemDto {
                            resource_type: ResourceTypeDto::Folder,
                            favorited_at: row.favorited_at,
                            resource: ResourceContentDto::Folder(dto),
                        }
                    } else {
                        let mime = row
                            .mime_type
                            .as_deref()
                            .unwrap_or("application/octet-stream");
                        let size_bytes = row.size.max(0) as u64;
                        // Route ETag through `File::compute_etag` so
                        // this listing's `etag` byte-equals what
                        // GET/HEAD/PROPFIND would return for the same
                        // file. `blob_hash` is `None` only for
                        // folder rows, which take the other branch.
                        let modified_at_u = row.modified_at.timestamp() as u64;
                        let content_hash = row.blob_hash.unwrap_or_default();
                        let etag = if content_hash.is_empty() {
                            String::new()
                        } else {
                            File::compute_etag(&content_hash, modified_at_u)
                        };
                        // Name-derived display classes borrow `row.name`;
                        // compute them before the name moves into the DTO.
                        let classes = classify_display(&row.name, mime);
                        let icon_class = intern_display(classes.icon_class);
                        let icon_special_class = intern_display(classes.icon_special_class);
                        let category = intern_display(classes.category);
                        let dto = FileDto {
                            id: row.resource_id.to_string(),
                            name: row.name,
                            path,
                            size: size_bytes,
                            mime_type: intern_mime(mime),
                            folder_id: row.parent_id.map(|u| u.to_string()),
                            created_at: row.resource_created_at.timestamp() as u64,
                            modified_at: modified_at_u,
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
                        FavoritesResourceItemDto {
                            resource_type: ResourceTypeDto::File,
                            favorited_at: row.favorited_at,
                            resource: ResourceContentDto::File(dto),
                        }
                    }
                })
                .collect();

            (
                StatusCode::OK,
                Json(FavoritesResourcesDto::with_cursor(items, next_cursor)),
            )
                .into_response()
        }
        Err(e) => AppError::from(e).into_response(),
    }
}

/// Add multiple items to favourites in a single transaction.
/// POST /api/favorites/batch
#[utoipa::path(
    post,
    path = "/api/favorites/batch",
    responses(
        (status = 200, description = "Batch add result", body = crate::application::dtos::favorites_dto::BatchFavoritesResult),
        (status = 400, description = "Invalid request")
    ),
    security(("bearerAuth" = [])),
    tag = "favorites"
)]
pub async fn batch_add_favorites(
    State(favorites_service): State<Arc<FavoritesService>>,
    auth_user: AuthUser,
    Json(body): Json<BatchFavoritesRequest>,
) -> impl IntoResponse {
    let user_id = auth_user.id;

    if body.items.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "items array must not be empty" })),
        )
            .into_response();
    }

    // Validate item types
    for item in &body.items {
        if item.item_type != "file" && item.item_type != "folder" {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "error": format!("Item type must be 'file' or 'folder', got '{}'", item.item_type)
                })),
            )
                .into_response();
        }
    }

    let items: Vec<(String, String)> = body
        .items
        .into_iter()
        .map(|i| (i.item_id, i.item_type))
        .collect();

    match favorites_service
        .batch_add_to_favorites(user_id, &items)
        .await
    {
        Ok(result) => {
            info!(
                "Batch favourites: {} requested, {} inserted, {} already existed",
                result.stats.requested, result.stats.inserted, result.stats.already_existed
            );
            (StatusCode::OK, Json(serde_json::json!(result))).into_response()
        }
        // Preserve DomainError→HTTP status mapping — the Round 1
        // AuthZ fix relies on a per-item NotFound propagating out
        // of the batch. A hardcoded 500 would mask the 404 that
        // signals a cross-tenant probe.
        Err(err) => AppError::from(err).into_response(),
    }
}
