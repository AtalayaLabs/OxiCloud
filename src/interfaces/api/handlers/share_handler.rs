use std::sync::Arc;
use uuid::Uuid;

use axum::{
    Json,
    body::Body,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
};
use http_range_header::parse_range_header;
use serde::Deserialize;
use serde_json::json;
use utoipa::ToSchema;

use crate::application::ports::file_ports::RangeContent;
use crate::application::services::share_browse_service::ZipTarget;
use crate::application::services::share_service::ShareService;
use crate::infrastructure::services::{share_ring, share_unlock_cookie};
use crate::interfaces::api::handlers::file_handler::build_content_disposition;
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
    req: axum::extract::Request,
) -> impl IntoResponse {
    // Borrow the headers (`req.headers()`) instead of the `HeaderMap` extractor's
    // full clone — the public-share download + Range path (benches/ROUND22.md §H1).
    let headers = req.headers();
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
    let unlock_jwt = unlock_jwt_from_headers(headers, &token);
    let share_dto = match share_service
        .get_shared_link_with_unlock(&token, unlock_jwt.as_deref())
        .await
    {
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

    // 4. Stream the file with full Range / 304 / 416 / 206 support.
    serve_share_file(
        &state,
        &share_dto.item_id,
        share_dto.item_name.as_deref(),
        headers,
    )
    .await
}

/// Stream a file for a public share. Honours `If-None-Match` (304),
/// `Range` (206 / 416), and falls back to a 200 via `get_file_optimized`.
async fn serve_share_file(
    state: &Arc<AppState>,
    file_id: &str,
    name_override: Option<&str>,
    request_headers: &HeaderMap,
) -> Response {
    let retrieval = &state.applications.file_retrieval_service;

    let file_dto = match retrieval.get_file(file_id).await {
        Ok(d) => d,
        Err(err) => return AppError::from(err).into_response(),
    };

    let display_name = name_override.unwrap_or(&file_dto.name);
    let etag = format!("\"{}-{}\"", file_dto.id, file_dto.modified_at);
    let mime = file_dto.mime_type.clone();
    let disposition = build_content_disposition(display_name, &mime, false);

    if let Some(inm) = request_headers.get(header::IF_NONE_MATCH)
        && let Ok(client_etag) = inm.to_str()
        && (client_etag == etag || client_etag == "*")
    {
        return Response::builder()
            .status(StatusCode::NOT_MODIFIED)
            .header(header::ETAG, &etag)
            .header(
                header::CACHE_CONTROL,
                "private, max-age=3600, must-revalidate",
            )
            .header(header::VARY, "Cookie, Range")
            .body(Body::empty())
            .unwrap()
            .into_response();
    }

    if let Some(range_hdr) = request_headers.get(header::RANGE)
        && let Ok(range_str) = range_hdr.to_str()
        && let Ok(ranges) = parse_range_header(range_str)
    {
        match ranges.validate(file_dto.size) {
            Ok(valid_ranges) => {
                if let Some(range) = valid_ranges.first() {
                    let start = *range.start();
                    let end = *range.end();
                    let length = end - start + 1;

                    match retrieval
                        .get_file_range_preloaded(&file_dto, start, Some(end + 1))
                        .await
                    {
                        Ok(content) => {
                            let body = match content {
                                RangeContent::Bytes(b) => Body::from(b),
                                RangeContent::Stream(s) => Body::from_stream(Box::into_pin(s)),
                            };
                            return Response::builder()
                                .status(StatusCode::PARTIAL_CONTENT)
                                .header(header::CONTENT_TYPE, &*mime)
                                .header(header::CONTENT_DISPOSITION, &disposition)
                                .header(header::CONTENT_LENGTH, length)
                                .header(
                                    header::CONTENT_RANGE,
                                    format!("bytes {}-{}/{}", start, end, file_dto.size),
                                )
                                .header(header::ACCEPT_RANGES, "bytes")
                                .header(header::ETAG, &etag)
                                .header(
                                    header::CACHE_CONTROL,
                                    "private, max-age=3600, must-revalidate",
                                )
                                .header(header::VARY, "Cookie, Range")
                                .body(body)
                                .unwrap()
                                .into_response();
                        }
                        Err(err) => {
                            tracing::error!("share range stream error: {}", err);
                        }
                    }
                }
            }
            Err(_) => {
                return Response::builder()
                    .status(StatusCode::RANGE_NOT_SATISFIABLE)
                    .header(header::CONTENT_RANGE, format!("bytes */{}", file_dto.size))
                    .header(
                        header::CACHE_CONTROL,
                        "private, max-age=3600, must-revalidate",
                    )
                    .header(header::VARY, "Cookie, Range")
                    .body(Body::empty())
                    .unwrap()
                    .into_response();
            }
        }
    }

    // The metadata was already fetched at the top of this fn — hand the DTO
    // to the `_preloaded` variant (as the authenticated download path does)
    // instead of letting `get_file_optimized` re-run the same metadata query.
    let file_size = file_dto.size;
    match retrieval
        .get_file_optimized_preloaded(file_id, file_dto, false, true)
        .await
    {
        Ok((_, content)) => match content {
            OptimizedFileContent::Bytes { data, .. } => Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, &*mime)
                .header(header::CONTENT_DISPOSITION, &disposition)
                .header(header::CONTENT_LENGTH, data.len())
                .header(header::ACCEPT_RANGES, "bytes")
                .header(header::ETAG, &etag)
                .header(
                    header::CACHE_CONTROL,
                    "private, max-age=3600, must-revalidate",
                )
                .header(header::VARY, "Cookie, Range")
                .body(Body::from(data))
                .unwrap()
                .into_response(),
            OptimizedFileContent::Stream(stream) => Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, &*mime)
                .header(header::CONTENT_DISPOSITION, &disposition)
                .header(header::CONTENT_LENGTH, file_size)
                .header(header::ACCEPT_RANGES, "bytes")
                .header(header::ETAG, &etag)
                .header(
                    header::CACHE_CONTROL,
                    "private, max-age=3600, must-revalidate",
                )
                .header(header::VARY, "Cookie, Range")
                .body(Body::from_stream(stream))
                .unwrap()
                .into_response(),
        },
        Err(err) => AppError::from(err).into_response(),
    }
}

// ── Public folder browsing endpoints ──────────────────────────────────────

fn sharing_disabled_response() -> Response {
    AppError::new(
        StatusCode::SERVICE_UNAVAILABLE,
        "Sharing is disabled",
        "Disabled",
    )
    .into_response()
}

fn share_browse_error_response(err: crate::common::errors::DomainError) -> Response {
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

// TODO: remove this and use the classic /api/files & /api/folders get, but with the token as session ?
#[utoipa::path(
    get,
    path = "/api/s/{token}/contents",
    params(("token" = String, Path, description = "Share token")),
    responses(
        (status = 200, description = "Folder contents (sub-folders + files)"),
        (status = 400, description = "Share is not a folder share"),
        (status = 401, description = "Password required"),
        (status = 410, description = "Share expired"),
        (status = 503, description = "Sharing disabled")
    ),
    tag = "shares"
)]
pub async fn list_share_contents_root(
    State(state): State<Arc<AppState>>,
    Path(token): Path<String>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let Some(browse) = state.share_browse_service.clone() else {
        return sharing_disabled_response();
    };
    let unlock_jwt = unlock_jwt_from_headers(&headers, &token);

    match browse.list_root(&token, unlock_jwt.as_deref()).await {
        Ok(listing) => (StatusCode::OK, Json(listing)).into_response(),
        Err(err) => share_browse_error_response(err),
    }
}

#[utoipa::path(
    get,
    path = "/api/s/{token}/contents/{folder_id}",
    params(
        ("token" = String, Path, description = "Share token"),
        ("folder_id" = String, Path, description = "Subfolder ID (must be inside the share)")
    ),
    responses(
        (status = 200, description = "Subfolder contents"),
        (status = 400, description = "Share is not a folder share"),
        (status = 401, description = "Password required"),
        (status = 404, description = "Subfolder not found or not in share scope"),
        (status = 410, description = "Share expired"),
        (status = 503, description = "Sharing disabled")
    ),
    tag = "shares"
)]
pub async fn list_share_contents_subfolder(
    State(state): State<Arc<AppState>>,
    Path((token, folder_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let Some(browse) = state.share_browse_service.clone() else {
        return sharing_disabled_response();
    };
    let unlock_jwt = unlock_jwt_from_headers(&headers, &token);

    match browse
        .list_subfolder(&token, &folder_id, unlock_jwt.as_deref())
        .await
    {
        Ok(listing) => (StatusCode::OK, Json(listing)).into_response(),
        Err(err) => share_browse_error_response(err),
    }
}

#[utoipa::path(
    get,
    path = "/api/s/{token}/file/{file_id}",
    params(
        ("token" = String, Path, description = "Share token"),
        ("file_id" = String, Path, description = "File ID (must be inside the share)")
    ),
    responses(
        (status = 200, description = "File content (or 206 for Range request)"),
        (status = 206, description = "Partial Content"),
        (status = 304, description = "Not Modified"),
        (status = 401, description = "Password required"),
        (status = 404, description = "File not found or not in share scope"),
        (status = 410, description = "Share expired"),
        (status = 416, description = "Range not satisfiable")
    ),
    tag = "shares"
)]
pub async fn download_share_file_in_folder(
    State(state): State<Arc<AppState>>,
    Path((token, file_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let Some(browse) = state.share_browse_service.clone() else {
        return sharing_disabled_response();
    };
    let unlock_jwt = unlock_jwt_from_headers(&headers, &token);

    if let Err(err) = browse
        .assert_file_in_share(&token, &file_id, unlock_jwt.as_deref())
        .await
    {
        return share_browse_error_response(err);
    }

    serve_share_file(&state, &file_id, None, &headers).await
}

#[utoipa::path(
    get,
    path = "/api/s/{token}/zip",
    params(("token" = String, Path, description = "Share token")),
    responses(
        (status = 200, description = "ZIP archive of the shared folder"),
        (status = 400, description = "Share is not a folder share"),
        (status = 401, description = "Password required"),
        (status = 410, description = "Share expired"),
        (status = 503, description = "Sharing or ZIP service disabled")
    ),
    tag = "shares"
)]
pub async fn download_share_zip_root(
    State(state): State<Arc<AppState>>,
    Path(token): Path<String>,
    headers: HeaderMap,
) -> impl IntoResponse {
    serve_share_zip(state, token, None, headers).await
}

#[utoipa::path(
    get,
    path = "/api/s/{token}/zip/{folder_id}",
    params(
        ("token" = String, Path, description = "Share token"),
        ("folder_id" = String, Path, description = "Subfolder ID (must be inside the share)")
    ),
    responses(
        (status = 200, description = "ZIP archive of the subfolder"),
        (status = 401, description = "Password required"),
        (status = 404, description = "Subfolder not found or not in share scope"),
        (status = 410, description = "Share expired"),
        (status = 503, description = "Sharing or ZIP service disabled")
    ),
    tag = "shares"
)]
pub async fn download_share_zip_subfolder(
    State(state): State<Arc<AppState>>,
    Path((token, folder_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> impl IntoResponse {
    serve_share_zip(state, token, Some(folder_id), headers).await
}

async fn serve_share_zip(
    state: Arc<AppState>,
    token: String,
    folder_id: Option<String>,
    headers: HeaderMap,
) -> Response {
    let Some(browse) = state.share_browse_service.clone() else {
        return sharing_disabled_response();
    };
    let zip_service = match &state.core.zip_service {
        Some(svc) => svc,
        None => {
            return AppError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "ZIP service not initialized",
                "Disabled",
            )
            .into_response();
        }
    };
    let unlock_jwt = unlock_jwt_from_headers(&headers, &token);

    let target: ZipTarget = match browse
        .resolve_zip_target(&token, folder_id.as_deref(), unlock_jwt.as_deref())
        .await
    {
        Ok(t) => t,
        Err(err) => return share_browse_error_response(err),
    };

    // Streamed archive: first byte after the first entry, not after the
    // whole ZIP is built (benches/ZIP-STREAM.md). No Content-Length.
    let stream = match zip_service
        .create_folder_zip_stream(&target.folder_id, &target.display_name)
        .await
    {
        Ok(s) => s,
        Err(err) => {
            tracing::error!("share zip: create_folder_zip failed: {}", err);
            return AppError::internal_error(format!("ZIP creation failed: {}", err))
                .into_response();
        }
    };
    let body = Body::from_stream(stream);

    let disposition = build_content_disposition(
        &format!("{}.zip", target.display_name),
        "application/zip",
        false,
    );

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/zip")
        .header(header::CONTENT_DISPOSITION, disposition)
        .header(header::CACHE_CONTROL, "private, no-store")
        .header(header::VARY, "Cookie")
        .body(body)
        .unwrap()
}
