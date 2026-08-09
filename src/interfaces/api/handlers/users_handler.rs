//! User-profile lookup for the frontend.
//!
//! `GET /api/users/{id}` returns a [`UserDto`] for the target user iff
//! the authenticated caller has a legitimate relationship with them.
//! The visibility rule lives in
//! [`AuthApplicationService::get_user_profile`] — handlers never embed
//! their own authz check (CLAUDE.md § Authorization).
//!
//! In addition to the per-request visibility check, every call is
//! throttled by a per-caller sliding-window limiter (60/min) so that a
//! stale JWT can't iterate UUIDs against the related-by-grant branch
//! of the visibility rule. The limiter shares the same `RateLimiter`
//! type as the login / register / refresh middlewares; this handler
//! invokes it inline rather than through a layer because the key is
//! the authenticated caller_id (not the client IP).

use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    routing::get,
};
use std::sync::Arc;
use uuid::Uuid;

use crate::common::di::AppState;
use crate::interfaces::errors::AppError;
use crate::interfaces::middleware::auth::AuthUser;

/// Build the `/users` router — mounted at the `/api/users` prefix by
/// `main.rs`. Auth + CSRF middlewares are applied by the caller.
pub fn user_routes() -> Router<Arc<AppState>> {
    Router::new().route("/{id}", get(get_user_profile))
}

#[utoipa::path(
    get,
    path = "/api/users/{id}",
    params(("id" = String, Path, description = "User UUID")),
    responses(
        (status = 200, description = "Profile of a user the caller can see"),
        (status = 404, description = "User does not exist OR caller has no visibility (anti-enumeration: indistinguishable)"),
        (status = 429, description = "Per-caller rate limit exceeded"),
    ),
    security(("bearerAuth" = [])),
    tag = "users",
)]
pub async fn get_user_profile(
    State(state): State<Arc<AppState>>,
    auth_user: AuthUser,
    Path(target_id): Path<Uuid>,
) -> Result<impl IntoResponse, AppError> {
    let caller_id = auth_user.id;

    // Rate limit FIRST so an attacker can't exhaust the visibility
    // query (which touches `access_grants`) by hammering with random
    // UUIDs.
    if let Err(()) = state
        .user_profile_rate_limiter
        .check_and_increment(&caller_id.to_string())
    {
        return Err(AppError::new(
            StatusCode::TOO_MANY_REQUESTS,
            "Too many user lookups; please retry shortly",
            "RateLimited",
        ));
    }

    let auth_svc = state
        .auth_service
        .as_ref()
        .ok_or_else(|| AppError::internal_error("Auth service not configured"))?;
    let pool = state
        .db_pool
        .as_ref()
        .ok_or_else(|| AppError::internal_error("Database pool not available"))?;

    let dto = auth_svc
        .auth_application_service
        .get_user_profile(
            caller_id,
            target_id,
            state.core.config.features.expose_system_users,
            pool,
        )
        .await
        .map_err(AppError::from)?;

    Ok(Json(dto))
}
