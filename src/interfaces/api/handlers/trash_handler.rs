use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use serde_json::json;
use tracing::{debug, error, instrument, warn};

use crate::application::dtos::trash_dto::{TrashResourcesDto, TrashResourcesQuery};
use crate::application::ports::trash_ports::TrashUseCase;
use crate::common::di::AppState;
use crate::interfaces::api::etag::{if_none_match_matches, not_modified, with_cache_headers};
use crate::interfaces::errors::AppError;
use crate::interfaces::middleware::auth::AuthUser;
use std::sync::Arc;

/// Cursor-paginated list of a user's trashed resources.
///
/// Sorts by `deletion_date` (default — soonest expiry first), `trashed_at`
/// (most recently trashed first), `name`, `type`, or `size`. Filter on
/// `resource_types=file` or `resource_types=folder` to narrow to one kind.
/// Items implicitly trashed as descendants of a trashed parent are excluded
/// (only top-level trashed items appear).
#[utoipa::path(
    get,
    path = "/api/trash/resources",
    params(TrashResourcesQuery),
    responses(
        (status = 200, description = "Paginated list of trashed resources",
         body = crate::application::dtos::trash_dto::TrashResourcesDto),
        (status = 400, description = "Invalid cursor or query parameters"),
        (status = 501, description = "Trash feature not enabled"),
    ),
    security(("bearerAuth" = [])),
    tag = "trash"
)]
#[instrument(skip_all)]
pub async fn get_trash_resources(
    State(state): State<Arc<AppState>>,
    auth_user: AuthUser,
    headers: HeaderMap,
    Query(q): Query<TrashResourcesQuery>,
) -> axum::response::Response {
    let user_id = auth_user.id;

    let trash_service = match state.trash_service.as_ref() {
        Some(service) => service,
        None => {
            return (
                StatusCode::NOT_IMPLEMENTED,
                Json(json!({ "error": "Trash feature is not enabled" })),
            )
                .into_response();
        }
    };

    let order_by = q.order_by.as_deref().unwrap_or("deletion_date").to_owned();
    let cursor_input = q.cursor.clone();
    let limit = q.limit_clamped();

    // Discard cursor if sort dimension or direction changed between pages.
    let cursor = q
        .decode_cursor()
        .filter(|c| c.order_by == order_by && c.reverse == q.reverse);

    let kinds = q.resource_kinds();

    match trash_service
        .list_resources_paged(
            user_id,
            limit,
            cursor,
            &order_by,
            kinds.as_deref(),
            q.reverse,
        )
        .await
    {
        Ok((items, next_cursor)) => {
            let envelope = TrashResourcesDto::with_cursor(items, next_cursor);
            // Fresh signal: newest `trashed_at` on the page — bumps
            // when a new row lands in trash, stable on repeat
            // revalidations of an unchanged page. An empty trash
            // collapses to 0, which stays stable across repeated
            // "is my trash still empty?" checks and bumps as soon as
            // the first item lands.
            let fresh_signal: u64 = envelope
                .items
                .iter()
                .map(|i| i.trashed_at.timestamp().max(0) as u64)
                .max()
                .unwrap_or(0);
            let etag = envelope.weak_etag(cursor_input.as_deref(), limit, fresh_signal);
            if if_none_match_matches(&headers, &etag) {
                return not_modified(&etag).into_response();
            }
            with_cache_headers(Json(envelope), &etag).into_response()
        }
        Err(e) => AppError::from(e).into_response(),
    }
}

/// Moves a file to the trash
#[utoipa::path(
    delete,
    path = "/api/trash/files/{id}",
    params(("id" = String, Path, description = "File ID")),
    responses(
        (status = 200, body = Object, description = "File moved to trash"),
        (status = 501, description = "Trash feature not enabled")
    ),
    security(("bearerAuth" = [])),
    tag = "trash"
)]
#[instrument(skip_all)]
pub async fn move_file_to_trash(
    State(state): State<Arc<AppState>>,
    auth_user: AuthUser,
    Path(item_id): Path<String>,
) -> axum::response::Response {
    let user_id = auth_user.id;
    debug!(
        "Request to move file to trash: id={}, user={}",
        item_id, user_id
    );

    let trash_service = match state.trash_service.as_ref() {
        Some(service) => service,
        None => {
            return (
                StatusCode::NOT_IMPLEMENTED,
                Json(json!({
                    "error": "Trash feature is not enabled"
                })),
            )
                .into_response();
        }
    };

    // Specify that it is a file
    let result = trash_service.move_to_trash(&item_id, "file", user_id).await;

    match result {
        Ok(_) => {
            debug!("File moved to trash successfully");
            (
                StatusCode::OK,
                Json(json!({
                    "success": true,
                    "message": "File moved to trash successfully"
                })),
            )
                .into_response()
        }
        Err(e) => {
            warn!("move_file_to_trash failed: {:?}", e);
            AppError::from(e).into_response()
        }
    }
}

/// Moves a folder to the trash
#[utoipa::path(
    delete,
    path = "/api/trash/folders/{id}",
    params(("id" = String, Path, description = "Folder ID")),
    responses(
        (status = 200, body = Object, description = "Folder moved to trash"),
        (status = 501, description = "Trash feature not enabled")
    ),
    security(("bearerAuth" = [])),
    tag = "trash"
)]
#[instrument(skip_all)]
pub async fn move_folder_to_trash(
    State(state): State<Arc<AppState>>,
    auth_user: AuthUser,
    Path(item_id): Path<String>,
) -> axum::response::Response {
    let user_id = auth_user.id;
    debug!(
        "Request to move folder to trash: id={}, user={}",
        item_id, user_id
    );

    let trash_service = match state.trash_service.as_ref() {
        Some(service) => service,
        None => {
            return (
                StatusCode::NOT_IMPLEMENTED,
                Json(json!({
                    "error": "Trash feature is not enabled"
                })),
            )
                .into_response();
        }
    };

    // Specify that it is a folder
    let result = trash_service
        .move_to_trash(&item_id, "folder", user_id)
        .await;

    match result {
        Ok(_) => {
            debug!("Folder moved to trash successfully");
            (
                StatusCode::OK,
                Json(json!({
                    "success": true,
                    "message": "Folder moved to trash successfully"
                })),
            )
                .into_response()
        }
        Err(e) => {
            warn!("move_folder_to_trash failed: {:?}", e);
            AppError::from(e).into_response()
        }
    }
}

/// Restores an item from the trash to its original location
#[utoipa::path(
    post,
    path = "/api/trash/{id}/restore",
    params(("id" = String, Path, description = "Trash item ID")),
    responses(
        (status = 200, body = Object, description = "Item restored from trash"),
        (status = 501, description = "Trash feature not enabled")
    ),
    security(("bearerAuth" = [])),
    tag = "trash"
)]
#[instrument(skip_all)]
pub async fn restore_from_trash(
    State(state): State<Arc<AppState>>,
    auth_user: AuthUser,
    Path(trash_id): Path<String>,
) -> axum::response::Response {
    debug!("Request to restore item {} from trash", trash_id);

    let trash_service = match state.trash_service.as_ref() {
        Some(service) => service,
        None => {
            return (
                StatusCode::NOT_IMPLEMENTED,
                Json(json!({
                    "error": "Trash feature is not enabled"
                })),
            )
                .into_response();
        }
    };
    let result = trash_service.restore_item(&trash_id, auth_user.id).await;

    match result {
        Ok(_) => {
            debug!("Item restored successfully");
            (
                StatusCode::OK,
                Json(json!({
                    "success": true,
                    "message": "Item restored successfully"
                })),
            )
                .into_response()
        }
        Err(e) => {
            warn!("restore_from_trash failed: {:?}", e);
            AppError::from(e).into_response()
        }
    }
}

/// Permanently deletes an item from the trash
#[utoipa::path(
    delete,
    path = "/api/trash/{id}",
    params(("id" = String, Path, description = "Trash item ID")),
    responses(
        (status = 200, body = Object, description = "Item permanently deleted"),
        (status = 501, description = "Trash feature not enabled")
    ),
    security(("bearerAuth" = [])),
    tag = "trash"
)]
#[instrument(skip_all)]
pub async fn delete_permanently(
    State(state): State<Arc<AppState>>,
    auth_user: AuthUser,
    Path(trash_id): Path<String>,
) -> axum::response::Response {
    debug!("Request to permanently delete item {}", trash_id);

    let trash_service = match state.trash_service.as_ref() {
        Some(service) => service,
        None => {
            return (
                StatusCode::NOT_IMPLEMENTED,
                Json(json!({
                    "error": "Trash feature is not enabled"
                })),
            )
                .into_response();
        }
    };
    let result = trash_service
        .delete_permanently(&trash_id, auth_user.id)
        .await;

    match result {
        Ok(_) => {
            debug!("Item permanently deleted");
            (
                StatusCode::OK,
                Json(json!({
                    "success": true,
                    "message": "Item deleted permanently"
                })),
            )
                .into_response()
        }
        Err(e) => {
            warn!("delete_permanently failed: {:?}", e);
            AppError::from(e).into_response()
        }
    }
}

/// Empties the trash completely for the current user
#[utoipa::path(
    delete,
    path = "/api/trash/empty",
    responses(
        (status = 200, body = Object, description = "Trash emptied successfully"),
        (status = 501, description = "Trash feature not enabled")
    ),
    security(("bearerAuth" = [])),
    tag = "trash"
)]
#[instrument(skip_all)]
pub async fn empty_trash(
    State(state): State<Arc<AppState>>,
    auth_user: AuthUser,
) -> (StatusCode, Json<serde_json::Value>) {
    debug!("Request to empty trash for user {}", auth_user.id);

    let trash_service = match state.trash_service.as_ref() {
        Some(service) => service,
        None => {
            return (
                StatusCode::NOT_IMPLEMENTED,
                Json(json!({
                    "error": "Trash feature is not enabled"
                })),
            );
        }
    };
    let result = trash_service.empty_trash(auth_user.id).await;

    match result {
        Ok(_) => {
            debug!("Trash emptied successfully");
            (
                StatusCode::OK,
                Json(json!({
                    "success": true,
                    "message": "Trash emptied successfully"
                })),
            )
        }
        Err(e) => {
            error!("Error emptying trash: {:?}", e);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({
                    "error": "Error emptying trash"
                })),
            )
        }
    }
}

/// `DELETE /api/trash/drive/{drive_id}` — per-drive empty trash.
///
/// Same destructive shape as the all-drives `DELETE /api/trash`, but
/// scoped to a single drive the caller can Delete in. Used by the
/// `/trash` page's Drive group-by, which exposes a per-row "Empty"
/// affordance so multi-drive owners don't have to wipe everything at
/// once.
///
/// Refused with `404` (anti-enum) when the caller has no Delete-bearing
/// role on the named drive — the user-facing drive listing would emit
/// the same shape for an unknown id.
#[utoipa::path(
    delete,
    path = "/api/trash/drive/{drive_id}",
    params(("drive_id" = Uuid, Path, description = "Drive UUID")),
    responses(
        (status = 200, body = Object, description = "Drive trash emptied successfully"),
        (status = 404, description = "Caller lacks Delete on this drive"),
        (status = 501, description = "Trash feature not enabled"),
    ),
    security(("bearerAuth" = [])),
    tag = "trash"
)]
#[instrument(skip_all)]
pub async fn empty_trash_for_drive(
    State(state): State<Arc<AppState>>,
    auth_user: AuthUser,
    Path(drive_id): Path<uuid::Uuid>,
) -> impl IntoResponse {
    debug!(
        "Request to empty trash for drive {} by user {}",
        drive_id, auth_user.id
    );

    let trash_service = match state.trash_service.as_ref() {
        Some(service) => service,
        None => {
            return (
                StatusCode::NOT_IMPLEMENTED,
                Json(json!({ "error": "Trash feature is not enabled" })),
            )
                .into_response();
        }
    };

    match trash_service
        .empty_trash_for_drive(auth_user.id, drive_id)
        .await
    {
        Ok(_) => (
            StatusCode::OK,
            Json(json!({ "success": true, "drive_id": drive_id })),
        )
            .into_response(),
        Err(e) => AppError::from(e).into_response(),
    }
}
