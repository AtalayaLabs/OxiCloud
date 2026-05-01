use std::sync::Arc;
use uuid::Uuid;

use axum::{
    Json,
    body::Body,
    extract::{Path, Query, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use serde_json::json;
use utoipa::ToSchema;

use crate::application::services::share_service::ShareService;
use crate::{
    application::{
        dtos::share_dto::{CreateShareDto, UpdateShareDto},
        ports::{
            file_ports::{FileRetrievalUseCase, OptimizedFileContent},
            share_ports::ShareUseCase,
        },
    },
    common::{di::AppState, errors::ErrorKind},
    domain::entities::share::ShareItemType,
    interfaces::errors::AppError,
    interfaces::middleware::auth::AuthUser,
};

#[derive(Debug, Deserialize)]
pub struct GetSharesQuery {
    pub page: Option<usize>,
    pub per_page: Option<usize>,
    pub item_id: Option<String>,
    pub item_type: Option<String>,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct VerifyPasswordRequest {
    pub password: String,
}

/// Create a new shared link
#[utoipa::path(
    post,
    path = "/api/shares",
    request_body = CreateShareDto,
    responses(
        (status = 201, description = "Share created", body = crate::application::dtos::share_dto::ShareDto),
        (status = 400, description = "Bad request")
    ),
    tag = "shares"
)]
pub async fn create_shared_link(
    State(share_use_case): State<Arc<ShareService>>,
    auth_user: AuthUser,
    Json(dto): Json<CreateShareDto>,
) -> impl IntoResponse {
    match share_use_case.create_shared_link(auth_user.id, dto).await {
        Ok(share) => (StatusCode::CREATED, Json(share)).into_response(),
        Err(err) => AppError::from(err).into_response(),
    }
}

/// Get information about a specific shared link by ID
#[utoipa::path(
    get,
    path = "/api/shares/{id}",
    params(("id" = String, Path, description = "Share ID")),
    responses(
        (status = 200, description = "Share details", body = crate::application::dtos::share_dto::ShareDto),
        (status = 404, description = "Share not found")
    ),
    tag = "shares"
)]
pub async fn get_shared_link(
    State(share_use_case): State<Arc<ShareService>>,
    auth_user: AuthUser,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let id = match Uuid::parse_str(&id) {
        Ok(id) => id,
        Err(_) => return AppError::bad_request("Invalid UUID").into_response(),
    };
    match share_use_case.get_shared_link(id, auth_user.id).await {
        Ok(share) => (StatusCode::OK, Json(share)).into_response(),
        Err(err) => AppError::from(err).into_response(),
    }
}

/// Get all shared links created by the current user.
/// Supports optional filtering by item_id + item_type query params.
#[utoipa::path(
    get,
    path = "/api/shares",
    responses(
        (status = 200, description = "List of shares", body = Vec<crate::application::dtos::share_dto::ShareDto>)
    ),
    tag = "shares"
)]
pub async fn get_user_shares(
    State(share_use_case): State<Arc<ShareService>>,
    auth_user: AuthUser,
    Query(query): Query<GetSharesQuery>,
) -> impl IntoResponse {
    let user_id = auth_user.id;

    // If both item_id and item_type are provided, return shares for that specific item
    if let (Some(item_id), Some(item_type_str)) = (&query.item_id, &query.item_type) {
        let item_type = match ShareItemType::try_from(item_type_str.as_str()) {
            Ok(t) => t,
            Err(_) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({ "error": format!("Invalid item_type: {}", item_type_str) })),
                )
                    .into_response();
            }
        };
        return match share_use_case
            .get_shared_links_for_item(item_id, &item_type, user_id)
            .await
        {
            Ok(shares) => (StatusCode::OK, Json(shares)).into_response(),
            Err(err) => AppError::from(err).into_response(),
        };
    }

    // Default: paginated list of all user shares
    let page = query.page.unwrap_or(1);
    let per_page = query.per_page.unwrap_or(20);

    match share_use_case
        .get_user_shared_links(user_id, page, per_page)
        .await
    {
        Ok(shares) => (StatusCode::OK, Json(shares)).into_response(),
        Err(err) => AppError::from(err).into_response(),
    }
}

/// Update a shared link's properties
#[utoipa::path(
    put,
    path = "/api/shares/{id}",
    params(("id" = String, Path, description = "Share ID")),
    request_body = UpdateShareDto,
    responses(
        (status = 200, description = "Share updated", body = crate::application::dtos::share_dto::ShareDto),
        (status = 404, description = "Share not found")
    ),
    tag = "shares"
)]
pub async fn update_shared_link(
    State(share_use_case): State<Arc<ShareService>>,
    auth_user: AuthUser,
    Path(id): Path<String>,
    Json(dto): Json<UpdateShareDto>,
) -> impl IntoResponse {
    let id = match Uuid::parse_str(&id) {
        Ok(id) => id,
        Err(_) => return AppError::bad_request("Invalid UUID").into_response(),
    };
    match share_use_case
        .update_shared_link(id, auth_user.id, dto)
        .await
    {
        Ok(share) => (StatusCode::OK, Json(share)).into_response(),
        Err(err) => AppError::from(err).into_response(),
    }
}

/// Delete a shared link
#[utoipa::path(
    delete,
    path = "/api/shares/{id}",
    params(("id" = String, Path, description = "Share ID")),
    responses(
        (status = 204, description = "Share deleted"),
        (status = 404, description = "Share not found")
    ),
    tag = "shares"
)]
pub async fn delete_shared_link(
    State(share_use_case): State<Arc<ShareService>>,
    auth_user: AuthUser,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let id = match Uuid::parse_str(&id) {
        Ok(id) => id,
        Err(_) => return AppError::bad_request("Invalid UUID").into_response(),
    };
    match share_use_case.delete_shared_link(id, auth_user.id).await {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(err) => AppError::from(err).into_response(),
    }
}

/// Access a shared item via its token
#[utoipa::path(
    get,
    path = "/api/s/{token}",
    params(("token" = String, Path, description = "Share token")),
    responses(
        (status = 200, description = "Shared item details"),
        (status = 401, description = "Password required"),
        (status = 410, description = "Share expired")
    ),
    tag = "shares"
)]
pub async fn access_shared_item(
    State(share_use_case): State<Arc<ShareService>>,
    Path(token): Path<String>,
) -> impl IntoResponse {
    // Register the access
    let _ = share_use_case.register_shared_link_access(&token).await;

    // Get the shared link
    match share_use_case.get_shared_link_by_token(&token).await {
        Ok(item) => (StatusCode::OK, Json(item)).into_response(),
        Err(err) => {
            // Special handling for share access errors
            if err.kind == ErrorKind::AccessDenied {
                if err.message.contains("password") {
                    return (
                        StatusCode::UNAUTHORIZED,
                        Json(json!({
                            "error": "Password required",
                            "requiresPassword": true
                        })),
                    )
                        .into_response();
                }
                if err.message.contains("expired") {
                    return AppError::new(StatusCode::GONE, err.message, "Expired").into_response();
                }
            }
            AppError::from(err).into_response()
        }
    }
}

/// Verify password for a password-protected shared item
#[utoipa::path(
    post,
    path = "/api/s/{token}/verify",
    params(("token" = String, Path, description = "Share token")),
    responses(
        (status = 200, description = "Password verified, item details returned"),
        (status = 401, description = "Invalid password"),
        (status = 410, description = "Share expired")
    ),
    tag = "shares"
)]
pub async fn verify_shared_item_password(
    State(share_use_case): State<Arc<ShareService>>,
    Path(token): Path<String>,
    Json(req): Json<VerifyPasswordRequest>,
) -> impl IntoResponse {
    match share_use_case
        .verify_shared_link_password(&token, &req.password)
        .await
    {
        Ok(item) => (StatusCode::OK, Json(item)).into_response(),
        Err(err) => {
            if err.kind == ErrorKind::AccessDenied {
                if err.message.contains("expired") {
                    return AppError::new(StatusCode::GONE, err.message, "Expired").into_response();
                }
                if err.message.contains("password") {
                    return AppError::unauthorized("Invalid password").into_response();
                }
            }
            AppError::from(err).into_response()
        }
    }
}

/// Download the actual file content for a shared file via its token.
///
/// Validates the share token, checks it refers to a file (not folder),
/// then streams the file content to the caller.
#[utoipa::path(
    get,
    path = "/s/{token}/download",
    params(("token" = String, Path, description = "Share token")),
    responses(
        (status = 200, description = "File content stream"),
        (status = 401, description = "Password required"),
        (status = 404, description = "Share not found"),
        (status = 410, description = "Share expired"),
        (status = 503, description = "Sharing disabled")
    ),
    tag = "shares"
)]
pub async fn download_shared_file(
    State(state): State<Arc<AppState>>,
    Path(token): Path<String>,
) -> impl IntoResponse {
    // 1. Resolve share service
    let share_service = match &state.share_service {
        Some(s) => s.clone(),
        None => {
            return AppError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "Sharing is disabled",
                "Disabled",
            )
            .into_response();
        }
    };

    // 2. Validate the share token (handles expiry + password checks)
    let share_dto = match share_service.get_shared_link_by_token(&token).await {
        Ok(dto) => dto,
        Err(err) => {
            if err.kind == ErrorKind::AccessDenied {
                if err.message.contains("password") {
                    return (
                        StatusCode::UNAUTHORIZED,
                        Json(json!({
                            "error": "Password required",
                            "requiresPassword": true
                        })),
                    )
                        .into_response();
                }
                if err.message.contains("expired") {
                    return AppError::new(StatusCode::GONE, err.message, "Expired").into_response();
                }
            }
            return AppError::from(err).into_response();
        }
    };

    // 3. Only file shares support direct download
    if share_dto.item_type != "file" {
        return AppError::bad_request("Download is only supported for file shares").into_response();
    }

    // 4. Retrieve file content via the internal (no-ownership-check) API
    let retrieval = &state.applications.file_retrieval_service;
    let file_id = &share_dto.item_id;

    match retrieval.get_file_optimized(file_id, false, true).await {
        Ok((file_dto, content)) => {
            let file_name = share_dto.item_name.as_deref().unwrap_or(&file_dto.name);
            let disposition = format!(
                "attachment; filename=\"{}\"",
                file_name.replace('"', "\\\"")
            );
            let mime = file_dto.mime_type.clone();

            match content {
                OptimizedFileContent::Bytes { data, .. } => Response::builder()
                    .status(StatusCode::OK)
                    .header(header::CONTENT_TYPE, &*mime)
                    .header(header::CONTENT_DISPOSITION, &disposition)
                    .header(header::CONTENT_LENGTH, data.len())
                    .body(Body::from(data))
                    .unwrap()
                    .into_response(),
                OptimizedFileContent::Mmap(mmap_data) => Response::builder()
                    .status(StatusCode::OK)
                    .header(header::CONTENT_TYPE, &*mime)
                    .header(header::CONTENT_DISPOSITION, &disposition)
                    .header(header::CONTENT_LENGTH, mmap_data.len())
                    .body(Body::from(mmap_data))
                    .unwrap()
                    .into_response(),
                OptimizedFileContent::Stream(stream) => Response::builder()
                    .status(StatusCode::OK)
                    .header(header::CONTENT_TYPE, &*mime)
                    .header(header::CONTENT_DISPOSITION, &disposition)
                    .header(header::CONTENT_LENGTH, file_dto.size)
                    .body(Body::from_stream(stream))
                    .unwrap()
                    .into_response(),
            }
        }
        Err(err) => AppError::from(err).into_response(),
    }
}
