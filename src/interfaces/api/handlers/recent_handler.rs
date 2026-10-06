use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
};
use std::sync::Arc;
use tracing::info;

use crate::application::dtos::display_helpers::{
    classify_display, format_file_size, intern_display, intern_mime,
};
use crate::application::dtos::file_dto::FileDto;
use crate::application::dtos::folder_dto::FolderDto;
use crate::application::dtos::grant_dto::{ResourceContentDto, ResourceTypeDto};
use crate::application::dtos::recent_dto::{
    RecentResourceItemDto, RecentResourcesDto, RecentResourcesQuery,
};
use crate::application::ports::recent_ports::RecentItemsUseCase;
use crate::application::services::recent_service::RecentService;
use crate::domain::entities::file::File;
use crate::interfaces::errors::AppError;
use crate::interfaces::middleware::auth::AuthUser;
use uuid::Uuid;

/// Record access to an item
#[utoipa::path(
    post,
    path = "/api/recent/{item_type}/{item_id}",
    params(
        ("item_type" = String, Path, description = "Item type (file or folder)"),
        ("item_id" = String, Path, description = "Item ID")
    ),
    responses(
        (status = 200, body = Object, description = "Access recorded"),
        (status = 400, description = "Invalid item type")
    ),
    security(("bearerAuth" = [])),
    tag = "recent"
)]
pub async fn record_item_access(
    State(recent_service): State<Arc<RecentService>>,
    auth_user: AuthUser,
    Path((item_type, item_id)): Path<(String, Uuid)>,
) -> impl IntoResponse {
    let user_id = auth_user.id;

    // Validate item type
    if item_type != "file" && item_type != "folder" {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": "Item type must be 'file' or 'folder'"
            })),
        )
            .into_response();
    }

    let mut id_buf = [0u8; 36];
    let item_id_str: &str = item_id.as_hyphenated().encode_lower(&mut id_buf);
    match recent_service
        .record_item_access(user_id, item_id_str, &item_type)
        .await
    {
        Ok(_) => {
            info!("Recorded access to {} '{}' in recents", item_type, item_id);
            (
                StatusCode::OK,
                Json(serde_json::json!({
                    "message": "Access recorded successfully"
                })),
            )
                .into_response()
        }
        // Preserve DomainError→HTTP status mapping — the Round 1
        // AuthZ fix relies on the NotFound from `authz.require`
        // propagating as 404 (anti-enum), not being masked as 500.
        Err(err) => AppError::from(err).into_response(),
    }
}

/// Remove an item from recents
#[utoipa::path(
    delete,
    path = "/api/recent/{item_type}/{item_id}",
    params(
        ("item_type" = String, Path, description = "Item type (file or folder)"),
        ("item_id" = String, Path, description = "Item ID")
    ),
    responses(
        (status = 200, body = Object, description = "Item removed from recents"),
        (status = 404, description = "Item not in recents")
    ),
    security(("bearerAuth" = [])),
    tag = "recent"
)]
pub async fn remove_from_recent(
    State(recent_service): State<Arc<RecentService>>,
    auth_user: AuthUser,
    Path((item_type, item_id)): Path<(String, Uuid)>,
) -> impl IntoResponse {
    let user_id = auth_user.id;

    let mut id_buf = [0u8; 36];
    let item_id_str: &str = item_id.as_hyphenated().encode_lower(&mut id_buf);
    match recent_service
        .remove_from_recent(user_id, item_id_str, &item_type)
        .await
    {
        Ok(removed) => {
            if removed {
                info!("Removed {} '{}' from recents", item_type, item_id);
                (
                    StatusCode::OK,
                    Json(serde_json::json!({
                        "message": "Item removed from recents"
                    })),
                )
                    .into_response()
            } else {
                info!("Item {} '{}' was not in recents", item_type, item_id);
                (
                    StatusCode::NOT_FOUND,
                    Json(serde_json::json!({
                        "message": "Item was not in recents"
                    })),
                )
                    .into_response()
            }
        }
        // Same rationale as `record_item_access` — preserve the
        // DomainError→HTTP mapping instead of collapsing to 500.
        Err(err) => AppError::from(err).into_response(),
    }
}

/// Clear all recent items
#[utoipa::path(
    delete,
    path = "/api/recent/clear",
    responses(
        (status = 200, body = Object, description = "Recent items cleared")
    ),
    security(("bearerAuth" = [])),
    tag = "recent"
)]
pub async fn clear_recent_items(
    State(recent_service): State<Arc<RecentService>>,
    auth_user: AuthUser,
) -> impl IntoResponse {
    let user_id = auth_user.id;

    match recent_service.clear_recent_items(user_id).await {
        Ok(_) => {
            info!("Cleared all recent items for user");
            (
                StatusCode::OK,
                Json(serde_json::json!({
                    "message": "Recent items cleared successfully"
                })),
            )
                .into_response()
        }
        // Same rationale as `record_item_access` — preserve the
        // DomainError→HTTP mapping instead of collapsing to 500.
        Err(err) => AppError::from(err).into_response(),
    }
}

/// List recently accessed resources with cursor pagination.
///
/// Sorted by `accessed_at` DESC by default (most recently accessed first).
/// `path` is cleared when the resource is not owned by the requesting user.
#[utoipa::path(
    get,
    path = "/api/recent/resources",
    params(RecentResourcesQuery),
    responses(
        (status = 200, description = "Paginated list of recently accessed resources",
         body = RecentResourcesDto),
        (status = 400, description = "Invalid cursor or query parameters"),
    ),
    security(("bearerAuth" = [])),
    tag = "recent"
)]
pub async fn list_recent_resources(
    State(recent_service): State<Arc<RecentService>>,
    auth_user: AuthUser,
    Query(q): Query<RecentResourcesQuery>,
) -> impl IntoResponse {
    let user_id = auth_user.id;

    let order_by = q.order_by.as_deref().unwrap_or("accessed_at").to_owned();

    // If a cursor exists, validate that it matches the requested sort/direction.
    let cursor = q
        .decode_cursor()
        .filter(|c| c.order_by == order_by && c.reverse == q.reverse);

    let kinds = q.resource_kinds();

    match recent_service
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
            let items: Vec<RecentResourceItemDto> = rows
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
                            is_favorite: row.is_favorite,
                            is_shared: row.is_shared,
                            mount: None,
                        };
                        RecentResourceItemDto {
                            resource_type: ResourceTypeDto::Folder,
                            accessed_at: row.accessed_at,
                            resource: ResourceContentDto::Folder(dto),
                        }
                    } else {
                        let mime = row
                            .mime_type
                            .as_deref()
                            .unwrap_or("application/octet-stream");
                        let size_bytes = row.size.max(0) as u64;
                        // Route ETag through `File::compute_etag` so this
                        // listing matches GET/HEAD/PROPFIND byte-for-byte
                        // for the same file.
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
                        RecentResourceItemDto {
                            resource_type: ResourceTypeDto::File,
                            accessed_at: row.accessed_at,
                            resource: ResourceContentDto::File(dto),
                        }
                    }
                })
                .collect();

            (
                StatusCode::OK,
                Json(RecentResourcesDto::with_cursor(items, next_cursor)),
            )
                .into_response()
        }
        Err(e) => AppError::from(e).into_response(),
    }
}
