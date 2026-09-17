//! Admin role guard — shared across handlers that gate on `claims.role == "admin"`.
//!
//! Extracted from `admin_handler.rs::admin_guard` so the subject-group
//! handler (and any future admin-only surface) can reuse the same code path
//! without duplication.
//!
//! Returns `(user_id, role)` on success so callers have the caller's UUID
//! for audit / ownership purposes.

use axum::http::{HeaderMap, StatusCode, header};
use smol_str::SmolStr;
use uuid::Uuid;

use crate::application::ports::auth_ports::TokenServicePort;
use crate::common::di::AppState;
use crate::domain::entities::user::UserRole;
use crate::interfaces::api::cookie_auth::{ACCESS_COOKIE, extract_cookie_value};
use crate::interfaces::errors::AppError;
use crate::interfaces::middleware::user::{LiveRole, resolve_live_role};

/// Validate the request's JWT (from the `Authorization: Bearer …` header
/// or the access-token cookie) and require `claims.role == "admin"`.
///
/// On success returns `(user_id, role)`; on failure returns:
///   - 401 if no token / invalid token,
///   - 403 if the token is valid but the role is not `admin`,
///   - 500 if the auth service is not configured.
pub async fn require_admin(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<(Uuid, SmolStr), AppError> {
    let auth = state
        .auth_service
        .as_ref()
        .ok_or_else(|| AppError::internal_error("Auth service not configured"))?;

    let token = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer ").map(|s| s.to_string()))
        .or_else(|| extract_cookie_value(headers, ACCESS_COOKIE))
        .ok_or_else(|| AppError::unauthorized("Authorization token required"))?;

    let claims = auth
        .token_service
        .validate_token(&token)
        .map_err(|e| AppError::unauthorized(format!("Invalid token: {}", e)))?;

    let user_id = Uuid::parse_str(&claims.sub)
        .map_err(|_| AppError::internal_error("Invalid user ID in token"))?;

    // Gate on the *live* role, not the JWT claim: a demotion or deactivation
    // must take effect within the flags-cache TTL rather than surviving until
    // the token expires.
    match resolve_live_role(
        auth.auth_application_service.as_ref(),
        user_id,
        &claims.role,
    )
    .await
    {
        // Rank, not spelling — the owner outranks admin and must pass.
        LiveRole::Active(role) if UserRole::str_at_least(&role, UserRole::Admin) => {
            Ok((user_id, role))
        }
        LiveRole::Active(role) => {
            tracing::info!(
                target: "audit",
                event = "authz.admin_denied",
                reason = "not_admin",
                caller_id = %user_id,
                role = %role,
                "👮🏻‍♂️ admin-only endpoint denied for non-admin caller"
            );
            Err(AppError::new(
                StatusCode::FORBIDDEN,
                "Admin access required",
                "Forbidden",
            ))
        }
        LiveRole::Revoked => Err(AppError::unauthorized("Account is no longer active")),
    }
}

/// Validate the request's JWT (any role) and return `(user_id, role)`.
///
/// Like `require_admin` but does not enforce the admin role — useful for
/// share-dialog autocomplete and similar surfaces that need a logged-in
/// caller but don't care about their role.
pub async fn require_authenticated(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<(Uuid, SmolStr), AppError> {
    let auth = state
        .auth_service
        .as_ref()
        .ok_or_else(|| AppError::internal_error("Auth service not configured"))?;

    let token = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer ").map(|s| s.to_string()))
        .or_else(|| extract_cookie_value(headers, ACCESS_COOKIE))
        .ok_or_else(|| AppError::unauthorized("Authorization token required"))?;

    let claims = auth
        .token_service
        .validate_token(&token)
        .map_err(|e| AppError::unauthorized(format!("Invalid token: {}", e)))?;

    let user_id = Uuid::parse_str(&claims.sub)
        .map_err(|_| AppError::internal_error("Invalid user ID in token"))?;

    // Reject tokens whose account was deactivated/deleted, and return the
    // caller's live role rather than the (possibly stale) JWT claim.
    match resolve_live_role(
        auth.auth_application_service.as_ref(),
        user_id,
        &claims.role,
    )
    .await
    {
        LiveRole::Active(role) => {
            // This function re-parses the token itself and never touches the
            // `AuthUser` / `CurrentUserId` extractors, so any rule added there
            // does not reach it. An anonymous public-share session must not
            // satisfy "is authenticated" — its one consumer,
            // `GET /api/groups/search`, is a group-directory enumeration.
            // See `src/AGENTS.md` § AuthZ enforcement points.
            if crate::domain::entities::user::UserRole::from_session(&role)
                .is_none_or(|r| r.is_anonymous())
            {
                tracing::info!(
                    target: "audit",
                    event = "authz.denied",
                    reason = "anonymous_not_authenticated",
                    caller_id = %user_id,
                    role = %role,
                    "👮🏻‍♂️ anonymous session refused a user-scoped endpoint",
                );
                return Err(AppError::forbidden("This endpoint requires a user account"));
            }
            Ok((user_id, role))
        }
        LiveRole::Revoked => Err(AppError::unauthorized("Account is no longer active")),
    }
}
