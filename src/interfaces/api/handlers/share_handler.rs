use std::sync::Arc;
use uuid::Uuid;

use axum::{
    Json,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use serde_json::json;
use utoipa::ToSchema;

use crate::application::services::share_service::ShareService;
use crate::infrastructure::services::{share_ring, share_unlock_cookie};
use crate::{
    application::{
        dtos::share_dto::{CreateShareDto, UpdateShareDto},
        ports::share_ports::ShareUseCase,
    },
    common::errors::ErrorKind,
    domain::entities::share::ShareItemType,
    interfaces::errors::AppError,
    interfaces::middleware::auth::AuthUser,
};

fn unlock_jwt_from_headers(headers: &HeaderMap, share_token: &str) -> Option<String> {
    headers
        .get(header::COOKIE)
        .and_then(|h| h.to_str().ok())
        .and_then(|cookie_header| {
            share_unlock_cookie::extract_from_cookie_header(cookie_header, share_token)
        })
}

/// Attach the visitor's share ring to a successful unlock response.
///
/// Both unlock paths end here — the password-less `GET /api/s/{token}` and the
/// `POST /api/s/{token}/verify` that accepted a password — because "the share
/// opened" is the single condition that earns a ring, and duplicating the
/// cookie construction across the two would let them drift.
///
/// Failure is silent by design: a visitor who cannot receive the ring still
/// gets the share metadata and the legacy `/api/s/*` endpoints. The ring only
/// unlocks the *normal* API, so its absence degrades features, not access.
fn attach_ring_cookie(
    share_use_case: &ShareService,
    headers: &HeaderMap,
    share_id: &str,
    response: &mut Response,
) {
    let Ok(share_id) = Uuid::parse_str(share_id) else {
        return;
    };
    let existing = headers
        .get(header::COOKIE)
        .and_then(|h| h.to_str().ok())
        .and_then(share_ring::extract_from_cookie_header);

    match share_use_case.grant_ring(existing, share_id) {
        Ok(jwt) => {
            let cookie = share_ring::build_set_cookie(&jwt, share_ring::DEFAULT_TTL_SECS);
            if let Ok(value) = header::HeaderValue::from_str(&cookie) {
                // `append`, not `insert`: `/verify` also sets the legacy
                // per-share unlock cookie, and the two must both survive.
                response.headers_mut().append(header::SET_COOKIE, value);
            }
        }
        Err(e) => {
            tracing::warn!(
                target: "oxicloud::shares",
                share_id = %share_id,
                error = %e,
                "failed to mint share ring — visitor falls back to legacy share endpoints"
            );
        }
    }
}

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
    security(("bearerAuth" = [])),
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
    security(("bearerAuth" = [])),
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
    security(("bearerAuth" = [])),
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
    security(("bearerAuth" = [])),
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
    security(("bearerAuth" = [])),
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
    req: axum::extract::Request,
) -> impl IntoResponse {
    // Honour an unlock cookie if one was issued by a prior `/verify` call.
    // Borrow the headers (`req.headers()`) instead of the `HeaderMap` extractor's
    // full clone to read the unlock cookie (benches/ROUND22.md §H1).
    let unlock_jwt = unlock_jwt_from_headers(req.headers(), &token);

    // The access-count increment doesn't gate the fetch — run both
    // round-trips concurrently instead of serially (one RTT saved on
    // every public share landing).
    let (_, item) = tokio::join!(
        share_use_case.register_shared_link_access(&token),
        share_use_case.get_shared_link_with_unlock(&token, unlock_jwt.as_deref()),
    );

    match item {
        Ok(item) => {
            // Reaching `Ok` means the share is open to this caller: either it
            // has no password, or an unlock cookie satisfied it. That is
            // exactly the condition for handing over a ring, so the visitor of
            // a password-less share is session-bearing from the first request
            // with no extra round-trip.
            let mut response = (StatusCode::OK, Json(&item)).into_response();
            attach_ring_cookie(&share_use_case, req.headers(), &item.id, &mut response);
            response
        }
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
    headers: HeaderMap,
    Json(req): Json<VerifyPasswordRequest>,
) -> impl IntoResponse {
    match share_use_case
        .verify_shared_link_password(&token, &req.password)
        .await
    {
        Ok(item) => {
            let mut response = match share_use_case.issue_unlock_jwt(&token) {
                Ok(jwt) => {
                    let cookie = share_unlock_cookie::build_set_cookie(
                        &token,
                        &jwt,
                        share_unlock_cookie::DEFAULT_TTL_SECS,
                    );
                    (StatusCode::OK, [(header::SET_COOKIE, cookie)], Json(&item)).into_response()
                }
                Err(_) => (StatusCode::OK, Json(&item)).into_response(),
            };
            attach_ring_cookie(&share_use_case, &headers, &item.id, &mut response);
            response
        }
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
