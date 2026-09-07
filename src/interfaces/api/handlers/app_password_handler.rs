//! HTTP handlers for App Password management.
//!
//! All endpoints require JWT authentication (the user must be logged in to
//! create/list/revoke their app passwords).

use crate::application::dtos::app_password_dto::CreateAppPasswordRequestDto;
use crate::common::di::AppState;
use crate::interfaces::errors::AppError;
use crate::interfaces::middleware::auth::AuthUser;
use axum::extract::State;
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use std::sync::Arc;
use uuid::Uuid;

/// Protected routes — require JWT auth middleware.
pub fn app_password_routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/app-passwords", post(create_app_password))
        .route("/app-passwords", get(list_app_passwords))
        .route("/app-passwords/{id}", delete(revoke_app_password))
}

/// POST /api/auth/app-passwords — Create a new app password.
///
/// Returns the plain-text password ONCE. The user must copy it immediately.
///
/// External users are rejected with 403: app passwords are persistent
/// credentials, and the magic-link-eligibility rule (`magic_link_eligibility`)
/// is built on the assumption that externals have NO other credential
/// configured. Letting an external mint an app password would break that
/// invariant — and the Basic-Auth surface (`/remote.php/*`, `/ocs/*`)
/// has no semantic meaning for them anyway. See the
/// [magic-link auth architecture page] for the full visibility model.
///
/// [magic-link auth architecture page]: ../../../../docs/architecture/magic-link-auth.md
async fn create_app_password(
    State(state): State<Arc<AppState>>,
    user: AuthUser,
    Json(request): Json<CreateAppPasswordRequestDto>,
) -> Result<Json<crate::application::dtos::app_password_dto::AppPasswordCreatedResponseDto>, AppError>
{
    // Gate externals BEFORE we touch the app_password_service. The
    // service treats every authenticated caller equally; the policy
    // that externals can't hold persistent credentials lives here.
    if let Some(auth_svc) = state.auth_service.as_ref()
        && let Err(err) = crate::interfaces::middleware::user::require_internal_user(
            &auth_svc.auth_application_service,
            user.id,
        )
        .await
    {
        tracing::info!(
            target: "audit",
            event = "auth.app_password_create_rejected",
            reason = "external_user",
            caller_id = %user.id,
            "👮🏻‍♂️ External user blocked from creating an app password"
        );
        return Err(err);
    }

    // Require a claimed username. NextCloud Basic Auth resolves users by
    // username; an app password is unusable without one. PublicUserDto carries
    // an empty string when the underlying `users.username` is NULL — the
    // entity rejects empty strings on construction, so empty here is an
    // unambiguous signal that the column is NULL.
    if let Some(auth_svc) = state.auth_service.as_ref() {
        let user_dto = auth_svc
            .auth_application_service
            .get_user_by_id(user.id)
            .await
            .map_err(AppError::from)?;
        if user_dto.username.is_none() {
            tracing::info!(
                target: "audit",
                event = "auth.app_password_create_rejected",
                reason = "no_username",
                caller_id = %user.id,
                "App-password creation requires a claimed username"
            );
            return Err(AppError::new(
                axum::http::StatusCode::CONFLICT,
                "Claim a username on your profile before creating an app password.",
                "UsernameRequired",
            ));
        }
    }

    let service = state
        .app_password_service
        .as_ref()
        .ok_or_else(|| AppError::internal_error("App password service not configured"))?;

    let response = service
        .create(user.id, request)
        .await
        .map_err(AppError::from)?;

    Ok(Json(response))
}

/// GET /api/auth/app-passwords — List all app passwords for the current user.
///
/// Never returns plain-text passwords (only prefix + metadata).
async fn list_app_passwords(
    State(state): State<Arc<AppState>>,
    user: AuthUser,
) -> Result<Json<crate::application::dtos::app_password_dto::AppPasswordListResponseDto>, AppError>
{
    let service = state
        .app_password_service
        .as_ref()
        .ok_or_else(|| AppError::internal_error("App password service not configured"))?;

    let response = service.list(user.id).await.map_err(AppError::from)?;

    Ok(Json(response))
}

/// DELETE /api/auth/app-passwords/:id — Revoke an app password.
async fn revoke_app_password(
    State(state): State<Arc<AppState>>,
    user: AuthUser,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Result<Json<crate::application::dtos::app_password_dto::AppPasswordRevokeResponseDto>, AppError>
{
    let service = state
        .app_password_service
        .as_ref()
        .ok_or_else(|| AppError::internal_error("App password service not configured"))?;

    let id = Uuid::parse_str(&id).map_err(|_| AppError::bad_request("Invalid UUID"))?;

    let response = service.revoke(user.id, id).await.map_err(AppError::from)?;

    Ok(Json(response))
}
