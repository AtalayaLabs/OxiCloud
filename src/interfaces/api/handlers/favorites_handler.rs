use axum::{
    Json,
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
};
use serde::Deserialize;
use std::sync::Arc;
use tracing::{error, info};
use utoipa::ToSchema;

use crate::application::ports::favorites_ports::FavoritesUseCase;
use crate::application::services::favorites_service::FavoritesService;
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

/// Handler for favorite-related API endpoints
#[utoipa::path(
    get,
    path = "/api/favorites",
    responses(
        (status = 200, description = "List of favorites", body = Vec<crate::application::dtos::favorites_dto::FavoriteItemDto>)
    ),
    tag = "favorites"
)]
pub async fn get_favorites(
    State(favorites_service): State<Arc<FavoritesService>>,
    auth_user: AuthUser,
) -> impl IntoResponse {
    let user_id = auth_user.id;

    match favorites_service.get_favorites(user_id).await {
        Ok(favorites) => {
            info!(
                "Retrieved {} favorites for user {}",
                favorites.len(),
                auth_user.id
            );
            (StatusCode::OK, Json(serde_json::json!(favorites))).into_response()
        }
        Err(err) => {
            error!("Error retrieving favorites: {}", err);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "error": "Failed to retrieve favorites"
                })),
            )
                .into_response()
        }
    }
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
        (status = 201, description = "Item added to favorites"),
        (status = 400, description = "Invalid item type")
    ),
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
        );
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
        }
        Err(err) => {
            error!("Error adding to favorites: {}", err);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "error": "Failed to add to favorites"
                })),
            )
        }
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
        (status = 200, description = "Item removed from favorites"),
        (status = 404, description = "Item not in favorites")
    ),
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
            } else {
                info!("Item {} '{}' was not in favorites", item_type, item_id);
                (
                    StatusCode::NOT_FOUND,
                    Json(serde_json::json!({
                        "message": "Item was not in favorites"
                    })),
                )
            }
        }
        Err(err) => {
            error!("Error removing from favorites: {}", err);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "error": "Failed to remove from favorites"
                })),
            )
        }
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
        Err(err) => {
            error!("Error in batch add favorites: {}", err);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "error": "Failed to batch add favorites"
                })),
            )
                .into_response()
        }
    }
}
